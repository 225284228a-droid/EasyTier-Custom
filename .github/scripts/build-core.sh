#!/usr/bin/env bash
set -euo pipefail
mkdir -p artifacts
SUFFIX=
[[ "$TARGET" != *windows* ]] || SUFFIX=.exe

BUILD=build
[[ "$TARGET" != *linux-musl* ]] || BUILD=zigbuild
test -s easytier-web/frontend/dist/index.html
test -s easytier-web/frontend/dist/config-generator/index.html
cargo "$BUILD" --locked --release --target "$TARGET" -p easytier \
  --features "$FEATURES" --bin easytier-core --bin easytier-cli
cargo "$BUILD" --locked --release --target "$TARGET" -p easytier-web --features embed
cp "target/$TARGET/release/easytier-web$SUFFIX" "artifacts/easytier-web-embed$SUFFIX"
cp "target/$TARGET/release/easytier-core$SUFFIX" "target/$TARGET/release/easytier-cli$SUFFIX" artifacts/

if [[ "$TARGET" == x86_64-unknown-linux-musl ]]; then
  # mini's linker flags and optional features must stay isolated from full builds.
  sudo apt-get install -qqy clang lld
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=clang \
    RUSTFLAGS='-C target-feature=+crt-static -C link-arg=-fuse-ld=lld' \
    cargo build --locked --profile mini --target "$TARGET" -p easytier-mini
  cp "target/$TARGET/mini/easytier-mini" artifacts/
  for binary in artifacts/easytier-*; do
    file "$binary"
    readelf -lW "$binary" > /tmp/easytier-program-headers.txt
    readelf -dW "$binary" > /tmp/easytier-dynamic-section.txt
    if grep -q INTERP /tmp/easytier-program-headers.txt || grep -q '(NEEDED)' /tmp/easytier-dynamic-section.txt; then
      echo "::error::$binary is not fully static"
      exit 1
    fi
    "$binary" --help > /dev/null
  done
fi

if [[ "$TARGET" == *windows* ]]; then
  bash .github/scripts/copy-windows-drivers.sh "$TARGET" artifacts
else
  chmod 755 artifacts/easytier-*
fi
