#!/usr/bin/env bash
set -euo pipefail

[[ "${GITHUB_REPOSITORY:-}" == "225284228a-droid/EasyTier-Custom" ]] || exit 78
if [[ -n "${GITHUB_WORKFLOW_REF:-}" ]]; then
  case "$GITHUB_WORKFLOW_REF" in
    .github/workflows/custom-static.yml@*|*/.github/workflows/custom-static.yml@*) ;;
    *) exit 78 ;;
  esac
fi
[[ "${GITHUB_REF:-}" == refs/heads/main ]] || exit 78
[[ "${GITHUB_ACTOR:-}" == "225284228a-droid" ]] || exit 78
if [[ -n "${GITHUB_TRIGGERING_ACTOR:-}" ]]; then
  [[ "$GITHUB_TRIGGERING_ACTOR" == "225284228a-droid" ]] || exit 78
fi

marker="${EASYTIER_CACHE_ACTIVE_MARKER:-/cache/.job-active}"
install -d -m 0700 "$(dirname "$marker")"
date -u +%s > "$marker"
