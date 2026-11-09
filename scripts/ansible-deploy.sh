#!/usr/bin/env bash
# Run the deployment playbook from wherever this repository is checked out.
#
# The inventory is committed (deploy/ansible/inventory.yml) and reads every host
# detail from the environment -- there is no local values file. The same variables
# come from repository secrets in GitHub Actions and from the shell (or the system
# environment) on a workstation; a missing required one is an error, not a default.
#
# Ansible has no Windows control node: `ansible-playbook` is POSIX-only. The wrapper
# therefore runs it natively when present (Linux, macOS, WSL) and otherwise inside the
# pinned image from deploy/ansible/Dockerfile. The key file stays on the machine; the
# container only gets a read-only mount of it.
set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
usage() {
  echo "usage: $0 TAG [INVENTORY]" >&2
  echo "       $0 --syntax-check" >&2
  exit 2
}
die() { echo "deploy: $*" >&2; exit 1; }

expand_tilde() {
  case $1 in
    "~"/*) printf '%s\n' "$HOME/${1#\~/}" ;;
    *) printf '%s\n' "$1" ;;
  esac
}

mode=${1:-}
case "$mode" in
  --syntax-check)
    shift
    inventory=deploy/ansible/inventory.yml
    tag=""
    goal=(--syntax-check)
    ;;
  -h|--help|"")
    usage
    ;;
  *)
    tag=$1
    shift
    inventory=${1:-deploy/ansible/inventory.yml}
    shift || true
    goal=()
    ;;
esac

case "$inventory" in
  /*) inventory_path=$inventory ;;
  *) inventory_path=$repo/$inventory ;;
esac
[[ -f "$inventory_path" ]] || die "inventory not found: $inventory_path"

connection=${DEPLOY_CONNECTION:-ssh}
if [[ ${#goal[@]} -eq 0 ]]; then
  # No file to fall back to: every value the connection needs must already be in the
  # environment, and a missing one is named rather than silently defaulted.
  missing=()
  if [[ $connection != local ]]; then
    [[ -n ${DEPLOY_HOST:-} ]] || missing+=(DEPLOY_HOST)
    [[ -n ${DEPLOY_USER:-} ]] || missing+=(DEPLOY_USER)
  fi
  [[ -n ${DEPLOY_DIR:-} ]] || missing+=(DEPLOY_DIR)
  [[ -n ${DEPLOY_API:-} ]] || missing+=(DEPLOY_API)
  if (( ${#missing[@]} > 0 )); then
    die "missing environment variable(s): ${missing[*]} -- export them (docs/deployment-guide.md, section 9)"
  fi
fi

args=(-i "$inventory" deploy/ansible/deploy.yml)
[[ ${#goal[@]} -gt 0 ]] && args+=("${goal[@]}")
[[ -n "$tag" ]] && args+=(-e "cormex_version=$tag")

# Upgrade approval is explicit and remains independent from candidate publication.
# The relative extra-vars path also works inside the mounted control-node image.
expectations_file=""
if (( ${#goal[@]} == 0 )) && [[ -n ${DEPLOY_FROM_VERSION:-} || -n ${DEPLOY_BUNDLE_SHA256:-} ]]; then
  [[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "invalid target tag"
  [[ ${DEPLOY_FROM_VERSION:-} =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "DEPLOY_FROM_VERSION must be vX.Y.Z"
  [[ ${DEPLOY_BUNDLE_SHA256:-} =~ ^[0-9a-f]{64}$ ]] || die "DEPLOY_BUNDLE_SHA256 must be the approved checksum"
  [[ ${DEPLOY_PULL:-true} != false ]] || die "deploy-upgrade requires registry verification (DEPLOY_PULL=true)"
  expectations_file=$(mktemp "$repo/.deploy-expectations.XXXXXX.json")
  trap 'rm -f "$expectations_file"' EXIT
  python3 "$repo/scripts/deploy_candidate.py" "$tag" "$DEPLOY_BUNDLE_SHA256" --output "$expectations_file"
  args+=(-e "cormex_from_version=$DEPLOY_FROM_VERSION" -e "@${expectations_file##*/}")
fi

if [[ -n ${DEPLOY_KNOWN_HOSTS:-} ]]; then
  export ANSIBLE_SSH_COMMON_ARGS="-o UserKnownHostsFile=$(expand_tilde "$DEPLOY_KNOWN_HOSTS") -o StrictHostKeyChecking=yes"
fi

if command -v ansible-playbook >/dev/null 2>&1; then
  cd "$repo"
  ansible-playbook "${args[@]}"
  exit $?
fi

command -v docker >/dev/null 2>&1 || die "no ansible-playbook and no docker on this host; install ansible-core (POSIX, WSL included) or Docker Desktop"

if command -v cygpath >/dev/null 2>&1; then
  # Docker Desktop wants Windows paths; Git Bash hands it /c/... and rewrites the
  # container side of -v, so convert explicitly.
  repo=$(cygpath -m "$repo")
fi

mounts=(-v "$repo:/repo")
[[ -d $HOME/.ssh ]] && mounts+=(-v "$HOME/.ssh:/root/.ssh:ro")
environment=()
for key in DEPLOY_HOST DEPLOY_USER DEPLOY_PORT DEPLOY_DIR DEPLOY_API \
           DEPLOY_PROJECT DEPLOY_CONNECTION DEPLOY_PULL DEPLOY_REQUIRE_ENCRYPTION; do
  [[ -n ${!key:-} ]] && environment+=(-e "$key=${!key}")
done
if [[ -n ${DEPLOY_SSH_KEY:-} ]]; then
  key_file=$(expand_tilde "$DEPLOY_SSH_KEY")
  [[ -f $key_file ]] || die "private key not found: $key_file"
  mounts+=(-v "$key_file:/root/.ssh/deploy_key:ro")
  environment+=(-e DEPLOY_SSH_KEY=/root/.ssh/deploy_key)
fi
if [[ -n ${DEPLOY_KNOWN_HOSTS:-} ]]; then
  known_hosts=$(expand_tilde "$DEPLOY_KNOWN_HOSTS")
  [[ -f $known_hosts ]] || die "known_hosts not found: $known_hosts"
  mounts+=(-v "$known_hosts:/root/.ssh/known_hosts_deploy:ro")
  environment+=(-e "ANSIBLE_SSH_COMMON_ARGS=-o UserKnownHostsFile=/root/.ssh/known_hosts_deploy -o StrictHostKeyChecking=yes")
fi

image=${DEPLOY_ANSIBLE_IMAGE:-cormex-ansible:control}
docker build -q -t "$image" "$repo/deploy/ansible" >/dev/null
docker run --rm --workdir /repo \
  "${mounts[@]}" \
  "${environment[@]}" \
  -e ANSIBLE_FORCE_COLOR=1 \
  "$image" "${args[@]}"
