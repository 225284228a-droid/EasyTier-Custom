# Network Telemetry

## Passive Bandwidth Estimate

Connection statistics expose `estimated_tx_bps` and `estimated_rx_bps` in bit/s.
The estimator retains 50 fixed 200 ms buckets and reports the highest eligible
business-payload rate from the last 10 seconds. Each direction needs at least
4 packets, 4,096 bytes and 100 ms of observation in a bucket. Zero means that
recent traffic is insufficient, so the UI displays `--`.

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
Machine identity is resolved using both instance and peer IDs, including
copied instance IDs across machines.

GeoIP coordinates are preferred. Country-only results use an explicitly
approximate country location derived from the embedded Natural Earth map.
Unknown locations remain in the node list instead of receiving invented
coordinates. The existing `--geoip-db` option can supply a city-level database
for finer geographic placement.

The Three.js globe loads on demand, supports rotation, zoom and node selection,
and releases its rendering resources when the dashboard closes.

## Verification

- Rust: `cargo test -p easytier-core --lib tunnel::stats::tests`
- Frontend unit tests: `pnpm --dir easytier-web/frontend-lib test:config-ui`
- Web build: `pnpm --dir easytier-web/frontend build`
- Browser QA: start Vite, then run `pnpm --dir easytier-web/frontend test:dashboard`.

Browser QA requires Playwright, PNGJS and a Chromium installation. It can use
bundled runtimes via `PLAYWRIGHT_MODULE`, `PNGJS_MODULE` and
`CHROMIUM_EXECUTABLE`; `DASHBOARD_URL` defaults to `http://127.0.0.1:5187`.
The test intercepts API responses with fixtures and checks desktop/mobile
continent pixels, animation, selection, layout, browser errors and empty state.
Screenshots are written under the system temporary directory by default.
