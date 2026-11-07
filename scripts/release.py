#!/usr/bin/env python3
"""Build once, stage a reviewable candidate, and publish those exact bytes.

Only stage/publish/dispatch change remote state. Checks and packaging are shared
by the local and Actions entry points; publishing never invokes a compiler.
"""
from __future__ import annotations

import argparse
from contextlib import contextmanager
import gzip
import hashlib
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


def run(*args, capture=False, cwd=None, env=None, input=None):
    # Never interpolate tag/target/user input into a shell command.
    settings = dict(os.environ, MSYS_NO_PATHCONV="1", MSYS2_ARG_CONV_EXCL="*")
    if env:
        settings.update(env)
    result = subprocess.run([str(x) for x in args], cwd=cwd or ROOT, env=settings,
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
    with (ROOT / "Cargo.toml").open("rb") as file:
        got = tomllib.load(file)["workspace"]["package"]["version"]
    require(got == version, f"Cargo.toml version {got} disagrees with {tag}")
    return {"tag": tag, "version": version, "commit": commit,
            "source": "github" if os.environ.get("GITHUB_ACTIONS") == "true" else "local"}


def preflight(tag):
    context = source_context(tag)
    require(remote_commit(tag) == context["commit"], "remote tag differs from the source commit")
    run("cargo", "metadata", "--locked", "--format-version", "1", capture=True)
    print(f"source verified: {tag} {context['commit']}")


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


def build_images(tag):
    context = source_context(tag)
    directory = output_dir(tag)
    for name in IMAGES:
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
                 "image_id": identity, "platform": "linux/amd64"}
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


def load_images(directory, manifest):
    for entry in manifest["images"]:
        path = directory / entry["file"]
        # Inspect the Docker save metadata before docker load, which mutates the local
        # daemon. Reject tags and extra image configs instead of loading arbitrary tags.
        with tarfile.open(path, "r:gz") as archive:
            member = archive.getmember("manifest.json")
            require(member.isfile() and member.size < 1024 * 1024, "invalid Docker archive manifest")
            saved = json.load(archive.extractfile(member))
            require(isinstance(saved, list) and len(saved) == 1, "Docker archive must contain exactly one image")
            require(not saved[0].get("RepoTags"), "saved image must not carry mutable registry tags")
            config_name = saved[0]["Config"]
            require(not config_name.startswith("/") and ".." not in config_name.split("/"), "unsafe Docker config path")
            config = archive.getmember(config_name)
            require(config.isfile() and config.size < 16 * 1024 * 1024, "invalid Docker image config")
            data = archive.extractfile(config).read()
            # The archive's config blob is **not** compared with `image_id`: only the classic
            # image store names an image by its config digest. Under the containerd image
            # store (Docker 29's default) the image ID is the manifest digest and
            # `docker save` re-serialises the config into the Docker schema, so the two
            # digests differ by design. The bytes are already pinned by SHA256SUMS, and the
            # identity check that matters -- load, then require the daemon to report the
            # reviewed ID -- happens below and holds in both stores.
            spec = json.loads(data)
            require(spec.get("os") == "linux" and spec.get("architecture") == "amd64", "Docker archive platform mismatch")
        run("docker", "image", "load", "--input", path)
        require(image_info(entry["image_id"]) == entry["image_id"], "loaded image ID mismatch")
        smoke_image(entry["name"], entry["image_id"], manifest["version"])


