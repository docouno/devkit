#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
image=$(sed -n 's/^DEVKIT_NODE_IMAGE=//p' "$root/toolchains.env")
if [[ ! "$image" =~ ^[a-z0-9./:-]+@sha256:[a-f0-9]{64}$ ]]; then
  printf '%s\n' 'Invalid pinned Node image in toolchains.env' >&2
  exit 1
fi
for executable in docker sha256sum; do
  command -v "$executable" >/dev/null || { printf 'Missing prerequisite: %s\n' "$executable" >&2; exit 1; }
done
if [[ ${1:-} == --setup ]]; then
  shift
  docker image inspect "$image" >/dev/null 2>&1 || docker pull "$image"
elif ! docker image inspect "$image" >/dev/null 2>&1; then
  printf '%s\n' 'Pinned Node image is missing. Run make setup.' >&2
  exit 1
fi
if [[ $# == 0 ]]; then
  printf '%s\n' 'Expected a command for the Node container.' >&2
  exit 1
fi
mkdir -p "$root/.artifacts/npm-cache"
token=$(printf '%s\n' "$root:$BASHPID:$RANDOM:$RANDOM" | sha256sum)
token=${token:0:24}
name="devkit-author-$token"
project=$(printf '%s' "$root" | sha256sum)
project="devkit-author-${project:0:16}"
export DEVKIT_AUTHOR_ROOT="$root" DEVKIT_NODE_IMAGE="$image"
export DEVKIT_UID="$(id -u)" DEVKIT_GID="$(id -g)"
export DEVKIT_INVOCATION_TOKEN="$token"
exec bash "$root/runtime/guard.sh" "$name" "$token" -- \
  docker compose --project-directory "$root" --env-file /dev/null \
  --project-name "$project" --file "$root/compose.yaml" \
  run --pull never --rm -T --no-deps --name "$name" author "$@"
