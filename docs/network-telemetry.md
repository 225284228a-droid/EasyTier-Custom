# Network Telemetry

## Transmission Window Estimate

Connection statistics expose `estimated_tx_bps` and `estimated_rx_bps` in bit/s,
with `bandwidth_estimate_version = 1`. The business-traffic peak estimator,
sparse-traffic fallback, sampling filter and five-minute peak retention have
been removed. The UI rejects unversioned/old estimates and closed connections.

The formula is `min(cwnd_bytes, peer_receive_window_bytes) * 8 / effective_RTT_seconds`.
If peer flow-control information is not exposed, the formula uses the real
congestion window alone. Effective RTT is the larger of the transport RTT and
an exponentially smoothed successful tunnel Ping/Pong RTT (1/8 new sample),
with a conservative 1 ms minimum. A local TCP/proxy leg with a tiny RTT therefore
cannot outweigh a slower measured end-to-end tunnel round trip. Missing/zero
RTT and zero windows cannot establish a new estimate; arithmetic overflow is
rejected instead of reported as a saturated `u64` value.
No socket send/receive buffer size is substituted for a congestion window.
The RTT minimum deliberately underestimates some very fast LANs; it is not a
10 Gbit/s capacity limit, and a sufficiently large valid window can exceed that.

Native TCP uses `TCP_INFO` on Linux and `SIO_TCP_INFO` on Windows. Linux's
segment-count cwnd is multiplied by the actual sending MSS; Windows already
reports cwnd in bytes. Snapshots are sampled at most once every 200 ms during
socket I/O. A temporarily missing or invalid sample retains the last valid
snapshot without refreshing its timestamp; the snapshot expires 60 seconds
after its last valid observation and cannot keep a socket alive.
WS/WSS retain their underlying TCP telemetry. QUIC/HTTP3 use Quinn's live path
congestion window and RTT; its peer flow-control credit is not exposed here.
Other native TCP platforms and transports without congestion-window telemetry
(plain UDP, WireGuard, fake TCP, ring/host transports) remain unknown.

Upload is the local transport's window estimate. Download is the remote
transport's upload estimate, carried in the existing per-connection Ping/Pong
path, inside session authentication when that security mode is enabled.
The first four sequence bytes remain compatible with old peers. A distinct
request/reply marker prevents an old peer's echo from being mistaken for a
remote measurement. Missing directions are not mirrored. Remote reports expire
after 90 seconds; polling and zero/invalid reports never extend their lifetime.
Zero reports retain an existing valid report until it expires and leave an
unmeasured direction unknown. Normal pings, rather
than business payloads or added speed-test traffic, maintain idle-link telemetry.

This is a window-limited throughput reference, not measured physical capacity,
unused headroom or an active speed test. Initial/app-limited congestion windows
can still underestimate a fast link before the transport learns its path.
TCP terminated by a CDN/proxy still supplies that leg's window, so even a
tunnel RTT correction cannot establish the capacity of every proxy backhaul.
Both nodes need the updated binary for two-sided window reporting; updating only
the web frontend cannot add window telemetry to an old node.
Connection reconciliation may replace TCP/QUIC with UDP/WireGuard or another
transport that has no congestion-window telemetry. That new connection remains
unknown; estimates are never copied across connections or retained after a
socket closes.

The shared status table combines cumulative upload/download into one Total
Traffic column, with upload and download on separate lines. Estimated Available
Bandwidth follows it in the same two-line format, before Loss Rate.

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
The default and reset view center near Hong Kong (22.3 N, 114.17 E);
saved camera preferences take precedence on reopening. Clicking the selected
node again clears its highlight/detail panel without moving the camera,
and the cleared selection is persisted too.
The node panel matches the map height and scrolls internally. At close zoom,
decluttered labels show node names earlier (camera distance <= 2.8), and
both measured traffic directions only at close zoom (distance <= 1.7).
Traffic labels use a cross layout: endpoint names flank the bidirectional
arrow, with the corresponding directional rates above and below the arrow.
A compact footer shows the mean measured round-trip latency (RTT), separate
from the directional rates; missing measurements remain unknown.
Automatic collection leaves the refresh control idle; only a requested manual
refresh shows its loading state. Labels and their connectors retain their DOM
identity, dimensions and previous attachment during polling, with names/rates/RTT
updated in place. Only departed nodes and links remove their labels.
Traffic connectors are two pixels wide with higher contrast.
Device names in the same reported city form one stable vertical column.
Links between the same two geographic city groups share another vertical
column, while retaining each device pair's separate rates, RTT and emitters.
Each column has one short shared connector. Long columns shrink by complete
rows and scroll internally instead of scattering, hiding or merging their
members. Scrolling a column does not zoom the globe.
Labels try multiple visible anchors along their own link and both sides of
its projected tangent, with at most a 48-pixel traffic connector (32 pixels
for node names). They avoid other labels, node markers and existing label
connectors. A valid previous attachment is preferred to prevent jitter.
Selected-node links take precedence; labels that cannot fit locally are
hidden instead of being pushed into distant rows.
Flow particles move in both directions. One-shot emission frequency is
independent of RTT and of all other links. It uses the same soft display
curve per direction: `6 * sqrt(bps / 16000000) / (1 + sqrt(bps / 16000000))`.
Two Mbit/s is about 1.6 dots/second, twenty Mbit/s about 3.2 dots/second,
and very large rates approach six dots/second without a hard saturation
threshold. Dot frequency is a compressed visual indicator, not a linear
byte/packet count; the labels retain the actual measured bit/s.
Sparse traffic is not rounded up to a minimum continuous stream. Faster
links have fewer particles in flight at equal throughput, not a higher
emission frequency. Each device pair and direction owns its own emitter
and RTT-derived speed even when several paths coincide on the map. Only
parallel tunnels belonging to the same device pair are aggregated.
Travel time follows the mean valid RTT of fresh open channels, expanded
twentyfold and bounded to 0.08-6 seconds for display. Latency ratios are
preserved within those limits; a missing RTT uses a conservative five-second
animation fallback. Particles retire at arrival instead of looping, and
fractional emissions/in-flight progress survive display rebuilds.
This is a visual timing scale, not individual-packet tracing. Zero/unknown/stale directions
do not show fabricated moving particles. Attribution is in the About dialog.

## Verification

- Core window math, telemetry and framing: `cargo test -p easytier-core --lib tunnel::`
- Bidirectional/legacy Ping compatibility: `cargo test -p easytier-core --lib peers::conn::peer_conn_ping::tests`
- Native TCP telemetry: `cargo test -p easytier --lib socket::tcp::window::tests`
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
