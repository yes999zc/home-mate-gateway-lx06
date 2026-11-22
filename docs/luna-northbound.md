# Luna bounded northbound candidate

Date: 2026-11-17. Gateway source baseline: `0e5f32e`. Work branch:
`codex/luna-northbound`. This source/test candidate changes the production
`Firmware.run` path. It is not a deployment, Dot2 performance measurement,
physical acoustic result or Luna milestone closure. No dependencies, devices,
credentials or unit acoustic parameters changed.

The paired design and acceptance authority is Core
[the A6 task card](https://github.com/yes999zc/home-mate-b/blob/codex/luna-northbound/docs/luna/tasks/L3-BOUNDED-NORTHBOUND.md),
tracking Core #115. Core owns the public schema, API and diagnostic UI; this
repository does not import Core implementation types or private configuration.

## Production transport

`firmware/northbound.rs` owns the actual `NorthTx`, encoded queues, first-fault
latch, complete-frame writer deadline and process-lifetime counters.
`north_link.rs` owns the production handshake, serving selects and joined
writer/socket cleanup. All direct command receipts, capture/KWS publication,
playback facts and supersession use this admission boundary. Hello and
pre-handshake refusal use the same bounded writer directly.

| Lane | Retained bound | Full/replacement policy |
|---|---|---|
| Critical: every runtime variant except the three below | 64 frames and 262144 encoded bytes, both inclusive of an in-flight critical frame and newline | Fail the connection; never silently drop a terminal fact |
| EchoHealth | One pending local-device frame, 4096 encoded bytes | Replace pending latest state; preserve source fields |
| NearEndPresence | One separate pending local-device frame, 4096 encoded bytes | Replace pending latest state; preserve source fields |
| AudioChunk | 25 queued frames, each at most8192 encoded bytes | Drop the new audio frame and increment drop count |

Audio supports the existing16kHz, s16le mono/mic_ref production envelope:
1280 frames ×2 channels ×2 bytes =5120 raw bytes =6828 base64 bytes; the
8192-byte envelope leaves1364 bytes for escaped identity and fixed metadata.
No capture framing, KWS eight-block queue or acoustic thresholds change.

Up to eight critical frames are followed by one health, one presence and one
audio frame, skipping empty lanes. A partially written frame cannot be
replaced. The maximum encoded **retained transport payload** is483328 bytes:
262144 critical +8192 pending telemetry +204800 queued audio +8192 single
noncritical in-flight allowance. Critical capacity slots are reserved before serialization and retained through
queueing/writing, so reserved encoders +queued +in-flight critical work together
never exceed64. This is a capacity reservation, not a delivered or successfully
encoded admission. `NorthTx` returns Admitted only after encoding and insertion;
FIFO order is completed encoded-queue admission order. Concurrent encoders may
finish in a different order from their reservation starts. An oversized/failed/unwinding encoder releases its reservation;
producer panic latches encode_failed rather than silently losing the fact.
The capped sink bounds encoded lengths at262144 critical,4096 telemetry and8192
audio. Vec allocation capacity is separately bounded/tested on the pinned
compiler at max(4096,2×encoded length), at most twice its lane cap. A conservative
retained Vec-capacity bound is1228800 bytes (2×critical byte cap +4096×64 count,
plus two8192-byte telemetry capacities,25×16384-byte audio capacities and one
16384-byte noncritical in-flight capacity). This is intentionally looser than
the483328-byte encoded-payload ceiling.

Transient critical encoding allocations are at most64×524288 =33554432 bytes;
these share the64 count permits with retained critical work, so these separate
worst cases need not be simultaneously attainable. The single capture producer
can additionally hold one noncritical encoder (at most16384 allocation bytes).
Original frame/string ownership, allocator metadata, decoder/playback PCM and
OS TCP buffers are separate;256KiB is not a claim about total RSS. The ordinary
critical producer classes are current-thread runtime, capture, decoder, KWS
manager and KWS reader; existing Wi-Fi change workers are dynamic, which is why
the encoder count is bounded by transport admission rather than a claimed fixed
thread count. No transport payload is cloned into a second encoded staging
queue. Producer calls do not await a queue slot or socket.
Short metadata locks do not span serialization, network/file I/O or waits.
Host producer timing includes successful, coalesced, dropped and failed calls;
it is not a hard real-time guarantee.

Each complete frame including its newline has a fixed1000ms monotonic deadline,
including partial writes; trickling bytes cannot extend it. Idle writers do not
time out. The existing dial, HelloAck and reconnect1–30s/protocol refusal pacing
remain. A successful handshake is recorded separately from clean EOF/backoff
state. The first terminal fault closes admission, releases pending envelopes,
wakes both reader and nested decode-capacity selects, cancels/joins the writer
and retires the connection. The old cloned handle remains closed after
reconnect. Successful TCP bytes are not evidence of Core consumption.

Internal playback identities contain original link and channel generations;
they are never derived from task IDs. Decoder publication revalidates the
original identity after CPU work, with original `NorthTx` for receipts. Runtime
admission only clones/removes immutable stream handles; conversion/resampling
uses decoder-owned state outside `Shared` and the stream-map lock. PCM/EOS/flush
commands and hardware completion events are stamped. Old cleanup can clear its
old local PCM, but cannot flush/settle a fresh stamped stream with a reused task
ID. Hardware calls stay outside `Shared`; a hardware operation already admitted
can finish locally after link loss. This does not implement owner leases or the
full typed receipt-stage work tracked in Core #118.

## Optional public diagnostics

Wire version/minimum stay0/0. `DeviceStatus.northbound` is optional JSON with
`schema:1`; missing legacy reports and unknown schemas still decode. The typed
schema lives in `protocol/northbound.rs`. The exact public fixture is
[fixtures/northbound-status-v1.json](fixtures/northbound-status-v1.json), mirrored
from Core `docs/fixtures/northbound-status-v1.json`, SHA256
`820efbe8bb6e6183a4d8d0584db47a359226d2edf378c7b5d98df6ebc6522ca2`.
Core `docs/contracts-v0.md` is authoritative. It grants no provenance, lease,
configuration-application or protocol2 capability.

`critical_current_count`/high-water count include reserved encoders +queued +in-flight;
critical byte counters count encoded retained envelopes (queued/in-flight),
not transient serialization buffers. The byte ceiling is independent of count.
Production snapshots attach to existing boot/ordinary DeviceStatus, without a
periodic critical telemetry stream. Counters survive reconnect for this process
boot. Pending-age high-water is recorded at dequeue, completion, replacement
and failure before timestamps disappear, independently of observation. Snapshot
uptime and fault fields are cloned under one lock to preserve timestamp order.

## Isolated executable fixture

Build with existing locked dependencies:

```sh
CARGO_TARGET_DIR=/tmp/luna-northbound-gateway-target cargo build --locked \
  -p home-mate-device-gateway --example northbound_fixture
/tmp/luna-northbound-gateway-target/debug/examples/northbound_fixture \
  --core 127.0.0.1:8932 --device fixture-0 \
  --state-dir /tmp/unique-fixture-state --probe-file /tmp/unique-fixture.jsonl
```

This calls actual `Firmware.run` with bounded FakeHardware counters. Any fixture
ID uses an explicit default profile; no physical unit profile is copied. Capture
is paced80ms by default; playback is paced by frames/48kHz. `--capture-pace-ms`
1..80 is pressure-only acceleration; normal600s evidence uses80. Optional
`--duration-secs`1..1800 is a child watchdog. Probe paths must not already exist.
Accepted stdin is exactly `button action`, `button mute_toggle`,
`button volume_up`, `button volume_down`, or `shutdown`. Buttons enter real
`ButtonEvent` handling through a bounded64-item fixture channel; rejected input
is counted. Shutdown flushes a final observation then exits the isolated child;
it does not prove cooperative Firmware/hardware thread teardown.

JSONL observations are appended once per second and at shutdown. Bounded fields:
`elapsed_ms`, `capture_calls`, `playback_calls`, `nonzero_playback_calls`,
`written_frames`, `flushes`, `capture_max_gap_us`, `playback_max_gap_us`,
`button_input_dropped`, raw schema1 `northbound`, `producer_max_us`,
`pending_health_count/bytes`, `pending_presence_count/bytes`, `inflight_bytes`,
`producer_calls`, `last_progress_uptime_ms`, `reader_tasks`, `writer_tasks`,
`decode_active`, `decode_pending`, `decode_waiters`, `handshakes`,
`handshake_completed`. No PCM/text history is retained. All timing below is
monotonic microseconds: `write_started_us`, `write_deadline_us`,
`last_fault_write_started_us`, `last_fault_write_deadline_us`, `last_fault_us`,
`last_cleanup_us`; matching `last_fault_generation`/`last_cleanup_generation`
identify the observation. Cleanup means socket/writer/queue retirement and
queued audio-generation cleanup, not physical completion of a blocked DAC call.

For an old-head baseline build, copy the same example into an isolated
`0e5f32e` worktree and remove only the two explicitly marked OPTIONAL NORTHBOUND
OBSERVER/SNAPSHOT regions. Hardware, pacing, counters, input and process exit
stay identical. Core's external orchestration uses actual management APIs and
real Core processes; it does not replace scheduler semantics or synthesize tool
results. The long-run report records executable/source hashes and host limits.

## Local verification checkpoint

`CARGO_TARGET_DIR=/tmp/luna-northbound-gateway-target cargo test --workspace
--all-targets --locked -- --test-threads=1` passed184 library,8 protocol and54
actual Firmware contract tests in the sequential run before the final diagnostic-
generation fence. The final fence and added late-retired-reservation test pass
all14 scoped transport tests, with no ignored tests. New tests cover exact
count/byte/newline boundaries inclusive in-flight; exhaustive lane mapping;
telemetry latest-state slots; audio25/drop; critical fairness; capped encoding
and failure; fixed deadline under no progress/trickle; idle writer; write zero,
error and panic; no-observer pending-age high-water; closed stale senders;
production reader/decode-full fatal selects; optional legacy/unknown schema;
and same-task-ID stale decoder/PCM/end/flush/terminal negatives with a current
execution positive control. Actual Firmware/socket tests additionally park an
old one-period DAC write, reconnect and queue a fresh same-ID two-period play:
no fresh terminal arrives until fresh writes are released, then only its85ms
completion is reported. A bounded-counter actual socket pressure case stops
peer reads under1ms accelerated capture, starts its clock at the actual pending
write, verifies1000ms decision plus100ms scheduling tolerance, then≤1000ms
fatal-to-retirement and fresh Hello. This pressure case is separate from paced
600-second acceptance. Further scoped tests cover64 paused pre-encode
reservations, unwinding/oversize/serialization failure release, first-cleanup
clock preservation across redials, late retired encoder release preserving
fresh-generation current counters, allocator capacity, and actual normal-
handshake Firmware ModeSet bursts independently exhausting critical count/bytes
before a writer timeout and reconnecting. Whole-file decode also checks retired
work before expensive conversion, then revalidates before publication. Existing
Firmware playback/capture/watchdog and wire assertions remain intact. Final
format/clippy gates pass locally; paired stress,
compatibility, independent audit and hosted exact-head CI are recorded by the
lead before any closure. Long-run evidence and physical release remain open at
this checkpoint.

## Reproducible scoped mutation controls

`python3 tools/luna_northbound_mutants.py --out /tmp/unique-mutants` copies the
actual working source, records its tracked/untracked file hashes, then runs each
positive test before mutating only its matching production gate. Every mutation
is restored before the next test. Reports retain exact command, patch/hash,
source manifest and stdout; a compiler error does not count as success.

The eight mutations disable the critical count guard, critical byte guard,
extend the1000ms writer deadline to60000ms, remove fatal observation from each
targeted serving select, force always-critical biased scheduling, remove the
old StreamDone identity check, and extend only the actual socket write timeout
to60000ms while its declared diagnostic deadline remains1000ms. Each serving select has two valid observations: direct
fatal latch and writer JoinHandle (the production writer also exits on fatal).
Thus a select mutation removes BOTH of that select's observers, leaving the
other select intact. The matching real-socket production serve test must fail
its100ms assertion. The count/byte mutations run actual `Firmware.run` normal-
handshake command-burst tests. These are specific guard-removal negative
controls; they do not reproduce the entire former unbounded multi-lane runtime.
The exact-source copied-control report is
`/tmp/luna-northbound-mutants-exact-final/report.json`, full copied-source
manifest SHA256 `46232fcea93eff5a5315269da4d273a793099bd15b7cf3797add5af042f69b96`.
All eight unmodified positives exited0; all eight mutants exited101 at matching
behavior assertions. Count/byte removal observed168 envelopes/270516 encoded
bytes. Biased scheduling failed the expected telemetry turn; removed StreamDone
identity prematurely finished the fresh same-ID task; actual socket timeout
removal missed its observed write deadline. The helper deadline and outer/
nested observation removals each missed their bounded assertion clocks. These
observed values are host results, not production thresholds. Patches and logs
are retained alongside the report. This is specific guard removal, not
reproduction of every old unbounded runtime path.

Final local fixture binary SHA256 is
`57e27303066f926c56bd8205cbf78263a4a6c114fa898553195c361d7a6919a3`;
Rust/Cargo source manifest SHA256 is
`7b5f2d213d8314d5be01b19cfb53def5ba470026fd71d260450f393fa8971b42`
at `/tmp/luna-gateway-northbound-manifest.json`. Every Rust/Cargo hash matches
the mutation copy's input. No build/test process remains running at this
checkpoint. Paired600s reports and independent acceptance remain lead-owned
open artifacts. No commit, release or deployment is implied.
