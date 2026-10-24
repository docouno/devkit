#!/bin/sh
# Initial transfer and delegation only; all project operations run in the ELF.
set -eu
mode=@@DEVKIT_MODE@@
version=@@DEVKIT_VERSION@@
expected=@@DEVKIT_ENGINE_SHA256@@
target=@@DEVKIT_ENGINE_TARGET@@
asset=devkit-x86_64-unknown-linux-musl
fail() { printf 'devkit: %s\n' "$*" >&2; exit 1; }
[ "$(uname -s):$(uname -m)" = Linux:x86_64 ] || fail 'this release requires Linux x86_64'
command=${1:-help}
if [ "$mode" = project ] && [ "$command" = help ]; then
  printf '%s\n' 'make setup | doctor | check | fix | test | build | dev | up | down | logs | status | package' \
    'Select finite tasks with app=NAME or package=NAME; down stops the project.' \
    'Use devkit --help for project creation, updates and native exec.'
  exit 0
fi
cache=${DEVKIT_CACHE_DIR:-${XDG_CACHE_HOME:-${HOME:?HOME or XDG_CACHE_HOME is required}/.cache}/docouno/devkit}
case "$cache" in /*) ;; *) fail 'the cache directory must be absolute' ;; esac
cache=$(realpath -ms -- "$cache")
[ "$(realpath -m -- "$cache")" = "$cache" ] || fail 'the cache path contains a symbolic link'
engine=$cache/engines/$target/$version/$expected/devkit
if [ -n "${DEVKIT_ENGINE_PATH:-}" ]; then
  engine=$DEVKIT_ENGINE_PATH
fi
valid_engine() {
  [ -f "$engine" ] && [ ! -L "$engine" ] && [ -x "$engine" ] || return 1
  observed=$(sha256sum -- "$engine")
  [ "${observed%% *}" = "$expected" ]
}
if ! valid_engine; then
  [ ! -e "$engine" ] && [ ! -L "$engine" ] || fail "cached engine integrity differs: $engine"
  [ -z "${DEVKIT_ENGINE_PATH:-}" ] || fail 'DEVKIT_ENGINE_PATH does not select the accepted engine'
  [ "$mode" = bootstrap ] || [ "$command" = setup ] || fail 'the accepted engine is unavailable; run make setup'
  parent=${engine%/*}
  [ "$(realpath -m -- "$parent")" = "$(realpath -ms -- "$parent")" ] || fail 'the engine cache path contains a symbolic link'
  umask 077
  mkdir -p -- "$parent"
  temporary=$(mktemp "$parent/.engine.XXXXXXXX")
  download_pid=
  cleanup() {
    if [ -n "$download_pid" ]; then
      kill -TERM "$download_pid" 2>/dev/null || :
      wait "$download_pid" 2>/dev/null || :
    fi
    rm -f -- "$temporary"
  }
  trap cleanup 0
  trap 'exit 130' INT
  trap 'exit 143' TERM
  curl --disable --fail --silent --show-error --location --max-redirs 5 \
    --proto '=https' --proto-redir '=https' --connect-timeout 10 --max-time 90 \
    --max-filesize 134217728 --output "$temporary" \
    "https://github.com/docouno/devkit/releases/download/v$version/$asset" &
  download_pid=$!
  if wait "$download_pid"; then
    download_pid=
  else
    status=$?
    download_pid=
    exit "$status"
  fi
  observed=$(sha256sum -- "$temporary")
  [ "${observed%% *}" = "$expected" ] || fail 'downloaded engine checksum differs'
  chmod 755 -- "$temporary"
  # Creating this link is atomic and cannot replace a concurrent installation.
  ln -- "$temporary" "$engine" 2>/dev/null || :
  valid_engine || fail 'the accepted engine could not be installed'
  cleanup
  trap - 0 INT TERM
fi
if [ "$mode" = bootstrap ]; then
  DEVKIT_BOOTSTRAP_INTERACTIVE=1
  export DEVKIT_BOOTSTRAP_INTERACTIVE
  exec "$engine" "$@"
fi
root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd -P)
if [ "$command" = exec ]; then
  for flag in "${DEVKIT_EXEC_WRITE:-0}" "${DEVKIT_EXEC_PREPARE:-0}"; do
    [ "$flag" = 0 ] || [ "$flag" = 1 ] || fail 'write and prepare must be 0 or 1'
  done
  set -- exec --root "$root" --network "${DEVKIT_EXEC_NETWORK:-none}"
  [ "${DEVKIT_EXEC_WRITE:-0}" = 0 ] || set -- "$@" --write
  [ "${DEVKIT_EXEC_PREPARE:-0}" = 0 ] || set -- "$@" --prepare
  exec "$engine" "$@"
fi
exec "$engine" task "$command" --root "$root"
