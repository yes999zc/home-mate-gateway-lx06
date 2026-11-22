#!/usr/bin/env python3
"""CI line-count ratchet gate (#26, #43).

Enforces:
  1. `crates/gateway/src/firmware/mod.rs` <= 800 lines
  2. Every `.rs` file under `crates/` <= 1,500 lines

Exits 0 when all files comply, 1 if any file exceeds its ceiling.
"""
import pathlib
import sys

MOD_RS_PATH = pathlib.Path("crates/gateway/src/firmware/mod.rs")
MOD_RS_MAX = 800
CRATES_RS_MAX = 1500


def count_lines(path: pathlib.Path) -> int:
    with path.open("rb") as f:
        return sum(1 for _ in f)


def main() -> int:
    root = pathlib.Path(__file__).resolve().parent.parent
    crates_dir = root / "crates"
    if not crates_dir.is_dir():
        print(f"line_gate: missing {crates_dir}", file=sys.stderr)
        return 2

    violations = []
    rs_files = sorted(crates_dir.rglob("*.rs"))
    for path in rs_files:
        rel = path.relative_to(root)
        lines = count_lines(path)
        limit = MOD_RS_MAX if rel == MOD_RS_PATH else CRATES_RS_MAX
        if lines > limit:
            violations.append((rel, lines, limit))

    if violations:
        for rel, lines, limit in violations:
            print(
                f"FAIL   {rel}: {lines} lines exceeds limit of {limit} lines",
                file=sys.stderr,
            )
        return 1

    mod_lines = count_lines(root / MOD_RS_PATH)
    max_file = max(rs_files, key=count_lines)
    max_lines = count_lines(max_file)
    print(
        f"PASS   line_gate: {len(rs_files)} .rs files checked "
        f"(firmware/mod.rs={mod_lines}/{MOD_RS_MAX}, "
        f"max={max_file.relative_to(root)} {max_lines}/{CRATES_RS_MAX})"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
