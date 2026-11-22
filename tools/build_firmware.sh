#!/bin/sh
# Build the on-device audio firmware (armv7 hard-float musl).
#
# R0, 2026-11-13 — why this script exists and why it is the only supported
# way to build the firmware:
#
#   `cargo zigbuild` hands the FINAL link to Zig's bundled LLD, which links
#   Zig's own musl build. Its malloc is `heap.SmpAllocator`: freed small
#   blocks go onto per-size free lists and never return to the OS, so the
#   firmware's RSS grew ~0.33 MB per second of played audio and the kernel
#   killed it after ~57 min (exit 137; gateway docs/d/d-stage-close §1.6).
#
#   The fix keeps `zig cc` for the C dependencies (vendored SpeexDSP: no
#   arm-linux-musleabihf-gcc on this host) and moves only the final link to
#   rust-lld, which uses Rust's own musl libc.a (mallocng). `.cargo/
#   config.toml`'s `linker=` is IGNORED under cargo zigbuild — the
#   CARGO_TARGET_<TRIPLE>_LINKER environment variable is the only knob that
#   reaches the final link.
#
# The artifact assertions (e_flags 0x05000400 hard float, Tag_ABI_VFP_args,
# Rust's __libc_malloc_impl, no Homebrew LLD in .comment) are checked by
# tools/elf_assert.py and the build FAILS if any of them regresses.
set -eu

TARGET=armv7-unknown-linux-musleabihf
HERE=$(cd "$(dirname "$0")/.." && pwd)
cd "$HERE"

SYSROOT=$(rustc --print sysroot)
HOST=$(rustc -vV | sed -n 's/^host: //p')
LINKER="$SYSROOT/lib/rustlib/$HOST/bin/rust-lld"
if [ ! -x "$LINKER" ]; then
    echo "build_firmware: rust-lld not found at $LINKER (rustup component missing?)" >&2
    exit 1
fi

# Keep zig cc for C compilation/archiving, rust-lld for the final link.
CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER="$LINKER"
export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER

cargo zigbuild --target "$TARGET" --release

BIN="target/$TARGET/release/home-mate-device-gateway"
python3 -I "$HERE/tools/elf_assert.py" "$BIN"

# The device update endpoint takes at most 16,777,216 bytes (16 MiB) per
# body. The five ELF assertions above say the image is correct; this one says
# it can still be delivered. It lives in this script, not in CI: the CI
# workflow runs host `cargo test`/`clippy` only and never cross-compiles
# armv7, so it has no firmware image to measure — a green CI run is NOT a
# size measurement and does not replace this check.
#
# Exit 0 = fits (a WARNING holds the artifact at 16,300,000+ so growth shows
# up in build logs before it becomes a hard failure); exit 1 = over 16 MiB,
# do not deploy.
python3 -I "$HERE/tools/size_gate.py" "$BIN"

echo "build_firmware: $BIN"
