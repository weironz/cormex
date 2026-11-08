#!/usr/bin/env python3
"""Build once, stage a reviewable candidate, and publish those exact bytes.

Only stage/publish/dispatch change remote state. Checks and packaging are shared
by the local and Actions entry points; publishing never invokes a compiler.
"""
from __future__ import annotations

import argparse
import copy
from datetime import date, datetime, timezone
from contextlib import contextmanager
import gzip
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import tomllib
import uuid
import zipfile

ROOT = Path(__file__).resolve().parents[1]
TARGETS = (
    "x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin", "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc",
)
IMAGES = ("cormex", "cormex-web")
REQUIRED_FLUTTER_VERSION = "3.44.1"
REGISTRIES = ("willdockerhub", "registry.cn-shenzhen.aliyuncs.com/willspace")
HEX = re.compile(r"[0-9a-f]{64}\Z")
COMMIT = re.compile(r"[0-9a-f]{40}\Z")


class ReleaseError(Exception):
    pass


def require(ok, message):
    if not ok:
        raise ReleaseError(message)


def validate_tag(tag):
    require(isinstance(tag, str) and re.fullmatch(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", tag),
            "tag must be vX.Y.Z (stable numeric version only)")
    return tag[1:]


def validate_checksum(checksum):
    require(isinstance(checksum, str) and HEX.fullmatch(checksum),
            "checksum must be 64 lowercase hexadecimal characters")
    return checksum


def child_environment(overrides=None):
    settings = dict(os.environ, MSYS_NO_PATHCONV="1", MSYS2_ARG_CONV_EXCL="*")
    for key, value in (overrides or {}).items():
        if value is None:
            settings.pop(key, None)
        else:
            settings[key] = value
    return settings


def run(*args, capture=False, cwd=None, env=None, input=None):
    # Never interpolate tag/target/user input into a shell command.
    result = subprocess.run([str(x) for x in args], cwd=cwd or ROOT, env=child_environment(env),
                            text=True, input=input, stdout=subprocess.PIPE if capture else None,
                            stderr=subprocess.PIPE if capture else None, check=False)
    if result.returncode:
        raise ReleaseError(f"{args[0]} {args[1] if len(args) > 1 else ''} failed ({result.returncode})"
                           + (f": {result.stderr.strip()}" if capture else ""))
    return result.stdout.strip() if capture else ""


def digest(path):
    with Path(path).open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()


def read_json(path):
    # Duplicate keys otherwise hide the value a reviewer actually saw.
    def pairs(items):
        result = {}
        for key, value in items:
            require(key not in result, f"duplicate JSON key: {key}")
            result[key] = value
        return result
    try:
        return json.loads(Path(path).read_text(encoding="utf-8"), object_pairs_hook=pairs)
    except (OSError, ValueError) as error:
        raise ReleaseError(f"cannot read JSON {Path(path).name}: {error}") from error


def write_json(path, value):
    Path(path).write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def output_dir(tag):
    validate_tag(tag)
    return ROOT / "dist" / tag


def repository():
    value = os.environ.get("CORMEX_REPOSITORY", "weironz/cormex")
    require(re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", value), "invalid CORMEX_REPOSITORY")
    return value


def api(path):
    return json.loads(run("gh", "api", f"repos/{repository()}/{path}", capture=True))


def remote_commit(tag):
    validate_tag(tag)
    obj = api(f"git/ref/tags/{tag}")["object"]
    for _ in range(8):
        if obj["type"] == "commit":
            require(COMMIT.fullmatch(obj["sha"]), "remote tag has an invalid commit")
            return obj["sha"]
        require(obj["type"] == "tag", "remote version tag does not reference a commit")
        obj = api(f"git/tags/{obj['sha']}")["object"]
    raise ReleaseError("too many nested annotated tags")


def source_context(tag):
    version = validate_tag(tag)
    require(not run("git", "status", "--porcelain", "--untracked-files=all", capture=True),
            "the source tree is dirty; commit all source changes before building a candidate")
    commit = run("git", "rev-parse", "HEAD", capture=True)
    require(COMMIT.fullmatch(commit), "HEAD must resolve to a commit")
    tagged = run("git", "rev-parse", f"refs/tags/{tag}^{{commit}}", capture=True)
    require(tagged == commit, f"{tag} does not identify HEAD")
    validate_versions(version)
    return {"tag": tag, "version": version, "commit": commit,
            "source": "github" if os.environ.get("GITHUB_ACTIONS") == "true" else "local"}


def preflight(tag, profile=None):
    profile = profile or ("source" if os.environ.get("GITHUB_ACTIONS") == "true" else "local")
    require(profile in ("source", "local"), "invalid preflight profile")
    context = source_context(tag)
    report = doctor(profile=profile, emit=False)
    failures = [check["name"] for check in report["checks"] if check["required"] and not check["ok"]]
    require(not failures, "preflight failed: " + ", ".join(failures) + "; run doctor for details")
    require(remote_commit(tag) == context["commit"], "remote tag differs from the source commit")
    run("cargo", "metadata", "--locked", "--format-version", "1", capture=True)
    print(f"source verified: {tag} {context['commit']}")
    return report


def archive_name(version, target):
    return f"cormex-{version}-{target}." + ("zip" if "windows" in target else "tar.gz")


def image_name(version, name):
    return f"{name}-{version}-linux-amd64.docker.tar.gz"


def ensure_new(path):
    require(not path.exists() and not path.is_symlink(), f"refusing to replace {path}; use a new candidate version")
    path.parent.mkdir(parents=True, exist_ok=True)


def cli_builder(target, host=None):
    """Cargo subcommand that produces `target` without a native toolchain on this host.

    musl goes through cargo-zigbuild; the MSVC targets go through cargo-xwin, which
    downloads the CRT and Windows SDK from Microsoft's CDN -- but only when this host is
    not Windows, because the Actions matrix builds those targets natively and has no
    cargo-xwin. Every other target links against the platform's own SDK, so it is only
    buildable on that platform.
    """
    host = host or sys.platform
    if target.endswith("-unknown-linux-musl"):
        return ["zigbuild"], "cargo-zigbuild"
    if target.endswith("-pc-windows-msvc") and host != "win32":
        return ["xwin", "build"], "cargo-xwin"
    return ["build"], None


def build_cli(tag, target):
    require(target in TARGETS, "unsupported CLI target")
    context = source_context(tag)
    directory = output_dir(tag)
    name = archive_name(context["version"], target)
    path = directory / name
    sidecar = directory / f"{target}.cli.json"
    ensure_new(path)
    ensure_new(sidecar)
    subcommand, tool = cli_builder(target)
    require(tool is None or shutil.which(tool) is not None,
            f"{target} needs {tool}; install it (cargo install {tool}) before building this target")
    run("rustup", "target", "add", target)
    run("cargo", *subcommand,
        "--release", "--locked", "-p", "cormex", "--target", target,
        "--target-dir", ROOT / "target")
    binary = ROOT / "target" / target / "release" / ("cormex.exe" if "windows" in target else "cormex")
    host = next(line[6:] for line in run("rustc", "-vV", capture=True).splitlines() if line.startswith("host: "))
    native = target == host or (host.endswith("-unknown-linux-gnu") and target == host.replace("-gnu", "-musl"))
    if native:
        require(run(binary, "--version", capture=True) == f"cormex {context['version']}", "CLI reports the wrong version")
    else:
        print(f"not executing cross target {target} on {host}")
    inner = f"cormex-{context['version']}-{target}"
    sources = [(binary, binary.name), (ROOT / "README.md", "README.md"), (ROOT / "LICENSE", "LICENSE")]
    if path.suffix == ".zip":
        with zipfile.ZipFile(path, "x", compression=zipfile.ZIP_DEFLATED) as archive:
            for source, filename in sources:
                archive.write(source, f"{inner}/{filename}")
    else:
        with tarfile.open(path, "w:gz") as archive:
            for source, filename in sources:
                archive.add(source, arcname=f"{inner}/{filename}", recursive=False)
    entry = {"target": target, "file": name, "sha256": digest(path)}
    write_json(sidecar, {**context, "cli": entry})


DEFAULT_LOCAL_TARGETS = {
    "linux": ["x86_64-unknown-linux-musl", "x86_64-pc-windows-msvc"],
    "darwin": ["x86_64-unknown-linux-musl", "x86_64-apple-darwin"],
    "win32": ["x86_64-pc-windows-msvc"],
}


def local_targets():
    """CLI targets a local candidate builds; `CORMEX_CLI_TARGETS` replaces the default.

    The default is the x86_64 set this host can actually produce: musl and MSVC cross
    compile from Linux, macOS builds its own. The aarch64 targets and the second Apple
    target are off by default -- they need a native runner or an SDK this host does not
    have -- and `CORMEX_CLI_TARGETS` is the opt-in, so enabling them never means editing
    this file. Anything named here is built and packed, and the rest are recorded as
    missing in the manifest.
    """
    override = os.environ.get("CORMEX_CLI_TARGETS", "")
    if override.strip():
        targets = [target.strip() for target in override.split(",") if target.strip()]
        for target in targets:
            require(target in TARGETS, f"unsupported CLI target in CORMEX_CLI_TARGETS: {target}")
        require(len(set(targets)) == len(targets), "CORMEX_CLI_TARGETS names a target twice")
        return targets
    return list(DEFAULT_LOCAL_TARGETS[sys.platform])


def build_cli_local(tag):
    targets = local_targets()
    for target in targets:
        build_cli(tag, target)
    missing = sorted(set(TARGETS) - set(targets))
    if missing:
        print("WARNING: missing CLI targets: " + ", ".join(missing))


def image_info(reference):
    result = json.loads(run("docker", "image", "inspect", reference, capture=True))[0]
    require(result["Os"] == "linux" and result["Architecture"] == "amd64", "image must be linux/amd64")
    require(re.fullmatch(r"sha256:[0-9a-f]{64}", result["Id"]), "invalid image ID")
    return result["Id"]


def smoke_image(name, reference, version):
    if name == "cormex":
        require(run("docker", "run", "--rm", reference, "--version", capture=True) == f"cormexd {version}",
                "server image reports the wrong version")
        run("docker", "run", "--rm", "--entrypoint", "/bin/sh", reference, "-c",
            "test -d /opt/cormex/migrations && test -d /opt/cormex/migrations-global")
        return
    container = "cormex-smoke-" + uuid.uuid4().hex[:12]
    run("docker", "run", "-d", "--name", container, reference, capture=True)
    try:
        for _ in range(60):
            status = json.loads(run("docker", "inspect", container, capture=True))[0]
            if status["State"].get("Health", {}).get("Status") == "healthy":
                break
            require(status["State"]["Running"], "web smoke container exited")
            time.sleep(2)
        else:
            raise ReleaseError("web smoke container did not become healthy")
        # Fetch inside the container: no fixed host port or localhost proxy assumptions.
        html = run("docker", "exec", container, "wget", "-qO-", "http://127.0.0.1/", capture=True)
        require("<title>" in html.lower(), "web root did not return a real HTML document")
        js = run("docker", "exec", container, "wget", "-qO-", "http://127.0.0.1/main.dart.js", capture=True)
        require('"/api"' in js or "'/api'" in js, "web bundle lost its /api prefix")
    finally:
        run("docker", "rm", "-f", container, capture=True)


def build_images(tag, names=IMAGES):
    context = source_context(tag)
    directory = output_dir(tag)
    for name in names:
        require(name in IMAGES, "unsupported image name")
        path = directory / image_name(context["version"], name)
        sidecar = directory / f"{name}.image.json"
        ensure_new(path)
        ensure_new(sidecar)
        reference = f"{name}:{tag}"
        dockerfile = "Dockerfile.cormexd" if name == "cormex" else "Dockerfile.web"
        run("docker", "buildx", "build", "--platform", "linux/amd64", "--provenance=false", "--load",
            "-f", f"scripts/docker/{dockerfile}", "-t", reference, ".")
        identity = image_info(reference)
        smoke_image(name, identity, context["version"])
        # Save the image ID, not an unrelated mutable local tag. Loading cannot replace
        # an existing local tag before verification because this archive has no RepoTags.
        with tempfile.TemporaryDirectory(dir=directory) as temp:
            raw = Path(temp) / "image.tar"
            run("docker", "image", "save", "--output", raw, identity)
            with raw.open("rb") as source, path.open("xb") as dest:
                with gzip.GzipFile(filename="", mode="wb", fileobj=dest, mtime=0) as compressed:
                    shutil.copyfileobj(source, compressed)
        require(path.stat().st_size < 2 * 1024**3, "image archive exceeds GitHub's 2 GiB asset limit")
        entry = {"name": name, "file": path.name, "sha256": digest(path),
                 "image_id": identity, "platform": "linux/amd64",
                 "content_sha256": archive_content_identity(path)}
        require(daemon_image_content(identity) == entry["content_sha256"], "saved image content mismatch")
        if name == "cormex-web":
            entry["web_main_js_sha256"] = web_image_hash(identity)
        write_json(sidecar, {**context, "image": entry})


def verify_bundle(directory, tag, checksum=None):
    version = validate_tag(tag)
    directory = Path(directory)
    require(directory.is_dir() and not directory.is_symlink(), "candidate directory is missing or a symlink")
    paths = list(directory.iterdir())
    require(all(p.is_file() and not p.is_symlink() for p in paths), "candidate may contain only regular files, no directories or symlinks")
    require((directory / "SHA256SUMS").is_file(), "SHA256SUMS is missing")
    if checksum is not None:
        validate_checksum(checksum)
        require(digest(directory / "SHA256SUMS") == checksum, "candidate checksum differs from the reviewed BUNDLE_SHA256")
    sums = {}
    raw_sums = (directory / "SHA256SUMS").read_bytes()
    require(raw_sums.endswith(b"\n") and b"\r" not in raw_sums,
            "SHA256SUMS must use LF-terminated lines")
    for line in raw_sums.decode("utf-8").splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9][A-Za-z0-9._-]*)", line)
        require(match is not None, "invalid checksum line or unsafe filename")
        value, filename = match.groups()
        require(filename not in sums, f"duplicate checksum filename: {filename}")
        require(filename != "SHA256SUMS", "SHA256SUMS cannot checksum itself")
        sums[filename] = value
    require("manifest.json" in sums, "manifest checksum is missing")
    require({p.name for p in paths} == set(sums) | {"SHA256SUMS"}, "candidate file set differs from SHA256SUMS")
    for filename, value in sums.items():
        require(digest(directory / filename) == value, f"checksum mismatch: {filename}")
    manifest = read_json(directory / "manifest.json")
    require(isinstance(manifest, dict), "manifest must be an object")
    require(type(manifest.get("schema_version")) is int and manifest["schema_version"] == 1, "unsupported manifest schema")
    require(manifest.get("project") == "cormex", "candidate is not Cormex")
    require(manifest.get("tag") == tag and manifest.get("version") == version, "candidate tag/version mismatch")
    require(isinstance(manifest.get("commit"), str) and COMMIT.fullmatch(manifest["commit"]), "invalid source commit")
    require(manifest.get("source") in ("local", "github"), "invalid build source")
    cli, images = manifest.get("cli"), manifest.get("images")
    require(isinstance(cli, list) and len(cli) > 0, "candidate needs at least one CLI archive")
    require(isinstance(images, list) and len(images) == 2, "candidate must include both images")
    targets, names, payloads = [], [], set()
    for entry in cli:
        require(isinstance(entry, dict), "invalid CLI entry")
        target = entry.get("target")
        require(target in TARGETS and target not in targets, "unsupported or duplicate CLI target")
        filename = archive_name(version, target)
        require(entry.get("file") == filename, "CLI archive filename mismatch")
        require(entry.get("sha256") == sums.get(filename) and filename in sums, "CLI manifest digest mismatch")
        targets.append(target)
        payloads.add(filename)
    for entry in images:
        require(isinstance(entry, dict), "invalid image entry")
        name = entry.get("name")
        require(name in IMAGES and name not in names, "unsupported or duplicate image")
        filename = image_name(version, name)
        require(entry.get("file") == filename, "image archive filename mismatch")
        require(entry.get("sha256") == sums.get(filename) and filename in sums, "image manifest digest mismatch")
        require(entry.get("platform") == "linux/amd64", "unsupported image platform")
        require(isinstance(entry.get("image_id"), str) and re.fullmatch(r"sha256:[0-9a-f]{64}", entry["image_id"]), "invalid image ID")
        for field in ("content_sha256", "web_main_js_sha256"):
            if field in entry:
                validate_checksum(entry[field])
        require("web_main_js_sha256" not in entry or name == "cormex-web", "web digest belongs only to the web image")
        names.append(name)
        payloads.add(filename)
    require(set(sums) == payloads | {"manifest.json"}, "unrecognized candidate payload")
    require(manifest.get("targets") == sorted(targets), "manifest targets differ from CLI entries")
    require(manifest.get("missing_targets") == sorted(set(TARGETS) - set(targets)), "missing targets must be explicitly and accurately recorded")
    return manifest


