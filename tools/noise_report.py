# Tail-pop forensic analysis (full-duplex-plan §8 step 1).
#
# Inputs: run_dir with pre.pcm (pre-AEC 7-mic acoustic), ref.pcm (dealias
# reference / raw ch8 = what the DAC received), both 16 kHz S16 mono and
# time-aligned; meta.json with the wall-clock stimulus schedule.
#
# For every scheduled playback we locate the true signal end in ref (last
# sample above threshold), then examine pre in [end, end+1s]:
#   - level profile in 20 ms slices vs the pre-play room baseline
#   - the loudest 100 ms burst: spectrum -> broadband vs low-frequency
#     thump vs periodic classification
#   - ref itself in the same window: a pop present in ref means the
#     artifact is DIGITAL (codec/PGA saw it); present only in pre means
#     it originates past the DAC data, i.e. analog/amp/power.
# The zeros playback is invisible in ref by construction (all-zero data);
# its window is derived from the schedule timing instead.
# Usage: noise_report.py <run_dir>
import json
import sys

import numpy as np

SR = 16000


def load(path):
    return np.fromfile(path, dtype="<i2").astype(np.float64) / 32768.0


def db(x):
    return 20 * np.log10(max(np.sqrt(np.mean(x**2)), 1e-10))


def events_from_ref(ref, thresh_db=-60, min_gap_ms=1500, min_len_ms=100):
    """Nonzero runs in ref = actual DAC output windows."""
    active = np.abs(ref) > 10 ** (thresh_db / 20)
    # close short holes (fade zeros inside a tone are shorter than 50 ms)
    idx = np.where(active)[0]
    if len(idx) == 0:
        return []
    runs = []
    start = idx[0]
    prev = idx[0]
    hole = int(SR * 0.05)
    gap = int(SR * min_gap_ms / 1000)
    for i in idx[1:]:
        if i - prev > hole and i - start < gap:
            pass
        if i - prev > gap:
            runs.append((start, prev))
            start = i
        prev = i
    runs.append((start, prev))
    return [(s, e) for s, e in runs if (e - s) >= int(SR * min_len_ms / 1000)]


def classify(burst):
    """Shape verdict for the loudest 100 ms of the post-end excess."""
    n = len(burst)
    spec = np.abs(np.fft.rfft(burst * np.hanning(n)))
    freqs = np.fft.rfftfreq(n, 1 / SR)
    spec = spec / (spec.max() + 1e-12)
    lf = spec[(freqs > 0) & (freqs < 300)].mean()
    mid = spec[(freqs >= 300) & (freqs < 2000)].mean()
    hf = spec[(freqs >= 2000) & (freqs <= 7000)].mean()
    # periodicity: normalized autocorrelation peak in 2..20 ms lags
    b = burst - burst.mean()
    ac = np.correlate(b, b, "full")[n - 1:]
    ac /= ac[0] + 1e-12
    lags = np.arange(2, int(0.02 * SR))
    peak = ac[lags].max() if len(lags) < len(ac) else 0
    kind = (
        "low-frequency thump" if lf > max(mid, hf) * 3
        else "broadband burst" if hf > 0.1 and mid > 0.1
        else "midband click"
    )
    if peak > 0.5:
        kind += " (periodic component)"
    return kind, dict(lf=round(float(lf), 3), mid=round(float(mid), 3),
                      hf=round(float(hf), 3), autocorr=round(float(peak), 3))


def analyze(run_dir):
    pre = load(f"{run_dir}/pre.pcm")
    ref = load(f"{run_dir}/ref.pcm")
    meta = json.load(open(f"{run_dir}/meta.json"))
    t0 = meta["t0_wall"]
    print(f"tracks: {len(pre)/SR:.1f}s  schedule events:")
    runs = events_from_ref(ref)
    ref_events = []
    for s, e in runs:
        ref_events.append((s, e))
        print(f"  ref event {s/SR:7.2f}s..{e/SR:7.2f}s  peak {db(ref[s:e+1]):6.1f} dBFS")

    # baseline: quietest 2s window in the whole capture
    win = 2 * SR
    rms = np.convolve(ref**2 + pre**2, np.ones(win) / win, "valid")
    q = int(np.argmin(rms))
    base_db = db(pre[q:q + win])
    print(f"room baseline (quietest 2s, pre track): {base_db:.1f} dBFS")

    # Map schedule items to ref runs BY ORDER + one global skew: the measure
    # tracks start at the firmware's north-link-up, which sits ~45-50 s
    # before the driver's t0 — per-event absolute matching failed exactly
    # that way on run1. Tones map to the three short runs in order, speech
    # to the long one; zeros (invisible in ref) rides the same skew.
    a0 = meta["events"][0]["at_s"]
    short_runs = [r for r in runs if (r[1] - r[0]) / SR < 2]
    long_runs = [r for r in runs if (r[1] - r[0]) / SR > 5]
    if len(short_runs) < 3 or not long_runs:
        print(f"!! schedule/ref shape mismatch: {len(short_runs)} short, {len(long_runs)} long runs")
        return
    skew = short_runs[0][0] / SR - (meta["events"][0]["at_s"] - a0)
    print(f"time skew (track - schedule): {skew:.2f}s")
    for i, ev in enumerate(meta["events"]):
        label, at = ev["label"], ev["at_s"]
        dur = ev.get("dur_ms", 0) / 1000
        if label == "tone":
            s, e = short_runs[i]
            est_s, est_e = s / SR, e / SR
            src = "ref"
        elif label == "speech":
            s, e = long_runs[0]
            est_s, est_e = s / SR, e / SR
            src = "ref"
        else:  # zeros: no ref signature by construction, ride the skew
            est_s, est_e = at - a0 + skew, at - a0 + skew + dur
            src = "schedule+skew"
        end = int(est_e * SR)
        print(f"\n== {label} (end ~{est_e:.2f}s, located by {src}) ==")
        # ref after end: is there any digital artifact?
        w_ref = ref[end + SR // 50: end + SR]  # skip 20 ms, take 1 s
        ref_peak = 20 * np.log10(np.abs(w_ref).max() + 1e-10)
        # pre after end in 20 ms slices (skip first 20 ms = air path)
        w_pre = pre[end + SR // 50: end + SR]
        n = len(w_pre) // (SR // 50)
        slices = [db(w_pre[i * SR // 50:(i + 1) * SR // 50]) for i in range(n)]
        above = [(i * 20, v) for i, v in enumerate(slices) if v > base_db + 6]
        peak_i = int(np.argmax(slices))
        print(f"  ref post-end 1s peak: {ref_peak:6.1f} dBFS")
        print(f"  pre post-end slices: first={slices[0]:6.1f} peak={max(slices):6.1f}"
              f" @+{peak_i*20}ms  last={slices[-1]:6.1f} dBFS")
        if above:
            span = above[0][0], above[-1][0] + 20
            burst = w_pre[peak_i * SR // 50: peak_i * SR // 50 + SR // 10]
            kind, m = classify(burst)
            print(f"  EXCESS above baseline+6dB: +{span[0]}..+{span[1]}ms -> {kind}")
            print(f"     {m}")
        else:
            print("  no excess above baseline+6dB in pre post-end")


if __name__ == "__main__":
    analyze(sys.argv[1])
