# G6 ERLE analysis (acceptance §0). Pure python, no numpy.
# Usage: erle_report.py <run_dir> [<run_dir> ...]
# Each run_dir must contain pre.pcm / post.pcm / ref.pcm (16k mono s16le)
# and meta.json from erle_run.py. Reports per-run ERLE + floor check and
# the median across runs. No preset pass number — this establishes the
# unit's baseline (regressions compare against it, nothing else).
#!/usr/bin/env python3
import json
import math
import os
import struct
import sys

RATE = 16000


def load_pcm(path: str) -> list[int]:
    data = open(path, "rb").read()
    n = len(data) // 2
    return list(struct.unpack(f"<{n}h", data[: n * 2]))


def energy_db(samples: list[int]) -> float:
    if not samples:
        return -120.0
    acc = sum(float(s) * s for s in samples)
    if acc <= 0:
        return -120.0
    return 10.0 * math.log10(acc / len(samples))


def playback_window(ref: list[int]) -> tuple[int, int]:
    """Onset/offset by short-window RMS on the reference (the DAC's exact
    output; the silence clock keeps between-play truly silent)."""
    win = RATE // 10  # 100ms windows
    thresh = 64.0  # amplitude floor, same notion as the dealias non-silent
    n = len(ref) // win
    active = []
    for i in range(n):
        seg = ref[i * win : (i + 1) * win]
        peak = max(abs(s) for s in seg)
        active.append(peak > thresh)
    first = active.index(True) if True in active else -1
    last = len(active) - 1 - active[::-1].index(True) if first >= 0 else -1
    if first < 0:
        return -1, -1
    return first * win, (last + 1) * win


def analyze(run_dir: str) -> dict:
    pre = load_pcm(os.path.join(run_dir, "pre.pcm"))
    post = load_pcm(os.path.join(run_dir, "post.pcm"))
    ref = load_pcm(os.path.join(run_dir, "ref.pcm"))
    n = min(len(pre), len(post), len(ref))
    pre, post, ref = pre[:n], post[:n], ref[:n]
    onset, offset = playback_window(ref)
    if onset < 0:
        return {"run": run_dir, "error": "no playback found in reference track"}

    # Trim 1s head/tail of the playback segment (acceptance §0).
    a, b = onset + RATE, offset - RATE
    if b - a < RATE:  # need at least 1s of usable segment
        return {"run": run_dir, "error": f"playback window too short ({offset-onset}s)"}
    pre_seg, post_seg = pre[a:b], post[a:b]

    # Noise floor: 2s before onset + 2s after offset, on the post track
    # (what remains when nothing plays — the honest comparison base).
    floor = post[max(0, onset - 2 * RATE) : onset] + post[offset : min(n, offset + 2 * RATE)]

    e_pre = energy_db(pre_seg)
    e_post = energy_db(post_seg)
    e_floor = energy_db(floor)
    e_ref = energy_db(ref[a:b])
    erle = e_pre - e_post
    floor_limited = (e_post - e_floor) < 3.0
    return {
        "run": run_dir,
        "playback_s": round((offset - onset) / RATE, 2),
        "pre_db": round(e_pre, 1),
        "post_db": round(e_post, 1),
        "floor_db": round(e_floor, 1),
        "ref_db": round(e_ref, 1),
        "erle_db": round(erle, 1),
        "floor_limited": floor_limited,
        "post_vs_floor_db": round(e_post - e_floor, 1),
    }


def main() -> int:
    results = [analyze(d) for d in sys.argv[1:]]
    for r in results:
        if "error" in r:
            print(f"{r['run']}: ERROR {r['error']}")
        else:
            flag = "  [受底噪限制 — 如实记录]" if r["floor_limited"] else ""
            print(
                f"{r['run']}: playback {r['playback_s']}s | "
                f"pre {r['pre_db']}dB post {r['post_db']}dB floor {r['floor_db']}dB ref {r['ref_db']}dB | "
                f"ERLE {r['erle_db']}dB (post-floor margin {r['post_vs_floor_db']}dB){flag}"
            )
    valid = [r["erle_db"] for r in results if "error" not in r]
    if valid:
        valid.sort()
        mid = valid[len(valid) // 2]
        print(f"\nmedian ERLE across {len(valid)} run(s): {mid} dB")
        if any(r.get("floor_limited") for r in results):
            print("NOTE: at least one run is floor-limited; the number is a LOWER bound of that run")
    meta_note = False
    for r in results:
        p = os.path.join(r.get("run", ""), "meta.json")
        if os.path.exists(p):
            m = json.load(open(p))
            lines = m.get("lock_lines_after") or []
            if not any("locked=true" in l for l in lines):
                print(f"WARNING: {r['run']}: dealias not observed locked after playback — "
                      "reference may have been raw ch8; run is suspect")
                meta_note = True
    return 0


if __name__ == "__main__":
    sys.exit(main())
