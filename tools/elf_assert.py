#!/usr/bin/env python3
"""R0 (2026-11-13) build assertions for the on-device firmware image.

The OOM blocker had a build-system root cause: `cargo zigbuild` handed the
FINAL link to Zig's LLD, which pulled Zig's own musl build (malloc backed by
`heap.SmpAllocator`) instead of Rust's bundled musl. Freed blocks were kept
on per-size free lists forever — firmware RSS grew ~0.33 MB per second of
played audio and the kernel killed it after ~57 min (exit 137; gateway
docs/d/d-stage-close-20261110 §1.6). The fix keeps `zig cc` for the C
dependencies (SpeexDSP) but final-links with rust-lld + Rust's libc.a.

This tool is the gate on the artifact. All five assertions must hold or the
image must not be deployed:

  1. ELF32 little-endian, EM_ARM, ET_EXEC, no PT_INTERP / PT_DYNAMIC
     (statically linked), EABI version 5 in e_flags.
  2. e_flags == 0x05000400 — EABI5 | EF_ARM_ABI_FLOAT_HARD (0x400).
     The archived 2026-11-08 binaries carry 0x200 (SOFT float): the drift
     was real, so the hard-float bit is asserted, not assumed.
  3. Tag_ABI_VFP_args == 1 in .ARM.attributes (hard-float calling
     convention on the FP ABI).
  4. `__libc_malloc_impl` present — Rust's musl malloc (mallocng). Zig's
     musl has no such symbol; its absence means the allocator regressed.
  5. `.comment` names no "Homebrew LLD" — the final link must be rust-lld
     (it reports `Linker: LLD <n> (https://github.com/rust-lang/...)`).
     A Homebrew clang string is expected and fine: `zig cc` compiles the C
     objects, and that compiler identifies itself that way.

Usage: elf_assert.py <binary>   (exit 0 = all assertions hold)
"""
import struct
import sys

EF_ARM_HASENTRY = 0x02
EF_ARM_ABI_FLOAT_SOFT = 0x200
EF_ARM_ABI_FLOAT_HARD = 0x400
EXPECTED_FLAGS = 0x05000000 | EF_ARM_ABI_FLOAT_HARD  # EABI5 + hard float

PT_LOAD, PT_DYNAMIC, PT_INTERP = 1, 2, 3
SHT_SYMTAB = 2
SHT_NOBITS = 8


class Elf:
    def __init__(self, path):
        self.path = path
        self.data = open(path, "rb").read()
        d = self.data
        if d[:4] != b"\x7fELF":
            raise SystemExit(f"{path}: not an ELF file")
        if d[4] != 1 or d[5] != 1:
            raise SystemExit(f"{path}: expected ELF32 little-endian (class={d[4]} data={d[5]})")
        (self.e_type, self.e_machine) = struct.unpack_from("<HH", d, 16)
        self.e_flags = struct.unpack_from("<I", d, 36)[0]
        (self.e_phoff, self.e_shoff) = struct.unpack_from("<II", d, 28)
        (self.e_phentsize, self.e_phnum, self.e_shentsize, self.e_shnum,
         self.e_shstrndx) = struct.unpack_from("<HHHHH", d, 42)
        self.sections = []
        for i in range(self.e_shnum):
            off = self.e_shoff + i * self.e_shentsize
            self.sections.append(struct.unpack_from("<IIIIIIIIII", d, off))

    def section_data(self, sh):
        _, sh_type, _, _, sh_offset, sh_size = sh[:6]
        if sh_type == SHT_NOBITS:
            return b""
        return self.data[sh_offset:sh_offset + sh_size]

    def shstrtab(self):
        return self.section_data(self.sections[self.e_shstrndx])

    def section(self, name):
        """(index, header) of the section called `name`, or None."""
        strtab = self.shstrtab()
        for i, sh in enumerate(self.sections):
            end = strtab.find(b"\0", sh[0])
            if strtab[sh[0]:end].decode("latin-1") == name:
                return i, sh
        return None

    def names_in(self, sh, strtab):
        out = []
        entsize = sh[9] or 16
        body = self.section_data(sh)
        for off in range(0, len(body) - 15, entsize):
            st_name = struct.unpack_from("<I", body, off)[0]
            if st_name == 0:
                continue
            end = strtab.find(b"\0", st_name)
            out.append(strtab[st_name:end].decode("latin-1"))
        return out

    def symbols(self):
        out = []
        for i, sh in enumerate(self.sections):
            if sh[1] != SHT_SYMTAB:
                continue
            strtab = self.section_data(self.sections[sh[6]])
            out.extend(self.names_in(sh, strtab))
        return out

    def program_headers(self):
        for i in range(self.e_phnum):
            off = self.e_phoff + i * self.e_phentsize
            yield struct.unpack_from("<IIIIIIII", self.data, off)


