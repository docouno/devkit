#!/usr/bin/env bash
set -euo pipefail

fail() { printf 'devkit: %s\n' "$*" >&2; exit 1; }
root=$(realpath -ms -- "$(dirname -- "${BASH_SOURCE[0]}")/../..")
runtime="$root/.devkit/runtime"
command=${1:-help}
[[ $# -le 1 ]] || fail 'expected one command; use app=<registered-name>'
case "$command" in
  help)
    printf '%s\n' 'make setup|check|fix|test|build [app=<name>]' \
      'make <command>-<name>  (ordinary Make dependency edges are supported)' \
      'make dev|up [app=<name>]   detached product services' \
      'make status|logs [app=<name>] [tail=100] [follow=1]' \
      'make down                 stop this checkout; preserve named volumes' \
      'make doctor               check host prerequisites and accepted tools'
    exit 0 ;;
  doctor|setup|check|fix|test|build|dev|up|down|logs|status|snapshot) ;;
  *) fail "unknown command: $command" ;;
esac

for tool in docker flock sha256sum realpath; do command -v "$tool" >/dev/null || fail "missing host prerequisite: $tool"; done
[[ -d $root && $(realpath -e -- "$root") == "$root" ]] || fail 'checkout path must not contain symlinks'
# Validate every existing prefix before mkdir, lock open, bind or SELinux relabel.
# This prevents accidental path redirection, not concurrent malicious rewrites.
safe_path() {
  local path=$1 kind=$2 relative current=$root part i
  [[ $path == "$root/"* ]] || fail "path is outside this checkout: $path"
  relative=${path#"$root/"}
  local -a parts
  IFS=/ read -r -a parts <<< "$relative"
  for ((i=0; i<${#parts[@]}; i++)); do
    part=${parts[$i]}
    [[ -n $part && $part != . && $part != .. ]] || fail "invalid runtime path: $path"
    current+="/$part"
    [[ ! -L $current ]] || fail "refusing symlink runtime path: $current"
    if [[ -e $current ]]; then
      if [[ $i -lt $((${#parts[@]} - 1)) || $kind == directory ]]; then
        [[ -d $current ]] || fail "expected runtime directory: $current"
      else
        [[ -f $current ]] || fail "expected regular runtime file: $current"
      fi
    fi
  done
}
for path in "$root/.devkit/state" "$root/.devkit/cache/npm" "$root/.devkit/cache/cargo" "$runtime"; do safe_path "$path" directory; done
for path in "$root/.devkit/state/task.lock" "$root/.devkit/state/rust-image" "$root/.devkit/runtime.sha256" "$root/.devkit/manifest.json" "$runtime/toolchains.env"; do safe_path "$path" file; done
[[ ! -e $root/.devkit/update-pending.json && ! -L $root/.devkit/update-pending.json ]] || fail 'an update is incomplete or its pending state is malformed; run the release updater recovery before ordinary commands'
[[ ${DEVKIT_APPS:-} != *$'\n'* ]] || fail 'DEVKIT_APPS must be a single whitespace-separated list'
read -r -a apps <<< "${DEVKIT_APPS:-}"
[[ ${#apps[@]} -gt 0 ]] || fail 'DEVKIT_APPS must register at least one app'
declare -A app_names=()
for app in "${apps[@]}"; do
  [[ $app =~ ^[a-z][a-z0-9]*(-[a-z0-9]+)*$ && ${#app} -le 63 ]] || fail "invalid app identifier: $app"
  [[ ! -v app_names[$app] ]] || fail "duplicate app registration: $app"
  app_names[$app]=1
  for path in "$root/apps/$app/node_modules" "$root/apps/$app/dist" "$root/apps/$app/target"; do safe_path "$path" directory; done
  for path in "$root/.devkit/state/node-$app.lock" "$root/.devkit/state/node-$app.lock.tmp" "$root/apps/$app/package.json" "$root/apps/$app/package-lock.json" "$root/apps/$app/Cargo.toml" "$root/apps/$app/Cargo.lock"; do safe_path "$path" file; done
done
mkdir -p -- "$root/.devkit/state"
exec 9>>"$root/.devkit/state/task.lock"
flock -n 9 || fail 'another devkit operation is active in this checkout'
[[ ! -e $root/.devkit/update-pending.json && ! -L $root/.devkit/update-pending.json ]] || fail 'an update is incomplete or its pending state is malformed; run the release updater recovery before ordinary commands'
[[ ${DEVKIT_EXPECTED_MANIFEST:-} =~ ^[a-f0-9]{64}$ ]] || fail 'missing accepted-toolkit snapshot; invoke the root Makefile'
[[ -f $root/.devkit/manifest.json ]] || fail 'missing accepted release manifest'
manifest_hash=$(sha256sum "$root/.devkit/manifest.json")
[[ ${manifest_hash%% *} == "$DEVKIT_EXPECTED_MANIFEST" ]] || fail 'accepted toolkit changed during this Make invocation; rerun the command'
[[ -f $root/.devkit/runtime.sha256 ]] || fail 'missing managed runtime.sha256'
(cd -- "$root"; sha256sum --check --status .devkit/runtime.sha256) || fail 'managed runtime differs from the accepted release; restore it or resolve the update conflict'
[[ -f $runtime/toolchains.env ]] || fail 'missing managed toolchains.env'

# This is a data file, not executable shell configuration.
declare -A toolchain_keys=()
unset DEVKIT_VERSION DEVKIT_NODE_IMAGE DEVKIT_NODE_VERSION DEVKIT_RUST_VERSION DEVKIT_RUST_RECIPE_HASH
while IFS= read -r line || [[ -n $line ]]; do
  [[ -z $line || $line == \#* ]] && continue
  [[ $line == *=* ]] || fail 'invalid toolchains.env record'
  key=${line%%=*} value=${line#*=}
  case "$key" in
    DEVKIT_VERSION|DEVKIT_NODE_IMAGE|DEVKIT_NODE_VERSION|DEVKIT_RUST_VERSION|DEVKIT_RUST_RECIPE_HASH) ;;
    *) fail "unknown toolchain key: $key" ;;
  esac
  [[ ! -v toolchain_keys[$key] ]] || fail "duplicate toolchain key: $key"
  toolchain_keys[$key]=1
  [[ -n $value && $value != *[[:space:]]* && $value != *\'* && $value != *\"* ]] || fail "invalid toolchain value: $key"
  printf -v "$key" '%s' "$value"
  export "$key"
done < "$runtime/toolchains.env"
for key in DEVKIT_VERSION DEVKIT_NODE_IMAGE DEVKIT_NODE_VERSION DEVKIT_RUST_VERSION DEVKIT_RUST_RECIPE_HASH; do
  [[ -n ${!key:-} ]] || fail "missing toolchain key: $key"
done
[[ $DEVKIT_NODE_IMAGE == *@sha256:* && $DEVKIT_RUST_RECIPE_HASH =~ ^[a-f0-9]{64}$ ]] || fail 'toolchain pins must be immutable'

declare -A languages=()
for app in "${apps[@]}"; do
  [[ $app =~ ^[a-z][a-z0-9]*(-[a-z0-9]+)*$ ]] || fail "invalid app identifier: $app"
  [[ ! -v languages[$app] ]] || fail "duplicate app registration: $app"
  path="$root/apps/$app"
  [[ -d $path && $(realpath -e -- "$path") == "$path" ]] || fail "missing or symlinked app directory: apps/$app"
  if [[ -f $path/package.json && ! -f $path/Cargo.toml ]]; then
    [[ -f $path/package-lock.json ]] || fail "apps/$app requires package-lock.json"
    languages[$app]=node
  elif [[ -f $path/Cargo.toml && ! -f $path/package.json ]]; then
    [[ -f $path/Cargo.lock ]] || fail "apps/$app requires Cargo.lock"
    languages[$app]=rust
  else
    fail "apps/$app must have exactly one supported native manifest (package.json or Cargo.toml)"
  fi
done
selector=${DEVKIT_APP:-}
[[ -z $selector || $selector =~ ^[a-z][a-z0-9]*(-[a-z0-9]+)*$ ]] || fail "invalid app selector: $selector"
[[ -z $selector || -v languages[$selector] ]] || fail "unregistered app: $selector"
target=${DEVKIT_TARGET_APP:-}
[[ -z $target || $target =~ ^[a-z][a-z0-9]*(-[a-z0-9]+)*$ ]] || fail "invalid target app: $target"
[[ -z $target || -v languages[$target] ]] || fail "unregistered target app: $target"
selected=("${apps[@]}")
if [[ -n $target ]]; then selected=("$target"); elif [[ -n $selector ]]; then selected=("$selector"); fi

export DEVKIT_PROJECT_ROOT=$root
export DEVKIT_PROJECT_NAME="devkit-$(printf '%s' "$root" | sha256sum | cut -c1-16)"
export DEVKIT_UID=$(id -u) DEVKIT_GID=$(id -g)
# Disregard ambient Compose routing/configuration. .env is explicitly disabled.
unset COMPOSE_FILE COMPOSE_PROJECT_NAME COMPOSE_PROFILES COMPOSE_ENV_FILES COMPOSE_PATH_SEPARATOR
compose=(docker compose --project-directory "$root" --project-name "$DEVKIT_PROJECT_NAME" --env-file /dev/null)
active_pid=
launching=0
cancel_status=0
set -m
cancel() {
  local status=$1
  trap '' INT TERM
  if [[ -n $active_pid ]]; then
    kill -TERM -- "-$active_pid" 2>/dev/null || true
    wait "$active_pid" 2>/dev/null || true
  fi
  exit "$status"
}
request_cancel() {
  cancel_status=$1
  if [[ $launching == 0 ]]; then cancel "$cancel_status"; fi
}
trap 'request_cancel 130' INT
trap 'request_cancel 143' TERM
run_child() {
  local result=0
  launching=1
  "$@" &
  active_pid=$!
  launching=0
  [[ $cancel_status == 0 ]] || cancel "$cancel_status"
  wait "$active_pid" || result=$?
  active_pid=
  return "$result"
}

node_image() {
  if ! docker image inspect "$DEVKIT_NODE_IMAGE" >/dev/null 2>&1; then
    [[ $command == setup ]] || fail 'accepted Node image is absent; run make setup'
    run_child docker pull "$DEVKIT_NODE_IMAGE"
  fi
}
rust_image() {
  local actual recorded label iid tmp
  actual=$(sha256sum "$runtime/rust.Dockerfile")
  [[ ${actual%% *} == "$DEVKIT_RUST_RECIPE_HASH" ]] || fail 'Rust Dockerfile differs from the accepted recipe'
  recorded=
  if [[ -f $root/.devkit/state/rust-image ]]; then recorded=$(<"$root/.devkit/state/rust-image"); fi
  if [[ $recorded =~ ^sha256:[a-f0-9]{64}$ ]]; then
    label=$(docker image inspect --format '{{index .Config.Labels "io.docouno.devkit.rust-recipe"}}' "$recorded" 2>/dev/null || true)
    if [[ $label == "$DEVKIT_RUST_RECIPE_HASH" ]]; then export DEVKIT_RUST_IMAGE=$recorded; return; fi
  fi
  [[ $command == setup ]] || fail 'accepted derived Rust image is absent/stale; run make setup'
  tmp=$(mktemp "$root/.devkit/state/rust-image.XXXXXX")
  if ! run_child docker build --pull=false --build-arg "DEVKIT_RECIPE_SHA=$DEVKIT_RUST_RECIPE_HASH" \
    --label "io.docouno.devkit.rust-recipe=$DEVKIT_RUST_RECIPE_HASH" --iidfile "$tmp" \
    --file "$runtime/rust.Dockerfile" "$runtime"; then
    rm -f -- "$tmp"
    fail 'Rust tool image preparation failed'
  fi
  iid=$(<"$tmp")
  [[ $iid =~ ^sha256:[a-f0-9]{64}$ ]] || { rm -f -- "$tmp"; fail 'build did not return an immutable Rust image ID'; }
  label=$(docker image inspect --format '{{index .Config.Labels "io.docouno.devkit.rust-recipe"}}' "$iid")
  [[ $label == "$DEVKIT_RUST_RECIPE_HASH" ]] || { rm -f -- "$tmp"; fail 'derived Rust image recipe label mismatch'; }
  printf '%s\n' "$iid" > "$tmp"
  mv -f -- "$tmp" "$root/.devkit/state/rust-image"
  export DEVKIT_RUST_IMAGE=$iid
}
node_lock_hash() { (cd -- "$1"; sha256sum package.json package-lock.json | sha256sum | cut -d' ' -f1); }
tool_task() {
  local app=$1 language=${languages[$1]} token name fingerprint state receipt
  export DEVKIT_CURRENT_APP=$app DEVKIT_APP_ROOT="$root/apps/$app" DEVKIT_SOURCE_READ_ONLY=true
  [[ $command != fix ]] || DEVKIT_SOURCE_READ_ONLY=false
  mkdir -p -- "$root/.devkit/cache/npm" "$root/.devkit/cache/cargo"
  if [[ $language == node ]]; then
    node_image
    fingerprint=$(node_lock_hash "$DEVKIT_APP_ROOT")
    state="$root/.devkit/state/node-$app.lock"
    if [[ $command == setup ]]; then
      # npm ci mutates the existing dependency tree. An old receipt cannot attest
      # that tree after a failed or interrupted repeat install with equal inputs.
      rm -f -- "$state"
    else
      [[ -d $DEVKIT_APP_ROOT/node_modules && -f $state && $(<"$state") == "$fingerprint" ]] || fail "apps/$app dependencies are absent/stale; run make setup app=$app"
    fi
    mkdir -p -- "$DEVKIT_APP_ROOT/node_modules" "$DEVKIT_APP_ROOT/dist"
  else
    rust_image
    mkdir -p -- "$DEVKIT_APP_ROOT/target"
  fi
  token=$(cat /proc/sys/kernel/random/uuid)
  name="$DEVKIT_PROJECT_NAME-task-$token"
  local argv=("${compose[@]}" --file "$runtime/compose.tools.yaml" run --pull never --rm -T --no-deps \
    --name "$name" --label "io.docouno.devkit.invocation=$token" "$language")
  if [[ $language == node ]]; then
    argv+=(sh -ec 'mkdir -p "$HOME"; node -e '\''const fs=require("node:fs");const p=JSON.parse(fs.readFileSync("package.json","utf8"));for(const name of ["check","fix","test","build","dev"]){if(typeof p.scripts?.[name]!=="string"||!p.scripts[name].trim())throw Error("Missing native npm script: "+name)}if(process.versions.node!==process.argv[1])throw Error("Unexpected Node version: "+process.versions.node)'\'' "$1"; shift; exec "$@"' sh "$DEVKIT_NODE_VERSION")
    if [[ $command == setup ]]; then argv+=(npm ci --include=dev --strict-peer-deps); else argv+=(npm run "$command"); fi
  else
    argv+=(bash /workspace/.devkit/runtime/rust-check.sh "$command")
  fi
  run_child bash "$runtime/guard.sh" "$name" "$token" -- "${argv[@]}"
  if [[ $language == node && $command == setup ]]; then
    [[ $(node_lock_hash "$DEVKIT_APP_ROOT") == "$fingerprint" ]] || fail 'native manifest/lock changed during setup'
    receipt=$(mktemp "$root/.devkit/state/node-$app.XXXXXX")
    printf '%s\n' "$fingerprint" > "$receipt"
    mv -f -- "$receipt" "$state"
  fi
}

docker compose version >/dev/null || fail 'Docker Compose is unavailable'
docker info --format '{{.OSType}}' | { read -r type; [[ $type == linux ]]; } || fail 'a working Linux Docker daemon is required'
# Control metadata validation has no dependency/cache/output mounts or network.
# Setup prepares this pinned base even for a Rust-only checkout.
node_image
export DEVKIT_CURRENT_APP=${selected[0]} DEVKIT_APP_ROOT="$root/apps/${selected[0]}" DEVKIT_SOURCE_READ_ONLY=true
token=$(cat /proc/sys/kernel/random/uuid)
name="$DEVKIT_PROJECT_NAME-control-$token"
run_child bash "$runtime/guard.sh" "$name" "$token" -- "${compose[@]}" --file "$runtime/compose.tools.yaml" \
  run --pull never --rm -T --no-deps --name "$name" --label "io.docouno.devkit.invocation=$token" \
  control node /workspace/.devkit/runtime/verify-manifest.mjs /workspace "$DEVKIT_VERSION" "${apps[@]}"
[[ $command != snapshot ]] || exit 0
case "$command" in
  setup|check|fix|test|build)
    for app in "${selected[@]}"; do tool_task "$app"; done ;;
  doctor)
    printf 'devkit %s\ncheckout %s\nproject %s\n' "$DEVKIT_VERSION" "$root" "$DEVKIT_PROJECT_NAME"
    docker compose version
    for app in "${selected[@]}"; do
      if [[ ${languages[$app]} == node ]]; then node_image; else rust_image; fi
      printf 'app %s: %s tool image present\n' "$app" "${languages[$app]}"
    done ;;
  dev|up|down|logs|status)
    [[ -f $root/compose.yaml ]] || fail 'this project has no compose.yaml for development services'
    # All variables are explicit; product-owned Compose controls app runtime env.
    export DEVKIT_RUST_IMAGE=devkit-unprepared-rust
    if [[ -f $root/.devkit/state/rust-image ]]; then export DEVKIT_RUST_IMAGE=$(<"$root/.devkit/state/rust-image"); fi
    if [[ $command == dev || $command == up ]]; then
      for app in "${selected[@]}"; do
        if [[ ${languages[$app]} == node ]]; then node_image; else rust_image; fi
      done
      run_child "${compose[@]}" --file "$root/compose.yaml" up --detach --no-build --pull never --wait --wait-timeout 30 "${selected[@]}"
    elif [[ $command == down ]]; then
      run_child "${compose[@]}" --file "$root/compose.yaml" down --timeout 5
    elif [[ $command == status ]]; then
      run_child "${compose[@]}" --file "$root/compose.yaml" ps --all "${selected[@]}"
    else
      tail=${DEVKIT_TAIL:-100} follow=${DEVKIT_FOLLOW:-0}
      [[ $tail =~ ^[0-9]+$ && ${#tail} -le 5 && $tail -le 10000 ]] || fail 'tail must be an integer from 0 through 10000'
      [[ $follow == 0 || $follow == 1 ]] || fail 'follow must be 0 or 1'
      args=(logs --tail "$tail")
      [[ $follow == 0 ]] || args+=(--follow)
      run_child "${compose[@]}" --file "$root/compose.yaml" "${args[@]}" "${selected[@]}"
    fi ;;
esac
