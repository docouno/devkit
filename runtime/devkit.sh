#!/usr/bin/env bash
set -euo pipefail

fail() { printf 'devkit: %s\n' "$*" >&2; exit 1; }
root=$(realpath -ms -- "$(dirname -- "${BASH_SOURCE[0]}")/../..")
runtime="$root/.devkit/runtime"
command=${1:-help}
[[ $# -le 1 ]] || fail 'expected one command; select app=<name> or package=<name>'
case "$command" in
  help)
    printf '%s\n' \
      'make setup|check|fix|test|build [app=<name> | package=<name>]' \
      'make dev|up [app=<name>] [headless=1]  start development services' \
      'make package [app=<desktop>]         build a Linux desktop archive' \
      'make status|logs [app=<name>] [tail=100] [follow=1]' \
      'make down                           stop this checkout; preserve data' \
      'make doctor                         verify accepted and prepared tools'
    exit 0 ;;
  doctor|setup|setup-node|check|fix|test|build|package|dev|up|down|logs|status|snapshot|package-snapshot) ;;
  *) fail "unknown command: $command" ;;
esac

for executable in docker flock sha256sum realpath; do command -v "$executable" >/dev/null || fail "missing prerequisite: $executable"; done
[[ -d $root && $(realpath -e -- "$root") == "$root" ]] || fail 'checkout path must not contain symlinks'
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
        [[ -d $current ]] || fail "expected directory: $current"
      else
        [[ -f $current ]] || fail "expected regular file: $current"
      fi
    fi
  done
}
for directory in .devkit/state .devkit/cache/npm .devkit/cache/cargo .devkit/cache/electron .devkit/runtime; do safe_path "$root/$directory" directory; done
for file in .devkit/manifest.json .devkit/runtime.sha256 .devkit/project.mk .devkit/compose.apps.yaml .devkit/compose.workspace.json .devkit/state/task.lock .devkit/state/node-workspace.lock .devkit/state/rust-image .devkit/state/desktop-image; do safe_path "$root/$file" file; done
[[ ! -e $root/.devkit/update-pending.json && ! -L $root/.devkit/update-pending.json ]] || fail 'an update is incomplete; run the selected release updater recovery'
mkdir -p -- "$root/.devkit/state"
exec 9>>"$root/.devkit/state/task.lock"
flock -n 9 || fail 'another devkit operation is active in this checkout'
[[ ! -e $root/.devkit/update-pending.json && ! -L $root/.devkit/update-pending.json ]] || fail 'an update is incomplete; run the selected release updater recovery'
[[ ${DEVKIT_EXPECTED_MANIFEST:-} =~ ^[a-f0-9]{64}$ ]] || fail 'invoke the root Makefile to capture the accepted version'
actual=$(sha256sum "$root/.devkit/manifest.json")
[[ ${actual%% *} == "$DEVKIT_EXPECTED_MANIFEST" ]] || fail 'accepted toolkit changed during this Make invocation; rerun the command'
(cd -- "$root"; sha256sum --check --status .devkit/runtime.sha256) || fail 'managed toolkit files differ from the accepted release'

