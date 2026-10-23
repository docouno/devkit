#!/usr/bin/env bash
# Delegation only. Versioned verified cache acquisition is supplied by the loader.
set -euo pipefail
root=$(realpath -ms -- "$(dirname -- "${BASH_SOURCE[0]}")/../..")
engine=${DEVKIT_ENGINE_PATH:-}
if [[ -z $engine || ! -f $engine || ! -x $engine ]]; then
  printf '%s\n' 'devkit: accepted native engine is unavailable; run make setup with the versioned devkit loader' >&2
  exit 1
fi
if [[ ${1:-help} == exec ]]; then
  for flag in "${DEVKIT_EXEC_WRITE:-0}" "${DEVKIT_EXEC_PREPARE:-0}"; do
    [[ $flag == 0 || $flag == 1 ]] || { printf "%s\n" "devkit: write and prepare must be 0 or 1" >&2; exit 1; }
  done
  args=(exec --root "$root")
  [[ ${DEVKIT_EXEC_WRITE:-0} == 0 ]] || args+=(--write)
  [[ ${DEVKIT_EXEC_PREPARE:-0} == 0 ]] || args+=(--prepare)
  exec "$engine" "${args[@]}" --network "${DEVKIT_EXEC_NETWORK:-none}"
fi
exec "$engine" task "${1:-help}" --root "$root"
