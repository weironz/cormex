#!/bin/sh
# Install the Cormex capture CLI.
#
#   curl -fsSL https://raw.githubusercontent.com/weironz/cormex/main/scripts/install.sh | sh
#
# # Why `sh` and not `bash`
#
# This is the first thing a new user runs, on an unknown machine. `sh` is on
# every Unix; `bash` is not (Alpine, and a minimal Debian, do not have it by
# default) — and the failure there is `bash: not found`, which reads as "the
# script is broken".
#
# # Why it verifies the checksum
#
# The download goes over TLS, which protects the transit but not the file at
# rest. `SHA256SUMS` comes from the same Release, so it does not defend against
# a compromised release — what it does defend against is a truncated or
# corrupted download, which is the failure that actually happens.
set -eu

REPO="weironz/cormex"
BIN="cormex"

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

need() {
    command -v "$1" >/dev/null 2>&1 || die "this installer needs $1, which is not installed"
}

# ── Platform ──────────────────────────────────────────────
os=$(uname -s)
arch=$(uname -m)
case "$os" in
    Linux)  os_part="unknown-linux-musl" ;;
    Darwin) os_part="apple-darwin" ;;
    *) die "unsupported operating system: $os. On Windows, use scripts/install.ps1" ;;
esac
case "$arch" in
    x86_64|amd64)  arch_part="x86_64" ;;
    arm64|aarch64) arch_part="aarch64" ;;
    *) die "unsupported architecture: $arch" ;;
esac
target="${arch_part}-${os_part}"

# ── Version ───────────────────────────────────────────────
version="${CORMEX_VERSION:-}"
if [ -z "$version" ]; then
    need curl
    # The redirect's last path segment is the tag. Parsing the JSON would need
    # `jq`, which is another dependency on a machine that may have nothing
    version=$(curl -fsSLI -o /dev/null -w '%{url_effective}' \
        "https://github.com/${REPO}/releases/latest" | sed 's#.*/tag/##')
    [ -n "$version" ] || die "could not work out the latest version; set CORMEX_VERSION=vX.Y.Z"
fi

say "installing cormex ${version} for ${target}"

# ── Download ──────────────────────────────────────────────
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
base="https://github.com/${REPO}/releases/download/${version}"
archive="${BIN}-${version#v}-${target}.tar.gz"

curl -fsSL "${base}/${archive}" -o "${tmp}/${archive}" \
    || die "could not download ${archive}"
curl -fsSL "${base}/SHA256SUMS" -o "${tmp}/SHA256SUMS" \
    || die "could not download SHA256SUMS"

# ── Verify ────────────────────────────────────────────────
# `sha256sum` on Linux, `shasum -a 256` on macOS. Comparing only the digest of
# the one file we downloaded, rather than running `sha256sum -c` over the whole
# list: the list covers six platforms, and the other five are not here
expected=$(grep " ${archive}\$" "${tmp}/SHA256SUMS" | cut -d' ' -f1)
[ -n "$expected" ] || die "SHA256SUMS does not list ${archive}"
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "${tmp}/${archive}" | cut -d' ' -f1)
else
    actual=$(shasum -a 256 "${tmp}/${archive}" | cut -d' ' -f1)
fi
[ "$expected" = "$actual" ] || die "checksum mismatch for ${archive}
  expected ${expected}
  actual   ${actual}
The download is corrupt. Try again; if it persists, the Release is broken."

# ── Install ───────────────────────────────────────────────
tar -xzf "${tmp}/${archive}" -C "$tmp"
dir="${HOME}/.local/bin"
mkdir -p "$dir"
install -m 0755 "${tmp}/${BIN}-${version#v}-${target}/${BIN}" "${dir}/${BIN}"

say ""
say "installed to ${dir}/${BIN}"
case ":${PATH}:" in
    *":${dir}:"*) ;;
    *) say ""
       say "⚠ ${dir} is not on your PATH. Add it:"
       say "    export PATH=\"${dir}:\$PATH\"" ;;
esac
say ""
say "next: ${BIN} login --server <your cormex server>"
say "then: ${BIN} init"
