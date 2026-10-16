// One source of truth for build matrices and release artifact validation.
const coreTargets = [
  ['x86_64-unknown-linux-musl', 'ubuntu-24.04', 'linux-x86_64'],
  ['aarch64-unknown-linux-musl', 'ubuntu-24.04-arm', 'linux-aarch64'],
  ['riscv64gc-unknown-linux-musl', 'ubuntu-24.04', 'linux-riscv64'],
  ['loongarch64-unknown-linux-musl', 'ubuntu-24.04', 'linux-loongarch64'],
  ['armv7-unknown-linux-musleabihf', 'ubuntu-24.04', 'linux-armv7hf'],
  ['armv7-unknown-linux-musleabi', 'ubuntu-24.04', 'linux-armv7'],
  ['arm-unknown-linux-musleabihf', 'ubuntu-24.04', 'linux-armhf'],
  ['arm-unknown-linux-musleabi', 'ubuntu-24.04', 'linux-arm'],
  ['mips-unknown-linux-musl', 'ubuntu-24.04', 'linux-mips'],
  ['mipsel-unknown-linux-musl', 'ubuntu-24.04', 'linux-mipsel'],
  ['x86_64-unknown-freebsd', 'ubuntu-24.04', 'freebsd-13.2-x86_64'],
  ['x86_64-apple-darwin', 'macos-latest', 'macos-x86_64'],
  ['aarch64-apple-darwin', 'macos-latest', 'macos-aarch64'],
  ['x86_64-pc-windows-msvc', 'windows-latest', 'windows-x86_64'],
  ['i686-pc-windows-msvc', 'windows-latest', 'windows-i686'],
  ['aarch64-pc-windows-msvc', 'windows-11-arm', 'windows-arm64'],
].map(([target, os, name]) => ({
  target, os, name,
  features: /^(riscv64|loongarch64|aarch64)|freebsd|windows/.test(target) ? 'mimalloc' : 'jemalloc',
}));

const guiTargets = coreTargets
  .filter(({ target }) => /windows|darwin/.test(target))
  .map(({ target, os, name }) => ({ target, os, name }));
const androidArchs = ['aarch64', 'armv7', 'i686', 'x86_64'];
const checkTargets = coreTargets.filter(({ name }) =>
  ['linux-x86_64', 'macos-aarch64', 'windows-x86_64'].includes(name));

const infrastructure = /^(\.github\/(workflows\/build\.yml|tauri-ci\.json$|actions\/|scripts\/)|package\.json$)/;
const rustPaths = /^(Cargo\.(toml|lock)$|rust-toolchain\.toml$|\.cargo\/|\.config\/nextest\.toml$|easytier\/|easytier-core\/|easytier-proto\/|easytier-rpc-build\/|easytier-web\/|easytier-gui\/|tauri-plugin-vpnservice\/|easytier-contrib\/)/;
const frontendPaths = /^(pnpm-(lock|workspace)\.yaml$|easytier-web\/(frontend\/|frontend-lib\/|config-generator\/)|easytier-gui\/|tauri-plugin-vpnservice\/|easytier-proto\/proto\/|easytier-core\/src\/config\/|easytier-core\/src\/lib\.rs$|Cargo\.(toml|lock)$|rust-toolchain\.toml$|\.cargo\/)/;

function buildPlan({ eventName, ref, draft = false, changedFiles = [] }) {
  const full = eventName === 'workflow_dispatch' || (eventName === 'push' && ref === 'refs/heads/main');
  const active = !draft && (full || eventName === 'pull_request' ||
    (eventName === 'push' && ref === 'refs/heads/develop'));
  const matches = pattern => changedFiles.some(file => pattern.test(file) || infrastructure.test(file));
  const rust = active && (full || matches(rustPaths));
  // Rust lint/unit tests also consume the generated frontend assets. Build once.
  const frontend = active && (rust || full || matches(frontendPaths));
  return {
    active, full: active && full, rust, frontend,
    ci: active,
    go: active && (eventName === 'workflow_dispatch' || matches(/^(easytier-go\/|easytier-proto\/proto\/)/)),
    js: active && (eventName === 'workflow_dispatch' || matches(/^(Cargo\.(toml|lock)$|easytier-(core|proto|js)\/)/)),
    nix: active && (eventName === 'workflow_dispatch' || matches(/(^|\/)[^/]+\.nix$|^flake\.lock$|^rust-toolchain\.toml$/)),
  };
}

module.exports = { coreTargets, guiTargets, androidArchs, checkTargets, buildPlan };
