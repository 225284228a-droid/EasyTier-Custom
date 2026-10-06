# Network Telemetry

## Passive Bandwidth Estimate

Connection statistics expose `estimated_tx_bps` and `estimated_rx_bps` in bit/s.
The estimator retains 50 fixed 200 ms buckets and reports the highest eligible
business-payload rate from the last 10 seconds. Each direction needs at least
4 packets, 4,096 bytes and 100 ms of observation in a bucket. Sparse traffic
falls back to a recent payload average after at least 1,024 bytes, using the
actual first/last payload sample span with a minimum 200 ms duration.
A usable estimate is retained for five minutes after its underlying sample,
not extended by UI polling. Zero means that no usable sample exists or the
retained estimate expired, so the UI displays `--`. Estimates are captured
when business traffic is sampled; opening or polling the UI does not create
them or change their expiration.

This is a traffic-derived capacity hint, not an active speed test, a guaranteed
delivery rate, or a measurement of unused headroom. It adds no probe traffic.
Control packets, ambiguous encrypted foreign-network payloads and packets
rejected by session authentication are not sampled. Existing wire byte and
packet counters remain separate.

The shared status table places Estimated Available Bandwidth between Download
and Loss Rate, with upload and download on separate lines.

## Dashboard Globe

The dashboard collects every running network for each managed machine, with
at most four remote collection requests in flight. Only live connection
records form links; a multi-hop route is not treated as a direct connection.
Nodes are identified by runtime network name and peer ID. Multiple tunnels,
underlay addresses and copied instance IDs do not create additional nodes.
The network count is the number of distinct runtime networks, not instances
across machines.

GeoIP is resolved from the running node's STUN/public IP, never the web or
config-server connection address. Device lists reuse the session's resolved
node-location cache, with background refresh attempts limited to once per
minute per session and four concurrent requests. Opening the device list
directly also warms this cache without blocking its response.
GeoIP coordinates are preferred. Country-only results use an explicitly
approximate country location derived from the embedded Natural Earth map.
Unknown locations remain in the node list instead of receiving invented
coordinates. The default offline DB-IP Country Lite database covers countries
globally, including IPv6, and performs no external IP lookup. The existing
`--geoip-db` option can supply a city-level database for finer geographic
placement. Dataset source and license are in `easytier-web/resources/GEOIP-DATA.md`.

The Three.js globe loads on demand, pauses automatic rotation during direct
manipulation and has no post-drag inertia. Zoom selects 24k/96k/288k point-cloud
levels, with country/coastline boundaries becoming clearer up close. Its
viewport uses a desktop golden-ratio layout and releases rendering resources
when the dashboard closes.

## Verification

- Rust: `cargo test -p easytier-core --lib tunnel::stats::tests`
- Node IP selection: `cargo test -p easytier-web node_public_ip`
- GeoIP and location cache: `cargo test -p easytier-web location`
- Frontend unit tests: `pnpm --dir easytier-web/frontend-lib test:config-ui`
- Web build: `pnpm --dir easytier-web/frontend build`
- Browser QA: start Vite, then run `pnpm --dir easytier-web/frontend test:dashboard`.

Browser QA requires Playwright, PNGJS and a Chromium installation. It can use
bundled runtimes via `PLAYWRIGHT_MODULE`, `PNGJS_MODULE` and
`CHROMIUM_EXECUTABLE`; `DASHBOARD_URL` defaults to `http://127.0.0.1:5187`.
The test intercepts API responses with fixtures and checks desktop/mobile
continent pixels, animation, selection, layout, browser errors and empty state.
Screenshots are written under the system temporary directory by default.
