//! Fakes and helpers shared by the firmware contract tests: the fake DAC
//! (`FakeHardware`), the fake core (`FakeCore`), the firmware spawner and the
//! WAV/PCM builders.
//!
//! Everything is `pub(crate)`: the theme modules are this module's siblings,
//! not its children, so a private item would not compile there.

use home_mate_device_gateway::firmware::{ButtonEvent, Firmware, FirmwareConfig, Hardware};
use home_mate_device_gateway::protocol::{AudioMeta, CoreFrame, GatewayFrame};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use base64::Engine as _;
use home_mate_device_gateway::protocol::CoreFrame as CF;

// --- fake hardware ---------------------------------------------------------------

#[derive(Default)]
pub(crate) struct Probes {
    pub(crate) leds: Vec<[(u8, u8, u8); 12]>,
    pub(crate) written_frames: u64,
    pub(crate) audio_write_started: u64,
    pub(crate) flushed: u64,
    pub(crate) flush_started: u64,
    /// Every call into `playback_flush`, including the ones the hardware
    /// fails (`fail_flush` returns before `flushed` moves). The failed-flush
    /// contract needs a fact for "the stop reached the hardware", and
    /// "flushed" is not it.
    pub(crate) flush_attempts: u64,
    pub(crate) drained: u64,
    pub(crate) wake_locks: u64,
    pub(crate) adc_mutes: Vec<bool>,
    pub(crate) first_samples: Vec<i16>, // first sample of every period (gain evidence)
    pub(crate) time_sets: Vec<u64>,     // clock projections received (G6 #2)
    pub(crate) all_samples: Vec<i16>,   // every left-channel sample written (v0.3 equivalence)
}

pub(crate) type CapturePlan = Arc<Mutex<std::collections::VecDeque<(Result<u32, i32>, i16)>>>;

pub(crate) struct FakeHardware {
    pub(crate) probes: Arc<Mutex<Probes>>,
    /// Short transport tests retain counters/last LED only, never raw PCM.
    pub(crate) bounded_probes: bool,
    pub(crate) buttons_rx: Mutex<std::sync::mpsc::Receiver<ButtonEvent>>,
    #[allow(dead_code)]
    pub(crate) volume: Arc<AtomicU64>,
    /// When Some, playback_write parks until a message arrives on the
    /// channel — the v0.3 overflow test needs playback progress frozen.
    pub(crate) write_gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    /// R0: when Some, playback_flush parks until a message arrives — the
    /// stop-ordering test holds the flush open and proves the receipt does
    /// not leave before the hardware fact exists.
    pub(crate) flush_gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    /// 2026-11-13: fail any write that carries real audio (a nonzero sample).
    /// Silence-clock periods (all zeros) keep working, so the firmware starts
    /// normally and only the data path dies — the Stop-after-write-failure
    /// contract needs exactly that.
    pub(crate) fail_audio_writes: bool,
    /// M4: park (block) a write that carries real audio, leaving the silence
    /// clock flowing. The first-write blind-spot test needs the play's very
    /// first period to be the write that wedges; a gate on EVERY write would
    /// park the idle clock first and never reach the play.
    pub(crate) park_audio_writes: bool,
    /// M6a: what the DAC "still holds" when asked (0 = fully consumed).
    /// `None` = the platform cannot report it (progress_basis "written").
    pub(crate) delay_frames: AtomicU64,
    pub(crate) report_delay: bool,
    /// M6a: make the hardware flush fail (the channel may still be audible).
    pub(crate) fail_flush: bool,
    /// #19: panic inside `playback_write` to verify playback thread panic
    /// triggers `AudioFault` and exit 75.
    pub(crate) panic_playback: bool,
    /// Capture pacing in ms (default 80 ms real-time; tests can lower it to
    /// saturate TCP backpressure quickly).
    pub(crate) capture_pace_ms: u64,
    /// Opt-in real DAC period pacing. Whole-file playback otherwise drains
    /// faster than capture, which is useful for unrelated receipt tests.
    pub(crate) pace_playback: bool,
    pub(crate) capture_plan: CapturePlan,
    /// 0=existing pattern, 1=verified DAC history + independent sine mic,
    /// 2=zeroed reference while that DAC continues playing.
    pub(crate) capture_reference_mode: Arc<AtomicU64>,
    /// Optional 16 kHz ch8 loopback queue fed by `playback_write` (every 3rd
    /// sample) and cleared by `playback_flush` so AEC/dealias contract tests
    /// see a real DAC->ch8 path.
    pub(crate) loopback_ch8: Option<Arc<Mutex<std::collections::VecDeque<i16>>>>,
}