def create_bundle(tag, require_all_targets=False):
    context = source_context(tag)
    directory = output_dir(tag)
    require(directory.is_dir(), "no build outputs to bundle")
    ensure_new(directory / "manifest.json")
    ensure_new(directory / "SHA256SUMS")
    cli, images, sidecars = [], [], []
    for suffix, key, entries in ((".cli.json", "cli", cli), (".image.json", "image", images)):
        for path in sorted(directory.glob("*" + suffix)):
            require(not path.is_symlink(), "metadata sidecar cannot be a symlink")
            data = read_json(path)
            require(all(data.get(key) == value for key, value in context.items()), "mixed source/version/build-channel metadata")
            entries.append(data[key])
            sidecars.append(path)
    require(cli and len(images) == 2, "candidate requires CLI archives and both Docker images")
    targets = sorted(entry["target"] for entry in cli)
    missing = sorted(set(TARGETS) - set(targets))
    require(not require_all_targets or not missing, "Actions candidate is missing CLI targets")
    manifest = {"schema_version": 1, "project": "cormex", **context,
                "targets": targets, "missing_targets": missing, "cli": cli, "images": images}
    # Validate before touching original build metadata. A failed aggregation leaves
    # all sidecars available for diagnosis instead of consuming a partial candidate.
    with tempfile.TemporaryDirectory() as temp:
        check = Path(temp)
        for entry in cli + images:
            filename = entry.get("file", "")
            require(re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", filename), "unsafe payload filename")
            source = directory / filename
            require(source.is_file() and not source.is_symlink(), f"missing regular payload: {filename}")
            require(source.stat().st_size < 2 * 1024**3, "asset exceeds GitHub's 2 GiB limit")
            # Hard links avoid another copy of gigabyte image archives.
            try:
                os.link(source, check / filename)
            except OSError:
                shutil.copyfile(source, check / filename)
        write_json(check / "manifest.json", manifest)
        files = sorted(p.name for p in check.iterdir())
        (check / "SHA256SUMS").write_text("".join(f"{digest(check / f)}  {f}\n" for f in files), encoding="utf-8")
        verify_bundle(check, tag)
        expected = {entry["file"] for entry in cli + images} | {p.name for p in sidecars}
        require({p.name for p in directory.iterdir()} == expected, "unexpected files in build output directory")
        shutil.copyfile(check / "manifest.json", directory / "manifest.json")
        shutil.copyfile(check / "SHA256SUMS", directory / "SHA256SUMS")
    for path in sidecars:
        path.unlink()
    verify_bundle(directory, tag)
    print_summary(directory, manifest)


def print_summary(directory, manifest):
    checksum = digest(directory / "SHA256SUMS")
    summary = f"BUNDLE_SHA256={checksum}\nTag: {manifest['tag']}\nCommit: {manifest['commit']}\n"
    summary += "CLI targets: " + ", ".join(manifest["targets"]) + "\n"
    if manifest["missing_targets"]:
        summary += "WARNING: missing CLI targets: " + ", ".join(manifest["missing_targets"]) + "\n"
    print(summary)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as file:
            file.write("```text\n" + summary + "```\n")


def find_release(tag):
    # A failed authenticated read must never be mistaken for "does not exist".
    pages = json.loads(run("gh", "api", "--paginate", "--slurp",
                           f"repos/{repository()}/releases?per_page=100", capture=True))
    matches = [release for page in pages for release in page if release["tag_name"] == tag]
    require(len(matches) <= 1, "multiple releases exist for this version")
    return matches[0] if matches else None


@contextmanager
def version_lock(tag):
    lock = ROOT / "dist" / f".{tag}.lock"
    lock.parent.mkdir(parents=True, exist_ok=True)
    try:
        lock.mkdir()
    except FileExistsError as error:
        raise ReleaseError(f"another local operation owns {lock.name}; do not run both channels concurrently") from error
    try:
        yield
    finally:
        lock.rmdir()


def stage(tag):
    validate_tag(tag)
    with version_lock(tag):
        directory = output_dir(tag)
        manifest = verify_bundle(directory, tag)
        require(remote_commit(tag) == manifest["commit"], "remote tag differs from candidate source")
        require(find_release(tag) is None, "a release/candidate already exists; never overwrite its assets")
        files = [str(path) for path in sorted(directory.iterdir())]
        run("gh", "release", "create", tag, *files, "--repo", repository(), "--draft", "--verify-tag", "--generate-notes")
        release = find_release(tag)
        require(release is not None and release["draft"], "draft upload outcome is uncertain; inspect it before retrying")
        with tempfile.TemporaryDirectory() as temp:
            run("gh", "release", "download", tag, "--repo", repository(), "--dir", temp)
            verify_bundle(Path(temp), tag, digest(directory / "SHA256SUMS"))
        print_summary(directory, manifest)
        print("Candidate staged as a draft. No production images or public Release were published.")


# Explicit sibling loading also works when imported by the offline test harness or
# by a controller; it never imports code from an untrusted candidate directory.
_identity_spec = importlib.util.spec_from_file_location("cormex_image_identity", Path(__file__).with_name("image_identity.py"))
_identity = importlib.util.module_from_spec(_identity_spec)
_identity_spec.loader.exec_module(_identity)
portable_image_content = _identity.portable_image_content
archive_content_identity = _identity.archive_content_identity
image_content_identity = _identity.image_content_identity
archive_image_content = archive_content_identity


def daemon_image_content(reference):
    inspected = json.loads(run("docker", "image", "inspect", reference, capture=True))
    require(isinstance(inspected, list) and len(inspected) == 1, "expected one inspected image")
    return image_content_identity(inspected[0])


def web_image_hash(reference):
    output = run("docker", "run", "--rm", "--entrypoint", "sha256sum", reference,
                 "/usr/share/nginx/html/main.dart.js", capture=True).split()
    require(bool(output), "web image returned no JavaScript hash")
    return validate_checksum(output[0])


def load_images(directory, manifest):
    loaded = {}
    for entry in manifest["images"]:
        path = Path(directory) / entry["file"]
        require(digest(path) == entry["sha256"], "image archive changed before load")
        expected = archive_image_content(path)
        require(entry.get("content_sha256", expected) == expected, "image content metadata mismatch")
        output = run("docker", "image", "load", "--input", path, capture=True)
        identities = re.findall(r"^Loaded image(?: ID)?: (sha256:[0-9a-f]{64})\s*$", output, re.MULTILINE)
        require(len(identities) == 1, "docker load did not identify exactly one untagged image")
        identity = image_info(identities[0])
        require(daemon_image_content(identity) == expected, "loaded image content mismatch")
        smoke_image(entry["name"], identity, manifest["version"])
        if "web_main_js_sha256" in entry:
            require(web_image_hash(identity) == entry["web_main_js_sha256"], "web JavaScript content mismatch")
        loaded[entry["name"]] = {"image_id": identity, "content_sha256": expected}
    return loaded


def registry_image(reference):
    """Read registry content through its immutable digest, never a cached local tag."""
    result = subprocess.run(["docker", "buildx", "imagetools", "inspect", reference,
                             "--format", "{{json .Manifest}}"], cwd=ROOT,
                            text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    if result.returncode:
        message = result.stderr.lower()
        absent = any(text in message for text in ("no such manifest", "manifest unknown"))
        absent = absent or message.strip().endswith(reference.lower() + ": not found")
        require(absent and not any(text in message for text in ("unauthorized", "denied", "timeout")),
                f"cannot determine whether registry image exists: {reference}")
        return None
    descriptor = json.loads(result.stdout)
    identity = descriptor.get("digest")
    require(isinstance(identity, str) and re.fullmatch(r"sha256:[0-9a-f]{64}", identity),
            "registry returned no valid manifest digest")
    if "manifests" in descriptor:
        manifests = descriptor["manifests"]
        require(isinstance(manifests, list) and len(manifests) == 1
                and manifests[0].get("platform", {}).get("os") == "linux"
                and manifests[0].get("platform", {}).get("architecture") == "amd64",
                "registry index must contain exactly one linux/amd64 image")
    pinned = reference.rsplit(":", 1)[0] + "@" + identity
    run("docker", "pull", "--platform", "linux/amd64", pinned, capture=True)
    return {"digest": identity, "content_sha256": daemon_image_content(pinned), "reference": pinned}


def registry_login():
    credentials = (("docker.io", "DOCKERHUB_USERNAME", "DOCKERHUB_TOKEN"),
                   ("registry.cn-shenzhen.aliyuncs.com", "ACR_USERNAME", "ACR_PASSWORD"))
    for server, username, token in credentials:
        user, secret = os.environ.get(username), os.environ.get(token)
        require(bool(user) == bool(secret), f"provide both {username} and {token}, or use an existing Docker login")
        if user:
            run("docker", "login", server, "--username", user, "--password-stdin", input=secret + "\n", capture=True)


def push_images(manifest, loaded, on_image=None):
    registry_login()
    pending, receipts = [], []
    # Check every destination before pushing any tag. The archive-derived runtime
    # fingerprint survives Docker/OCI serialization and either daemon's image IDs.
    for entry in manifest["images"]:
        local = loaded[entry["name"]]
        require(daemon_image_content(local["image_id"]) == local["content_sha256"],
                "local image changed before publication")
        for registry in REGISTRIES:
            reference = f"{registry}/{entry['name']}:{manifest['tag']}"
            existing = registry_image(reference)
            require(existing is None or existing["content_sha256"] == local["content_sha256"],
                    f"refusing to overwrite different image at {reference}")
            pending.append((entry, local, reference, existing))
    for entry, local, reference, existing in pending:
        # Re-read immediately before mutation as well as during the initial sweep.
        current = registry_image(reference)
        require(current == existing, f"registry tag changed during publication: {reference}")
        if current is None:
            run("docker", "tag", local["image_id"], reference)
            run("docker", "push", reference)
            current = registry_image(reference)
        require(current is not None and current["content_sha256"] == local["content_sha256"],
                f"registry image mismatch after push: {reference}")
        smoke_image(entry["name"], current["reference"], manifest["version"])
        result = {"name": entry["name"], "tag_reference": reference, **current}
        receipts.append(result)
        if on_image:
            on_image(result)
    return receipts


def publish_npm(directory, manifest):
    if os.environ.get("PUBLISH_NPM") != "true":
        print("npm channel disabled (PUBLISH_NPM is not true)")
        return
    require(bool(os.environ.get("NPM_TOKEN")), "PUBLISH_NPM=true requires NPM_TOKEN")
    require(not manifest["missing_targets"], "npm publication requires all six CLI targets")
    # Use wrappers from the candidate commit, not the release workflow's checkout.
    with tempfile.TemporaryDirectory() as temp:
        root = Path(temp)
        for name in ("publish.mjs", "package.json", "bin/cormex.js"):
            path = root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(run("git", "show", f"{manifest['commit']}:npm/{name}", capture=True) + "\n", encoding="utf-8")
        config = root / "npmrc"
        config.write_text("//registry.npmjs.org/:_authToken=${NPM_TOKEN}\n", encoding="utf-8")
        run("node", root / "publish.mjs", manifest["version"], directory.resolve(),
            env={"NPM_CONFIG_USERCONFIG": str(config)})


def release_snapshot(release):
    return (release["id"], release["tag_name"], release["draft"],
            sorted((x["id"], x["name"], x["size"], x.get("updated_at")) for x in release["assets"]))


def publish(tag, checksum):
    validate_tag(tag)
    validate_checksum(checksum)
    with version_lock(tag), tempfile.TemporaryDirectory() as temp:
        directory = Path(temp)
        release = find_release(tag)
        require(release is not None and release["draft"], "publication requires an existing unpublished draft candidate")
        snapshot = release_snapshot(release)
        run("gh", "release", "download", tag, "--repo", repository(), "--dir", directory)
        manifest = verify_bundle(directory, tag, checksum)
        require(remote_commit(tag) == manifest["commit"], "remote tag moved away from the reviewed source")
        if os.environ.get("PUBLISH_NPM") == "true":
            require(bool(os.environ.get("NPM_TOKEN")) and not manifest["missing_targets"], "npm enabled but token/full target set is missing")
            run("git", "cat-file", "-e", f"{manifest['commit']}:npm/publish.mjs", capture=True)
        current = api(f"releases/{release['id']}")
        require(release_snapshot(current) == snapshot, "candidate changed during verification")
        receipt_path = ROOT / "dist" / f".{tag}.publish-receipt.json"
        publication_identity = {"tag": tag, "commit": manifest["commit"], "bundle_sha256": checksum}
        receipt = Receipt(receipt_path, publication_identity, resume=receipt_path.exists())
        loaded = receipt.step("load-and-smoke", lambda: load_images(directory, manifest))
        # Recheck after potentially slow image loading/smoke tests and before side effects.
        require(remote_commit(tag) == manifest["commit"], "remote tag changed before image publication")
        require(release_snapshot(api(f"releases/{release['id']}")) == snapshot, "candidate changed before publication")
        def record_registry_image(result):
            previous = receipt.data.setdefault("registry_images", [])
            previous[:] = [item for item in previous if item["tag_reference"] != result["tag_reference"]]
            previous.append(result)
            receipt.save()
        registry_receipts = receipt.step("push-and-smoke", lambda: push_images(manifest, loaded, on_image=record_registry_image))
        receipt.data["registry_images"] = registry_receipts
        receipt.save()
        receipt.step("npm", lambda: publish_npm(directory, manifest))
        require(remote_commit(tag) == manifest["commit"], "remote tag changed during publication; some image pushes may already have succeeded")
        require(release_snapshot(api(f"releases/{release['id']}")) == snapshot, "candidate changed during publication; some image pushes may already have succeeded")
        # Download once more: asset metadata alone is not a content proof.
        with tempfile.TemporaryDirectory() as confirm:
            run("gh", "release", "download", tag, "--repo", repository(), "--dir", confirm)
            verify_bundle(Path(confirm), tag, checksum)
        receipt.step("publish-release", lambda: run("gh", "release", "edit", tag, "--repo", repository(), "--draft=false", "--verify-tag"))
        published = api(f"releases/{release['id']}")
        require(not published["draft"] and published["tag_name"] == tag, "publication outcome uncertain; inspect the release before retrying")
        print(f"Published {tag} from {manifest['commit']} with BUNDLE_SHA256={checksum}")


def dispatch(tag, checksum=None):
    validate_tag(tag)
    if checksum is not None:
        validate_checksum(checksum)
    ref = os.environ.get("CORMEX_WORKFLOW_REF", "main")
    require(ref and not ref.startswith("-"), "invalid workflow ref")
    args = ["gh", "workflow", "run", "release.yml" if checksum else "build.yml",
            "--repo", repository(), "--ref", ref, "-f", f"tag={tag}"]
    if checksum:
        args += ["-f", f"checksum={checksum}"]
    run(*args)
    print("Workflow dispatched; this is not a completed build or release.")


def workspace_packages():
    workspace = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]
    packages = {}
    for member in workspace["members"]:
        package = tomllib.loads((ROOT / member / "Cargo.toml").read_text(encoding="utf-8"))["package"]
        require(package.get("version") == {"workspace": True}, "workspace packages must inherit the release version")
        packages[package["name"]] = member
    return workspace["package"]["version"], packages


def validate_versions(expected=None):
    version, packages = workspace_packages()
    validate_tag("v" + version)
    require(expected is None or expected == version, "workspace version disagrees with the release tag")
    app = re.search(r"^version: ([0-9]+\.[0-9]+\.[0-9]+)\+([0-9]+)\s*$",
                    (ROOT / "app/pubspec.yaml").read_text(encoding="utf-8"), re.MULTILINE)
    require(app is not None and app[1] == version, "app version disagrees with workspace version")
    locked = tomllib.loads((ROOT / "Cargo.lock").read_text(encoding="utf-8"))["package"]
    own = {entry["name"]: entry["version"] for entry in locked
           if entry["name"] in packages and "source" not in entry}
    require(own == dict.fromkeys(packages, version), "Cargo.lock workspace versions disagree")
    changelog = (ROOT / "CHANGELOG.md").read_text(encoding="utf-8")
    first = re.search(r"^## (?:\[)?([0-9]+\.[0-9]+\.[0-9]+)(?:\])?(?:\s|$)", changelog, re.MULTILINE)
    require(first is not None and first[1] == version, "latest changelog release disagrees with workspace version")
    return {"version": version, "build_number": int(app[2])}


def prepare_version(tag, build_number=None, release_date=None):
    version = validate_tag(tag)
    current = validate_versions()
    require(tuple(map(int, version.split("."))) > tuple(map(int, current["version"].split("."))),
            "new release version must increase")
    number = current["build_number"] + 1 if build_number is None else build_number
    require(type(number) is int and number > current["build_number"], "app build number must increase")
    day = date.fromisoformat(release_date) if release_date else date.today()
    files = [ROOT / name for name in ("Cargo.toml", "Cargo.lock", "app/pubspec.yaml", "CHANGELOG.md")]
    require(all(path.is_file() and not path.is_symlink() for path in files), "version files must be regular files")
    original = {path: path.read_bytes() for path in files}
    old_lock = tomllib.loads(original[files[1]].decode())
    _, packages = workspace_packages()
    cargo = original[files[0]].decode().replace("\r\n", "\n")
    section = re.search(r"(?ms)^\[workspace\.package\]\n.*?(?=^\[|\Z)", cargo)
    require(section is not None, "workspace.package section is missing")
    changed, count = re.subn(r'^version\s*=\s*"[^"\n]+"', f'version = "{version}"', section[0], flags=re.MULTILINE)
    require(count == 1, "expected exactly one workspace version")
    app, count = re.subn(r"^version: .+$", f"version: {version}+{number}", original[files[2]].decode().replace("\r\n", "\n"), flags=re.MULTILINE)
    require(count == 1, "expected exactly one app version")
    changelog = original[files[3]].decode().replace("\r\n", "\n")
    require(not re.search(rf"^## \[?{re.escape(version)}(?:\]|\s|$)", changelog, re.MULTILINE), "changelog release already exists")
    changelog, count = re.subn(r"^## (?:Unreleased|\[Unreleased\])\s*\n", f"## Unreleased\n\n## {version} — {day.isoformat()}\n\n", changelog, count=1, flags=re.MULTILINE)
    require(count == 1, "changelog needs an Unreleased section")
    try:
        files[0].write_text(cargo[:section.start()] + changed + cargo[section.end():], encoding="utf-8", newline="\n")
        files[2].write_text(app, encoding="utf-8", newline="\n")
        files[3].write_text(changelog, encoding="utf-8", newline="\n")
        # metadata conservatively updates the existing lock; generate-lockfile would
        # discard its dependency choices. Refuse *any* unrelated resolver change.
        run("cargo", "metadata", "--offline", "--format-version", "1", capture=True)
        new_lock = tomllib.loads(files[1].read_text(encoding="utf-8"))
        expected_lock = copy.deepcopy(old_lock)
        for entry in expected_lock["package"]:
            if entry["name"] in packages and "source" not in entry:
                entry["version"] = version
            if "dependencies" in entry:
                entry["dependencies"] = [re.sub(
                    r"^(" + "|".join(re.escape(name) for name in packages) + r") " + re.escape(current["version"]) + r"$",
                    lambda match: match[1] + " " + version, dependency)
                    for dependency in entry["dependencies"]]
        require(new_lock == expected_lock, "cargo metadata changed dependencies; version preparation rolled back")
        validate_versions(version)
        run("cargo", "metadata", "--offline", "--locked", "--format-version", "1", capture=True)
    except BaseException:
        for path, content in original.items():
            path.write_bytes(content)
        raise
    print(f"Prepared {tag}, app build {number}; review the four changed version files before committing/tagging.")


def probe(*args, env=None):
    """Bounded, captured diagnostics. Raw command errors may contain credentials."""
    try:
        result = subprocess.run([str(arg) for arg in args], cwd=ROOT,
                                env=child_environment(env), text=True,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=25, check=False)
        return result.returncode == 0, result.stdout.strip() if result.returncode == 0 else "unavailable (check failed)"
    except (OSError, subprocess.TimeoutExpired):
        return False, "unavailable (missing executable or timed out)"


def database_url():
    if os.environ.get("DATABASE_URL"):
        return os.environ["DATABASE_URL"]
    path = ROOT / ".env"
    if path.is_file():
        for line in path.read_text(encoding="utf-8").splitlines():
            match = re.fullmatch(r"\s*(?:export\s+)?DATABASE_URL\s*=\s*(.*?)\s*", line)
            if match:
                return match[1].strip("\"'")
    return ""


def local_database_environment(value):
    from urllib.parse import unquote, urlsplit
    parsed = urlsplit(value)
    require(parsed.scheme in ("postgres", "postgresql") and parsed.hostname in ("localhost", "127.0.0.1", "::1")
            and not parsed.query and not parsed.fragment,
            "DATABASE_URL must use a loopback development target with no connection overrides")
    name = unquote(parsed.path.lstrip("/"))
    require(bool(re.fullmatch(r"[A-Za-z0-9_-]+", name)) and not re.search(r"prod|production|live", name, re.IGNORECASE),
            "DATABASE_URL must name a development/test database")
    # Empty PGSERVICE asks libpq to look up a service named "". None instructs
    # child_environment to remove even inherited service settings completely.
    return {"PGHOST": parsed.hostname, "PGPORT": str(parsed.port or 5432), "PGDATABASE": name,
            "PGUSER": unquote(parsed.username or ""), "PGPASSWORD": unquote(parsed.password or ""),
            "PGCONNECT_TIMEOUT": "5", "PGOPTIONS": "-c default_transaction_read_only=on",
            "PGHOSTADDR": "::1" if parsed.hostname == "::1" else "127.0.0.1",
            "PGSERVICE": None, "PGSERVICEFILE": None}


def validate_development_stack(inspected, db_env):
    """Tie the selected daemon's Compose database to the loopback URL CI will use."""
    require(isinstance(inspected, list) and len(inspected) == 1, "expected one development Postgres container")
    container = inspected[0]
    require(container.get("State", {}).get("Running") is True, "development Postgres container is stopped")
    labels = container.get("Config", {}).get("Labels") or {}
    require(labels.get("com.docker.compose.project") == os.environ.get("COMPOSE_PROJECT_NAME", "cormex")
            and labels.get("com.docker.compose.service") == "postgres",
            "selected daemon's Postgres container belongs to another Compose project/service")
    working_dir = labels.get("com.docker.compose.project.working_dir")
    require(isinstance(working_dir, str) and Path(working_dir).resolve() == ROOT.resolve(),
            "selected daemon's development stack belongs to another checkout")
    config_files = labels.get("com.docker.compose.project.config_files", "").split(",")
    require((ROOT / "docker-compose.yml").resolve() in {Path(path).resolve() for path in config_files if path},
            "selected daemon's development stack does not use this checkout's root Compose file")
    environment = {key: value for entry in container.get("Config", {}).get("Env", [])
                   if isinstance(entry, str) and "=" in entry for key, value in [entry.split("=", 1)]
                   if key in ("POSTGRES_DB", "POSTGRES_USER")}
    database = environment.get("POSTGRES_DB") or environment.get("POSTGRES_USER", "postgres")
    require(database == db_env["PGDATABASE"], "development container database disagrees with DATABASE_URL")
    bindings = container.get("NetworkSettings", {}).get("Ports", {}).get("5432/tcp") or []
    addresses = {"::1", "::"} if db_env["PGHOSTADDR"] == "::1" else {"127.0.0.1", "0.0.0.0", ""}
    require(any(binding.get("HostPort") == db_env["PGPORT"] and binding.get("HostIp") in addresses
                for binding in bindings), "selected daemon's development Postgres port disagrees with DATABASE_URL")


def development_stack_check(db_env):
    container = os.environ.get("CORMEX_PG_CONTAINER", "cormex-postgres")
    require(re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", container), "invalid development Postgres container name")
    ok, text = probe("docker", "container", "inspect", container)
    require(ok, "development Postgres is absent from the selected Docker daemon; select the correct context or run just up")
    try:
        inspected = json.loads(text)
    except ValueError as error:
        raise ReleaseError("cannot inspect the selected daemon's development stack") from error
    validate_development_stack(inspected, db_env)
    return {"container": container, "checkout": str(ROOT.resolve()), "database_port_matches": True}


def flutter_executable():
    found = shutil.which("flutter")
    fallback = Path("/opt/flutter/bin/flutter")
    return str(Path(found).resolve()) if found else str(fallback) if fallback.is_file() else None


def diagnostic_detail(value):
    # Inspect only approved fields, then redact embedded endpoint userinfo and any
    # credential supplied in the environment. Never include raw failed-command stderr.
    if isinstance(value, dict):
        return {key: diagnostic_detail(item) for key, item in value.items()}
    if isinstance(value, list):
        return [diagnostic_detail(item) for item in value]
    if not isinstance(value, str):
        return value
    value = re.sub(r"([A-Za-z][A-Za-z0-9+.-]*://)[^/\s]*@", r"\1[redacted]@", value)
    for key, secret in os.environ.items():
        if len(secret) >= 4 and re.search(r"TOKEN|PASSWORD|SECRET|CREDENTIAL|PRIVATE_KEY", key):
            value = value.replace(secret, "[redacted]")
    return value


def doctor(*, deployment=False, profile="local", emit=True, as_json=False):
    checks = []
    def add(name, ok, detail, required=True):
        checks.append({"name": name, "ok": bool(ok), "required": required, "detail": diagnostic_detail(detail)})
    def tool(name, args, required=True):
        executable = shutil.which(name)
        ok, text = probe(executable or name, *args)
        add(name, ok, {"executable": executable, "version": text.splitlines()[0] if text else ""}, required)
        return ok, text
    try:
        add("versions", True, validate_versions())
    except (ReleaseError, OSError, ValueError, KeyError):
        add("versions", False, "workspace, lockfile, app and latest changelog release must agree")
    for name, args in (("git", ["--version"]), ("cargo", ["--version"]), ("rustc", ["-vV"]),
                       ("rustup", ["--version"]), ("gh", ["--version"])):
        tool(name, args)
    ok, _ = probe("gh", "api", f"repos/{repository()}", "--jq", ".permissions.push")
    add("github-access", ok, "repository access verified" if ok else "authenticate gh and check repository access")
    local = profile == "local"
    ok, context = probe("docker", "context", "show")
    endpoint_ok, endpoint = probe("docker", "context", "inspect", "--format", "{{.Endpoints.docker.Host}}")
    add("docker-context", ok and endpoint_ok,
        {"name": context, "endpoint": endpoint, "DOCKER_HOST": os.environ.get("DOCKER_HOST"),
         "DOCKER_CONTEXT": os.environ.get("DOCKER_CONTEXT")}, local)
    ok, info = probe("docker", "info", "--format", "{{json .}}")
    engine = {}
    if ok:
        try:
            data = json.loads(info)
            engine = {key: data.get(key) for key in ("ID", "Name", "ServerVersion", "OSType", "Architecture", "Driver", "DockerRootDir")}
        except ValueError:
            ok = False
    add("docker-engine", ok, engine if ok else "Docker engine unavailable", local)
    ok, builder = probe("docker", "buildx", "inspect")
    # Read existing builder only: --bootstrap or context use would change global state.
    builder_lines = [line.strip() for line in builder.splitlines()
                     if re.match(r"\s*(Name|Driver|Endpoint|Status|Platforms|BuildKit version):", line)] if ok else []
    add("docker-builder", ok, builder_lines if ok else "Buildx unavailable", local)
    if local:
        tool("just", ["--version"])
        tool("sqlx", ["--version"])
        for name in ("psql", "pg_dump", "pg_restore"):
            ok, text = tool(name, ["--version"])
            version = re.search(r"\b(\d+)(?:\.\d+)?", text)
            if ok and (version is None or int(version[1]) < 17):
                checks[-1].update(ok=False, detail="Postgres 17+ client required")
        flutter = flutter_executable()
        ok, text = probe(flutter or "flutter", "--version", "--machine",
                         env={"CI": "true", "FLUTTER_SUPPRESS_ANALYTICS": "true"})
        try:
            data = json.loads(text) if ok else {}
            if not isinstance(data, dict):
                raise ValueError("Flutter version output must be an object")
            version = {key: data.get(key) for key in ("frameworkVersion", "frameworkRevision", "engineRevision", "dartSdkVersion")}
            ok = ok and version["frameworkVersion"] == REQUIRED_FLUTTER_VERSION
        except ValueError:
            ok, version = False, {}
        add("flutter", ok, {"executable": flutter, "version": version,
                            "required_version": REQUIRED_FLUTTER_VERSION})
        try:
            targets = local_targets()
            supported = all(not target.endswith("apple-darwin") or sys.platform == "darwin" for target in targets)
            add("cli-targets", supported, targets if supported else "Apple CLI targets require a macOS host")
        except ReleaseError:
            targets = []
            add("cli-targets", False, "invalid CORMEX_CLI_TARGETS")
        required_tools = set()
        for target in targets:
            _, cross = cli_builder(target)
            if cross:
                required_tools.add(cross)
            if "windows" in target and sys.platform != "win32":
                required_tools.update(("clang-cl", "llvm-lib"))
        for name in sorted(required_tools):
            tool(name, ["--version"])
        if any("musl" in target for target in targets):
            ok, text = probe("zig", "version")
            if not ok:
                ok, text = probe(sys.executable, "-m", "ziglang", "version")
            add("zig", ok, text)
        try:
            db_env = local_database_environment(database_url())
            try:
                add("development-stack", True, development_stack_check(db_env))
            except ReleaseError as error:
                add("development-stack", False, str(error) + "; select the correct Docker context or run just up")
            except (ValueError, KeyError, TypeError, AttributeError, OSError):
                add("development-stack", False, "cannot verify development stack provenance; select the correct Docker context or run just up")
            ok, text = probe("psql", "--no-psqlrc", "--no-password", "--tuples-only", "--no-align",
                             "--command", "SELECT 1", env=db_env)
            add("development-database", ok and text == "1", "safe loopback target reachable" if ok and text == "1" else "safe local target is not reachable")
        except (ReleaseError, ValueError):
            add("development-database", False, "configure a loopback development DATABASE_URL; connection overrides and production names are refused")
    usage = shutil.disk_usage(ROOT)
    add("disk", usage.free >= 10 * 1024**3, {"free_bytes": usage.free, "minimum_bytes": 10 * 1024**3}, local)
    if deployment:
        for name in ("DEPLOY_HOST", "DEPLOY_USER", "DEPLOY_DIR", "DEPLOY_API"):
            add(name, bool(os.environ.get(name)), "set" if os.environ.get(name) else "missing")
        for name in ("DEPLOY_SSH_KEY", "DEPLOY_KNOWN_HOSTS"):
            value = os.environ.get(name)
            ok = not value or Path(value).expanduser().is_file()
            detail = "using SSH defaults/agent" if not value else "file available" if ok else "explicit file missing"
            add(name, ok, detail)
    report = {"schema_version": 1, "profile": profile, "checks": checks,
              "ok": all(check["ok"] for check in checks if check["required"])}
    if emit:
        if as_json:
            print(json.dumps(report, indent=2, sort_keys=True))
        else:
            for check in checks:
                print(f"{'OK' if check['ok'] else 'FAIL' if check['required'] else 'INFO'} {check['name']}: {json.dumps(check['detail'], sort_keys=True)}")
    return report


class Receipt:
    """Small local journal, deliberately stored outside the immutable candidate."""
    def __init__(self, path, identity, resume=False):
        self.path = path
        if resume:
            require(path.is_file() and not path.is_symlink(), "resume requires an existing regular receipt")
            self.data = read_json(path)
            require(self.data.get("identity") == identity, "source/tag/toolchain/environment changed; refusing to resume")
        else:
            ensure_new(path)
            self.data = {"schema_version": 1, "identity": identity, "stages": []}
        self.save()

    def save(self):
        self.path.parent.mkdir(parents=True, exist_ok=True)
        temporary = self.path.with_name(self.path.name + ".tmp")
        require(not temporary.is_symlink(), "receipt temporary file cannot be a symlink")
        write_json(temporary, self.data)
        temporary.replace(self.path)

    def latest(self, name):
        return next((stage for stage in reversed(self.data["stages"]) if stage["name"] == name), None)

    def step(self, name, action, outputs=None):
        record = {"name": name, "status": "running", "started_at": datetime.now(timezone.utc).isoformat()}
        self.data["stages"].append(record)
        self.save()
        start = time.monotonic()
        try:
            result = action()
            if outputs:
                record["outputs"] = outputs()
            record["status"] = "passed"
            return result
        except BaseException:
            # Exception text can contain command stderr, paths or credentials.
            record["status"] = "failed"
            raise
        finally:
            record["duration_seconds"] = round(time.monotonic() - start, 3)
            record["finished_at"] = datetime.now(timezone.utc).isoformat()
            self.save()


def build_runtime_identity(report):
    # Hash the complete environment without recording its values. CI always reruns;
    # even artifacts cannot be reused after a credential/build-option/DB change.
    # Shell/just invocation bookkeeping changes between the normal and resume
    # recipes. All build options, credentials, paths and service settings still hash.
    invocation_only = {"_", "SHLVL", "PWD", "OLDPWD", "TERM", "COLORTERM", "LINES", "COLUMNS"}
    environment = {key: value for key, value in os.environ.items()
                   if key not in invocation_only and not key.startswith("JUST_")}
    environment["DATABASE_URL"] = database_url()
    tools = copy.deepcopy([check for check in report["checks"] if check["name"] not in ("disk", "development-database")])
    for check in tools:
        if check["name"] == "docker-builder" and isinstance(check["detail"], list):
            check["detail"] = [line for line in check["detail"] if line.startswith(("Name:", "Driver:", "Endpoint:", "BuildKit version:"))]
    return {"tools": tools,
            "environment_sha256": hashlib.sha256(json.dumps(environment, sort_keys=True).encode()).hexdigest(),
            "dotenv_sha256": digest(ROOT / ".env") if (ROOT / ".env").is_file() else None,
            "python": sys.version, "platform": sys.platform, "targets": local_targets()}


def build_local(tag, resume=False):
    with version_lock(tag):
        started_at = datetime.now(timezone.utc).isoformat()
        started = time.monotonic()
        report = preflight(tag, "local")
        context = source_context(tag)
        identity = {**context, "runtime": build_runtime_identity(report)}
        receipt = Receipt(ROOT / "dist" / f".{tag}.build-receipt.json", identity, resume)
        receipt.data["stages"].append({"name": "preflight", "status": "passed", "started_at": started_at,
                                      "finished_at": datetime.now(timezone.utc).isoformat(),
                                      "duration_seconds": round(time.monotonic() - started, 3)})
        receipt.save()
        directory = output_dir(tag)
        bundle = directory / "SHA256SUMS"
        if not resume:
            require(not directory.exists(), "build outputs already exist; use --resume with their receipt or a new candidate")
        def unchanged():
            require(source_context(tag) == context, "source changed during local build")
            require(build_runtime_identity(doctor(emit=False)) == identity["runtime"], "runtime changed during local build")
        # No cached check result ever bypasses mandatory CI, including after a fully
        # packaged build. Flutter's discovered path applies only to child processes.
        flutter = flutter_executable()
        ci_env = local_database_environment(database_url())
        # CI migrations must write, but ambient libpq hostaddr/service/options must
        # not redirect the URL that doctor just verified to another database.
        ci_env.update(DATABASE_URL=database_url(), PGOPTIONS="")
        if flutter:
            ci_env["PATH"] = str(Path(flutter).parent) + os.pathsep + os.environ.get("PATH", "")
        receipt.step("ci-full", lambda: run("just", "ci-full", env=ci_env))
        unchanged()
        packaged = receipt.latest("bundle")
        if bundle.exists():
            require(resume and packaged is not None and packaged["status"] == "passed", "unrecorded candidate cannot be resumed")
            expected = packaged["outputs"]["SHA256SUMS"]
            manifest = verify_bundle(directory, tag, expected)
            require(all(manifest.get(key) == value for key, value in context.items()), "resumed bundle source differs")
            print_summary(directory, manifest)
            return
        def hashes(paths):
            return {path.name: digest(path) for path in paths}
        def artifact(name, action, paths):
            prior = receipt.latest(name)
            if resume and prior and prior["status"] == "passed":
                require(all(path.is_file() and not path.is_symlink() for path in paths), "resumed artifact missing")
                require(hashes(paths) == prior.get("outputs"), "resumed artifact checksum changed")
                receipt.step(name + ":reuse", unchanged)
                return
            require(not any(path.exists() or path.is_symlink() for path in paths),
                    f"partial/unrecorded outputs for {name}; inspect and remove them before resuming")
            receipt.step(name, action, lambda: hashes(paths))
            unchanged()
        for target in local_targets():
            paths = [directory / archive_name(context["version"], target), directory / f"{target}.cli.json"]
            artifact("cli:" + target, lambda target=target: build_cli(tag, target), paths)
        for name in IMAGES:
            paths = [directory / image_name(context["version"], name), directory / f"{name}.image.json"]
            artifact("image:" + name, lambda name=name: build_images(tag, (name,)), paths)
        receipt.step("bundle", lambda: create_bundle(tag), lambda: {"SHA256SUMS": digest(bundle)})
        print(f"Build receipt: {receipt.path}. Stage this candidate separately; building never publishes.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    doctor_parser = commands.add_parser("doctor", help="read-only readiness checks; no tag required")
    doctor_parser.add_argument("--deployment", action="store_true")
    doctor_parser.add_argument("--json", action="store_true")
    for command in ("preflight", "prepare-version", "build-local", "build-cli", "build-cli-local", "build-images", "bundle", "stage",
                    "verify", "publish", "dispatch-build", "dispatch-release"):
        sub = commands.add_parser(command)
        sub.add_argument("tag")
        if command == "preflight":
            sub.add_argument("--profile", choices=("source", "local"))
        if command == "prepare-version":
            sub.add_argument("--build-number", type=int)
            sub.add_argument("--date", dest="release_date")
        if command == "build-local":
            sub.add_argument("--resume", action="store_true")
        if command == "build-cli":
            sub.add_argument("target", choices=TARGETS)
        if command in ("verify", "publish", "dispatch-release"):
            sub.add_argument("checksum")
        if command == "verify":
            sub.add_argument("directory", type=Path)
        if command == "bundle":
            sub.add_argument("--require-all-targets", action="store_true")
    args = parser.parse_args()
    actions = {
        "doctor": lambda: doctor(deployment=args.deployment, as_json=args.json),
        "preflight": lambda: preflight(args.tag, args.profile),
        "prepare-version": lambda: prepare_version(args.tag, args.build_number, args.release_date),
        "build-local": lambda: build_local(args.tag, args.resume),
        "build-cli": lambda: build_cli(args.tag, args.target),
        "build-cli-local": lambda: build_cli_local(args.tag), "build-images": lambda: build_images(args.tag),
        "bundle": lambda: create_bundle(args.tag, args.require_all_targets), "stage": lambda: stage(args.tag),
        "verify": lambda: verify_bundle(args.directory, args.tag, args.checksum), "publish": lambda: publish(args.tag, args.checksum),
        "dispatch-build": lambda: dispatch(args.tag), "dispatch-release": lambda: dispatch(args.tag, args.checksum),
    }
    try:
        result = actions[args.command]()
        if args.command == "doctor" and not result["ok"]:
            return 1
    except (ReleaseError, OSError, ValueError, KeyError, TypeError, tarfile.TarError) as error:
        print(f"release: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
