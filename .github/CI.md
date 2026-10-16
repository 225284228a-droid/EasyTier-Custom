# CI

[中文](CI.zh-CN.md)

## Entry points

There are three workflows: **EasyTier Build**, **EasyTier Release**, and
**EasyTier Docker**. Build is the only automatic entry point.

| Trigger | Behavior |
| --- | --- |
| Push to `main` | Full platform builds, checks, and downloadable Actions artifacts |
| Push to `develop`, or PR targeting `main`/`develop` | Relevant checks; no release binaries or installers |
| Manual EasyTier Build on any branch | Full platform builds and all checks |
| Draft PR | Skip builds/tests; retain the aggregate result |

Preparation calculates path filters and matrices once. New runs cancel older
runs for the same branch/PR and mode; checks do not cancel full builds. Go,
JavaScript Hosts, and Nix retain independent path filters. The single aggregate
check is **Build result** (`build-result`): required jobs must succeed, including
jobs that would otherwise be unexpectedly skipped.

Scheduled builds, automatic releases, HarmonyOS/Magisk build jobs, and Linux
GUI installers have been removed. Their source directories remain available.

## Platforms and shared work

| Product | Targets |
| --- | --- |
| Core/CLI/Web | 16: Linux x64, ARM64, RISC-V, LoongArch, ARMv7 hard/soft float, ARM hard/soft float, MIPS/MIPSEL; FreeBSD x64; macOS Intel/ARM64; Windows x64/x86/ARM64 |
| Desktop GUI | 5: macOS Intel/ARM64 and Windows x64/x86/ARM64 |
| Android APK | 4: aarch64, armv7, i686, x86_64 |

Windows CLI and GUI have separate jobs and artifacts. MIPS retains its existing
Core/CLI-only payload. Linux x64 also includes mini and validates static linking
and executable startup. Windows validates runtime dependencies and packages its
network drivers. Linux/Windows x64 run the network-creation browser smoke test
against the shipped Core and embedded Web binaries.

One frontend job generates proto bindings, builds the shared library and VPN
plugin API, builds Web/config-generator/GUI assets, and runs frontend unit tests.
Other jobs download those resources. CI-only Tauri configuration disables its
frontend build hook; local development commands retain their normal behavior.
Setup actions install tools/dependencies without rebuilding frontend packages.

CI script tests run once. Rust checks retain fmt, Clippy, individual feature
combinations in full mode, WASI, locked dependencies, protocol/Web/GUI backend
tests, and Linux network integration tests. Equivalent platform pre-checks run
only in check mode. The GUI backend still has headless Linux tests. Network tests
compile once and retain their three disjoint nextest groups and existing
exclusive/concurrent scheduling. Go tests and generated-proto validation share
one tool installation. Nix builds without a duplicate preliminary cargo check.

Desktop/command-line builds are unsigned/unnotarized by default. Manual `sign_windows` enables
SignPath test signing for command-line binaries; `sign_macos` enables signed,
notarized GUI DMGs. These opt-ins require their corresponding repository secrets.

## Downloads, cleanup, and manual releases

Product artifacts expire after seven days. Core artifacts are named
`easytier-<platform>` and contain a ready-made `.tar.gz` on Unix or `.zip` on
Windows. Tarballs retain executable modes. GUI artifacts are
`easytier-gui-<platform>`; APKs use `easytier-mobile-android-<arch>`. Native
installers retain their filenames. Binary/native payloads include `COMMIT.txt`
and `SHA256SUMS`, including shipped drivers. `easytier-web-dashboard` contains a
ready-made frontend tarball. Internal `frontend-assets` and `tests` expire after
one day and are excluded from releases.

Only after all planned full-build jobs and checks succeed on `main`, cleanup
deletes artifacts from older, completed main builds. It also migrates artifacts
from the retired Core/GUI/Mobile/Custom Static/OHOS workflows. Running, newer,
PR, develop, unrelated, or unidentified runs are preserved; failed new builds
preserve the previous usable outputs. GitHub Release assets are not deleted.
Normally only the latest successful main build remains downloadable, subject to
the seven-day expiration.

For a version release, run **EasyTier Release** with one `build_run_id`, `version`,
and `make_latest`. The run must be a successful full EasyTier Build in this
repository. All 26 expected product artifacts must exist and be unexpired.
Release downloads only these artifacts, preserves ready-made archives/native
installers, adds combined commit/checksum files, and creates a draft at the
validated build commit. It performs no compilation or automatic publication.
Choose a retained build before cleanup or expiration removes its artifacts.

Docker remains a manual upstream-image workflow: its run ID still refers to
Core artifacts in `EasyTier/EasyTier`; existing actor and registry settings are
retained.

## Local verification

Run `actionlint -no-color`, `pnpm test:ci-scripts`, and `pnpm test:unit`.
`pnpm build:frontend:ci` also verifies shared production frontend compilation
and requires the Rust WASM target and wasm-pack. Full platform compilation and
privileged Linux network tests are validated on GitHub runners.