declare -A pins=()
unset DEVKIT_VERSION DEVKIT_NODE_IMAGE DEVKIT_NODE_VERSION DEVKIT_RUST_VERSION DEVKIT_RUST_RECIPE_HASH DEVKIT_DESKTOP_RECIPE_HASH
while IFS= read -r line || [[ -n $line ]]; do
  [[ -z $line || $line == \#* ]] && continue
  [[ $line == *=* ]] || fail 'invalid toolchains.env record'
  key=${line%%=*} value=${line#*=}
  case "$key" in
    DEVKIT_VERSION|DEVKIT_NODE_IMAGE|DEVKIT_NODE_VERSION|DEVKIT_RUST_VERSION|DEVKIT_RUST_RECIPE_HASH|DEVKIT_DESKTOP_RECIPE_HASH) ;;
    *) fail "unknown toolchain key: $key" ;;
  esac
  [[ ! -v pins[$key] ]] || fail "duplicate toolchain key: $key"
  [[ -n $value && $value != *[[:space:]]* && $value != *\'* && $value != *\"* ]] || fail "invalid toolchain value: $key"
  pins[$key]=1
  printf -v "$key" '%s' "$value"
  export "$key"
done < "$runtime/toolchains.env"
for key in DEVKIT_VERSION DEVKIT_NODE_IMAGE DEVKIT_NODE_VERSION DEVKIT_RUST_VERSION DEVKIT_RUST_RECIPE_HASH DEVKIT_DESKTOP_RECIPE_HASH; do [[ -n ${!key:-} ]] || fail "missing toolchain key: $key"; done
[[ $DEVKIT_NODE_IMAGE =~ ^[a-z0-9./:-]+@sha256:[a-f0-9]{64}$ && $DEVKIT_RUST_RECIPE_HASH =~ ^[a-f0-9]{64}$ && $DEVKIT_DESKTOP_RECIPE_HASH =~ ^[a-f0-9]{64}$ ]] || fail 'invalid immutable tool pins'

export DEVKIT_PROJECT_ROOT=$root DEVKIT_UID=$(id -u) DEVKIT_GID=$(id -g)
export DEVKIT_PROJECT_NAME="devkit-$(printf '%s' "$root" | sha256sum | cut -c1-16)"
export DEVKIT_CURRENT_COMPONENT=. DEVKIT_COMPONENT_ROOT=$root DEVKIT_WORKDIR=/workspace DEVKIT_SOURCE_READ_ONLY=true DEVKIT_TASK_NETWORK=none
export VITE_API_ENABLED=false
export DEVKIT_RUST_IMAGE=devkit-unprepared-rust DEVKIT_DESKTOP_IMAGE=devkit-unprepared-desktop
unset COMPOSE_FILE COMPOSE_PROJECT_NAME COMPOSE_PROFILES COMPOSE_ENV_FILES COMPOSE_PATH_SEPARATOR
compose=(docker compose --project-directory "$root" --project-name "$DEVKIT_PROJECT_NAME" --env-file /dev/null)
tool_compose=("${compose[@]}" --file "$runtime/compose.tools.yaml" --file "$root/.devkit/compose.workspace.json")
app_compose=("${compose[@]}" --file "$root/.devkit/compose.apps.yaml")
[[ ! -f $root/compose.yaml ]] || app_compose+=(--file "$root/compose.yaml")
temporary_files=()
cleanup() { for file in "${temporary_files[@]}"; do rm -f -- "$file"; done; }
trap cleanup EXIT
active_pid= launching=0 cancel_status=0
set -m
cancel() {
  local status=$1
  trap '' INT TERM
  if [[ -n $active_pid ]]; then kill -TERM -- "-$active_pid" 2>/dev/null || true; wait "$active_pid" 2>/dev/null || true; fi
  exit "$status"
}
request_cancel() { cancel_status=$1; [[ $launching != 0 ]] || cancel "$cancel_status"; }
trap 'request_cancel 130' INT
trap 'request_cancel 143' TERM
run_child() {
  local status=0
  launching=1
  "$@" &
  active_pid=$!
  launching=0
  [[ $cancel_status == 0 ]] || cancel "$cancel_status"
  wait "$active_pid" || status=$?
  active_pid=
  return "$status"
}
node_image() {
  if ! docker image inspect "$DEVKIT_NODE_IMAGE" >/dev/null 2>&1; then
    [[ $command == setup || $command == setup-node ]] || fail 'accepted Node image is absent; run make setup'
    run_child docker pull "$DEVKIT_NODE_IMAGE"
  fi
}
derived_image() {
  local kind=$1 recipe pin label state key recorded= actual tmp iid
  if [[ $kind == rust ]]; then
    recipe=rust.Dockerfile pin=$DEVKIT_RUST_RECIPE_HASH label=io.docouno.devkit.rust-recipe state=rust-image key=DEVKIT_RUST_IMAGE
  else
    recipe=desktop.Dockerfile pin=$DEVKIT_DESKTOP_RECIPE_HASH label=io.docouno.devkit.desktop-recipe state=desktop-image key=DEVKIT_DESKTOP_IMAGE
  fi
  actual=$(sha256sum "$runtime/$recipe")
  [[ ${actual%% *} == "$pin" ]] || fail "$recipe differs from the accepted recipe"
  if [[ -f $root/.devkit/state/$state ]]; then recorded=$(<"$root/.devkit/state/$state"); fi
  if [[ $recorded =~ ^sha256:[a-f0-9]{64}$ ]] && [[ $(docker image inspect --format "{{index .Config.Labels \"$label\"}}" "$recorded" 2>/dev/null || true) == "$pin" ]]; then
    printf -v "$key" '%s' "$recorded"; export "$key"; return
  fi
  [[ $command == setup || $command == setup-node ]] || fail "accepted $kind image is absent/stale; run make setup"
  tmp=$(mktemp "$root/.devkit/state/$state.XXXXXXXX")
  temporary_files+=("$tmp")
  run_child docker build --pull=false --build-arg "DEVKIT_RECIPE_SHA=$pin" --label "$label=$pin" --iidfile "$tmp" --file "$runtime/$recipe" "$runtime" || fail "$kind image preparation failed"
  iid=$(<"$tmp")
  [[ $iid =~ ^sha256:[a-f0-9]{64}$ && $(docker image inspect --format "{{index .Config.Labels \"$label\"}}" "$iid") == "$pin" ]] || fail "invalid $kind image identity"
  printf '%s\n' "$iid" > "$tmp"
  mv -f -- "$tmp" "$root/.devkit/state/$state"
  printf -v "$key" '%s' "$iid"; export "$key"
}
control() {
  local action=$1 output=$2 token name
  shift 2
  token=$(cat /proc/sys/kernel/random/uuid)
  name="$DEVKIT_PROJECT_NAME-control-$token"
  run_child bash "$runtime/guard.sh" "$name" "$token" -- \
    "${compose[@]}" --file "$runtime/compose.tools.yaml" run --pull never --rm -T --no-deps --name "$name" --label "io.docouno.devkit.invocation=$token" \
    control node /workspace/.devkit/runtime/project.mjs "$action" /workspace "$DEVKIT_VERSION" "$@" > "$output"
}
docker compose version >/dev/null || fail 'Docker Compose is unavailable'
docker info --format '{{.OSType}}' | { read -r type; [[ $type == linux ]]; } || fail 'a Linux Docker daemon is required'
node_image
metadata=$(mktemp "$root/.devkit/state/metadata.XXXXXXXX")
temporary_files+=("$metadata")
control describe "$metadata"
declare -A languages=() profiles=() roles=() kinds=() services=() apis=()
node_fingerprint=
while IFS=$'\t' read -r record first second third fourth fifth sixth seventh; do
  case "$record" in
    node-fingerprint) node_fingerprint=$first ;;
    component) languages["$first"]=$second; profiles["$first"]=$third; roles["$first"]=$fourth; kinds["$first"]=$fifth; services["$first"]=$sixth; apis["$first"]=$seventh ;;
    directory) safe_path "$root/$first" directory; mkdir -p -- "$root/$first" ;;
    *) fail 'invalid metadata response' ;;
  esac
