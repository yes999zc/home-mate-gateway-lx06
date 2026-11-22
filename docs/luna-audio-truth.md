# Luna audio-truth candidate

Date: 2026-11-17. Source baseline: `bbdfb3ceba3bf3bd8c8fa8076685bdad9b77844d`.
This is an isolated source/test candidate, not a release, deployment or acoustic acceptance record. No device, credentials, private recording or per-unit calibration was accessed or changed. A6 telemetry backpressure, A7 owner leases, effective acoustic configuration application and protocol-2 provenance remain outside this batch.

Current PR/CI/merge status is recorded in [Core Luna handoff](https://github.com/yes999zc/home-mate-b/blob/main/docs/luna/HANDOFF-NEXT.md); local validation below is dated environment evidence and must not be mistaken for the last hosted CI result. Source merge never implies device deployment.

## Implemented behavior

| Audit | Change | Behavior verification |
|---|---|---|
| A2 | A zero reference is per-block Degraded unless it matches DAC history silence; keep the internal relock cursor without claiming current verification. | Actual Dealias tests: nonzero history → zero gap → immediate relock; verified DAC silence remains Locked. |
| A3 | Actual KWS LevelSource returns raw post-AEC dBFS, like passive presence. Remove the Gateway correction helper; retain old profile offset as compatibility metadata. Core already adds its own per-unit offset. | Unit frame/source-window test with nonzero stored profile offset; real-worker sidecar contract asserts raw RMS while stored offset is +6 dB. Whole cross-Core/nonzero-offset election acceptance remains pending. |
| A4 | Keep AEC correlation detector alive when KWS is enabled even if semantic barge-in is off. Feed its rings without arming semantic voice onset. Missing AEC/correlation is unverified during playback; idle KWS remains available. | Actual detector positive/negative tests; immediate-hit publication barrier; real-worker fake Hardware/sidecar positive healthy independent KWS, zeroed-reference negative and idle AEC-off positive contracts. No full-duplex feature is enabled. |
| A5 | Production Hello uses configured effective enabled/tail, with None when AEC is disabled. | 2560/10240-sample and disabled cases; production firmware handshake contract. |
| A8 | Honor signed ALSA transfer result, reject excessive counts, report recovered overruns as discontinuities. Complete checked short reads in a bounded assembler; discard partial buffers on failure. Publish heartbeat/DSP/KWS only for a complete block; invalidate reference alignment and health on error. | Actual result decoder and assembler tests; real capture worker partial-read/overrun/recovery contract. No claim about fault frequency on the vendor driver. |
| A9 | Classify possible speech against conservative prior before updating floor; require three quiet acquisition blocks. Potential startup speech cannot permanently bootstrap its own floor. | Startup -45 dBFS synthetic signal, quiet acquisition and noise-step tests. Unacquired floor is provisional internally and emits **no passive presence**; KWS still works. Speech-corpus quality and initial passive availability remain pending rather than a false calibrated claim. |

The production capture worker publishes current health/correlation before feeding the same PCM to asynchronous KWS. A sidecar callback cannot consult the previous block's Healthy state for a newly fed zero-reference block. Tests deliberately call an immediate-hit consumer at this boundary.

## Wire compatibility

Actual `PROTOCOL_VERSION=0`, `PROTOCOL_MIN_VERSION=0` remain unchanged. The earlier “v1 mechanism” name describes the interval-negotiation implementation; it is not evidence that the paired fleet wire already runs protocol 1. This batch adds no serialized fields or frame types. It fixes existing advertised semantics: Keyword RMS is raw; Hello optional tail is effective. Core continues decoding the existing optional field. A future required `ConfigApply`/control-lease/provenance contract needs its own explicitly paired protocol bump and tests; this candidate does not claim those features.

## Validation record

Commands use `CARGO_TARGET_DIR=/tmp/home-mate-luna/gateway-target` and the repository lockfile, offline:

- `cargo test --workspace --lib --locked --offline`: **167 passed, 0 failed** in the final scoped run.
- `cargo test --workspace --all-targets --no-run --locked --offline`: all targets, including new real-worker contracts, compile.
- `cargo clippy --workspace --all-targets --no-deps --locked --offline`, `cargo fmt --all --check`, `python3 tools/line_gate.py`: all pass in the final scoped run; clippy has no warnings, line gate checks 53 Rust files (largest `role.rs` 1454/1500 lines).
- All-target runtime reaches 8 protocol contract tests, which fail binding loopback with `PermissionDenied: Operation not permitted` in this workspace. A separate firmware-contract runtime run before the final added guard cases had 2 harness no-op passes and 46 failures: 44 directly fail local binds; 2 watchdog parent assertions see the child exit 101 before its expected audio-fault behavior because the child also needs the same listener. These are **environment-blocked**, not passing acceptance. New real-worker cases are compiled but unexecuted here.

Independent review and CI on a runner permitted to bind local TCP are required before merging or closing audit items. After review, run the full workspace/firmware contracts on that runner; a green host-module suite alone does not establish Core/Gateway end-to-end behavior. Paired Core nonzero-offset behavior, hardware stop latency, true capture fault recovery and speech/noise corpora remain acceptance work. Owner-authorized canary-first deployment gates still apply; no new permission request is implied by this document.
