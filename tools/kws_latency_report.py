# T4 latency report: phrase end -> speaker silence, from the firmware's own
# dual-track measurement (aec-measure pre.pcm/post.pcm, written by the SAME
# capture loop iteration, so they are sample-aligned by construction).
#
#   pre.pcm   pre-AEC fused mono: our playback is LOUD here, the user's voice
#             rides on top. Playback end = the level collapsing.
#   post.pcm  post-AEC mono — exactly what the keyword spotter consumes. The
#             user's phrase stands above the residual echo here.
#
# One number per attempt: t_silence(pre) - t_phrase_end(post). This is the
# physical quantity the acceptance talks about ("speak the phrase, playback
# stops within 300 ms"), not a log timestamp comparison.
#
# Usage: kws_latency_report.py <dir-with-pre-post-pcm> [--play-db -35]
#        [--quiet-db -45] [--speech-margin 10] [--json out.json]
import json
import math
import os
import statistics
import sys

FRAME = 160  # 10 ms at 16 kHz
SR = 16000


def load(path):
    raw = open(path, "rb").read()
    n = len(raw) // 2
    return [int.from_bytes(raw[i * 2:i * 2 + 2], "little", signed=True) for i in range(n)]


def db_frames(samples):
    out = []
    for i in range(0, len(samples) - FRAME + 1, FRAME):
        chunk = samples[i:i + FRAME]
        acc = 0.0
        for s in chunk:
            f = s / 32768.0
            acc += f * f
        rms = math.sqrt(acc / len(chunk))
        out.append(20.0 * math.log10(rms) if rms > 1e-9 else -120.0)
    return out


def median(xs):
    return statistics.median(xs) if xs else -120.0


def smooth(db, win=5):
    """50 ms moving median: kills single-frame dips inside speech."""
    out = []
    for i in range(len(db)):
        lo = max(0, i - win // 2)
        out.append(statistics.median(db[lo:i + win // 2 + 1]))
    return out


def find_playback_segments(pre_db, play_db, quiet_db, min_s=1.5, bridge_s=0.6):
    """Playback runs in the pre-AEC track.

    Speech has pauses, so "one dip below the floor" is not an end: runs are
    bridged across gaps shorter than `bridge_s`, and only runs at least
    `min_s` long count. Returns [(start_frame, end_frame)] where end is the
    last frame still above the quiet threshold (i.e. when real silence
    starts).
    """
    db = smooth(pre_db)
    gap_frames = int(bridge_s * 100)
    min_frames = int(min_s * 100)
    segs = []
    i, n = 0, len(db)
    while i < n:
        if db[i] <= quiet_db:
            i += 1
            continue
        start = i
        last_loud = i
        j = i
        while j < n:
            if db[j] > quiet_db:
                last_loud = j
                j += 1
                continue
            # quiet stretch: bridge it if it is short and something loud follows
            k = j
            while k < n and db[k] <= quiet_db:
                k += 1
            if k - j <= gap_frames and k < n and db[k] > quiet_db:
                j = k
                continue
            break
        end = last_loud
        if end - start >= min_frames and max(db[start:end + 1]) > play_db:
            segs.append((start, end))
        i = max(j, end + 1)
    return segs


def phrase_end_before(post_db, seg_start, seg_end, peak_margin, lookback_s=3.0):
    """End of the spoken phrase that triggered the stop.

    Inside a playback segment the post-AEC track carries the AEC residual
    (which is NOT silence), so an absolute floor threshold cannot find the
    phrase. The phrase is what stands well above that residual: threshold on
    the segment's own peak minus `peak_margin`, and take the last frame
    above it inside the lookback window. Returns None when the segment never
    rises clearly above its own residual (nothing separable).
    """
    lo = max(seg_start, seg_end - int(lookback_s * 100))
    window = post_db[lo:seg_end + 1]
    if not window:
        return None
    peak = max(window)
    thr = peak - peak_margin
    last = None
    for k in range(lo, seg_end + 1):
        if post_db[k] >= thr:
            last = k
    if last is None or last <= lo:
        return None
    return last


def main() -> int:
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    root = args[0] if args else "/tmp/hm-t4/measure"
    play_db = float(sys.argv[sys.argv.index("--play-db") + 1]) if "--play-db" in sys.argv else -35.0
    quiet_db = float(sys.argv[sys.argv.index("--quiet-db") + 1]) if "--quiet-db" in sys.argv else -45.0
    margin = float(sys.argv[sys.argv.index("--peak-margin") + 1]) if "--peak-margin" in sys.argv else 12.0

    pre = db_frames(load(os.path.join(root, "pre.pcm")))
    post = db_frames(load(os.path.join(root, "post.pcm")))
    n = min(len(pre), len(post))
    pre, post = pre[:n], post[:n]
    floor = median(post)
    print(f"tracks: {n * 10 / 1000:.1f}s each, post noise floor {floor:.1f} dBFS, "
          f"pre max {max(pre):.1f}, post max {max(post):.1f}")

    segs = find_playback_segments(pre, play_db, quiet_db)
    print(f"playback segments detected: {len(segs)}")
    rows = []
    for (a, b) in segs:
        pe = phrase_end_before(post, a, b, margin)
        t_start, t_end = a * 10, b * 10
        if pe is None:
            rows.append((t_end, None, None))
            print(f"  play {t_start/1000:7.2f}-{t_end/1000:7.2f}s  phrase not separable")
            continue
        t_phrase = pe * 10
        lat = t_end - t_phrase
        rows.append((t_end, t_phrase, lat))
        print(f"  play {t_start/1000:7.2f}-{t_end/1000:7.2f}s  phrase_end={t_phrase/1000:7.2f}s  latency={lat}ms")

    lats = [r[2] for r in rows if r[2] is not None]
    if lats:
        lats_sorted = sorted(lats)
        p50 = statistics.median(lats_sorted)
        p95 = lats_sorted[min(len(lats_sorted) - 1, int(round(0.95 * (len(lats_sorted) - 1))))]
        print(f"\nlatency n={len(lats)}  P50={p50:.0f}ms  P95={p95:.0f}ms  "
              f"min={min(lats)}  max={max(lats)}")
        json_path = sys.argv[sys.argv.index("--json") + 1] if "--json" in sys.argv else None
        if json_path:
            json.dump({"floor_db": floor, "rows": rows, "p50_ms": p50, "p95_ms": p95},
                      open(json_path, "w"), indent=1)
            print(f"json -> {json_path}")
    else:
        print("no measurable attempts (phrase/drop not separable)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
