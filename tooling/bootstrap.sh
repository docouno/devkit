#!/usr/bin/env bash
# Release builder replaces four markers; no host language SDK is used.
# Parse the full function before a curl pipe can perform any operation.
devkit_bootstrap_main() {
  set -euo pipefail
  export LC_ALL=C
  unset TAR_OPTIONS GZIP
  fail() { printf 'devkit bootstrap: %s\n' "$*" >&2; exit 1; }
  version_ok() { [[ ${#1} -le 128 && $1 =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[a-z0-9][a-z0-9.-]*)?$ ]]; }
  bootstrap_version='@@BOOTSTRAP_VERSION@@'
  version_ok "$bootstrap_version" || fail 'run a rendered release bootstrap, not its source template'
  if [[ $# == 0 || ${1:-} == help || ${1:-} == --help || ${1:-} == -h ]]; then
    printf 'Devkit bootstrap %s\n' "$bootstrap_version"
    printf '%s\n' \
      'Usage: bootstrap COMMAND TARGET [--version X.Y.Z] [--release-base-url URL | --release-archive FILE] [CLI options...]' \
      'Commands: init, add-app, add-package, register, update, recover' \
      'Init, update and recover require an exact --version.' \
      'Add-app, add-package and register use the project accepted version.' \
      'Set --release-base-url or DEVKIT_RELEASE_BASE_URL for remote releases; local archives need their .sha256 sidecar.'
    return 0
  fi
  [[ $# -ge 2 ]] || fail 'expected COMMAND TARGET [--version X.Y.Z] [--release-base-url URL | --release-archive FILE]'
  operation=$1 requested=$2
  shift 2
  case "$operation" in init|update|recover|register|add-app|add-package) ;; *) fail "unknown command: $operation" ;; esac
  selected= archive= base=${DEVKIT_RELEASE_BASE_URL:-} child= stage= child_deadline=0 child_label= launching=0 cancel_status=0
  factory_args=()
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --version) [[ $# -ge 2 && -z $selected ]] || fail 'missing or repeated --version'; selected=$2; shift 2 ;;
      --release-base-url) [[ $# -ge 2 ]] || fail 'missing release URL'; base=$2; shift 2 ;;
      --release-archive) [[ $# -ge 2 && -z $archive ]] || fail 'missing or repeated local archive'; archive=$2; shift 2 ;;
      *) factory_args+=("$1"); shift ;;
    esac
  done
  case "$operation" in init|update|recover) [[ -n $selected ]] || fail "$operation requires explicit --version" ;; esac
  [[ -z $selected ]] || version_ok "$selected" || fail 'invalid release version'
  for tool in bash curl docker flock realpath sha256sum tar gzip mktemp head stdbuf mkfifo tee sleep; do
    command -v "$tool" >/dev/null || fail "missing prerequisite: $tool"
  done
  [[ $(tar --version) == *'GNU tar'* ]] || fail 'GNU tar is required'
  logical=$(realpath -ms -- "$requested")
  target=$(realpath -m -- "$requested")
  [[ $logical == "$target" && $target != / ]] || fail 'target path must not contain symlinks or be /'
  [[ -d $(dirname -- "$target") ]] || fail 'target parent directory must already exist'
  [[ -z $archive || -f $archive && ! -L $archive ]] || fail 'local archive must be a regular file'
  if [[ -z $archive ]]; then
    [[ -n $base && $base != *[$'\n\r\t ']* && $base != *'?'* && $base != *'#'* && $base != *'@'* ]] || fail 'invalid or missing release base URL'
    case "$base" in
      https://*) protocols='=https' ;;
      http://127.0.0.1:*|http://localhost:*|http://\[::1\]:*) protocols='=http,https' ;;
      *) fail 'use HTTPS, or an explicit loopback HTTP fixture' ;;
    esac
    base=${base%/}
  fi
  umask 077
  # Include ordinary descendants of the launcher before it reaches its guard.
  set -m
  stop_child() {
    [[ -n $child ]] || return 0
    local deadline=$((SECONDS + 12))
    kill -TERM -- "-$child" 2>/dev/null || true
    while kill -0 "$child" 2>/dev/null && [[ $SECONDS -lt $deadline ]]; do sleep 0.05; done
    kill -KILL -- "-$child" 2>/dev/null || true
    wait "$child" 2>/dev/null || true
    child=
  }
  cleanup() {
    local status=$?
    trap - EXIT
    trap '' INT TERM
    stop_child
    [[ -z $stage ]] || rm -rf -- "$stage"
    exit "$status"
  }
  cancel() { cancel_status=$1; [[ $launching == 1 ]] || exit "$cancel_status"; }
  trap cleanup EXIT
  trap 'cancel 130' INT
  trap 'cancel 143' TERM
  stage=$(mktemp -d "${TMPDIR:-/tmp}/devkit-bootstrap.XXXXXXXX")
  start_child() {
    local seconds=$1
    child_label=$2
    shift 2
    launching=1
    (
      set +m
      trap 'trap "" TERM; wait; exit 143' TERM
      "$@"
    ) &
    child=$!
    launching=0
    [[ $cancel_status == 0 ]] || exit "$cancel_status"
    child_deadline=0
    [[ $seconds == 0 ]] || child_deadline=$((SECONDS + seconds))
  }
  finish_child() {
    local status=0
    while kill -0 "$child" 2>/dev/null; do
      if [[ $child_deadline != 0 && $SECONDS -ge $child_deadline ]]; then
        fail "$child_label timed out"
      fi
      sleep 0.05
    done
    wait "$child" || status=$?
    kill -KILL -- "-$child" 2>/dev/null || true
    child=
    return "$status"
  }
  run_child() { start_child "$@"; finish_child; }
  bounded_producer() {
    local maximum=$1 output=$2
    shift 2
    "$@" | head -c "$((maximum + 1))" > "$output"
  }
  capture() {
    local seconds=$1 maximum=$2 output=$3 label=$4 status=0
    shift 4
    run_child "$seconds" "$label" bounded_producer "$maximum" "$output" "$@" || status=$?
    [[ $(wc -c < "$output") -le $maximum ]] || fail "$label exceeds $maximum bytes"
    return "$status"
  }
  cat > "$stage/guard.sh" <<'DEVKIT_EMBEDDED_GUARD'
@@GUARD@@
DEVKIT_EMBEDDED_GUARD
  node_image='@@NODE_IMAGE@@'
  node_version='@@NODE_VERSION@@'
  metadata_version() {
    local mode=$1 location=$2 token name mount_target
    token=$(cat /proc/sys/kernel/random/uuid)
    name="devkit-bootstrap-$token"
    mount_target=${location//\"/\"\"}
    run_child 30 'Node image lookup' docker image inspect "$node_image" >/dev/null 2>&1 || run_child 180 'Node image pull' docker pull "$node_image"
    # The program is a fixed literal, the manifest is data, and the project is read-only.
    capture 30 128 "$stage/metadata-version" 'version metadata' bash "$stage/guard.sh" "$name" "$token" -- \
      docker run --rm --init --pull never --read-only --network none --name "$name" \
      --label "io.docouno.devkit.invocation=$token" --user "$(id -u):$(id -g)" \
      --mount "type=bind,\"source=$mount_target\",target=/input,readonly" \
      --env "DEVKIT_EXPECTED_NODE_VERSION=$node_version" \
      --entrypoint node "$node_image" -e '
        const fs = require("node:fs");
        if (process.versions.node !== process.env.DEVKIT_EXPECTED_NODE_VERSION) throw Error("unexpected bootstrap Node version");
        const version = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[a-z0-9][a-z0-9.-]*)?$/;
        const file = process.argv[1] === "project" ? "/input/.devkit/manifest.json" : "/input/release.json";
        const fd = fs.openSync(file, "r");
        const maximum = 4 * 1024 * 1024;
        const buffer = Buffer.alloc(maximum + 1);
        let bytes = 0, count;
        while (bytes <= maximum && (count = fs.readSync(fd, buffer, bytes, buffer.length - bytes, null)) > 0) bytes += count;
        fs.closeSync(fd);
        if (bytes > maximum) throw Error("manifest exceeds metadata limit");
        const value = JSON.parse(buffer.toString("utf8", 0, bytes));
        if (value === null || typeof value !== "object" || Array.isArray(value) ||
            ![1, 2].includes(value.schemaVersion) || typeof value.version !== "string" ||
            value.version.length > 128 || !version.test(value.version)) throw Error("invalid project version/schema");
        process.stdout.write(value.version);
      ' "$mode"
  }
  if [[ $operation == register || $operation == add-app || $operation == add-package ]]; then
    [[ -d $target/.devkit && ! -L $target/.devkit && -f $target/.devkit/manifest.json && ! -L $target/.devkit/manifest.json ]] || fail 'missing or symlinked project manifest'
    metadata_version project "$target"
    accepted=$(cat "$stage/metadata-version")
    version_ok "$accepted" || fail 'invalid accepted version response'
    [[ -z $selected || $selected == "$accepted" ]] || fail 'this action requires the project accepted version'
    selected=$accepted
  fi
  filename="devkit-$selected.tar.gz"
  if [[ -n $archive ]]; then
    capture 30 33554432 "$stage/$filename" 'archive' cat -- "$archive"
    [[ -f $archive.sha256 && ! -L $archive.sha256 ]] || fail 'local archive requires adjacent .sha256'
    capture 30 256 "$stage/checksum" 'checksum metadata' cat -- "$archive.sha256"
  else
    fetch() {
      capture 180 "$3" "$2" "$4" curl -q --fail --silent --show-error --location --globoff \
        --proto "$protocols" --proto-redir '=https' --max-redirs 5 \
        --connect-timeout 15 --max-time 180 -- "$1"
    }
    fetch "$base/v$selected/$filename.sha256" "$stage/checksum" 256 'checksum metadata'
    fetch "$base/v$selected/$filename" "$stage/$filename" 33554432 'archive'
  fi
  [[ $(wc -c < "$stage/checksum") -le 256 ]] || fail 'checksum metadata is too large'
  mapfile -t checksum_lines < "$stage/checksum"
  [[ ${#checksum_lines[@]} == 1 ]] || fail 'checksum must contain exactly one record'
  record=${checksum_lines[0]}
  expected=${record%% *}
  [[ $expected =~ ^[a-f0-9]{64}$ && $record == "$expected  $filename" ]] || fail 'invalid checksum record'
  observed=$(sha256sum -- "$stage/$filename")
  [[ ${observed%% *} == "$expected" ]] || fail 'archive checksum mismatch'
  # Validate the line-buffered listing while tar is still producing it. All
  # buffered metadata is independently capped; rejection stops its process group.
  listing_producer() {
    # This also bounds extended tar metadata before GNU tar can materialize it.
    ulimit -v 524288
    gzip -dc -- "$stage/$filename" | head -c 301989889 | \
      stdbuf -oL tar --list --verbose --absolute-names --numeric-owner --full-time \
      --quoting-style=escape --file - | \
      stdbuf -o0 head -c 4194305 | tee "$stage/preflight.metadata" > "$stage/headers.fifo"
  }
  mkfifo "$stage/headers.fifo"
  # Linux O_RDWR opens without waiting for a writer. Keep the anchor until the
  # producer is reaped; EOF is determined by liveness plus a drained read.
  exec 12<> "$stage/headers.fifo"
  start_child 60 'archive preflight' listing_producer
  count=0 total_size=0 metadata_size=0 stream_ended=0
  declare -A seen=()
  while :; do
    header=
    while :; do
      [[ $SECONDS -lt $child_deadline ]] || fail 'archive preflight timed out'
      fragment= read_status=0
      IFS= read -r -t 0.1 fragment <&12 || read_status=$?
      header+=$fragment
      [[ ${#header} -le 4096 ]] || fail 'archive metadata line exceeds 4096 bytes'
      [[ $read_status != 0 ]] || break
      if ! kill -0 "$child" 2>/dev/null; then
        stream_ended=1
        break
      fi
    done
    [[ $stream_ended == 0 ]] || break
    metadata_size=$((metadata_size + ${#header} + 1))
    [[ $metadata_size -le 4194304 ]] || fail 'archive metadata exceeds 4194304 bytes'
    [[ ${#header} -le 4096 ]] || fail 'archive metadata line exceeds 4096 bytes'
    [[ $header =~ ^([-d][rwx-]{9})[[:space:]]+([0-9]+/[0-9]+)[[:space:]]+([0-9]+)[[:space:]]+([0-9]{4}-[0-9]{2}-[0-9]{2})[[:space:]]+([0-9:.]+)[[:space:]](.*)$ ]] || fail 'archive contains an unsupported file type or mode'
    mode=${BASH_REMATCH[1]} size=${BASH_REMATCH[3]} name=${BASH_REMATCH[6]}
    case "$mode" in -rw-r--r--|-rwxr-xr-x|drwxr-xr-x) ;; *) fail 'archive contains an unsupported file type or mode' ;; esac
    [[ $size =~ ^[0-9]{1,9}$ ]] || fail 'invalid archive member size'
    total_size=$((total_size + 10#$size))
    [[ $total_size -le 268435456 ]] || fail 'archive expands beyond the bootstrap limit'
    count=$((count + 1))
    [[ $count -le 10000 ]] || fail 'invalid archive file count'
    [[ $name =~ ^[A-Za-z0-9._/-]+$ && $name == "devkit-$selected"/* ]] || fail "unsafe archive path: $name"
    normalized=${name%/}
    IFS=/ read -ra parts <<< "$normalized"
    for part in "${parts[@]}"; do
      case "$part" in ''|.|..|__proto__|prototype|constructor) fail 'unsafe archive path component' ;; esac
    done
    [[ ! ${seen[$normalized]+exists} ]] || fail 'duplicate archive entry'
    seen[$normalized]=1
  done
  exec 12<&-
  if [[ -f $stage/preflight.metadata && $(wc -c < "$stage/preflight.metadata") -gt 4194304 ]]; then
    fail 'archive metadata exceeds 4194304 bytes'
  fi
  finish_child
  [[ -z $header ]] || fail 'incomplete archive metadata line'
  [[ $count -gt 1 ]] || fail 'invalid archive file count'
  mkdir "$stage/unpacked"
  extract_release() {
    umask 022
    exec tar --extract --gzip --file "$stage/$filename" --directory "$stage/unpacked" \
      --no-same-owner --no-same-permissions --delay-directory-restore --keep-old-files
  }
  run_child 60 'archive extraction' extract_release
  release="$stage/unpacked/devkit-$selected"
  [[ -x $release/devkit && -f $release/release.json ]] || fail 'incomplete release archive'
  metadata_version release "$release"
  [[ $(cat "$stage/metadata-version") == "$selected" ]] || fail 'archive release version differs from requested version'
  # The unchanged release launcher enters its existing exact-resource guard.
  run_child 0 'release operation' bash "$release/devkit" "$operation" "$target" "${factory_args[@]}"
}
devkit_bootstrap_main "$@"
