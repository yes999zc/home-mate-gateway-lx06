#!/usr/bin/env python3
"""D/T5 firmware debt #6: per-LED ("逐灯旋转") physical rotation calibration.

The ring's 12 LEDs are wired to 36 PWM channels; the assumed sequential
mapping (3k,3k+1,3k+2) has never been visually confirmed. This tool walks
the ring one lamp at a time, asks the operator which PHYSICAL lamp lit (the
human is the sensor here — no camera automation is claimed), then writes
`led-map.json` and pushes it to the device. The firmware loads it on the
next start (firmware/leds.rs::with_calibrated_map).

Usage:
  led_calibrate.py [--port /dev/cu.usbmodem31101] [--binary /data/.../current]
                    [--out led-map.json] [--verify]

  --verify: re-walk the ring using the just-written map; every lamp must
            now appear at the operator's expected position (reports PASS/
            FAIL, does not change the file).

The binary must be a build containing the `led-cal` subcommand (T5 / 0.0.17+).
A device-side sleep loop was deliberately NOT used: a time64 syscall on this
3.18 kernel can kill the board (P0 root cause) — the host drives the timing.

Physical visual check is part of the on-site runbook; this tool is the
operator's script, not an unattended test.
"""
import argparse
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from fw_restart import Serial  # noqa: E402
from serial_push import main as push_main  # noqa: E402

DIR = "/data/local/tmp/hm-direct"
BINARY = f"{DIR}/current"
LEDS = 12


def show(ser, binary, idx, map_path=None):
    arg = "off" if idx is None else str(idx)
    extra = f" --map {map_path}" if map_path else ""
    out = ser.cmd(f"{binary} led-cal {arg}{extra}", 2)
    if "led-cal" not in out and "written" not in out:
        print(f"  [warn] unexpected reply: {out.strip()[:120]}")
    return out


def ask(idx):
    while True:
        raw = input(f"  ring index {idx:2d} is lit. Which PHYSICAL lamp? "
                    f"(0-11 clockwise from the marked one, or 'x' if none) > ").strip()
        if raw.lower() == "x":
            return None
        if raw.isdigit() and 0 <= int(raw) < LEDS:
            return int(raw)
        print("    enter 0..11 or x")


def build_map(order):
    """order[ring_index] = physical index (or None). Invert and expand to
    the three PWM channels that physically drive that lamp."""
    if any(v is None for v in order) or len(set(order)) != LEDS:
        raise ValueError("each ring index must map to a distinct physical lamp 0..11")
    # ring index k should light physical lamp k after calibration: invert
    # the observed permutation so the frame encoder writes the right PWMs.
    inv = [0] * LEDS
    for ring_idx, physical in enumerate(order):
        inv[physical] = ring_idx
    # For each desired ring position `p`, the channels that light physical
    # lamp `p` are the ones we observed when ring index inv[p] was lit:
    # sequential channels of inv[p]. Build the map row p -> those channels.
    m = []
    for p in range(LEDS):
        r = inv[p]
        m.append([3 * r, 3 * r + 1, 3 * r + 2])
    return m


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", default="/dev/cu.usbmodem31101")
    ap.add_argument("--binary", default=BINARY)
    ap.add_argument("--out", default="led-map.json")
    ap.add_argument("--verify", action="store_true")
    a = ap.parse_args()

    ser = Serial(a.port)
    try:
        if a.verify and os.path.exists(a.out):
            print("VERIFY pass: each lamp should now appear at its ring index.")
            bad = []
            for i in range(LEDS):
                show(ser, a.binary, i, f"{DIR}/led-map.json")
                got = ask(i)
                if got != i:
                    bad.append((i, got))
            show(ser, a.binary, None)
            print(f"VERIFY: {'PASS' if not bad else 'FAIL ' + str(bad)}")
            return 0 if not bad else 1

        print("Calibration walk: 12 lamps, one at a time. Operator reads the ring.")
        order = []
        for i in range(LEDS):
            show(ser, a.binary, i)
            order.append(ask(i))
        show(ser, a.binary, None)
        try:
            m = build_map(order)
        except ValueError as e:
            print(f"REFUSE: {e}; observation={order}", file=sys.stderr)
            return 2
        with open(a.out, "w") as f:
            json.dump(m, f)
        print(f"map -> {a.out}\n{json.dumps(m)}")
        # Push it to the device and confirm the md5 (serial_push.py).
        sys.argv = ["serial_push.py", a.out, f"{DIR}/led-map.json", a.port]
        rc = push_main()
        if rc != 0:
            print("push FAILED — map not installed", file=sys.stderr)
            return rc
        print("installed. Restart the firmware to load it; then run --verify.")
        return 0
    finally:
        ser.close()


if __name__ == "__main__":
    sys.exit(main())