done < "$metadata"
[[ $node_fingerprint =~ ^[a-f0-9]{64}$ ]] || fail 'missing workspace fingerprint'
rm -f -- "$metadata"
for directory in .devkit/cache/npm .devkit/cache/cargo .devkit/cache/electron; do mkdir -p -- "$root/$directory"; done
target=${DEVKIT_TARGET_COMPONENT:-}
app=${DEVKIT_APP:-} package=${DEVKIT_PACKAGE:-}
[[ -z $app || -z $package ]] || fail 'app and package selectors are mutually exclusive'
if [[ -n $app ]]; then [[ $app =~ ^[a-z][a-z0-9]*(-[a-z0-9]+)*$ && -v languages[apps/$app] ]] || fail "unregistered app: $app"; fi
if [[ -n $package ]]; then [[ $package =~ ^[a-z][a-z0-9]*(-[a-z0-9]+)*$ && -v languages[packages/$package] ]] || fail "unregistered package: $package"; fi
[[ -z $target || -v languages[$target] ]] || fail "unregistered target component: $target"
if [[ -z $target && -n $app ]]; then target="apps/$app"; elif [[ -z $target && -n $package ]]; then target="packages/$package"; fi
node_ready() {
  [[ -f $root/.devkit/state/node-workspace.lock && $(<"$root/.devkit/state/node-workspace.lock") == "$node_fingerprint" && -d $root/node_modules ]] || fail 'workspace dependencies are absent/stale; run make setup'
}
run_node() {
  local token name service=$1; shift
  token=$(cat /proc/sys/kernel/random/uuid); name="$DEVKIT_PROJECT_NAME-task-$token"
  run_child bash "$runtime/guard.sh" "$name" "$token" -- "${tool_compose[@]}" run --pull never --rm -T --no-deps --name "$name" --label "io.docouno.devkit.invocation=$token" "$service" \
    sh -ec 'mkdir -p "$HOME"; node -e '\''if(process.versions.node!==process.argv[1])throw Error("Unexpected Node version")'\'' "$1"; shift; exec "$@"' sh "$DEVKIT_NODE_VERSION" "$@"
}
run_rust() {
  local token name
  token=$(cat /proc/sys/kernel/random/uuid); name="$DEVKIT_PROJECT_NAME-task-$token"
  run_child bash "$runtime/guard.sh" "$name" "$token" -- "${tool_compose[@]}" run --pull never --rm -T --no-deps --name "$name" --label "io.docouno.devkit.invocation=$token" rust \
    bash /workspace/.devkit/runtime/rust-check.sh "$command"
}

