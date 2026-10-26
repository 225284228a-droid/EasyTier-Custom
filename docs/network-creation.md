# Network creation contract

The GUI and device-management Web console use the shared `RemoteManagement`
component. Official Web nodes retain `save_config` and Web-database ownership
of saved network configurations; their legacy save does not write node TOML.
Fork nodes advertising persisted revision management receive a `PatchConfig`
request with an empty expected revision; creation saves a disabled instance
and never starts a network implicitly.

The frontend compiler reads standard Protobuf field descriptors to exclude
deprecated fields from both creation masks and edit diffs. Protobuf-ts's runtime
field metadata retains deprecated fields but does not expose the standard
`deprecated` option, so using every runtime field as an editable field is unsafe.
This previously selected the four legacy VPN Portal fields and
`quic_listen_port`, which have no persisted TOML mapping. Rebuilding alone did
not remove them.

Two checks keep schema and implementations aligned without adding RPC fields:

- Core verifies that every non-deprecated `NetworkConfig` field other than
  `instance_id` has an explicit persisted document mapping.
- Shared frontend, GUI/Web adapters and real Core tests consume
  `easytier-web/frontend-lib/tests/fixtures/network-create-patches.json`. Frontend
  tests compare real builder output to this fixture; Core tests save it through
  the real persisted RPC and native atomic file storage, verify the projection,
  and reopen the configuration/state directory with a new manager.

When intentionally changing defaults or schema, update the fixture from the
real frontend builder and run both sides. Do not regenerate the fixture during
tests: doing so would hide contract drift. The fixture covers full and limited
capabilities and Manual, PublicServer and Standalone form modes.

Core returns safe fixed validation reasons and field names using the existing
response message. Web preserves that message; invalid patches remain HTTP 422
with `local_config_invalid_patch`, while missing revision targets are HTTP 404
with `local_config_not_found`. Raw conversion errors and configuration values
are not included in these diagnostics.

## Validation

Run `pnpm test:unit`, `cargo test -p easytier-core --lib persisted_merge`,
`cargo test -p easytier --lib instance::persisted_config_tests` and
`cargo test -p easytier-web local_config`.

After building the shared frontend, Web, GUI and native binaries, run
`pnpm --dir easytier-web/frontend test:creation`. Set `EASYTIER_SMOKE_BIN_DIR`
to a release/artifact directory to check the shipped binaries instead of
`target/debug`. The smoke test uses an isolated temporary account and config
directory, clicks the actual embedded console's Create New Network button,
submits every contract fixture, verifies error diagnostics, and restarts Core
to ensure all newly saved instances remain disabled. Install Chromium with
`pnpm --dir easytier-web/frontend exec playwright install chromium` first.
Set `EASYTIER_SMOKE_CORE_BINARY` to the rebuilt GUI executable to exercise its
embedded Core through the same smoke test. If `OFFICIAL_EASYTIER_BIN` points to
an official Core executable, the test also verifies that its creation request
uses the legacy save endpoint and filters unsupported extensions.

Custom Static Builds runs this smoke test against its packaged Core and embedded
Web before uploading either platform's archive. A failed creation check blocks
that upload. GUI and Web should be upgraded together with Core; old frontends
that still submit deprecated masks receive a specific field error.
