#!/usr/bin/env bash
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
first=$(realpath -e -- "${1:?expected first release directory}")
second=$(realpath -e -- "${2:?expected second release directory}")
image=$(sed -n 's/^DEVKIT_NODE_IMAGE=//p' "$root/toolchains.env")
[[ $image =~ @sha256:[a-f0-9]{64}$ ]] || { printf '%s\n' 'Invalid Node image pin' >&2; exit 1; }
token=$(cat /proc/sys/kernel/random/uuid)
name="devkit-registry-$token"
mkdir -p -- "$root/.artifacts/registry"
set -o pipefail
bash "$root/runtime/guard.sh" "$name" "$token" -- docker run --rm --init --pull never \
  --name "$name" --label "io.docouno.devkit.invocation=$token" \
  --user "$(id -u):$(id -g)" --env HOME=/tmp/devkit-home --env CI=true \
  --volume "$first:/release-v1:ro,z" --volume "$second:/release-v2:ro,z" \
  --volume "$root/.artifacts/cli-author:/fixture:ro,z" \
  --volume "$root/test/registry-consumer.mjs:/registry-consumer.mjs:ro,z" \
  "$image" node /registry-consumer.mjs 2>&1 | tee "$root/.artifacts/registry/latest.log"