case "$command" in
  snapshot) exit 0 ;;
  package-snapshot)
    [[ -z $package ]] || fail 'package selects a desktop app, not a library'
    found=0
    for id in "${!profiles[@]}"; do [[ ${profiles[$id]} != electron ]] || found=1; done
    [[ $found == 1 ]] || fail 'no desktop component is registered'
    exit 0 ;;
  setup-node)
    export DEVKIT_WORKDIR=/workspace DEVKIT_COMPONENT_ROOT=$root DEVKIT_CURRENT_COMPONENT=. DEVKIT_TASK_NETWORK=bridge
    rm -f -- "$root/.devkit/state/node-workspace.lock"
    run_node node npm ci --include=dev --strict-peer-deps
    for id in "${!profiles[@]}"; do
      if [[ ${profiles[$id]} == electron ]]; then
        run_node node npm exec --offline --workspace="$id" -- install-electron
      fi
    done
    after=$(mktemp "$root/.devkit/state/fingerprint.XXXXXXXX"); temporary_files+=("$after")
    control fingerprint "$after"
    [[ $(<"$after") == "$node_fingerprint" ]] || fail 'native inputs changed during setup'
    receipt=$(mktemp "$root/.devkit/state/node-workspace.XXXXXXXX"); temporary_files+=("$receipt")
    printf '%s\n' "$node_fingerprint" > "$receipt"
    mv -f -- "$receipt" "$root/.devkit/state/node-workspace.lock"
    exit 0 ;;
  setup|check|fix|test|build|package)
    [[ -n $target ]] || fail 'finite task must select a registered component through Make'
    export DEVKIT_CURRENT_COMPONENT=$target DEVKIT_COMPONENT_ROOT="$root/$target" DEVKIT_WORKDIR="/workspace/$target"
    if [[ ${languages[$target]} == rust ]]; then
      [[ $command != package ]] || fail 'package is defined for desktop applications'
      derived_image rust
      [[ $command != setup ]] || export DEVKIT_TASK_NETWORK=bridge
      run_rust
    else
      node_ready
      service=node
      if [[ ${profiles[$target]} == web && ${apis[$target]} != - ]]; then export VITE_API_ENABLED=true; fi
      if [[ ${profiles[$target]} == electron ]]; then derived_image desktop; service=desktop; fi
      if [[ $command == setup ]]; then exit 0; fi
      [[ $command != package || ${profiles[$target]} == electron ]] || fail 'package is defined for desktop applications'
      [[ $command != fix ]] || export DEVKIT_SOURCE_READ_ONLY=false
      run_node "$service" npm run "$command"
    fi
    exit 0 ;;
  doctor)
    have_node=0
    for id in "${!languages[@]}"; do
      if [[ ${languages[$id]} == rust ]]; then derived_image rust; else have_node=1; fi
      [[ ${profiles[$id]} != electron ]] || derived_image desktop
    done
    [[ $have_node == 0 ]] || node_ready
    printf 'Devkit %s; Node %s; Rust %s; accepted components verified.\n' "$DEVKIT_VERSION" "$DEVKIT_NODE_VERSION" "$DEVKIT_RUST_VERSION"
    exit 0 ;;
