# G6 barge-in reference track (acceptance §5): a FIXED loud music-like
# stimulus. Synthesized deterministically (no copyright material, fully
# reproducible from this file). Heavy-metal style: distorted power chords,
# palm-mute chugs, double kick, crash bursts — broadband loud content is
# the point (it must mask speech at 3m while AEC still lets the wake
# word through). Swappable for a user-picked track BEFORE the barge-in
# baseline is recorded; after that it is frozen like the ERLE stimulus.
# Usage: bargein_track_gen.py [out.wav]
#!/usr/bin/env python3
import hashlib
import math
import struct
import sys
import wave

RATE = 16000
BPM = 160.0
BEAT = 60.0 / BPM  # 0.375s
BAR = BEAT * 4


def clip(x: float, drive: float = 8.0) -> float:
    # Soft-clip "distortion amp": tanh-style saturation, hard at the rails.
    return math.tanh(x * drive) / math.tanh(drive)


def power_chord(t: float) -> float:
    # E1 + E2 + B2 (root + octave + fifth) as additive saws, harmonics all
    # the way to Nyquist — distorted guitars are broadband by themselves.
    out = 0.0
    for f in (41.2, 82.4, 123.5):
        hmax = int((RATE / 2 - 1) / f)
        norm = 0.0
        for h in range(1, hmax + 1):
            amp = 1.0 / h if f > 60 else 1.0 / (h * 1.4)
            out += amp * math.sin(2 * math.pi * f * h * t)
            norm += amp
    return out / norm


def kick(t: float, phase: float) -> float:
    # 50Hz sine with fast pitch drop + fast decay.
    dt = t - phase
    if dt < 0 or dt > 0.18:
        return 0.0
    f = 120.0 * math.exp(-dt * 28.0) + 45.0
    return math.sin(2 * math.pi * f * dt) * math.exp(-dt * 16.0)


def crash(t: float, phase: float) -> float:
    # Filtered-ish noise burst (xorshift PRNG, fixed seed -> deterministic).
    dt = t - phase
    if dt < 0 or dt > 0.35:
        return 0.0
    idx = int(dt * RATE)
    seed = 0x1234ABCD + idx * 2654435761
    seed = (seed ^ (seed >> 15)) & 0xFFFFFFFF
    n = (seed / 0xFFFFFFFF) * 2.0 - 1.0
    return n * math.exp(-dt * 9.0) * 1.1


def main() -> int:
    out = sys.argv[1] if len(sys.argv) > 1 else "bargein-track.wav"
    total = int(RATE * 30.0)
    samples = []
    # Structure: 4 bars chug verse (E), 2 bars half-time, repeat; crash on
    # each bar head; double kick sixteenths in bars 3-4 of each group.
    n_bars = int(30.0 / BAR)
    for i in range(total):
        t = i / RATE
        bar = int(t / BAR)
        bt = t - bar * BAR  # within-bar time
        beat_i = int(bt / BEAT)
        sig = 0.0
        # Chugs: sixteenth palm-mutes except beat heads ring (chord)
        sixteenth = bt / (BEAT / 4.0)
        on_chug = (int(sixteenth) % 2) == 0
        env = 1.0 if beat_i == 0 else (0.55 if on_chug else 0.12)
        sig += clip(power_chord(t) * 1.6) * env * 0.8
        # Double kick sixteenths in bars 3,4 of every 4; else four-on-floor
        if bar % 4 in (2, 3):
            for k in range(4):
                for s16 in range(4):
                    sig += kick(t, bar * BAR + k * BEAT + s16 * BEAT / 4.0) * 0.9
        else:
            for k in range(4):
                sig += kick(t, bar * BAR + k * BEAT) * 0.9
        # Crash each bar head; snare-ish offbeats; persistent ride eighths
        # (the hats are what make metal broadband — keep them up front).
        sig += crash(t, bar * BAR) * 1.0
        if bar % 4 in (1,) and beat_i in (1, 3):
            sig += crash(t, bar * BAR + beat_i * BEAT) * 0.5
        for k in range(8):
            sig += crash(t, bar * BAR + k * BEAT / 2.0) * 0.22
        samples.append(max(-1.0, min(1.0, sig)) * 0.92)

    pcm = struct.pack(f"<{len(samples)}h", *[int(s * 32767) for s in samples])
    with wave.open(out, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes(pcm)
    print(f"wrote {out}: {len(pcm)} bytes, 30s, 16kHz mono s16")
    print(f"SHA256(file) {hashlib.sha256(open(out, 'rb').read()).hexdigest()}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
