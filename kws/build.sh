#!/usr/bin/env bash
# Build the streaming KWS sidecar for dot2-d (aarch64, glibc).
#
# The device runs glibc binaries through the bundled Debian loader (emOS has
# no /lib/ld-linux-aarch64.so.1), so this cross-builds for aarch64-linux-gnu
# at the same glibc floor the device's bundled libc provides (2.36). The
# sherpa-onnx shared release only needs GLIBC_2.17, verified with strings.
#
# Prereq: the official runtime package next to this script or announced via
# SHERPA_PKG:
#   gh release download v1.12.15 --repo k2-fsa/sherpa-onnx \
#     --pattern 'sherpa-onnx-v1.12.15-linux-aarch64-shared-cpu.tar.bz2'
#   tar xjf sherpa-onnx-v1.12.15-linux-aarch64-shared-cpu.tar.bz2
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
pkg="${SHERPA_PKG:-$here/sherpa-onnx-v1.12.15-linux-aarch64-shared-cpu}"
out="${OUT:-$here/hm-kws}"

if [[ ! -f "$pkg/lib/libsherpa-onnx-c-api.so" ]]; then
  echo "missing sherpa runtime at $pkg (set SHERPA_PKG)" >&2
  exit 1
fi
command -v zig >/dev/null || { echo "zig is required (cargo zigbuild toolchain)" >&2; exit 1; }

zig cc \
  -target aarch64-linux-gnu.2.17 \
  -O2 \
  -Wall -Wextra \
  -I"$here" \
  -o "$out" \
  "$here/driver.c" \
  -L"$pkg/lib" \
  -lsherpa-onnx-c-api \
  -Wl,-rpath,'$ORIGIN' \
  -Wl,--allow-shlib-undefined

file "$out"
ls -l "$out"