impl FakeHardware {
    pub(crate) fn new(probes: Arc<Mutex<Probes>>) -> (Self, std::sync::mpsc::Sender<ButtonEvent>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (
            FakeHardware {
                probes,
                bounded_probes: false,
                buttons_rx: Mutex::new(rx),
                volume: Arc::new(AtomicU64::new(100)),
                write_gate: Mutex::new(None),
                flush_gate: Mutex::new(None),
                fail_audio_writes: false,
                park_audio_writes: false,
                delay_frames: AtomicU64::new(0),
                report_delay: true,
                fail_flush: false,
                panic_playback: false,
                capture_pace_ms: 80,
                pace_playback: false,
                capture_plan: Arc::new(Mutex::new(Default::default())),
                capture_reference_mode: Arc::new(AtomicU64::new(0)),
                loopback_ch8: None,
            },
            tx,
        )
    }
}

impl Hardware for FakeHardware {
    fn capture_read(&self, buf: &mut [u8], frames: u32) -> io::Result<u32> {
        let scripted = self.capture_plan.lock().unwrap().pop_front();
        if let Some((count, sample)) = scripted {
            let count = count.map_err(io::Error::from_raw_os_error)?;
            let bytes = ((sample as i32) << 4).to_le_bytes();
            for frame in buf.as_chunks_mut::<27>().0.iter_mut().take(count as usize) {
                for channel in frame.as_chunks_mut::<3>().0 {
                    channel.copy_from_slice(&bytes[..3]);
                }
            }
            std::thread::sleep(Duration::from_millis(5));
            return Ok(count.min(frames));
        }
        let mode = self
            .capture_reference_mode
            .load(std::sync::atomic::Ordering::Relaxed);
        if mode != 0 {
            let history = self.probes.lock().unwrap().all_samples.clone();
            let span = frames as usize * 3 + 192;
            let start = history.len().saturating_sub(span) + 64;
            for f in 0..frames as usize {
                let mic =
                    (3000.0 * (f as f32 * 440.0 * std::f32::consts::TAU / 16000.0).sin()) as i32;
                for c in 0..9 {
                    let value = if c == 8 {
                        if mode == 1 {
                            (history.get(start + f * 3).copied().unwrap_or(0) as i32) << 8
                        } else {
                            0
                        }
                    } else {
                        mic << 4
                    };
                    let base = (f * 9 + c) * 3;
                    buf[base..base + 3].copy_from_slice(&value.to_le_bytes()[..3]);
                }
            }
            std::thread::sleep(Duration::from_millis(self.capture_pace_ms));
            return Ok(frames);
        }
        // Deterministic 9ch pattern: channel c, frame f → c*1000+f in the
        // top bits of S24_3LE; ref channel 8 carries loopback_ch8 (if armed)
        // or 20000+f.
        let chs = 9usize;
        let mut lb = self.loopback_ch8.as_ref().map(|q| q.lock().unwrap());
        for f in 0..frames as usize {
            let lb_sample = lb.as_mut().map(|q| q.pop_front().unwrap_or(0));
            for c in 0..chs {
                let v: i32 = if c == 8 {
                    if let Some(s) = lb_sample {
                        (s as i32) << 8
                    } else {
                        (20000 + f as i32) << 4
                    }
                } else {
                    (c as i32 * 1000 + f as i32) << 4
                };
                let base = (f * chs + c) * 3;
                buf[base] = (v & 0xff) as u8;
                buf[base + 1] = ((v >> 8) & 0xff) as u8;
                buf[base + 2] = ((v >> 16) & 0xff) as u8;
            }
        }
        drop(lb);
        // Real-time pacing: one 80 ms block per call (configurable in tests).
        std::thread::sleep(Duration::from_millis(self.capture_pace_ms));
        Ok(frames)
    }
    fn playback_write(&self, stereo: &[i16], frames: u32) -> io::Result<u32> {
        if self.panic_playback {
            panic!("injected playback thread panic");
        }
        let real_audio = stereo.iter().any(|s| *s != 0);
        if real_audio {
            self.probes.lock().unwrap().audio_write_started += 1;
        }
        if self.fail_audio_writes && real_audio {
            return Err(io::Error::other("injected DAC write failure"));
        }
        // `park_audio_writes` narrows the gate to real audio (the first-write
        // blind-spot test needs the idle clock to keep flowing); without it
        // the gate holds EVERY write, which is what the stop/overflow tests
        // were built on.
        if !self.park_audio_writes || real_audio {
            if let Some(gate) = self.write_gate.lock().unwrap().as_mut() {
                let _ = gate.recv();
            }
        }
        let mut p = self.probes.lock().unwrap();
        p.written_frames += frames as u64;
        if !self.bounded_probes {
            p.first_samples.push(stereo.first().copied().unwrap_or(0));
            for f in 0..frames as usize {
                p.all_samples.push(stereo.get(f * 2).copied().unwrap_or(0));
            }
        }
        if let Some(q) = &self.loopback_ch8 {
            let mut q = q.lock().unwrap();
            for f in (0..frames as usize).step_by(3) {
                q.push_back(stereo.get(f * 2).copied().unwrap_or(0));
            }
        }
        // Release probe/history locks before the simulated hardware wait so
        // concurrent capture can observe the DAC period while it advances.
        drop(p);
        if self.pace_playback {
            std::thread::sleep(Duration::from_nanos(frames as u64 * 1_000_000_000 / 48_000));
        }
        Ok(frames)
    }
    fn playback_flush(&self) -> io::Result<()> {
        self.probes.lock().unwrap().flush_attempts += 1;
        if self.fail_flush {
            return Err(io::Error::other("injected flush failure"));
        }
        if let Some(gate) = self.flush_gate.lock().unwrap().as_mut() {
            self.probes.lock().unwrap().flush_started += 1;
            let _ = gate.recv();
        }
        if let Some(q) = &self.loopback_ch8 {
            q.lock().unwrap().clear();
        }
        self.probes.lock().unwrap().flushed += 1;
        Ok(())
    }
    fn playback_delay_frames(&self) -> Option<u64> {
        self.report_delay
            .then(|| self.delay_frames.load(std::sync::atomic::Ordering::Relaxed))
    }
    fn playback_drain(&self) -> io::Result<()> {
        self.probes.lock().unwrap().drained += 1;
        Ok(())
    }
    fn playback_underruns(&self) -> u64 {
        0
    }
    fn led_write(&self, rgb: &[(u8, u8, u8); 12]) -> io::Result<()> {
        let mut probes = self.probes.lock().unwrap();
        if self.bounded_probes && !probes.leds.is_empty() {
            probes.leds[0] = *rgb;
        } else {
            probes.leds.push(*rgb);
        }
        Ok(())
    }
    fn button_next(&self) -> io::Result<ButtonEvent> {
        match self.buttons_rx.lock().unwrap().recv() {
            Ok(ev) => Ok(ev),
            Err(_) => {
                std::thread::sleep(Duration::from_secs(3600));
                Err(io::Error::other("button channel closed"))
            }
        }
    }
    fn wake_lock(&self) -> io::Result<()> {
        self.probes.lock().unwrap().wake_locks += 1;
        Ok(())
    }
    fn set_adc_mute(&self, on: bool) -> io::Result<()> {
        self.probes.lock().unwrap().adc_mutes.push(on);
        Ok(())
    }
    fn set_system_time(&self, ts_ms: u64) -> bool {
        self.probes.lock().unwrap().time_sets.push(ts_ms);
        true
    }
}

