# Custom static builds

`Custom Static Builds` runs on every push to `main`, including merged updates,
and supports manual runs from the Actions page. It does not synchronize upstream
source automatically.

| Artifact | Contents |
| --- | --- |
| `custom-static-linux-x86_64` | core, cli, web-embed, mini (musl) |
| `custom-static-windows-x86_64` | gui NSIS installer, core, cli, web-embed, network DLLs/drivers |

Linux archives preserve executable permissions. Each archive contains the source
commit and SHA-256 checksums. The embedded web server includes the dashboard and
config generator. Mini uses its own size-optimized Cargo profile and a separate
invocation to avoid feature unification with the full binaries.

Linux binaries are checked for ELF interpreter/shared-library dependencies and
smoke-tested with `--help`. Windows uses the repository's `+crt-static` setting;
CI rejects external VC++/UCRT runtime imports. The existing VC-LTL compatibility
layer uses the Windows-provided `msvcrt.dll`, which is allowed. Windows still needs system DLLs, the supplied
network drivers, and WebView2 for the GUI. The NSIS installer includes the WebView2
bootstrapper, which may download the runtime. These are not completely standalone
Windows executables, and the installer is unsigned.

After both builds succeed on main, cleanup keeps the current run's artifacts
and deletes older completed main-branch build artifacts from this workflow and the
legacy Core, GUI, Mobile and OHOS workflows. PRs, other branches, running/newer
builds, test artifacts, releases and Git history are untouched. A failed build
does not delete the previous good build. All new artifacts expire after seven
days, including partial failed/cancelled runs and the latest successful build.
Artifacts consume Actions storage, not Git repository history.

In EasyTier-Custom, the legacy Core, GUI, Mobile and OHOS workflows are disabled
in Actions settings to avoid duplicate/unrequested platform builds. Their source
is retained; they can be explicitly re-enabled if needed. Test workflows remain
enabled. No Rust target cache is uploaded by the custom workflow.

Mini uses native Clang/LLD because Zig 0.16 does not accept its RELR/ICF linker
options. It builds before the full Linux executables so linker failures surface
early. On Windows, all frontend workspace packages build in dependency order,
including `tauri-plugin-vpnservice-api`, before the Rust builds start.

## Warning audit (2026-09-26)

The first build run failed on mini's unsupported Zig linker options and the
missing VPN plugin frontend output. Neither failure was caused by a warning.

| Diagnostic | Assessment |
| --- | --- |
| `private_interfaces`, unused test imports/helper, Clippy style checks | Needs source maintenance. The separate Test workflow uses `-D warnings` and fails on these; the packaging workflow does not disable warnings. |
| `management-rpc` alone cannot import `management::full` | Actual existing feature-gating error found by Test's feature matrix, not a warning. It needs a separate source fix. Full builds and mini's `web-client` configuration include the required module. |
| Unused items in mini/WASM and Unix-only imports on Windows | Mostly conditional-compilation hygiene; not evidence of a runtime failure. Gate declarations/imports with their consumers when maintaining these modules. |
| musl drops `cdylib` | Expected for this static target; the Rust library and requested executables still build. |
| `easytier_core.pdb` filename collision | Debug-symbol output naming conflict between the library and similarly named binary. Does not affect these stripped release executables, but should be resolved before publishing debug symbols. |
| `VC-LTL5 Enabled`, `YY-Thunks Enabled` | Informational build-script output for the intended Windows compatibility layer. |
| Node 20, `url.parse()`, `punycode` deprecations | Tool/action dependency maintenance. The actions run under Node 24 successfully; upgrade the affected upstream actions when compatible versions are available. |
| Vite chunks over 500 kB | Frontend download/startup performance advisory; does not prevent embedding or installation. |

The separate Test run also recorded timeouts in the relay-coverage and UPnP
integration tests. These are test failures, not harmless warnings. No test was
disabled or marked successful as part of this build repair.