def uleb(buf, pos):
    shift = value = 0
    while True:
        b = buf[pos]
        pos += 1
        value |= (b & 0x7F) << shift
        shift += 7
        if not b & 0x80:
            return value, pos


def vfp_args(attrs):
    """Tag_ABI_VFP_args (28) from the `aeabi` file-scope attribute list.

    Layout (ARM ABI "Build Attributes"): 'A', then a 4-byte little-endian
    length covering everything after the 'A'; vendor name "aeabi\\0"; then
    one sub-section per scope: tag (uleb128) + 4-byte little-endian length
    (covering tag + length + attributes) + the attribute body.
    """
    assert attrs[:1] == b"A"
    pos = 5  # 'A' + the 4-byte section length
    pos = attrs.find(b"aeabi\0", pos) + len(b"aeabi\0")
    tag, pos = uleb(attrs, pos)                 # Tag_File (1)
    size = struct.unpack_from("<I", attrs, pos)[0]
    body = attrs[pos + 4:pos + size]            # size counts from `tag`
    i = 0
    while i < len(body):
        tag, i = uleb(body, i)
        if tag in (4, 5, 65):        # strings (CPU_raw_name / CPU_name / conformance)
            i = body.index(b"\0", i) + 1
        elif tag >= 32:
            n, i = uleb(body, i)
            i += n
        else:
            value, i = uleb(body, i)
            if tag == 28:
                return value
    return None


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: elf_assert.py <binary>", file=sys.stderr)
        return 2
    elf = Elf(sys.argv[1])
    results = []

    def check(name, ok, detail):
        results.append((name, ok, detail))
        print(f"{'PASS' if ok else 'FAIL'}  {name}: {detail}")

    # 1. Identity + static linkage.
    phdrs = list(elf.program_headers())
    ptypes = {p[0] for p in phdrs}
    eabi = (elf.e_flags >> 24) & 0xFF
    check("elf32-le-arm-exec-static",
          elf.e_type == 2 and elf.e_machine == 40 and PT_INTERP not in ptypes
          and PT_DYNAMIC not in ptypes,
          f"e_type={elf.e_type} e_machine={elf.e_machine} "
          f"interp={'yes' if PT_INTERP in ptypes else 'no'} "
          f"dynamic={'yes' if PT_DYNAMIC in ptypes else 'no'}")
    check("eabi-version-5", eabi == 5, f"EABI version {eabi}")

    # 2. Hard-float e_flags. 0x200 (soft) is the 2026-11-08 drift.
    soft = bool(elf.e_flags & EF_ARM_ABI_FLOAT_SOFT)
    hard = bool(elf.e_flags & EF_ARM_ABI_FLOAT_HARD)
    check("e_flags-hard-float", elf.e_flags == EXPECTED_FLAGS and hard and not soft,
          f"e_flags=0x{elf.e_flags:08x} (expected 0x{EXPECTED_FLAGS:08x}; "
          f"soft={'yes' if soft else 'no'} hard={'yes' if hard else 'no'})")

    # 3. FP ABI attribute.
    sec = elf.section(".ARM.attributes")
    vfp = vfp_args(elf.section_data(sec[1])) if sec else None
    check("Tag_ABI_VFP_args-1", vfp == 1, f"Tag_ABI_VFP_args={vfp}")

    # 4. Rust's musl malloc (mallocng). Zig's musl does not export it.
    syms = elf.symbols()
    check("rust-musl-malloc", "__libc_malloc_impl" in syms,
          f"__libc_malloc_impl={'present' if '__libc_malloc_impl' in syms else 'MISSING'} "
          f"({len(syms)} symbols)")

    # 5. Final linker identity.
    comment = elf.section(".comment")
    text = elf.section_data(comment[1]).decode("latin-1") if comment else ""
    linker_lines = [l for l in text.split("\0") if "Linker:" in l]
    check("rust-lld-final-link",
          "Homebrew LLD" not in text and any("Linker: LLD" in l for l in linker_lines),
          "; ".join(linker_lines) or "no Linker: line in .comment")

    failed = [n for n, ok, _ in results if not ok]
    print(f"\n{len(results) - len(failed)}/{len(results)} assertions hold "
          f"for {sys.argv[1]}")
    if failed:
        print("FAILED: " + ", ".join(failed))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
