#!/usr/bin/env bash
set -euo pipefail

rm -f -- "${EASYTIER_CACHE_ACTIVE_MARKER:-/cache/.job-active}"
work_dir="${RUNNER_WORK_DIRECTORY:-$HOME/work}"
if [[ -d "$work_dir" ]]; then
  find "$work_dir" -mindepth 1 -maxdepth 1 -exec rm -rf -- {} +
fi
