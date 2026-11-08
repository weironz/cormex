#!/usr/bin/env bash
# Run the deployment playbook from wherever this repository is checked out.
#
# The inventory is committed (deploy/ansible/inventory.yml) and reads every host
# detail from the environment, so the same file works for GitHub Actions (secrets),
# a Linux maintainer (deploy/ansible/deploy.env or the shell) and Windows (the same
# env file, executed inside a control-node container).
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

load_env_file() {
  local file=$1 line key value
  while IFS= read -r line || [[ -n $line ]]; do
    line=${line%$'\r'}
    [[ $line =~ ^[[:space:]]*(#|$) ]] && continue
    [[ $line == *=* ]] || continue
    key=${line%%=*}
    value=${line#*=}
    key=${key//[[:space:]]/}
    value=${value#"${value%%[![:space:]]*}"}
    [[ $key =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] || continue
    # A matching pair of quotes is the only quoting supported; anything else would be
    # shell syntax, and this file is data -- it is never sourced.
    if [[ $value == \"*\" || $value == \'*\' ]]; then value=${value:1:${#value}-2}; fi
    export "$key=$value"
  done < "$file"
}

expand_tilde() {
  case $1 in
    "~"/*) printf '%s\n' "$HOME/${1#\~/}" ;;
    *) printf '%s\n' "$1" ;;
  esac
}

# Local, gitignored values. Explicit path wins, so a second environment (a rehearsal
# box, a different operator) can keep its own file.
values_file=${CORMEX_DEPLOY_ENV_FILE:-deploy/ansible/deploy.env}
[[ $values_file == /* ]] || values_file=$repo/$values_file
[[ -f $values_file ]] && load_env_file "$values_file"

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

connection=${CORMEX_DEPLOY_CONNECTION:-ssh}
if [[ -z ${CORMEX_DEPLOY_HOST:-} && $connection != local && ${#goal[@]} -eq 0 ]]; then
  die "CORMEX_DEPLOY_HOST is empty; set it in the environment or in $values_file (copy deploy/ansible/deploy.env.example)"
fi

args=(-i "$inventory" deploy/ansible/deploy.yml)
[[ ${#goal[@]} -gt 0 ]] && args+=("${goal[@]}")
[[ -n "$tag" ]] && args+=(-e "cormex_version=$tag")

if [[ -n ${CORMEX_DEPLOY_KNOWN_HOSTS:-} ]]; then
  export ANSIBLE_SSH_COMMON_ARGS="-o UserKnownHostsFile=$(expand_tilde "$CORMEX_DEPLOY_KNOWN_HOSTS") -o StrictHostKeyChecking=yes"
fi

if command -v ansible-playbook >/dev/null 2>&1; then
  cd "$repo"
  exec ansible-playbook "${args[@]}"
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
for key in CORMEX_DEPLOY_HOST CORMEX_DEPLOY_USER CORMEX_DEPLOY_PORT CORMEX_DEPLOY_DIR \
           CORMEX_DEPLOY_API CORMEX_DEPLOY_PROJECT CORMEX_DEPLOY_CONNECTION; do
  [[ -n ${!key:-} ]] && environment+=(-e "$key=${!key}")
done
if [[ -n ${CORMEX_DEPLOY_KEY:-} ]]; then
  key_file=$(expand_tilde "$CORMEX_DEPLOY_KEY")
  [[ -f $key_file ]] || die "private key not found: $key_file"
  mounts+=(-v "$key_file:/root/.ssh/deploy_key:ro")
  environment+=(-e CORMEX_DEPLOY_KEY=/root/.ssh/deploy_key)
fi
if [[ -n ${CORMEX_DEPLOY_KNOWN_HOSTS:-} ]]; then
  known_hosts=$(expand_tilde "$CORMEX_DEPLOY_KNOWN_HOSTS")
  [[ -f $known_hosts ]] || die "known_hosts not found: $known_hosts"
  mounts+=(-v "$known_hosts:/root/.ssh/known_hosts_deploy:ro")
  environment+=(-e "ANSIBLE_SSH_COMMON_ARGS=-o UserKnownHostsFile=/root/.ssh/known_hosts_deploy -o StrictHostKeyChecking=yes")
fi

image=${CORMEX_ANSIBLE_IMAGE:-cormex-ansible:control}
docker build -q -t "$image" "$repo/deploy/ansible" >/dev/null
exec docker run --rm --workdir /repo \
  "${mounts[@]}" \
  "${environment[@]}" \
  -e ANSIBLE_FORCE_COLOR=1 \
  "$image" "${args[@]}"
