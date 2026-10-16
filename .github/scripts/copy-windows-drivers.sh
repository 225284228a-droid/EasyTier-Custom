#!/usr/bin/env bash
set -euo pipefail
case "$1" in
  x86_64-pc-windows-msvc) ARCH_DIR=x86_64 ;;
  i686-pc-windows-msvc) ARCH_DIR=i686 ;;
  aarch64-pc-windows-msvc) ARCH_DIR=arm64 ;;
  *) echo "Unsupported Windows target: $1" >&2; exit 1 ;;
esac
mkdir -p "$2"
find "easytier/third_party/$ARCH_DIR" -maxdepth 1 -type f \
  \( -name '*.dll' -o -name '*.sys' \) -exec cp {} "$2/" \;
