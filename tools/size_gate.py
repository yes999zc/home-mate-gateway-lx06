#!/usr/bin/env python3
"""OTA size gate: the release artifact must fit the 16 MiB update body.

Why this exists
---------------
The device update endpoint accepts at most 16,777,216 bytes (16 MiB) per
body. An artifact that links and passes every ELF assertion but exceeds that
number is simply unshippable — the device rejects the update, so the build
result is worthless no matter how green everything else is. Both repositories
produce such an artifact (hm-core here, the gateway firmware image in
home-mate-device-gateway-b) and both run close to the line, which is why the
number is checked on every release build instead of being rediscovered at
deploy time.

WARN vs HARD
------------
HARD is the endpoint limit: one byte over and the build fails (exit 1).
WARN is a headroom signal, not a limit: it fires while the artifact still
fits, so growth shows up in build logs early instead of on the first update
that no longer fits. The default WARN of 16,300,000 leaves ~477 KB of room
before the hard line; it is deliberately not the 14,000,000 figure that was
once floated as a tighter target — that would fail every build today, and a
gate that is always red is a gate people learn to ignore.

Why the build script and not CI
-------------------------------
Neither repository's CI workflow builds a release artifact: they run host
`cargo build` / `cargo test` / `cargo clippy` only — no aarch64-musl or armv7
cross build, no strip, therefore no shippable bytes to measure. A CI job
would either have to grow the whole cross toolchain first or measure nothing.
The release build script is the one place the shippable artifact exists, so
that is where the gate runs, right after the ELF assertions. A green CI run
is not a size measurement.

Exit codes
----------
  0  under the HARD limit (WARNING printed if at or above WARN)
  1  over the HARD limit — do not ship this artifact
  2  usage error, or the file is missing / unreadable

Usage: size_gate.py <binary-path> [--hard BYTES] [--warn BYTES]
"""
import argparse
import os
import sys

MiB = 1024 * 1024
DEFAULT_HARD = 16 * MiB   # 16,777,216 — the device update body limit
DEFAULT_WARN = 16_300_000  # headroom signal, ~477 KB below the hard limit


def _n(value: int) -> str:
    """Thousands separator, so 16777216 reads as 16,777,216."""
    return f"{value:,}"


def _pct(part: int, whole: int) -> str:
    return f"{part * 100.0 / whole:.1f}%"


def run_self_test() -> int:
    """Verify size_gate exit codes (under limit -> 0, over limit -> 1, empty -> 2)
    and check any built release binary if present (#9, #43).
    """
    import tempfile

    with tempfile.TemporaryDirectory() as tmp:
        dummy = os.path.join(tmp, "dummy.bin")
        with open(dummy, "wb") as f:
            f.write(b"x" * 4096)

        # 1. Under limit -> exits 0
        rc_ok = main([dummy, "--hard", "8192", "--warn", "6144"])
        if rc_ok != 0:
            print(f"self-test FAIL: expected 0 for under-limit file, got {rc_ok}", file=sys.stderr)
            return 1

        # 2. Over limit -> exits 1
        rc_over = main([dummy, "--hard", "2048"])
        if rc_over != 1:
            print(f"self-test FAIL: expected 1 for over-limit file, got {rc_over}", file=sys.stderr)
            return 1

        # 3. Empty file -> exits 2
        empty = os.path.join(tmp, "empty.bin")
        with open(empty, "wb"):
            pass
        rc_empty = main([empty])
        if rc_empty != 2:
            print(f"self-test FAIL: expected 2 for empty file, got {rc_empty}", file=sys.stderr)
            return 1

    # 4. If a built firmware or host binary is present, verify it passes the 16 MiB gate.
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    candidates = [
        os.path.join(root, "target", "armv7-unknown-linux-musleabihf", "release", "home-mate-device-gateway"),
        os.path.join(root, "target", "release", "home-mate-device-gateway"),
    ]
    for cand in candidates:
        if os.path.isfile(cand) and os.path.getsize(cand) > 0:
            rc_bin = main([cand])
            if rc_bin != 0:
                return rc_bin

    print("PASS   size_gate --self-test")
    return 0


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(
        description="Fail a release build whose artifact exceeds the 16 MiB OTA limit.",
        epilog="Exit 0 = under the hard limit, 1 = over it, 2 = usage/file error.",
    )
    ap.add_argument("binary", nargs="?", help="path to the release artifact to measure")
    ap.add_argument("--self-test", action="store_true",
                    help="run self-test drill (under/over limit exit codes)")
    ap.add_argument("--hard", type=int, default=None, metavar="BYTES",
                    help=f"hard limit in bytes (default {_n(DEFAULT_HARD)} = 16 MiB)")
    ap.add_argument("--warn", type=int, default=None, metavar="BYTES",
                    help=f"warn line in bytes (default {_n(DEFAULT_WARN)}); prints WARNING, does not fail")
    args = ap.parse_args(argv)
    if args.self_test:
        return run_self_test()
    if not args.binary:
        ap.error("the following arguments are required: binary (or --self-test)")

    hard = DEFAULT_HARD if args.hard is None else args.hard
    # A lower --hard alone must still be usable for drills ("make it fail"), so
    # an implicit warn is pulled down to the hard line instead of erroring;
    # only a warn the caller spelled out can be a typo.
    warn = DEFAULT_WARN if args.warn is None else args.warn
    if args.warn is None and warn > hard:
        warn = hard

    if hard <= 0:
        print(f"size_gate: --hard must be positive, got {hard}", file=sys.stderr)
        return 2
    if warn < 0:
        print(f"size_gate: --warn must not be negative, got {warn}", file=sys.stderr)
        return 2
    if warn > hard:
        print(f"size_gate: --warn ({_n(warn)}) is above --hard ({_n(hard)}); "
              "a warning that can never fire is a typo", file=sys.stderr)
        return 2

    path = args.binary
    if not os.path.exists(path):
        print(f"size_gate: no such file: {path}", file=sys.stderr)
        return 2
    if not os.path.isfile(path):
        print(f"size_gate: not a regular file: {path}", file=sys.stderr)
        return 2
    try:
        size = os.path.getsize(path)
    except OSError as exc:
        print(f"size_gate: cannot stat {path}: {exc}", file=sys.stderr)
        return 2
    if size == 0:
        print(f"size_gate: {path} is 0 bytes — refusing to certify an empty artifact",
              file=sys.stderr)
        return 2

    if size > hard:
        over = size - hard
        print(f"FAIL   {path}")
        print(f"FAIL   {_n(size)} bytes is {_pct(size, hard)} of the {_n(hard)} "
              f"byte OTA limit")
        print(f"FAIL   over the hard limit by {_n(over)} bytes ({over / 1024.0:.1f} KiB)")
        print("FAIL   the device rejects update bodies this size — this artifact cannot ship")
        return 1

    headroom = hard - size
    if size >= warn:
        print(f"WARNING {path}")
        print(f"WARNING {_n(size)} bytes is {_pct(size, hard)} of the {_n(hard)} "
              f"byte OTA limit")
        print(f"WARNING only {_n(headroom)} bytes ({headroom / 1024.0:.1f} KiB) of headroom "
              f"left; warn line is {_n(warn)} — it still fits, but not for long")
        return 0

    print(f"PASS   {path}")
    print(f"PASS   {_n(size)} bytes is {_pct(size, hard)} of the {_n(hard)} "
          f"byte OTA limit")
    print(f"PASS   {_n(headroom)} bytes ({headroom / 1024.0:.1f} KiB) of headroom before the hard limit")
    return 0


if __name__ == "__main__":
    sys.exit(main())
