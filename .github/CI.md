# CI

[中文](CI.zh-CN.md)

## Entry points

Two automatic workflows keep testing separate from product compilation:

| Workflow | Triggers and responsibility |
| --- | --- |
| **EasyTier CI** (`checks.yml`) | Push to `main`/`develop`, PR targeting either branch, or manual: relevant checks and tests |
| **EasyTier Build** (`build.yml`) | Push to `main` or manual: Release binaries, installers, Web assets and artifact cleanup |

**EasyTier Release** and **EasyTier Docker** remain manual entry points. Draft
PRs skip expensive checks. Unrelated PR/develop changes skip unrelated checks;
Go, JavaScript Hosts and Nix retain their own path filters. Each automatic
workflow cancels its older runs for the same branch/PR independently.

`Build result` requires every planned compilation job to succeed. `CI result`
requires every planned test/check job to succeed, including jobs unexpectedly
skipped. Build uploads and cleanup are independent of the CI test result;
failed tests remain visible in EasyTier CI.

Scheduled builds, automatic publication/retry chains, HarmonyOS/Magisk builds,
FreeBSD builds and Linux GUI installers are removed. Platform source remains.

## Platforms and shared work

| Product | Targets / downloadable artifact count |
| --- | --- |
| Core/CLI/Web | Linux, macOS and Windows, x86-64 only / 3 |
| Desktop GUI | macOS and Windows, x86-64 only / 2 |
| Android APK | ARM64 phones only: aarch64 (arm64-v8a) / 1 |

There are **6 product artifacts**. Windows CLI and GUI remain separate jobs and
artifacts. macOS uses an Intel runner. Linux x64 includes mini, verifies static
linking and executable startup. Windows packages network drivers and verifies
runtime dependencies. Linux/Windows run the network-creation browser smoke test
against the shipped Core and embedded Web binaries.

Product builds use Release mode. Mini inherits Release with size optimizations;
Tauri desktop/Android builds use their default Release mode and collect only
`release` outputs. Signing/notarization is optional: manual `sign_windows`
enables SignPath test signing for command-line binaries, and `sign_macos`
enables signed, notarized GUI DMGs. Corresponding repository secrets are needed.

The build workflow generates proto, shared library, VPN plugin API, Web,
configuration-generator WASM and GUI assets once. Platform jobs download these
resources through a cache scoped to the current run. No frontend
resource bundle is uploaded to the artifact list. The Tauri CI configuration
disables repeated frontend build hooks.
The CI workflow builds only the frontend resources needed for tests and Rust
backend checks. PR/develop and manual CI additionally compile configuration-generator
WASM when its frontend or Rust interface changes; main uses the production build
for this coverage. Shared frontend
libraries are built once within each workflow; frontend unit tests and CI script
tests run only in EasyTier CI.

CI retains fmt, Clippy, WASI, locked dependencies, protocol/Web/GUI backend tests
and Linux network integration tests. Feature combinations run on main and manual
CI. Platform and Android pre-checks run on PR/develop and manual CI; a main push
already compiles those products in EasyTier Build. Linux GUI backend tests remain,
with no Linux GUI installer. Network tests compile once and preserve the three
nextest groups and existing exclusive/concurrent scheduling. Go tests and
proto consistency share tool setup; Nix skips its redundant preliminary check.

## Downloads, cleanup and release

Product artifacts expire after seven days. All six files use direct,
unarchived uploads (`actions/upload-artifact@v7`, `archive: false`):

| Download | Contents |
| --- | --- |
| `easytier-linux-x86_64.tar.gz` | Command-line programs, commit and checksums |
| `easytier-macos-x86_64.tar.gz` | Command-line programs, commit and checksums |
| `easytier-windows-x86_64.zip` | Command-line programs, network drivers, commit and checksums |
| `easytier-gui-windows-x86_64.exe` | Windows GUI installer |
| `easytier-gui-macos-x86_64.dmg` | macOS GUI installer |
| `easytier-mobile-android-aarch64.apk` | ARM64 Android application |

Command-line archives need one extraction and preserve Unix executable modes.
EXE, DMG and APK downloads are the installer itself, without an additional ZIP.
The dashboard and configuration generator ship inside `easytier-web-embed` in
the command-line packages; there is no separate Web download and no standalone
`easytier-web` binary. Per-file SHA-256
and the source commit appear in the job summary. The manual Release validates
the downloaded bytes against GitHub's recorded artifact digests, preserves each
file and produces combined `COMMIT.txt` and `SHA256SUMS`.

Frontend resources and the compiled network-test archive use exact caches scoped
to the run, with no fallback to another build. A successful new run removes older
completed resource caches from the same workflow and branch. Current-run caches
remain available for job retries and follow GitHub's normal cache eviction policy.
If a cache is unavailable (including fork PRs with read-only cache access), the
consumer rebuilds that resource locally; tests are still required. Internal
resources are absent from downloadable artifacts.

After a main build succeeds, cleanup first validates all **6 new product
artifacts** are present, nonempty, unexpired and from this run. Only then does it
delete artifacts from older completed main builds, including retired
Core/GUI/Mobile/Custom Static/OHOS workflows and their old architecture outputs.
Running, newer, PR, develop, unrelated and unidentified runs are preserved.
Failed or incomplete builds keep the previous usable artifacts. GitHub Release
assets are untouched; the latest successful build still expires after seven days.

Run **EasyTier Release** with one successful full `build_run_id`, a `version`
and `make_latest`. It validates the 6 expected artifacts in this repository,
downloads only those files, adds combined commit/checksums and creates a draft
at the build commit. It does no compilation or automatic publication. Choose a
build whose artifacts have not been cleaned up or expired.

Docker retains the existing manual upstream-image flow: its run ID refers to
Core artifacts in `EasyTier/EasyTier`, with the existing actor/registry settings.

## Local verification

Run `actionlint -no-color` and `pnpm test:ci-scripts`. Run
`pnpm build:frontend:check` then `pnpm test:frontend:ci` for frontend CI.
`pnpm build:frontend:ci` also verifies production configuration-generator WASM
and needs its Rust target and wasm-pack. Full platform compilation and privileged
network tests are validated on GitHub runners.
