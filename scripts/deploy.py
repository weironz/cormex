#!/usr/bin/env python3
"""Deploy a published Cormex version from this repository's compose stack.

Release and deploy stay separate: nothing here builds, tags or uploads anything. It
pulls the pinned images out of the registry, starts `deploy/docker-compose.yml` (plus
`compose.override.yaml` when the operator wrote one), and then asks the running service
what it is. A stack that answers with a different version than the one asked for is a
failed deploy, not a detail -- "which version is running" is the question this exists to
answer without reading the host by hand.

`rehearsal` is the same stack on an isolated compose project with generated credentials
and its own ports: fresh volumes, its own container names, no shared state with the
development stack. It is how this file gets tested somewhere other than production.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import secrets
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
REHEARSAL = ROOT / ".deploy-rehearsal"
REHEARSAL_PROJECT = "cormex-rehearsal"
REHEARSAL_PORT = 18080
REHEARSAL_S3_PORT = 19000
REQUIRED_ENV = ("CORMEX_VERSION", "CORMEX_AUTH_TOKEN_SHA256", "POSTGRES_PASSWORD", "RUSTFS_SECRET_KEY")
TAG = re.compile(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\Z")
HEX64 = re.compile(r"[0-9a-f]{64}\Z")


class DeployError(Exception):
    pass


def require(ok, message):
    if not ok:
        raise DeployError(message)


def validate_tag(tag):
    require(isinstance(tag, str) and TAG.fullmatch(tag), "tag must be vX.Y.Z")
    return tag


def run(*args, cwd=None, capture=False, env=None):
    settings = dict(os.environ, MSYS_NO_PATHCONV="1", MSYS2_ARG_CONV_EXCL="*")
    if env is not None:
        settings = dict(env, MSYS_NO_PATHCONV="1", MSYS2_ARG_CONV_EXCL="*")
    result = subprocess.run([str(x) for x in args], cwd=cwd or ROOT, env=settings, text=True,
                            stdout=subprocess.PIPE if capture else None,
                            stderr=subprocess.PIPE if capture else None, check=False)
    if result.returncode:
        raise DeployError(f"{args[0]} {' '.join(str(a) for a in args[1:3])} failed "
                          f"({result.returncode})" + (f": {result.stderr.strip()}" if capture else ""))
    return result.stdout.strip() if capture else ""


# ── the stack on disk ────────────────────────────────────

def load_env(directory):
    """Parse `<directory>/.env`. Values are literal: compose does the same, and a
    second interpolation scheme here would disagree with the one that matters."""
    path = Path(directory) / ".env"
    require(path.is_file(), f"{path} is missing; copy deploy/.env.example and fill it in")
    values = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        values[key.strip()] = value.strip().strip('"').strip("'")
    return values


def compose(directory, *args, project=None, capture=False):
    """Run compose against `directory`.

    Compose prefers an exported shell variable over the same key in the directory's
    `.env`, and `just` exports the repository root's `.env` into every recipe. A
    `CORMEX_VERSION` left over from some other checkout would then deploy a version
    nobody asked for. Drop the keys the stack's own `.env` defines, so the file the
    operator pinned is the only source of truth.
    """
    directory = Path(directory)
    require((directory / "docker-compose.yml").is_file(), f"{directory}/docker-compose.yml is missing")
    command = ["docker", "compose"]
    if project:
        command += ["-p", project]
    command += ["-f", "docker-compose.yml"]
    if (directory / "compose.override.yaml").is_file():
        command += ["-f", "compose.override.yaml"]
    shadowed = set(load_env(directory)) if (directory / ".env").is_file() else set()
    environment = {key: value for key, value in os.environ.items() if key not in shadowed}
    return run(*command, *args, cwd=directory, capture=capture, env=environment)


def base_url(env):
    bind = env.get("CORMEX_BIND_ADDR", "127.0.0.1")
    port = env.get("CORMEX_PORT", "8080")
    require(bind in ("127.0.0.1", "localhost", "0.0.0.0"), f"cannot verify through CORMEX_BIND_ADDR={bind}")
    return f"http://127.0.0.1:{port}"


# ── talking to the running stack ─────────────────────────

def fetch(url, timeout=10):
    with urllib.request.urlopen(url, timeout=timeout) as response:
        return response.headers.get("content-type", ""), response.read()


def health(base, timeout=240):
    deadline = time.monotonic() + timeout
    last = "no attempt"
    while time.monotonic() < deadline:
        try:
            content_type, body = fetch(f"{base}/api/health")
        except (urllib.error.URLError, OSError) as error:
            last = str(error)
        else:
            require(content_type.startswith("application/json"),
                    f"/api/health answered {content_type}, so the proxy is handing /api to the web "
                    "image instead of the server")
            return json.loads(body)
        time.sleep(2)
    raise DeployError(f"{base}/api/health did not answer within {timeout}s; last error: {last}")


def verify(directory, tag=None):
    env = load_env(directory)
    require(env.get("CORMEX_VERSION"), "CORMEX_VERSION is not set in .env")
    pin = tag or env["CORMEX_VERSION"]
    validate_tag(pin)
    # /health reports the Cargo version, which carries no `v`; the pin does.
    expected = pin[1:]
    base = base_url(env)
    status = health(base)
    require(status.get("role") == "cormexd", f"the stack is not talking to cormexd: {status.get('role')!r}")
    require(status.get("version") == expected,
            f"running version is {status.get('version')!r}, but {pin!r} was deployed")
    content_type, body = fetch(f"{base}/")
    require(content_type.startswith("text/html") and b"<title>" in body.lower(),
            "the web root did not return an HTML document")
    print(f"stack: {base}  version {status['version']}  role {status['role']}")
    print(f"database: {status.get('database')}  blobs: {status.get('blob_backend')}  auth: {status.get('auth')}")
    if status.get("capture_encryption") == "plaintext":
        print("WARNING: capture_encryption is plaintext -- CORMEX_SECRET_KEY is not reaching cormexd")
    else:
        print(f"capture: {status.get('capture_encryption')}")
    embedding = status.get("embedding") or {}
    model = embedding.get("model") if isinstance(embedding, dict) else None
    if not model:
        print("WARNING: no embedding model is configured; vector search will fail on first use")
    else:
        print(f"embedding: {model}")
    return status


# ── operations ───────────────────────────────────────────

def up(tag, directory, project=None):
    validate_tag(tag)
    env = load_env(directory)
    for key in REQUIRED_ENV:
        require(env.get(key), f"{key} is empty in {Path(directory) / '.env'}; compose would refuse to start")
    require(env["CORMEX_VERSION"] == tag,
            f".env pins {env['CORMEX_VERSION']}, not {tag}; change the pin rather than the caller")
    # Both images are pulled explicitly: `up` alone would silently keep whatever an
    # older tag left behind when the pin did not move but the registry did.
    compose(directory, "pull", "cormex", "web", project=project)
    compose(directory, "up", "-d", project=project)
    print(f"waiting for {base_url(env)}/api/health ...")
    verify(directory, tag)
    stop = "just deploy-rehearsal-down" if project else "just deploy-down"
    print(f"deployed {tag}; stop with `{stop}`")


def down(directory):
    compose(directory, "down")
    print("stack stopped; volumes were left in place (`docker compose down -v` would delete data)")


# ── local rehearsal ──────────────────────────────────────

def port_free(port):
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        return probe.connect_ex(("127.0.0.1", port)) != 0


def token_hash(tag):
    """The digest only. The plaintext token is printed once by the image and is not
    kept here: a rehearsal does not need a legacy client credential, and a file is
    exactly where that token must not end up."""
    output = run("docker", "run", "--rm", f"willdockerhub/cormex:{tag}", "--generate-token", capture=True)
    match = re.search(r"^CORMEX_AUTH_TOKEN_SHA256=([0-9a-f]{64})$", output, re.MULTILINE)
    require(match is not None, "could not read CORMEX_AUTH_TOKEN_SHA256 from `--generate-token`")
    return match.group(1)


def write_file(path, text, mode=0o600):
    Path(path).write_text(text, encoding="utf-8")
    Path(path).chmod(mode)


def write_env(path, values):
    write_file(path, "".join(f"{key}={value}\n" for key, value in values))


def rehearsal_files(tag):
    REHEARSAL.mkdir(exist_ok=True)
    for name in ("docker-compose.yml", "proxy.conf"):
        (REHEARSAL / name).write_bytes((ROOT / "deploy" / name).read_bytes())
    env_file = REHEARSAL / ".env"
    override = REHEARSAL / "compose.override.yaml"
    if env_file.is_file():
        # Reuse the credentials and volumes: a rehearsal that regenerates its secrets
        # every run would also invalidate the encrypted data it is there to exercise.
        values = load_env(REHEARSAL)
        values["CORMEX_VERSION"] = tag
        write_env(env_file, values.items())
        return
    write_env(env_file, {
        "CORMEX_VERSION": tag,
        "CORMEX_BIND_ADDR": "127.0.0.1",
        "CORMEX_PORT": str(REHEARSAL_PORT),
        "CORMEX_S3_PORT": str(REHEARSAL_S3_PORT),
        "CORMEX_PUBLIC_URL": f"http://127.0.0.1:{REHEARSAL_PORT}",
        "CORMEX_S3_PUBLIC_URL": f"http://127.0.0.1:{REHEARSAL_S3_PORT}",
        "CORMEX_AUTH_TOKEN_SHA256": token_hash(tag),
        "POSTGRES_PASSWORD": secrets.token_hex(24),
        "RUSTFS_SECRET_KEY": secrets.token_hex(32),
        "CORMEX_SECRET_KEY": secrets.token_hex(32),
        "CORMEX_ADMIN_USERNAME": "admin",
        "CORMEX_ADMIN_PASSWORD": secrets.token_hex(16),
        "CORMEX_OPEN_REGISTRATION": "false",
    }.items())
    # Only a new rehearsal claims ports; a second run reuses the running stack, whose
    # own listeners are exactly what looks like a conflict from the outside.
    for port in (REHEARSAL_PORT, REHEARSAL_S3_PORT):
        require(port_free(port), f"port {port} is already in use; stop whatever owns it or pick another rehearsal")
    # The compose file passes an explicit allowlist of variables to cormexd, so the
    # admin account and the master key have to be added here rather than in .env.
    write_file(override, f"""services:
  postgres:
    container_name: {REHEARSAL_PROJECT}-postgres
  rustfs:
    container_name: {REHEARSAL_PROJECT}-rustfs
  web:
    container_name: {REHEARSAL_PROJECT}-web
  proxy:
    container_name: {REHEARSAL_PROJECT}-proxy
  cormex:
    container_name: {REHEARSAL_PROJECT}-cormex
    environment:
      CORMEX_ADMIN_USERNAME: ${{CORMEX_ADMIN_USERNAME}}
      CORMEX_ADMIN_PASSWORD: ${{CORMEX_ADMIN_PASSWORD}}
      CORMEX_SECRET_KEY: ${{CORMEX_SECRET_KEY}}
      CORMEX_OPEN_REGISTRATION: ${{CORMEX_OPEN_REGISTRATION}}
