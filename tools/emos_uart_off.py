#!/usr/bin/env python3
"""Turn the debug UART console off in an emOS boot image (M8b, gate 1).

Why the DTB and not the cmdline
-------------------------------
`console=ttyMT0,921600n1` comes from the bootloader (LK, partitions p3/p5),
not from the Android boot header: the header only carries `bootopt`,
`ramoops.*` and `emos.system=`, and LK prepends its own args. Patching the
boot header therefore cannot remove the console, and patching LK means writing
the bootloader — the one component both physical recovery gates run through.

`console=` only registers a console for a device that exists. The kernel
probes `soc/uart0@11002000` from the DTB in *this* image (LK rewrites only
/chosen/bootargs in it), and uart1/2/3 in the same DTB carry `status =
"disable"` and never appear in /proc/consoles. So marking uart0 unavailable
removes ttyMT0 without touching the bootloader: the image stays recoverable
through the TWRP gate exactly like any other boot-image change.

The replacement is same-length on purpose (`okay` -> `off` plus NULs): the
FDT structure block is left byte-identical apart from those bytes, so no
offset anywhere moves. The kernel treats any status other than "okay"/"ok" as
unavailable (`__of_device_is_available`).

Usage:
  python3 tools/emos_uart_off.py --ref <16MiB image> --out <new image> [--check]

--check only reports what would change (no file written).
"""

import argparse
import struct
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "build-staging" / "EchoMuse-upstream" / "controller"))
import em_emos_build as emb  # noqa: E402

FDT_MAGIC = b"\xd0\x0d\xfe\xed"
BEGIN_NODE, END_NODE, PROP, END_TREE = 1, 2, 3, 9


def walk_props(blob: bytes, base_offset: int):
    """Yield (path, prop_name, value_start, value_len) for one FDT blob."""
    magic, total, off_struct, off_strings, _rsv, _ver, _lastc, _boot, size_str, size_s = struct.unpack_from(
        ">10I", blob, 0
    )
    if magic != 0xD00DFEED:
        return
    struct_blk = blob[off_struct:off_struct + size_s]
    strings = blob[off_strings:off_strings + size_str]
    pos = 0
    path = []
    while pos < len(struct_blk):
        tok = struct.unpack_from(">I", struct_blk, pos)[0]
        pos += 4
        if tok == BEGIN_NODE:
            end = struct_blk.find(b"\0", pos)
            name = struct_blk[pos:end].decode("utf-8", "replace")
            pos = (end + 4) & ~3
            path.append(name)
        elif tok == END_NODE:
            if path:
                path.pop()
        elif tok == PROP:
            ln, noff = struct.unpack_from(">II", struct_blk, pos)
            pos += 8
            val_at = pos                          # offset in struct_blk
            pos = (pos + ln + 3) & ~3
            end = strings.find(b"\0", noff)
            pname = strings[noff:end].decode("utf-8", "replace")
            yield "/".join(x for x in path if x), pname, base_offset + off_struct + val_at, ln
        elif tok == END_TREE:
            break


def fdt_blobs(region: bytes, base: int):
    """Every complete FDT in `region`, in order, as (file_offset, blob)."""
    pos = 0
    while True:
        i = region.find(FDT_MAGIC, pos)
        if i < 0:
            return
        if i + 8 > len(region):
            return
        total = struct.unpack_from(">I", region, i + 4)[0]
        if total < 32 or i + total > len(region):
            pos = i + 4
            continue
        yield base + i, region[i:i + total]
        pos = i + total


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--ref", required=True, help="16 MiB image currently on p10")
    ap.add_argument("--out", help="where to write the patched image")
    ap.add_argument("--check", action="store_true", help="report only")
    args = ap.parse_args()

    ref = Path(args.ref).read_bytes()
    if len(ref) != 16 * 1024 * 1024:
        sys.exit(f"{args.ref}: expected the 16 MiB padded image, got {len(ref)} bytes")
    parts = emb.split_reference(ref)
    # The image on p10 is the boot image padded to the 16 MiB partition. The
    # roundtrip gate runs on the boot image itself and requires the rest to be
    # zeros — that is the same check, without treating the pad as a difference.
    rebuilt = emb.pack(parts, parts["zimage"], parts["dtbs"], parts["ramdisk"], extra_cmdline="")
    if ref[:len(rebuilt)] != rebuilt or ref[len(rebuilt):].strip(b"\0"):
        sys.exit("the packer does not reproduce this image byte-for-byte; refusing to build")
    # split_reference: kernel = ref[PAGE:PAGE+ksz]; payload = kernel[0x200:]
    # zimage = payload[:i]; dtbs = payload[i:], so the DTB region starts at
    # PAGE + 0x200 + len(zimage) in the file.
    dtb_region = parts["dtbs"]
    dtb_base = emb.PAGE + 0x200 + len(parts["zimage"])

    patches = []          # (file_offset, new_bytes)
    report = []
    for foff, blob in fdt_blobs(dtb_region, dtb_base):
        for path, pname, val_at, ln in walk_props(blob, foff):
            if pname != "status":
                continue
            val = ref[val_at:val_at + ln]
            node = path.rsplit("/", 1)[-1]
            if not node.startswith("uart0"):
                continue
            report.append((path, val.rstrip(b"\0").decode("utf-8", "replace")))
            if val.rstrip(b"\0") == b"okay" and ln >= 3:
                patches.append((val_at, b"off" + b"\0" * (ln - 3)))

    if not report:
        sys.exit("no uart0 status property found — this is not the image this tool was written for")
    for path, status in report:
        print(f"uart0 node {path}: status={status!r}")
    print(f"{len(patches)} propert{'y' if len(patches) == 1 else 'ies'} would change okay -> off")
    if args.check:
        return 0
    if not patches:
        sys.exit("nothing to patch (uart0 is already unavailable?)")
    if not args.out:
        sys.exit("--out is required unless --check")

    dtbs = bytearray(dtb_region)
    for foff, new in patches:
        rel = foff - dtb_base
        if dtbs[rel:rel + len(new)] != new:
            dtbs[rel:rel + len(new)] = new

    out = emb.pack(
        parts,
        parts["zimage"],
        bytes(dtbs),
        parts["ramdisk"],
        extra_cmdline="",          # the reference already carries ramoops + emos.system=
        system_part=None,
    )
    out = emb.pad(out)
    if len(out) != 16 * 1024 * 1024:
        out = out + b"\0" * (16 * 1024 * 1024 - len(out))

    # Gates: the kernel payload must differ ONLY in those bytes, and the
    # ramdisk must be identical. A re-packer that quietly re-compressed the
    # kernel would pass a size check and fail an actual boot.
    new = bytearray(out)
    diff = [i for i in range(len(ref)) if ref[i] != new[i]]
    allowed = set()
    for foff, val in patches:
        allowed.update(range(foff, foff + len(val)))
    unexpected = [i for i in diff if i not in allowed and not (576 <= i < 608)]
    if unexpected:
        sys.exit(f"unexpected differences at {[hex(i) for i in unexpected[:8]]}")
    rsz = struct.unpack_from("<I", ref, 16)[0]
    roff = emb.PAGE + len(emb.pad(parts["zimage"] + parts["dtbs"]))
    if ref[roff:roff + rsz] != out[roff:roff + rsz]:
        sys.exit("ramdisk changed — refusing to ship")

    Path(args.out).write_bytes(out)
    print(f"wrote {args.out} ({len(out)} bytes); {len(diff)} bytes differ (uart0 status + image id)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