// --- fake core (LDJSON TCP) ---------------------------------------------------------

pub(crate) struct FakeCore {
    pub(crate) frames: std::sync::Mutex<mpsc::UnboundedReceiver<GatewayFrame>>,
    pub(crate) writer: std::sync::Mutex<Option<tokio::net::tcp::OwnedWriteHalf>>,
}

impl FakeCore {
    pub(crate) async fn send(&self, frame: &CoreFrame) {
        let line = serde_json::to_string(frame).unwrap();
        let mut guard = self.writer.lock().unwrap();
        guard
            .as_mut()
            .expect("core writer not connected")
            .write_all(line.as_bytes())
            .await
            .unwrap();
        guard.as_mut().unwrap().write_all(b"\n").await.unwrap();
    }

    /// A hand-written JSON line (old-shaped frames: a release-67-era core
    /// that has never heard of min_protocol).
    pub(crate) async fn send_raw(&self, line: &str) {
        let mut guard = self.writer.lock().unwrap();
        guard
            .as_mut()
            .expect("core writer not connected")
            .write_all(line.as_bytes())
            .await
            .unwrap();
        guard.as_mut().unwrap().write_all(b"\n").await.unwrap();
    }

    pub(crate) async fn recv(&self) -> GatewayFrame {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        tokio::time::timeout_at(deadline, self.frames.lock().unwrap().recv())
            .await
            .expect("timeout waiting for north frame")
            .expect("firmware link died")
    }

