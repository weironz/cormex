#!/usr/bin/env bash
# Run the deployment playbook from wherever this repository is checked out.
#
# Ansible has no Windows control node -- `ansible-playbook` is POSIX-only -- so this
# wrapper runs it natively when the host has it (Linux, macOS, WSL) and otherwise
# inside the pinned control-node image (deploy/ansible/Dockerfile). That keeps
# `just deploy vX.Y.Z` the same command on every machine; there is no second set of
# instructions for Windows, only a slower first run while the image is built.
set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
usage() {
  echo "usage: $0 TAG [INVENTORY]" >&2
  echo "       $0 --syntax-check" >&2
  exit 2
}

mode=${1:-}
case "$mode" in
  --syntax-check)
    shift
    inventory=deploy/ansible/inventory.example.ini
    tag=""
    goal=(--syntax-check)
    ;;
  -h|--help|"")
    usage
    ;;
  *)
    tag=$1
    shift
    inventory=${1:-deploy/ansible/inventory.ini}
    shift || true
    goal=()
    ;;
esac

case "$inventory" in
  /*) inventory_path=$inventory ;;
  *) inventory_path=$repo/$inventory ;;
esac
[[ -f "$inventory_path" ]] || {
  echo "inventory not found: $inventory_path" >&2
  echo "copy deploy/ansible/inventory.example.ini to deploy/ansible/inventory.ini and fill it in" >&2
  exit 1
}

args=(-i "$inventory" deploy/ansible/deploy.yml)
[[ ${#goal[@]} -gt 0 ]] && args+=("${goal[@]}")
[[ -n "$tag" ]] && args+=(-e "cormex_version=$tag")

if command -v ansible-playbook >/dev/null 2>&1; then
  cd "$repo"
  exec ansible-playbook "${args[@]}"
fi

command -v docker >/dev/null 2>&1 || {
  echo "no ansible-playbook and no docker on this host." >&2
  echo "Install ansible-core (POSIX control node, WSL included) or Docker Desktop." >&2
  exit 1
}

# Docker Desktop wants Windows paths; Git Bash hands it /c/... and rewrites the
# container side of -v, so convert explicitly and stop MSYS from touching the rest.
if command -v cygpath >/dev/null 2>&1; then
  repo=$(cygpath -m "$repo")
fi
image=${CORMEX_ANSIBLE_IMAGE:-cormex-ansible:control}
docker build -q -t "$image" "$repo/deploy/ansible" >/dev/null
exec docker run --rm --workdir /repo \
  -v "$repo:/repo" \
  -v "${HOME}/.ssh:/root/.ssh:ro" \
  -e ANSIBLE_FORCE_COLOR=1 \
  "$image" "${args[@]}"
