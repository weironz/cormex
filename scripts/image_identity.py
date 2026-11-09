"""Portable Docker runtime identity, shared by release and deployment checks.

Only standard-library code: this file can be copied beside a remote deploy helper.
The identity includes all runtime configuration and ordered uncompressed layer hashes.
It deliberately excludes history, timestamps and legacy builder metadata.
"""
import hashlib
import json
import re
import tarfile


def require(ok, message):
    if not ok:
        raise ValueError(message)


def portable_image_content(spec):
    require(spec.get("os") == "linux" and spec.get("architecture") == "amd64",
            "Docker archive platform mismatch")
    config = spec.get("config") or {}
    require(isinstance(config, dict), "invalid runtime image config")
    false_defaults = {"ArgsEscaped", "NetworkDisabled", "Tty", "OpenStdin", "StdinOnce",
                      "AttachStdin", "AttachStdout", "AttachStderr"}
    # Store conversion inserts empty top-level defaults. Nested values (such as an
    # empty label string) and array order carry meaning and must survive normalization.
    config = {key: value for key, value in config.items()
              if value not in (None, "", [], {}) and not (key in false_defaults and value is False)}
    rootfs = spec.get("rootfs", {})
    require(rootfs.get("type") == "layers" and isinstance(rootfs.get("diff_ids"), list),
            "invalid image rootfs")
    require(all(isinstance(value, str) and re.fullmatch(r"sha256:[0-9a-f]{64}", value)
                for value in rootfs["diff_ids"]), "invalid image layer digest")
    content = {"os": spec["os"], "architecture": spec["architecture"],
               "variant": spec.get("variant") or "", "config": config, "rootfs": rootfs}
    return hashlib.sha256(json.dumps(content, sort_keys=True, separators=(",", ":"),
                                     ensure_ascii=True).encode()).hexdigest()


def image_content_identity(inspect_dict):
    rootfs = inspect_dict.get("RootFS", {})
    return portable_image_content({
        "os": inspect_dict.get("Os"), "architecture": inspect_dict.get("Architecture"),
        "variant": inspect_dict.get("Variant"), "config": inspect_dict.get("Config"),
        "rootfs": {"type": rootfs.get("Type"), "diff_ids": rootfs.get("Layers", [])},
    })


def archive_content_identity(path, *, allow_tags=False):
    # Never extract an archive. Docker save includes manifest.json with either
    # classic config filenames or OCI-compatible blobs/sha256 config paths.
    with tarfile.open(path, "r:*") as archive:
        def blob(name, limit):
            require(isinstance(name, str) and not name.startswith("/")
                    and "\\" not in name and ".." not in name.split("/"), "unsafe Docker archive path")
            matches = [member for member in archive.getmembers() if member.name == name]
            require(len(matches) == 1 and matches[0].isfile() and matches[0].size < limit,
                    "invalid or duplicate Docker archive member")
            return json.load(archive.extractfile(matches[0]))
        saved = blob("manifest.json", 1024 * 1024)
        require(isinstance(saved, list) and len(saved) == 1, "Docker archive must contain exactly one image")
        require(allow_tags or not saved[0].get("RepoTags"), "saved image must not carry mutable registry tags")
        return portable_image_content(blob(saved[0]["Config"], 16 * 1024 * 1024))
