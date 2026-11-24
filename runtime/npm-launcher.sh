#!/bin/sh
# npm may expose the launcher through a chain of relative .bin symlinks.
set -eu
fail() { printf 'devkit: %s\n' "$*" >&2; exit 1; }
host_os=$(uname -s)
host_cpu=$(uname -m)
case "$host_os:$host_cpu" in
# @@DEVKIT_ENGINE_PINS@@
  *) fail "this release does not support $host_os $host_cpu" ;;
esac
script=$0
case "$script" in
  /*) ;;
  *) script=$PWD/$script ;;
esac
hops=0
while [ -L "$script" ]; do
  hops=$((hops + 1))
  [ "$hops" -le 40 ] || fail 'too many launcher symlinks'
  link=$(readlink "$script") || fail 'cannot resolve npm launcher'
  case "$link" in
    /*) script=$link ;;
    *) script=${script%/*}/$link ;;
  esac
done
root=$(CDPATH= cd -P "${script%/*}/.." && pwd -P) || fail 'cannot locate npm package'
engine=$root/$payload
[ ! -L "$engine" ] && [ -f "$engine" ] && [ -x "$engine" ] || fail "missing executable for $target"
case "$host_os" in
  Linux) actual=$(sha256sum < "$engine") ;;
  Darwin) actual=$(/usr/bin/env -i /usr/bin/shasum -a 256 < "$engine") ;;
esac
[ "${actual%% *}" = "$sha256" ] || fail "engine checksum differs for $target"
exec "$engine" "$@"
