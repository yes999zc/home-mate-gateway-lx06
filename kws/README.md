# hm-kws — streaming keyword-spotter sidecar (D/T4)

A single-purpose process that turns the firmware's post-AEC audio into keyword
events. It exists because the sherpa-onnx release CLIs only decode WAV files,
while the firmware owns the audio device — so live spotting needs something
that accepts PCM on a pipe.

```
firmware capture thread
  └─ 20 ms slices of the post-AEC mono block ──stdin──▶ hm-kws
                                                         │ sherpa-onnx
                                                         │ streaming zipformer
  ┌──────────── one JSON line per event ◀────stdout──────┘
  └─ GatewayFrame::Keyword (fact) ──north──▶ core decides whether to stop
```

The gateway reports; the core decides — for every word EXCEPT the deployed
retreat set. Those cut the audio LOCALLY first (contract v0.5 local exit, the
only local audio action the KWS has; physical mute, volume and link teardown
are separate local behaviors that never went through the spotter): hit → both
channels flushed (≤300 ms from the hit, measured at the hardware flush), the
`Keyword` fact and the `LocalExitFlushed` fact travel north as each is ready
(the contract test accepts either order), and the SESSION itself is still
closed only by the core. A false WAKE-word positive therefore never stops or
starts anything on its own; a false retreat-word positive costs one silence,
never a semantics execution.

## Build

```bash
# 1) official runtime (shared CPU build, aarch64, glibc)
gh release download v1.12.15 --repo k2-fsa/sherpa-onnx \
  --pattern 'sherpa-onnx-v1.12.15-linux-aarch64-shared-cpu.tar.bz2'
tar xjf sherpa-onnx-v1.12.15-linux-aarch64-shared-cpu.tar.bz2

# 2) driver (zig cc is the project's cross toolchain)
SHERPA_PKG=./sherpa-onnx-v1.12.15-linux-aarch64-shared-cpu ./build.sh
```

Output `hm-kws` is a ~31 KB aarch64-linux-gnu ELF. The shipped libraries only
require GLIBC_2.17; emOS has no `/lib/ld-linux-aarch64.so.1`, so it runs under
the bundled Debian loader (same trick as the KWS spike, 2026-11-10).

## Runtime layout on the device

```
/data/local/tmp/hm-kws/
  hm-kws  ld-linux-aarch64.so.1  libc.so.6  libm.so.6  libpthread.so.0
  libdl.so.2  librt.so.1  libgcc_s.so.1  libstdc++.so.6
  libsherpa-onnx-c-api.so  libonnxruntime.so
  model/{encoder…chunk-16…int8.onnx, decoder…onnx, joiner…int8.onnx, tokens.txt}
  kw.txt      # n ǐ h ǎo g āo d á @你好高达
  run.sh      # exec loader --library-path . ./hm-kws … (see deploy/dot2d/kws)
```

~25 MB on /data. The firmware spawns `run.sh` with `KWS_CPU` (default 3) and
supervises it: 1/2/5/10/30 s backoff, a >60 s healthy run resets the schedule.
Every state change goes north as `kws_report`, so "keyword interruption is
unavailable" is a fact the console can show, never a silent assumption.

## Interface

stdin: raw S16LE mono 16 kHz, any block size (the firmware feeds 20 ms slices —
feeding at the 80 ms uplink cadence would add up to 80 ms of dead time to every
interruption).

stdout, one JSON object per line:

| event | fields |
|---|---|
| `ready` | `load_ms`, `threshold`, `score`, `trailing_blanks`, `threads`, `cpu` |
| `keyword` | `keyword`, `fed_samples`, `fed_ms`, `last_token_s`, `detect_ms`, `wall_ms`, `n` |
| `exit` | `fed_samples`, `detections`, `uptime_ms` |
| `error` | `stage` (`load`/`stream`) |

Exit codes: 0 = stdin EOF or SIGTERM, 2 = model/stream creation failure.

`fed_ms` is the stream position at detection (useful for diagnostics).
`last_token_s`/`detect_ms` come from the model's own token timestamps and are
**not** reliable on this stream (they report 0.84 s where the acoustic phrase
ends at 2.5 s) — do not use them for latency claims. Real latency is measured
off the firmware's aligned pre/post measurement tracks
(`tools/kws_latency_report.py`).
