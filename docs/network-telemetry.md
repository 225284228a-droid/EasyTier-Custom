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

The dashboard polls about every two seconds after a completed collection.
It limits each request to eight seconds, retries transient per-machine failures
twice, and buffers the last good machines/snapshots for up to 60 seconds.
An independent expiry timer keeps this limit during long collection batches.
Confirmed stopped instances are removed immediately. Cached data is marked stale and is never sampled as
new traffic; switching servers clears both snapshot and counter caches.
An incomplete-data warning appears only after a running machine has not
successfully reported for a continuous minute, and clears on recovery.
Camera orientation, zoom, selected node and automatic rotation are stored in
a scoped 180-day preference cookie. A separate browser-local archive keeps
minimal last-known node/link metadata for seven days, never credentials,
configuration, byte counters or real-time rates. Archived nodes are restored
only after a successful authorized machine listing, filtered by currently
running instances, and marked stale; they never generate traffic particles.
Real-time bit/s is derived from cumulative connection byte counters.
Two endpoint reports do not double-count the same link; concurrent tunnels
are aggregated. Missing/reset counters do not create invented rates.

GeoIP is resolved from the running node's STUN/public IP, never the web or
config-server connection address. Device lists reuse the session's resolved
node-location cache, with background refresh attempts limited to once per
minute per session and four concurrent requests. Opening the device list
directly also warms this cache without blocking its response.
IPv4 is preferred for display when available, regardless of STUN list order.
City lookup first uses the node's IPv4 and falls back to its IPv6 if needed;
using an IPv6 city result does not change the IPv4 address displayed.
GeoIP coordinates are preferred. Country-only results use an explicitly
approximate country location derived from the embedded Natural Earth map.
Unknown locations remain in the node list instead of receiving invented
coordinates. City-level lookup now uses an independent online-populated
SQLite cache with automatic refresh, expiry, quotas and offline fallback.
See `docs/city-geolocation.md` for privacy, providers and configuration.
The embedded DB-IP Country Lite database remains the offline fallback.
`--geoip-db` can still supply a local city-level MMDB.

The Three.js globe loads on demand, pauses automatic rotation during direct
manipulation and has no post-drag inertia. Zoom selects 24k/96k/288k point-cloud
levels with equal-area land/ocean point distributions. Country/coastline
boundaries switch between real 110m/50m/10m source detail and increasingly
fine spherical subdivisions on zoom. Higher-detail assets load on demand. Its
viewport uses a desktop golden-ratio layout and releases rendering resources
when the dashboard closes.
The node panel matches the map height and scrolls internally. At close zoom,
decluttered labels show node names earlier (camera distance <= 2.8), and
both measured traffic directions only at close zoom (distance <= 1.7).
Traffic labels use a cross layout: endpoint names flank the bidirectional
arrow, with the corresponding directional rates above and below the arrow.
No latency value is displayed in this label.
Flow particles move in both directions, with logarithmically scaled density
based on each direction's measured traffic. Travel time follows the mean
valid RTT of fresh open channels, with a logarithmic 0.35-8 second display
range; a missing RTT uses a conservative five-second animation fallback.
This is a visual timing scale, not individual-packet tracing. Zero/unknown/stale directions
do not show fabricated moving particles. Attribution is in the About dialog.

## Verification

- Rust: `cargo test -p easytier --lib tunnel::stats::tests`
- Node IP selection: `cargo test -p easytier-web node_public_ip`
- GeoIP and location cache: `cargo test -p easytier-web location`
- Online city cache: `cargo test -p easytier-web geolocation`
- Frontend unit tests: `pnpm --dir easytier-web/frontend-lib test:config-ui`
- Web build: `pnpm --dir easytier-web/frontend build`
- Browser QA: start Vite, then run `pnpm --dir easytier-web/frontend test:dashboard`.

Browser QA requires Playwright, PNGJS and a Chromium installation. It can use
bundled runtimes via `PLAYWRIGHT_MODULE`, `PNGJS_MODULE` and
`CHROMIUM_EXECUTABLE`; `DASHBOARD_URL` defaults to `http://127.0.0.1:5187`.
The test intercepts API responses with fixtures and checks desktop/mobile
continent pixels, animation, selection, layout, browser errors and empty state.
Screenshots are written under the system temporary directory by default.
