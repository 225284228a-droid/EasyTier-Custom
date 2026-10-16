# Static builds

The former `Custom Static Builds` workflow is retired. Static builds now belong
to **EasyTier Build**, separate from **EasyTier CI** testing. See the current
[CI guide](../.github/CI.md) / [中文说明](../.github/CI.zh-CN.md) for triggers,
platforms, downloads and cleanup. The audit below records the historical workflow.

Linux, Windows and macOS products target x86-64 only. Android targets ARM64;
FreeBSD, HarmonyOS, Magisk and Linux GUI installers are excluded. Product builds
use Release mode, with mini's size-optimized profile inheriting Release.

Linux x64 archives include Core, CLI, Web, embedded Web and mini. They preserve
executable permissions and include the source commit and SHA-256 checksums.
CI rejects dynamic interpreter/shared-library dependencies and smoke-tests
startup. Mini uses native Clang/LLD for its linker options.

Windows CLI archives include the network drivers and stay separate from GUI
NSIS installers. CI rejects external VC++/UCRT runtime imports while allowing the
existing Windows-provided `msvcrt.dll` compatibility layer. The GUI still needs
WebView2. Frontend workspace packages, including the VPN plugin API, build in
dependency order; platform bundling reuses their output.

A successful main build validates all six new product artifacts before deleting
older completed main build artifacts, including retired workflows and removed
architectures. Failed or incomplete new builds keep the previous usable outputs.
Products expire after seven days; Release assets and Git history are untouched.

## Warning audit (2026-09-26)

The first build run failed on mini's unsupported Zig linker options and the
missing VPN plugin frontend output. Neither failure was caused by a warning.

| Diagnostic | Assessment |
| --- | --- |
| `private_interfaces`, unused test imports/helper, Clippy style checks | Source repair makes the connection-source type public like its API, removes unused imports, shares the tested protocol parser with production, and groups related hole-punch arguments. The Test workflow retains `-D warnings`. |
| `management-rpc` alone cannot import `management::full` | Source repair gates state-store exports with `web-client`, matching the module definition, and gates the native RPC import with its management-only consumers. |
| Unused items in mini/WASM and Unix-only imports on Windows | Mostly conditional-compilation hygiene; not evidence of a runtime failure. Gate declarations/imports with their consumers when maintaining these modules. |
| musl drops `cdylib` | Expected for this static target; the Rust library and requested executables still build. |
| `easytier_core.pdb` filename collision | Debug-symbol output naming conflict between the library and similarly named binary. Does not affect these stripped release executables, but should be resolved before publishing debug symbols. |
| `VC-LTL5 Enabled`, `YY-Thunks Enabled` | Informational build-script output for the intended Windows compatibility layer. |
| Node 20, `url.parse()`, `punycode` deprecations | Tool/action dependency maintenance. The actions run under Node 24 successfully; upgrade the affected upstream actions when compatible versions are available. |
| Vite chunks over 500 kB | Frontend download/startup performance advisory; does not prevent embedding or installation. |

The UPnP integration test explicitly disables WSS/HTTP3 preference because it
verifies raw UDP mapping within ten seconds. The default preference gives those
transports a thirty-second head start, which prevented the original test from
observing the mapping. Its timeout and mapping/connection assertions are retained.
The relay-coverage/failover test still has an intermittent startup ping timeout.
Its root cause is unresolved; its original assertions and the relay implementation
are unchanged. This separate Test failure does not gate build-artifact uploads.
No test or warning check is disabled by these repairs.