""")


def rehearsal(tag):
    validate_tag(tag)
    rehearsal_files(tag)
    # Same pull/up/verify path as a real deployment; only the compose project and the
    # directory differ, so a rehearsal that passes is evidence about that path.
    up(tag, REHEARSAL, project=REHEARSAL_PROJECT)
    print(f"credentials: {REHEARSAL / '.env'} (admin account, 0600)")
    print("volumes: docker volume ls | grep cormex-rehearsal")


def rehearsal_down():
    compose(REHEARSAL, "down", project=REHEARSAL_PROJECT)
    print(f"{REHEARSAL_PROJECT} stopped; its volumes and generated credentials were kept")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    up_cmd = commands.add_parser("up", help="pull and start the pinned version, then verify it")
    up_cmd.add_argument("tag")
    up_cmd.add_argument("directory", type=Path, nargs="?", default=ROOT / "deploy")
    verify_cmd = commands.add_parser("verify", help="check role, version, web root and the /api split")
    verify_cmd.add_argument("directory", type=Path, nargs="?", default=ROOT / "deploy")
    verify_cmd.add_argument("--tag", help="expected version; defaults to the CORMEX_VERSION in .env")
    down_cmd = commands.add_parser("down", help="stop the stack, keep the volumes")
    down_cmd.add_argument("directory", type=Path, nargs="?", default=ROOT / "deploy")
    rehearsal_cmd = commands.add_parser("rehearsal", help="deploy on an isolated project and verify")
    rehearsal_cmd.add_argument("tag")
    commands.add_parser("rehearsal-down", help="stop the isolated rehearsal, keep its volumes")
    args = parser.parse_args()
    actions = {
        "up": lambda: up(args.tag, args.directory),
        "verify": lambda: verify(args.directory, args.tag),
        "down": lambda: down(args.directory),
        "rehearsal": lambda: rehearsal(args.tag),
        "rehearsal-down": lambda: rehearsal_down(),
    }
    try:
        actions[args.command]()
    except (DeployError, OSError, ValueError, KeyError) as error:
        print(f"deploy: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