    /// Drain north frames for `wait` and fail if a PlayFinished names
    /// `task_id`. A stop is a StopReceipt, never a finish (see the fix note
    /// in firmware/mod.rs: reporting a flush as a finish made the core drop
    /// the NEXT play's record).
    pub(crate) async fn no_finish_for(&self, task_id: &str, wait: Duration) {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            match tokio::time::timeout_at(deadline, self.frames.lock().unwrap().recv()).await {
                Ok(Some(GatewayFrame::PlayFinished {
                    task_id: got,
                    played_ms,
                    ..
                })) => {
                    assert_ne!(
                        got, task_id,
                        "a STOPPED play must not report a finish (played_ms={played_ms})"
                    );
                }
                Ok(Some(_)) => continue,
                Ok(None) => return,
                Err(_) => return,
            }
        }
    }

    pub(crate) async fn until(&self, pred: impl Fn(&GatewayFrame) -> bool + Send) -> GatewayFrame {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        loop {
            let f = tokio::time::timeout_at(deadline, self.frames.lock().unwrap().recv())
                .await
                .expect("timeout waiting for expected north frame")
                .expect("firmware link died");
            if pred(&f) {
                return f;
            }
        }
    }
}

pub(crate) async fn spawn_fake_core() -> (Arc<FakeCore>, u16) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let (tx, rx) = mpsc::unbounded_channel::<GatewayFrame>();
    let core = Arc::new(FakeCore {
        frames: std::sync::Mutex::new(rx),
        writer: std::sync::Mutex::new(None),
    });
    let c2 = core.clone();
    tokio::spawn(async move {
        let (stream, _) = l.accept().await.expect("fake core accept");
        let (rd, wr) = stream.into_split();
        *c2.writer.lock().unwrap() = Some(wr);
        let mut lines = BufReader::new(rd).lines();
        loop {
            let line = match lines.next_line().await {
                Ok(Some(l)) => l,
                Ok(None) => break,
                Err(_) => break,
            };
            if line.is_empty() {
                continue;
            }
            if let Ok(f) = serde_json::from_str::<GatewayFrame>(&line) {
                let _ = tx.send(f);
            }
        }
    });
    (core, port)
}

// --- helpers ------------------------------------------------------------------------

pub(crate) fn state_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hm-fw-north-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub(crate) fn spawn_firmware(cfg: FirmwareConfig, hw: Arc<dyn Hardware>) {
    std::thread::spawn(move || {
        let fw = Firmware::new(cfg, hw);
        let _ = fw.run();
    });
}

/// Tiny 48 kHz mono WAV of `samples` samples, ±amp alternating.
pub(crate) fn wav_48k(samples: usize, amp: i16) -> Vec<u8> {
    let mut w = Vec::with_capacity(44 + samples * 2);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + samples as u32 * 2).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&48_000u32.to_le_bytes());
    w.extend_from_slice(&96_000u32.to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(samples as u32 * 2).to_le_bytes());
    for i in 0..samples {
        let s = if i % 2 == 0 { amp } else { -amp };
        w.extend_from_slice(&s.to_le_bytes());
    }
    w
}

pub(crate) fn play_frame(task: &str, wav: &[u8], channel: Option<&str>) -> CoreFrame {
    use base64::Engine;
    CoreFrame::Play {
        task_id: task.into(),
        device: "dot2-d".into(),
        content_ref: "test".into(),
        audio: AudioMeta {
            format: "wav".into(),
            sample_rate: 48_000,
            channels: 1,
        },
        audio_b64: Some(base64::engine::general_purpose::STANDARD.encode(wav)),
        channel: channel.map(str::to_string),
        text: None,
        simulated_ms: None,
        barge_in: None,
    }
}

