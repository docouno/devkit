#!/usr/bin/env bash
set -euo pipefail
fail() { printf 'devkit: %s\n' "$*" >&2; exit 1; }
release=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
if [[ $# == 0 || ${1:-} == help || ${1:-} == --help || ${1:-} == -h ]]; then
  printf '%s\n' 'devkit init TARGET --template node|react|rust|mixed [--package-source local|registry] [--name slug]' \
    'devkit update TARGET [--package-source local|registry]' 'devkit recover TARGET' \
    'devkit register TARGET --app slug --template node|react|rust'
  exit 0
fi
[[ $# -ge 2 ]] || fail 'expected a command and target path; run devkit --help'
operation=$1 requested=$2
case "$operation" in init|update|recover|register) ;; *) fail "unknown command: $operation" ;; esac
shift 2
for tool in docker flock realpath sha256sum; do command -v "$tool" >/dev/null || fail "missing prerequisite: $tool"; done
[[ -n $requested ]] || fail 'target path cannot be empty'
logical=$(realpath -ms -- "$requested")
canonical=$(realpath -m -- "$requested")
[[ $logical == "$canonical" ]] || fail 'target path must not contain symlinks'
parent=$(dirname -- "$canonical")
leaf=$(basename -- "$canonical")
[[ -d $parent && $canonical != / ]] || fail 'target parent directory must already exist'
if [[ $operation != init ]]; then
  [[ -d $canonical && -d $canonical/.devkit ]] || fail 'target is not a generated project'
  for path in "$canonical/.devkit" "$canonical/.devkit/state" "$canonical/.devkit/state/task.lock"; do
    [[ ! -L $path ]] || fail "refusing symlink state path: $path"
  done
  [[ ! -e $canonical/.devkit/state || -d $canonical/.devkit/state ]] || fail 'state must be a directory'
  [[ ! -e $canonical/.devkit/state/task.lock || -f $canonical/.devkit/state/task.lock ]] || fail 'task.lock must be a regular file'
  mkdir -p -- "$canonical/.devkit/state"
  exec 9>"$canonical/.devkit/state/task.lock"
  flock -n 9 || fail 'another devkit operation is active in this checkout'
fi
image= node_version=
while IFS= read -r line || [[ -n $line ]]; do
  case "$line" in DEVKIT_NODE_IMAGE=*) [[ -z $image ]] || fail 'duplicate Node image pin'; image=${line#*=} ;;
    DEVKIT_NODE_VERSION=*) [[ -z $node_version ]] || fail 'duplicate Node version pin'; node_version=${line#*=} ;; esac
done < "$release/runtime/toolchains.env"
[[ $image =~ ^[a-z0-9./:-]+@sha256:[a-f0-9]{64}$ && $node_version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail 'invalid release Node pins'
# Factory operations are explicit; ordinary consumer gates never pull tools.
docker image inspect "$image" >/dev/null 2>&1 || docker pull "$image"
token=$(cat /proc/sys/kernel/random/uuid)
name="devkit-factory-$token"
project=$(printf '%s' "$canonical" | sha256sum)
project="devkit-factory-${project:0:16}"
export DEVKIT_RELEASE_ROOT=$release DEVKIT_FACTORY_PARENT=$parent
export DEVKIT_NODE_IMAGE=$image DEVKIT_NODE_VERSION=$node_version
export DEVKIT_UID=$(id -u) DEVKIT_GID=$(id -g) DEVKIT_INVOCATION_TOKEN=$token
unset COMPOSE_FILE COMPOSE_PROJECT_NAME COMPOSE_PROFILES COMPOSE_ENV_FILES COMPOSE_PATH_SEPARATOR
exec bash "$release/runtime/guard.sh" "$name" "$token" -- \
  docker compose --project-directory "$release" --env-file /dev/null --project-name "$project" \
  --file "$release/compose.factory.yaml" run --pull never --rm -T --no-deps --name "$name" \
  --label "io.docouno.devkit.invocation=$token" factory "$operation" "/workspace/$leaf" "$@"