def registry_image(reference):
    """Return the image ID the registry serves at `reference`, or None when it is absent.

    The comparison is made on the daemon's image ID rather than on the registry's config
    digest, because the two image stores name images differently: the classic store uses
    the config digest, while the containerd image store (Docker 29's default) uses the
    manifest digest and pushes a config blob under a different digest. An image ID, by
    contrast, survives push and pull unchanged in both stores -- verified by removing the
    local tag first, so this reads the registry's bytes and not a leftover local copy.
    """
    result = subprocess.run(["docker", "manifest", "inspect", reference], cwd=ROOT,
                            text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    if result.returncode:
        # Fail closed on network, permission and authentication failures.
        message = result.stderr.lower()
        require("no such manifest" in message or "manifest unknown" in message,
                f"cannot determine whether {reference} exists: {result.stderr.strip()}")
        return None
    manifest = json.loads(result.stdout)
    require(manifest.get("schemaVersion") == 2 and "config" in manifest,
            f"{reference} is an index/unsupported manifest; refusing to replace it")
    subprocess.run(["docker", "image", "rm", reference], cwd=ROOT, text=True,
                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    run("docker", "pull", reference, capture=True)
    return image_info(reference)


def registry_login():
    credentials = (("docker.io", "DOCKERHUB_USERNAME", "DOCKERHUB_TOKEN"),
                   ("registry.cn-shenzhen.aliyuncs.com", "ACR_USERNAME", "ACR_PASSWORD"))
    for server, username, token in credentials:
        user, secret = os.environ.get(username), os.environ.get(token)
        require(bool(user) == bool(secret), f"provide both {username} and {token}, or use an existing Docker login")
        if user:
            run("docker", "login", server, "--username", user, "--password-stdin", input=secret + "\n", capture=True)


def push_images(manifest):
    registry_login()
    pending = []
    # Detect all conflicting tags before the first push. Identical image IDs allow
    # resuming a partial two-registry failure without silently replacing a release.
    for entry in manifest["images"]:
        for registry in REGISTRIES:
            reference = f"{registry}/{entry['name']}:{manifest['tag']}"
            existing = registry_image(reference)
            require(existing in (None, entry["image_id"]), f"refusing to overwrite different image at {reference}")
            pending.append((entry, reference, existing))
    for entry, reference, existing in pending:
        if existing is None:
            run("docker", "tag", entry["image_id"], reference)
            run("docker", "push", reference)
            # Re-read the registry now that the tag exists: the local tag alone proves nothing.
            require(registry_image(reference) == entry["image_id"],
                    f"registry image mismatch after push: {reference}")
        smoke_image(entry["name"], reference, manifest["version"])


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
        load_images(directory, manifest)
        # Recheck after potentially slow image loading/smoke tests and before side effects.
        require(remote_commit(tag) == manifest["commit"], "remote tag changed before image publication")
        require(release_snapshot(api(f"releases/{release['id']}")) == snapshot, "candidate changed before publication")
        push_images(manifest)
        publish_npm(directory, manifest)
        require(remote_commit(tag) == manifest["commit"], "remote tag changed during publication; some image pushes may already have succeeded")
        require(release_snapshot(api(f"releases/{release['id']}")) == snapshot, "candidate changed during publication; some image pushes may already have succeeded")
        # Download once more: asset metadata alone is not a content proof.
        with tempfile.TemporaryDirectory() as confirm:
            run("gh", "release", "download", tag, "--repo", repository(), "--dir", confirm)
            verify_bundle(Path(confirm), tag, checksum)
        run("gh", "release", "edit", tag, "--repo", repository(), "--draft=false", "--verify-tag")
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    for command in ("preflight", "build-cli", "build-cli-local", "build-images", "bundle", "stage",
                    "verify", "publish", "dispatch-build", "dispatch-release"):
        sub = commands.add_parser(command)
        sub.add_argument("tag")
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
        "preflight": lambda: preflight(args.tag), "build-cli": lambda: build_cli(args.tag, args.target),
        "build-cli-local": lambda: build_cli_local(args.tag), "build-images": lambda: build_images(args.tag),
        "bundle": lambda: create_bundle(args.tag, args.require_all_targets), "stage": lambda: stage(args.tag),
        "verify": lambda: verify_bundle(args.directory, args.tag, args.checksum), "publish": lambda: publish(args.tag, args.checksum),
        "dispatch-build": lambda: dispatch(args.tag), "dispatch-release": lambda: dispatch(args.tag, args.checksum),
    }
    try:
        actions[args.command]()
    except (ReleaseError, OSError, ValueError, KeyError, TypeError, tarfile.TarError) as error:
        print(f"release: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