esac

# Status/down need no prepared images. Development exports only admitted IDs.
service_args=()
if [[ -n $target ]]; then
  [[ ${roles[$target]} == app ]] || fail 'development services require an app selector'
  name=${services[$target]}; [[ $name != - ]] || name=${target#apps/}
  service_args+=("$name")
fi
case "$command" in
  dev|up)
    [[ -z $package && -n $target ]] || fail 'development must select an app through Make'
    model=$(mktemp "$root/.devkit/state/compose.XXXXXXXX.json"); temporary_files+=("$model")
    reached=$(mktemp "$root/.devkit/state/development.XXXXXXXX"); temporary_files+=("$reached")
    # Inspect all profiles so an explicitly selected profiled service is present.
    # Actual up below retains normal selection; the helper filters inactive deps.
    run_child "${app_compose[@]}" --profile '*' config --no-env-resolution --format json > "$model"
    control development "$reached" ".devkit/state/${model##*/}" "${service_args[0]}"
    mapfile -t started_components < "$reached"
    [[ ${#started_components[@]} -gt 0 ]] || fail 'selected Compose service has no registered application'
    for id in "${started_components[@]}"; do
      [[ -v languages[$id] && ${roles[$id]} == app ]] || fail 'invalid development component response'
      if [[ ${languages[$id]} == node ]]; then node_ready; else derived_image rust; fi
      if [[ ${profiles[$id]} == electron ]]; then derived_image desktop; fi
    done
    for id in "${started_components[@]}"; do
      if [[ ${profiles[$id]} != electron ]]; then continue; fi
      desktop_service=${services[$id]}; [[ $desktop_service != - ]] || desktop_service=${id#apps/}
      display=${DISPLAY:-} authority=${XAUTHORITY:-${HOME:-}/.Xauthority} headless=${DEVKIT_HEADLESS:-0}
      [[ $headless == 0 || $headless == 1 ]] || fail 'headless must be 0 or 1'
      if [[ $headless == 0 ]]; then
        [[ $display =~ ^:[0-9]+(\.[0-9]+)?$ && -d /tmp/.X11-unix && -f $authority && -r $authority ]] || fail 'desktop requires local DISPLAY and readable XAUTHORITY; use headless=1 for a virtual display'
        authority=$(realpath -e -- "$authority")
      fi
      overlay=$(mktemp "$root/.devkit/state/display.XXXXXXXX"); temporary_files+=("$overlay")
      token=$(cat /proc/sys/kernel/random/uuid); name="$DEVKIT_PROJECT_NAME-display-$token"
      run_child bash "$runtime/guard.sh" "$name" "$token" -- "${compose[@]}" --file "$runtime/compose.tools.yaml" run --pull never --rm -T --no-deps --name "$name" --label "io.docouno.devkit.invocation=$token" control \
        node -e 'const [name,headless,display,authority]=process.argv.slice(1);const service={environment:{DEVKIT_HEADLESS:headless}};if(headless==="0"){service.environment.DISPLAY=display;service.environment.XAUTHORITY="/run/devkit-xauthority";service.volumes=[{type:"bind",source:"/tmp/.X11-unix",target:"/tmp/.X11-unix",read_only:true,bind:{create_host_path:false}},{type:"bind",source:authority,target:"/run/devkit-xauthority",read_only:true,bind:{create_host_path:false}}]};process.stdout.write(JSON.stringify({services:{[name]:service}}))' \
        "$desktop_service" "$headless" "$display" "$authority" > "$overlay"
      app_compose+=(--file "$overlay")
    done
    run_child "${app_compose[@]}" up -d --wait --pull never "${service_args[@]}" ;;
  down) run_child "${app_compose[@]}" --profile '*' down ;;
  status) run_child "${app_compose[@]}" ps --all "${service_args[@]}" ;;
  logs)
    tail=${DEVKIT_TAIL:-100} follow=${DEVKIT_FOLLOW:-0}
    [[ $tail =~ ^[0-9]+$ && $follow =~ ^[01]$ ]] || fail 'invalid logs options'
    args=(logs --tail "$tail")
    [[ $follow == 0 ]] || args+=(--follow)
    run_child "${app_compose[@]}" --profile '*' "${args[@]}" "${service_args[@]}" ;;
esac
