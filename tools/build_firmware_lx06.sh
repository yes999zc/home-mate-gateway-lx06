#!/bin/sh
# Build the on-device LX06 audio firmware (aarch64 static musl).
#
# Uses zig cc for C dependencies (vendored SpeexDSP) and rust-lld for the
# final link so Rust's own musl libc.a (mallocng) is linked instead of Zig's
# SmpAllocator (see tools/build_firmware.sh and home-mate-b/tools/build_core.sh).
set -eu

TARGET=aarch64-unknown-linux-musl
HERE=$(cd "$(dirname "$0")/.." && pwd)
cd "$HERE"

SYSROOT=$(rustc --print sysroot)
HOST=$(rustc -vV | sed -n 's/^host: //p')
LINKER="$SYSROOT/lib/rustlib/$HOST/bin/rust-lld"
if [ ! -x "$LINKER" ]; then
    echo "build_firmware_lx06: rust-lld not found at $LINKER" >&2
    exit 1
fi

CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="$LINKER"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER

RUSTFLAGS="${RUSTFLAGS:-} -C strip=symbols"
export RUSTFLAGS

cargo zigbuild --locked --target "$TARGET" --release

TARGET_DIR=$(cargo metadata --locked --no-deps --format-version 1 | python3 -Ic 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
BIN="$TARGET_DIR/$TARGET/release/home-mate-device-gateway"
python3 -I - "$BIN" <<'PY'
import struct
import sys
from pathlib import Path

path = Path(sys.argv[1])
data = path.read_bytes()
if len(data) < 64 or data[:7] != b"\x7fELF\x02\x01\x01":
    sys.exit(f"{path}: expected a little-endian ELF64 executable")
kind, machine = struct.unpack_from("<HH", data, 16)
entry, phoff, shoff = struct.unpack_from("<QQQ", data, 24)
phsize, phnum, shsize, shnum, shnames = struct.unpack_from("<5H", data, 54)
if machine != 183 or kind not in (2, 3) or phsize != 56 or phoff + phnum * phsize > len(data):
    sys.exit(f"{path}: expected an AArch64 executable with valid program headers")
can_enter = False
for i in range(phnum):
    segment, flags, offset, addr, _, size, memsize, _ = struct.unpack_from("<II6Q", data, phoff + i * phsize)
    if offset + size > len(data) or segment == 3:  # PT_INTERP
        sys.exit(f"{path}: invalid segment or external ELF interpreter")
    if segment == 2:  # PT_DYNAMIC: static PIE may have relocations but no DT_NEEDED.
        tags = struct.iter_unpack("<qQ", data[offset:offset + size])
        if any(tag == 1 for tag, _ in tags):
            sys.exit(f"{path}: executable needs shared libraries")
    if segment == 1 and flags & 1 and addr <= entry < addr + memsize:
        can_enter = True
if not can_enter:
    sys.exit(f"{path}: entry point outside executable LOAD segments")

if shsize != 64 or shnames >= shnum or shoff + shnum * shsize > len(data):
    sys.exit(f"{path}: missing ELF section metadata")
sections = list(struct.iter_unpack("<II4QII2Q", data[shoff:shoff + shnum * shsize]))
names_offset, names_size = sections[shnames][4:6]
names = data[names_offset:names_offset + names_size]
for sec in sections:
    if names[sec[0]:].split(b"\0", 1)[0] == b".comment":
        comment = data[sec[4]:sec[4] + sec[5]]
        linkers = [s for s in comment.split(b"\0") if s.startswith(b"Linker:")]
        if linkers and all(s.startswith(b"Linker: LLD ") and b"rust-lang/llvm-project" in s for s in linkers):
            break
else:
    sys.exit(f"{path}: missing rust-lld final-link marker in .comment")
print("PASS  elf64-aarch64-static + rust-lld-final-link")
PY

python3 -I "$HERE/tools/size_gate.py" "$BIN"
echo "build_firmware_lx06: $BIN"
