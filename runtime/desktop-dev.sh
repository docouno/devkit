#!/usr/bin/env bash
set -euo pipefail
mkdir -p -- "$HOME"
if [[ ${DEVKIT_HEADLESS:-0} == 1 ]]; then
  exec xvfb-run -a -s '-screen 0 1280x800x24' npm run dev
fi
if [[ -z ${DISPLAY:-} ]]; then
  printf '%s\n' 'devkit: Desktop development needs DISPLAY. Use make dev app=<name> headless=1 for an explicit virtual display.' >&2
  exit 1
fi
exec npm run dev
