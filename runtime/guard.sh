#!/usr/bin/env bash
# Keep the creator alive under our control until its exact container is cleaned.
set -euo pipefail

die() { printf 'devkit guard: %s\n' "$*" >&2; exit 1; }
[[ $# -ge 4 && $3 == -- ]] || die 'usage: guard.sh <container-name> <invocation-token> -- <creator argv...>'
container=$1 token=$2
shift 3
[[ $container =~ ^[a-zA-Z0-9][a-zA-Z0-9_-]+$ && $token =~ ^[a-zA-Z0-9_-]+$ ]] || die 'invalid ownership identity'
client_pid=
starting=1
cancel_status=0

# An unexpected pre-existing object must never be adopted, even with our token.
existing=$(docker container ls --all --filter "name=^/${container}$" --format '{{.Names}}')
[[ -z $existing ]] || die "container name already exists: $container"

# Noninteractive job control puts the creator and its ordinary children in an
# owned group. A pre-create shell/plugin must not survive to create after cleanup.
set -m
cleanup() {
  local status=$? failure=0 observed own remaining i attempt attempts=1
  trap - EXIT
  trap '' INT TERM
  set +e
  if [[ -n $client_pid ]]; then
    kill -TERM -- "-$client_pid" 2>/dev/null
    for ((i=0; i<40; i++)); do
      kill -0 -- "-$client_pid" 2>/dev/null || break
      sleep 0.05
    done
    if kill -0 -- "-$client_pid" 2>/dev/null; then
      kill -KILL -- "-$client_pid" 2>/dev/null
    fi
    wait "$client_pid" 2>/dev/null
  fi

  # Listing distinguishes absent from a failed daemon query. Inspect failure is
  # only an ordinary absence if a successful exact-name listing agrees.
  # A cancelled HTTP client can leave an already accepted daemon request in
  # flight. Recheck briefly after reaping, even if the first listing is empty.
  [[ $cancel_status == 0 ]] || attempts=10
  for ((attempt=0; attempt<attempts; attempt++)); do
    observed=$(docker container ls --all --filter "name=^/${container}$" --format '{{.Names}}')
    if [[ $? -ne 0 ]]; then
      printf 'devkit guard: cannot verify cleanup of %s\n' "$container" >&2
      failure=1
      break
    elif [[ -n $observed ]]; then
      own=$(docker inspect --format '{{index .Config.Labels "io.docouno.devkit.invocation"}}' "$container" 2>/dev/null)
      if [[ $own != "$token" ]]; then
        printf 'devkit guard: refusing unowned container %s\n' "$container" >&2
        failure=1
        break
      else
        # --rm may race stop/removal; the final listing is the oracle.
        docker stop --time 2 "$container" >/dev/null 2>&1
        docker rm --force "$container" >/dev/null 2>&1
      fi
    fi
    [[ $attempts == 1 ]] || sleep 0.1
  done
  remaining=$(docker container ls --all --filter "name=^/${container}$" --format '{{.Names}}')
  if [[ $? -ne 0 || -n $remaining ]]; then
    printf 'devkit guard: cleanup incomplete for %s\n' "$container" >&2
    failure=1
  fi
  if [[ $failure != 0 && $status == 0 ]]; then status=1; fi
  exit "$status"
}
trap cleanup EXIT
request_cancel() {
  cancel_status=$1
  if [[ $starting == 0 ]]; then exit "$cancel_status"; fi
}
trap 'request_cancel 130' INT
trap 'request_cancel 143' TERM

"$@" &
client_pid=$!
starting=0
[[ $cancel_status == 0 ]] || exit "$cancel_status"
status=0
wait "$client_pid" || status=$?
exit "$status"
