# EasyTier repository runner

This directory describes the single repository-level Linux runner for
`225284228a-droid/EasyTier-Custom`. The workflow keeps Windows on
GitHub-hosted runners and schedules only the Linux build here.

The image contains Privoxy. Its direct rule is first, and the GitHub rules are
last, so GitHub API, Actions, artifact, package, and release traffic uses the
SOCKS5 service at `10.126.126.5:1080`; other destinations use the container's
normal network. The runner package is downloaded and checked on the server
before the Docker build, so an unstable SSH connection does not affect it.

On the server, copy this directory to `/opt/easytier-runner`, then run:

```bash
install -d -o 1000 -g 1000 /srv/easytier-runner/cache
cd /opt/easytier-runner
url='https://github.com/actions/runner/releases/download/v2.338.0/actions-runner-linux-x64-2.338.0.tar.gz'
out='actions-runner-linux-x64-2.338.0.tar.gz'
for attempt in $(seq 1 120); do
  if [ -s "$out" ] && gzip -t "$out" 2>/dev/null; then break; fi
  curl --http1.1 --fail --location --continue-at - \
    --proxy socks5h://10.126.126.5:1080 \
    --connect-timeout 20 --speed-limit 20480 --speed-time 60 --max-time 180 \
    "$url" -o "$out" || true
  sleep 5
done
gzip -t "$out"
echo 'af4b794c1bc41d73d40535e3fe092a39f9679cd8d965954c2aca25a05ca41d32  actions-runner-linux-x64-2.338.0.tar.gz' | sha256sum -c -

docker build --network host \
  --build-arg BUILD_SOCKS5_PROXY=socks5h://10.126.126.5:1080 \
  -t easytier-owner-runner:latest .
```

Install and enable the host cache cleaner:

```bash
install -D -m 0755 cache-cleaner.py /opt/easytier-runner/cache-cleaner.py
install -D -m 0644 easytier-runner-cache-cleaner.service /etc/systemd/system/easytier-runner-cache-cleaner.service
install -D -m 0644 easytier-runner-cache-cleaner.timer /etc/systemd/system/easytier-runner-cache-cleaner.timer
systemctl daemon-reload
systemctl enable --now easytier-runner-cache-cleaner.timer
```

Generate a repository-level registration token in **Settings > Actions >
Runners** and start the runner. The token is consumed only on first
registration; the state volume keeps the registration across restarts.

```bash
docker run -d --name easytier-owner-runner \
  --user 1000:1000 --restart unless-stopped \
  -e ACCESS_TOKEN='RUNNER_REGISTRATION_TOKEN' \
  -e GITHUB_URL='https://github.com/225284228a-droid/EasyTier-Custom' \
  -e RUNNER_NAME=easytier-owner-static \
  -e RUNNER_LABELS='self-hosted,linux,x64,easytier-owner-static' \
  -v easytier-runner-state:/actions-runner \
  -v easytier-runner-home:/home/runner \
  -v /srv/easytier-runner/cache:/cache \
  --read-only --tmpfs /tmp:rw,nosuid,nodev,size=1g \
  --cap-drop=ALL --security-opt=no-new-privileges:true \
  --pids-limit 512 --memory 10g --memory-swap 10g --cpus 12 \
  easytier-owner-runner:latest
```

The workflow has an owner gate, a fixed repository/workflow/ref check in the
runner hook, and clears the work directory after every completed job. Do not
add other workflows to this runner or enable pull-request triggers for it.
