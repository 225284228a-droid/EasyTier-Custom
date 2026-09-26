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
CI rejects dynamic CRT imports. Windows still needs system DLLs, the supplied
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
