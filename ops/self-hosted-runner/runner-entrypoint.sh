#!/usr/bin/env bash
set -euo pipefail

privoxy --no-daemon /etc/privoxy/config &
proxy_pid=$!
proxy_ready=false
for _ in {1..20}; do
  if ! kill -0 "$proxy_pid" 2>/dev/null; then
    echo "privoxy exited before startup" >&2
    exit 78
  fi
  if (exec 3<>/dev/tcp/127.0.0.1/8118) 2>/dev/null; then
    exec 3>&-
    exec 3<&-
    proxy_ready=true
    break
  fi
  sleep 0.1
done
[[ "$proxy_ready" == true ]] || { echo "privoxy did not listen" >&2; exit 78; }

proxy_ok=false
for _ in {1..10}; do
  if curl --http1.1 --fail --silent --show-error --max-time 20 \
    --proxy "$HTTPS_PROXY" https://api.github.com/ -o /dev/null; then
    proxy_ok=true
    break
  fi
  sleep 2
done
[[ "$proxy_ok" == true ]] || { echo "SOCKS5 proxy preflight failed" >&2; exit 78; }

[[ "$GITHUB_URL" == "https://github.com/225284228a-droid/EasyTier-Custom" ]] || exit 78
[[ "$RUNNER_NAME" == "easytier-owner-static" ]] || exit 78
[[ "$RUNNER_LABELS" == "self-hosted,linux,x64,easytier-owner-static" ]] || exit 78

cd /actions-runner
if [[ ! -s .runner ]]; then
  [[ -n "${ACCESS_TOKEN:-}" ]] || { echo "ACCESS_TOKEN is required for first registration" >&2; exit 78; }
  ./config.sh --url "$GITHUB_URL" --token "$ACCESS_TOKEN" \
    --name "$RUNNER_NAME" --work "$HOME/work" --labels "$RUNNER_LABELS" \
    --unattended --replace --disableupdate
fi
unset ACCESS_TOKEN
exec ./run.sh