// --- pacing the parked writer ---------------------------------------------------------
//
// `FakeHardware::write_gate` hands out one permit per `playback_write`, and the
// permit channel buffers. A test that fires permits on a timer therefore races
// the writer in two ways, and Ubuntu CI picks whichever one the scheduler
// allows (macOS happened to sit in the window between them):
//
//   * too many too early: the burst piles up in the channel, the writer drains
//     every queued permit before it returns to the command queue, so a Stop
//     that "arrived one period ago" measures seven or eight periods late;
//   * too few: the permits are gone before the role loop has queued the Stop,
//     the writer parks with the Stop still waiting — and ten seconds later the
//     firmware's stall watchdog exits the process (code 75).
//
// Neither is firmware behaviour; both are artefacts of the test's own clock.
// So the tests below never guess: permit ONE period, wait for that write to
// actually land, then decide. One permit is one period, which is what lets a
// test say how many periods a stop cost instead of how many sleeps it took.

/// Let exactly one write through and wait until it has completed.
pub(crate) async fn write_one_period(
    probes: &Arc<Mutex<Probes>>,
    gate: &std::sync::mpsc::Sender<()>,
) {
    let before = probes.lock().unwrap().written_frames;
    let _ = gate.send(());
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    while probes.lock().unwrap().written_frames == before {
        assert!(
            std::time::Instant::now() < deadline,
            "the parked playback write never took its permit"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// Wait until the role loop has handled everything sent before this call.
///
/// The north loop reads frames in order and handles each one fully, so the
/// receipt for a frame sent AFTER a Stop proves the Stop's `PlayCmd::Flush`
/// is already in the playback thread's queue. That is what makes "the writer
/// is still parked, so nothing played while the test waited" true, instead of
/// hoping a sleep was long enough on a loaded runner. A stop for an id
/// nothing is playing is the cheapest barrier available: it answers at once
/// and touches neither the DAC nor the LED ring.
pub(crate) async fn stop_barrier(core: &FakeCore) {
    core.send(&CoreFrame::Stop {
        task_id: "zz-barrier".into(),
        flush: true,
    })
    .await;
    let _ = core
        .until(
            |f| matches!(f, GatewayFrame::StopReceipt { task_id, .. } if task_id == "zz-barrier"),
        )
        .await;
}

/// One period in flight at a time until `done` holds on the probes.
/// Returns false if `limit` elapsed first.
pub(crate) async fn write_until(
    probes: &Arc<Mutex<Probes>>,
    gate: &std::sync::mpsc::Sender<()>,
    done: impl Fn(&Probes) -> bool,
    limit: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        if done(&probes.lock().unwrap()) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        write_one_period(probes, gate).await;
    }
}

// --- v0.3 streamed playback (D/T3) --------------------------------------------------

pub(crate) fn pcm16k_ramp(n: usize) -> Vec<i16> {
    // A deterministic non-trivial signal: sum of two sines + ramp.
    (0..n)
        .map(|i| {
            let t = i as f32 / 16000.0;
            let v = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 4000.0
                + (2.0 * std::f32::consts::PI * 1234.0 * t).sin() * 2000.0
                + (i as f32 / n as f32) * 1500.0;
            v as i16
        })
        .collect()
}

pub(crate) fn pcm_to_wav_16k(pcm: &[i16]) -> Vec<u8> {
    let mut w = Vec::with_capacity(44 + pcm.len() * 2);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + pcm.len() as u32 * 2).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&16_000u32.to_le_bytes());
    w.extend_from_slice(&32_000u32.to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(pcm.len() as u32 * 2).to_le_bytes());
    for s in pcm {
        w.extend_from_slice(&s.to_le_bytes());
    }
    w
}

pub(crate) fn begin_frame(task: &str, rate: u32) -> CF {
    CF::PlayBegin {
        task_id: task.into(),
        device: "dot2-d".into(),
        content_ref: "tts:stream-test".into(),
        audio: AudioMeta {
            format: "pcm_s16le".into(),
            sample_rate: rate,
            channels: 1,
        },
        channel: Some("voice".into()),
        text: None,
        barge_in: None,
    }
}

pub(crate) fn chunk_frame(task: &str, seq: u64, pcm: &[i16]) -> CF {
    let mut bytes = Vec::with_capacity(pcm.len() * 2);
    for s in pcm {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    CF::PlayChunk {
        task_id: task.into(),
        seq,
        audio_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
    }
}

pub(crate) fn trim_zero_edges(v: &[i16]) -> &[i16] {
    let mut a = 0;
    let mut b = v.len();
    while a < b && v[a] == 0 {
        a += 1;
    }
    while b > a && v[b - 1] == 0 {
        b -= 1;
    }
    &v[a..b]
}
