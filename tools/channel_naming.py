#!/usr/bin/env python3
"""D/T5 firmware debt #5: name the LAST 9-channel column (codec DSP pre/post).

ch0..6 are the mic array, ch8 is the DAC loopback reference. ch7 is the
unresolved one: is it a tap BEFORE the codec's DSP (raw, unaffected by our
software loudness stage) or AFTER it (same content as ch8, processed)?

The A/B is the user-directed `--loudness off` comparison: our loudness
stage is applied in software BEFORE the dealias reference feed, so a tap
downstream of that stage changes level/spectrum between the two captures,
while an upstream tap does not. Take two 9ch s16le dumps of the SAME test
tone (one with loudness on, one with `--loudness off`) and compare.

  # on device (serial console), for each arm:
  #   /data/local/tmp/hm-direct/current firmware ... [--loudness off] \
  #       --loopback-probe ...      (arm the probe files)
  # on Mac:
  serial_pull.py /run/hm-loop/mics.pcm on.pcm
  serial_pull.py /run/hm-loop/mics.pcm off.pcm
  channel_naming.py on.pcm off.pcm

Usage: channel_naming.py <on.pcm> <off.pcm> [--rate 16000] [--out report.json]

This tool REPORTS, it does not decide the naming by itself: the verdict is
"consistent with pre-DSP / consistent with post-DSP / inconclusive" with the
numbers that justify it, and the final name goes into the T5 record after a
human reads the A/B. No audio is played by this tool.
"""
import argparse
import json
import math
import struct
import sys

CHANNELS = 9
RATE = 16000


def load(path, rate):
    raw = open(path, "rb").read()
    frame = CHANNELS * 2
    n = len(raw) // frame
    cols = [[] for _ in range(CHANNELS)]
    for i in range(n):
        vals = struct.unpack_from("<9h", raw, i * frame)
        for c, v in enumerate(vals):
            cols[c].append(v)
    return cols


def rms(x):
    if not x:
        return 0.0
    return math.sqrt(sum(v * v for v in x) / len(x))


def db(x):
    v = rms(x)
    return 20 * math.log10(v / 32768) if v > 0 else -120.0


def corr_lag(a, b, max_lag=64):
    """Best normalized correlation of a vs b over ±max_lag (same-window)."""
    n = min(len(a), len(b)) - 2 * max_lag
    if n <= 0:
        return 0.0, 0
    ea = sum(v * v for v in a[max_lag:max_lag + n]) or 1
    best, best_lag = 0.0, 0
    for lag in range(-max_lag, max_lag + 1):
        # b shifted by lag relative to a
        dot = 0.0
        eb = 0.0
        for i in range(n):
            j = i + max_lag + lag
            if 0 <= j < len(b):
                bv = b[j]
                dot += a[max_lag + i] * bv
                eb += bv * bv
        if eb <= 0:
            continue
        c = dot / math.sqrt(ea * eb)
        if abs(c) > abs(best):
            best, best_lag = c, lag
    return best, best_lag


def spectral_centroid(x, rate):
    """Coarse spectral tilt via zero-crossing rate (no numpy dependency)."""
    if len(x) < 2:
        return 0.0
    zc = sum(1 for i in range(1, len(x)) if (x[i - 1] < 0) != (x[i] < 0))
    return zc * rate / (2 * len(x))


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("on_pcm")
    ap.add_argument("off_pcm")
    ap.add_argument("--rate", type=int, default=RATE)
    ap.add_argument("--out", default="")
    a = ap.parse_args()
    on = load(a.on_pcm, a.rate)
    off = load(a.off_pcm, a.rate)

    report = {"arms": {"loudness_on": a.on_pcm, "loudness_off": a.off_pcm},
              "channels": {}, "verdict": "inconclusive", "basis": []}
    for c in range(CHANNELS):
        lvl_on, lvl_off = db(on[c]), db(off[c])
        zc_on, zc_off = spectral_centroid(on[c], a.rate), spectral_centroid(off[c], a.rate)
        report["channels"][str(c)] = {
            "rms_db_on": round(lvl_on, 2),
            "rms_db_off": round(lvl_off, 2),
            "delta_db": round(lvl_off - lvl_on, 2),
            "zcr_on_hz": round(zc_on, 1),
            "zcr_off_hz": round(zc_off, 1),
        }
    c7_on, c8_on = corr_lag(on[7], on[8])
    c7_off, c8_off = corr_lag(off[7], off[8])
    report["ch7_vs_ch8"] = {
        "loudness_on": {"corr": round(c7_on, 3), "lag": c8_on},
        "loudness_off": {"corr": round(c7_off, 3), "lag": c8_off},
    }

    d7 = abs(report["channels"]["7"]["delta_db"])
    d8 = abs(report["channels"]["8"]["delta_db"])
    d0 = abs(report["channels"]["0"]["delta_db"])
    # A mic column is the control: it must NOT move when our software stage
    # is toggled. If it does, the two captures are not comparable.
    if d0 > 3.0:
        report["basis"].append(f"control ch0 moved {d0:.1f} dB — captures not comparable, retake")
    elif d7 < 1.0 and d8 > 3.0:
        report["verdict"] = "ch7_consistent_with_pre_dsp"
        report["basis"].append(f"ch7 unchanged ({d7:.1f} dB) while ch8 moved {d8:.1f} dB")
    elif d7 > 3.0 and abs(c7_on - c7_off) < 0.1:
        report["verdict"] = "ch7_consistent_with_post_dsp"
        report["basis"].append(f"ch7 moved {d7:.1f} dB with the loudness stage")
    else:
        report["basis"].append(f"ch7 delta {d7:.1f} dB, ch8 delta {d8:.1f} dB — no clean separation")

    print(json.dumps(report, ensure_ascii=False, indent=2))
    if a.out:
        with open(a.out, "w") as f:
            json.dump(report, f, ensure_ascii=False, indent=2)
        print(f"report -> {a.out}")
    print("\nHUMAN DECISION REQUIRED: this is a measurement, not a naming.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
