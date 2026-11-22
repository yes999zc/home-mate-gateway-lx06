//! One connection's bounded encoded northbound scheduler. No producer awaits I/O.
use super::*;
use std::io::Write;
use std::sync::Weak;
use std::time::{Duration, Instant};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::Notify;

pub const CRITICAL_COUNT_CAP: usize = 64;
pub const CRITICAL_BYTE_CAP: usize = 262_144;
pub const TELEMETRY_BYTE_CAP: usize = 4096;
// 1280 frames * 2 channels * 2 bytes = 5120 PCM bytes = 6828 base64 bytes.
// The remaining 1364 bytes bound escaped identity and fixed AudioChunk metadata.
pub const AUDIO_BYTE_CAP: usize = 8192;
pub const WRITE_DEADLINE_MS: u64 = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    Critical,
    Health,
    Presence,
    Audio,
    Handshake,
}

fn classify(frame: &GatewayFrame) -> Lane {
    use GatewayFrame::*;
    match frame {
        Hello { .. } => Lane::Handshake,
        EchoHealth { .. } => Lane::Health,
        NearEndPresence { .. } => Lane::Presence,
        AudioChunk { .. } => Lane::Audio,
        Wake { .. }
        | Utterance { .. }
        | Button { .. }
        | MuteState { .. }
        | PlayReceipt { .. }
        | PlayRejected { .. }
        | StopReceipt { .. }
        | VolumeReceipt { .. }
        | ModeReport { .. }
        | PlayFinished { .. }
        | WifiState { .. }
        | MicReport { .. }
        | DeviceStatus { .. }
        | Keyword { .. }
        | BargeIn { .. }
        | StoppedLocally { .. }
        | KwsReport { .. }
        | KwsDeployReceipt { .. }
        | LocalExitFlushed { .. }
        | ProtocolError { .. }
        | ConfigState { .. }
        | ConfigValidated { .. }
        | ConfigReceipt { .. } => Lane::Critical,
    }
}

struct CappedBuffer {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for CappedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("encoded frame exceeds lane cap"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn encode(frame: &impl serde::Serialize, limit: usize) -> Result<Vec<u8>, &'static str> {
    let mut sink = CappedBuffer {
        bytes: Vec::with_capacity(limit.min(4096)),
        limit,
    };
    if let Err(error) = serde_json::to_writer(&mut sink, frame) {
        return Err(if error.is_io() {
            "oversized_frame"
        } else {
            "encode_failed"
        });
    }
    sink.write_all(b"\n").map_err(|_| "oversized_frame")?;
    Ok(sink.bytes)
}

#[derive(Default)]
struct Probe {
    health_pending: u64,
    presence_pending: u64,
    health_pending_bytes: u64,
    presence_pending_bytes: u64,
    inflight_bytes: u64,
    write_started_us: u64,
    write_deadline_us: u64,
    last_fault_write_started_us: u64,
    last_fault_write_deadline_us: u64,
    last_fault_us: u64,
    last_cleanup_us: u64,
    last_fault_generation: u64,
    last_cleanup_generation: u64,
    reader_tasks: u64,
    writer_tasks: u64,
    decode_active: u64,
    decode_waiters: u64,
    producer_calls: u64,
    producer_max_ns: u64,
    last_progress_uptime_ms: u64,
    handshakes: u64,
    handshake_completed: bool,
}

pub struct NorthboundStats {
    boot: Instant,
    status: Mutex<crate::protocol::NorthboundStatus>,
    current: Mutex<Weak<Connection>>,
    probe: Mutex<Probe>,
    decode_queue: Mutex<Option<mpsc::WeakSender<DecodeCmd>>>,
}
impl Default for NorthboundStats {
    fn default() -> Self {
        let boot_id = format!(
            "{}_{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        Self {
            boot: Instant::now(),
            status: Mutex::new(crate::protocol::NorthboundStatus {
                schema: 1,
                boot_id,
                ..Default::default()
            }),
            current: Mutex::new(Weak::new()),
            probe: Mutex::new(Probe::default()),
            decode_queue: Mutex::new(None),
        }
    }
}
impl NorthboundStats {
    fn update(&self, f: impl FnOnce(&mut crate::protocol::NorthboundStatus)) {
        f(&mut self.status.lock().unwrap_or_else(|e| e.into_inner()));
    }
    fn uptime(&self) -> u64 {
        self.boot.elapsed().as_millis().min(u64::MAX as u128) as u64
    }
    pub fn snapshot(&self) -> crate::protocol::NorthboundStatus {
        let current = self
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .upgrade();
        let age = current
            .as_ref()
            .and_then(|c| c.oldest())
            .map(|at| at.elapsed().as_millis().min(u64::MAX as u128) as u64)
            .unwrap_or(0);
        // Sample and clone under one lock: a concurrent fault must never
        // carry a timestamp newer than this snapshot's sampled uptime.
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        status.sampled_uptime_ms = self.uptime();
        status.oldest_pending_ms = age;
        status.oldest_pending_high_water_ms = status.oldest_pending_high_water_ms.max(age);
        status.clone()
    }
    /// Bounded observer of the same production counters; contains no audio/text.
    pub fn probe(&self) -> serde_json::Value {
        let status = self.snapshot();
        let decode_pending = self
            .decode_queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|weak| weak.upgrade())
            .map_or(0, |tx| tx.max_capacity() - tx.capacity());
        let p = self.probe.lock().unwrap_or_else(|e| e.into_inner());
        json!({"reader_tasks":p.reader_tasks,"writer_tasks":p.writer_tasks,"decode_active":p.decode_active,"decode_waiters":p.decode_waiters,"decode_pending":decode_pending,"status":status,"health_pending":p.health_pending,"presence_pending":p.presence_pending,
            "health_pending_bytes":p.health_pending_bytes,"presence_pending_bytes":p.presence_pending_bytes,"inflight_bytes":p.inflight_bytes,"producer_calls":p.producer_calls,"producer_max_ns":p.producer_max_ns,
            "write_started_us":p.write_started_us,"write_deadline_us":p.write_deadline_us,"last_fault_write_started_us":p.last_fault_write_started_us,"last_fault_write_deadline_us":p.last_fault_write_deadline_us,"last_fault_us":p.last_fault_us,"last_cleanup_us":p.last_cleanup_us,"last_fault_generation":p.last_fault_generation,"last_cleanup_generation":p.last_cleanup_generation,
            "last_progress_uptime_ms":p.last_progress_uptime_ms,"handshakes":p.handshakes,"handshake_completed":p.handshake_completed})
    }
    fn age(&self, at: Instant) {
        let age = at.elapsed().as_millis().min(u64::MAX as u128) as u64;
        self.update(|s| s.oldest_pending_high_water_ms = s.oldest_pending_high_water_ms.max(age));
    }
    pub(super) fn set_decode_queue(&self, tx: mpsc::WeakSender<DecodeCmd>) {
        *self.decode_queue.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    }
    pub(super) fn activity(self: &Arc<Self>, kind: ActivityKind) -> Activity {
        let mut p = self.probe.lock().unwrap_or_else(|e| e.into_inner());
        match kind {
            ActivityKind::Reader => p.reader_tasks += 1,
            ActivityKind::Writer => p.writer_tasks += 1,
            ActivityKind::Decode => p.decode_active += 1,
            ActivityKind::DecodeWait => p.decode_waiters += 1,
        };
        Activity {
            stats: self.clone(),
            kind,
        }
    }
    fn micros(&self) -> u64 {
        self.boot.elapsed().as_micros().min(u64::MAX as u128) as u64
    }
    pub(super) fn cleanup(&self) {
        self.cleanup_at(self.micros());
    }
    fn cleanup_at(&self, now: u64) {
        let generation = self
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .link_generation;
        let mut p = self.probe.lock().unwrap_or_else(|e| e.into_inner());
        if p.last_fault_generation != generation
            || p.last_fault_us == 0
            || (p.last_cleanup_generation == generation && p.last_cleanup_us != 0)
        {
            return;
        }
        p.last_cleanup_us = now;
        p.last_cleanup_generation = generation;
    }
    fn production_call(&self, start: Instant) {
        let mut p = self.probe.lock().unwrap_or_else(|e| e.into_inner());
        p.producer_calls = p.producer_calls.saturating_add(1);
        p.producer_max_ns = p
            .producer_max_ns
            .max(start.elapsed().as_nanos().min(u64::MAX as u128) as u64);
    }
}

struct WriterPanicGuard(NorthTx);
impl Drop for WriterPanicGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.fail("writer_panic");
        }
    }
}

pub(super) enum ActivityKind {
    Reader,
    Writer,
    Decode,
    DecodeWait,
}
pub(super) struct Activity {
    stats: Arc<NorthboundStats>,
    kind: ActivityKind,
}
impl Drop for Activity {
    fn drop(&mut self) {
        let mut p = self.stats.probe.lock().unwrap_or_else(|e| e.into_inner());
        match self.kind {
            ActivityKind::Reader => p.reader_tasks = p.reader_tasks.saturating_sub(1),
            ActivityKind::Writer => p.writer_tasks = p.writer_tasks.saturating_sub(1),
            ActivityKind::Decode => p.decode_active = p.decode_active.saturating_sub(1),
            ActivityKind::DecodeWait => p.decode_waiters = p.decode_waiters.saturating_sub(1),
        };
    }
}

/// Count admission precedes serialization; at most64 critical encoders,
/// queued entries and in-flight entries together can retain their originals.
struct CountPermit {
    connection: Weak<Connection>,
    armed: bool,
}
impl Drop for CountPermit {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(c) = self.connection.upgrade() {
            {
                let mut q = c.queue.lock().unwrap_or_else(|e| e.into_inner());
                q.retained_count = q.retained_count.saturating_sub(1);
                c.stats.update(|s| {
                    if s.link_generation == c.generation {
                        s.critical_current_count = q.retained_count as u64;
                    }
                });
            }
            // An unwinding producer cannot silently lose its admitted fact.
            if std::thread::panicking() {
                c.fail("encode_failed");
            }
        }
    }
}

struct Entry {
    bytes: Vec<u8>,
    lane: Lane,
    at: Instant,
    reservation: Weak<Connection>,
    charged: bool,
    inflight: bool,
}
impl Drop for Entry {
    fn drop(&mut self) {
        if let Some(c) = self.reservation.upgrade() {
            c.stats.age(self.at);
            let mut q = c.queue.lock().unwrap_or_else(|e| e.into_inner());
            if self.inflight {
                q.inflight_at = None;
                c.stats
                    .probe
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .inflight_bytes = 0;
            }
            if self.charged {
                debug_assert_eq!(self.lane, Lane::Critical);
                q.retained_count = q.retained_count.saturating_sub(1);
                q.retained_bytes = q.retained_bytes.saturating_sub(self.bytes.len());
                c.stats.update(|s| {
                    if s.link_generation == c.generation {
                        s.critical_current_count = q.retained_count as u64;
                        s.critical_current_bytes = q.retained_bytes as u64;
                    }
                });
            }
        }
    }
}
#[derive(Default)]
struct Queue {
    critical: VecDeque<Entry>,
    health: Option<Entry>,
    presence: Option<Entry>,
    audio: VecDeque<Entry>,
    retained_count: usize,
    retained_bytes: usize,
    inflight_at: Option<Instant>,
    critical_burst: u8,
    round_lane: u8,
}
struct Connection {
    device: String,
    generation: u64,
    queue: Mutex<Queue>,
    terminal: AtomicBool,
    reason: Mutex<Option<&'static str>>,
    ready: Notify,
    fatal: Notify,
    stats: Arc<NorthboundStats>,
    dropped_audio: Arc<AtomicU64>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Admission {
    Admitted,
    Coalesced,
    DroppedAudio,
    Closed,
}
#[derive(Clone)]
pub(super) struct NorthTx {
    connection: Arc<Connection>,
}

impl NorthTx {
    pub(super) fn new(
        device: String,
        generation: u64,
        stats: Arc<NorthboundStats>,
        dropped_audio: Arc<AtomicU64>,
    ) -> Self {
        let connection = Arc::new(Connection {
            device,
            generation,
            queue: Mutex::new(Queue::default()),
            terminal: AtomicBool::new(false),
            reason: Mutex::new(None),
            ready: Notify::new(),
            fatal: Notify::new(),
            stats,
            dropped_audio,
        });
        connection.stats.update(|s| {
            s.link_generation = generation;
            s.critical_current_count = 0;
            s.critical_current_bytes = 0;
            s.audio_current_count = 0;
            s.oldest_pending_ms = 0;
        });
        connection
            .stats
            .probe
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .handshake_completed = false;
        *connection
            .stats
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(&connection);
        Self { connection }
    }
    pub(super) fn send_ctrl(&self, mut frame: GatewayFrame) -> Admission {
        let started = Instant::now();
        let result = if self.is_terminal() {
            // Include destruction of a rejected owned payload in call timing.
            drop(frame);
            Admission::Closed
        } else {
            if let GatewayFrame::DeviceStatus { northbound, .. } = &mut frame {
                *northbound = serde_json::to_value(self.connection.stats.snapshot()).ok();
            }
            self.admit(frame, false)
        };
        self.connection.stats.production_call(started);
        result
    }
    pub(super) fn mark_handshake(&self) {
        let mut p = self
            .connection
            .stats
            .probe
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        p.handshake_completed = true;
        p.handshakes = p.handshakes.saturating_add(1);
    }
    pub(super) fn try_send_audio(&self, frame: GatewayFrame) -> Admission {
        self.send(frame, true)
    }
    fn send(&self, frame: GatewayFrame, audio: bool) -> Admission {
        let started = Instant::now();
        let result = self.admit(frame, audio);
        self.connection.stats.production_call(started);
        result
    }
    fn reserve_critical(&self) -> Option<CountPermit> {
        let c = &self.connection;
        let mut q = c.queue.lock().unwrap_or_else(|e| e.into_inner());
        if c.terminal.load(Ordering::Acquire) {
            return None;
        }
        if q.retained_count >= CRITICAL_COUNT_CAP {
            drop(q);
            self.fail("critical_count_full");
            return None;
        }
        q.retained_count += 1;
        c.stats.update(|s| {
            s.critical_current_count = q.retained_count as u64;
            s.critical_high_water_count = s.critical_high_water_count.max(s.critical_current_count);
        });
        Some(CountPermit {
            connection: Arc::downgrade(c),
            armed: true,
        })
    }
    fn admit(&self, frame: GatewayFrame, audio: bool) -> Admission {
        let c = &self.connection;
        if c.terminal.load(Ordering::Acquire) {
            return Admission::Closed;
        }
        let lane = classify(&frame);
        if lane == Lane::Handshake || audio != (lane == Lane::Audio) {
            self.fail("invalid_frame");
            return Admission::Closed;
        }
        let source = match &frame {
            GatewayFrame::EchoHealth { device, .. }
            | GatewayFrame::NearEndPresence { device, .. }
            | GatewayFrame::AudioChunk { device, .. }
            | GatewayFrame::Wake { device, .. }
            | GatewayFrame::Utterance { device, .. }
            | GatewayFrame::Button { device, .. }
            | GatewayFrame::MuteState { device, .. }
            | GatewayFrame::PlayFinished { device, .. }
            | GatewayFrame::ModeReport { device, .. }
            | GatewayFrame::WifiState { device, .. }
            | GatewayFrame::MicReport { device, .. }
            | GatewayFrame::DeviceStatus { device, .. }
            | GatewayFrame::Keyword { device, .. }
            | GatewayFrame::BargeIn { device, .. }
            | GatewayFrame::StoppedLocally { device, .. }
            | GatewayFrame::KwsReport { device, .. }
            | GatewayFrame::KwsDeployReceipt { device, .. }
            | GatewayFrame::LocalExitFlushed { device, .. }
            | GatewayFrame::PlayRejected { device, .. }
            | GatewayFrame::ConfigState { device, .. }
            | GatewayFrame::ConfigValidated { device, .. }
            | GatewayFrame::ConfigReceipt { device, .. } => Some(device),
            GatewayFrame::VolumeReceipt { device, .. } => device.as_ref(),
            _ => None,
        };
        if source.is_some_and(|device| device != &c.device) {
            self.fail("invalid_source");
            return Admission::Closed;
        }
        if matches!(&frame,GatewayFrame::DeviceStatus{descriptor:Some(descriptor),..} if descriptor.stable_id!=c.device)
        {
            self.fail("invalid_source");
            return Admission::Closed;
        }
        if let GatewayFrame::AudioChunk {
            pcm_b64,
            sample_rate,
            channels,
            format,
            layout,
            ..
        } = &frame
        {
            if pcm_b64.len() > 6828
                || *sample_rate != CAP_RATE
                || !matches!(channels, 1 | 2)
                || format != "s16le"
                || !matches!(layout.as_str(), "mono" | "mic_ref")
            {
                self.fail("invalid_frame");
                return Admission::Closed;
            }
        }
        let limit = match lane {
            Lane::Critical => CRITICAL_BYTE_CAP,
            Lane::Audio => AUDIO_BYTE_CAP,
            _ => TELEMETRY_BYTE_CAP,
        };
        let mut permit = if lane == Lane::Critical {
            match self.reserve_critical() {
                Some(permit) => Some(permit),
                None => return Admission::Closed,
            }
        } else {
            None
        };
        let bytes = match encode(&frame, limit) {
            Ok(bytes) => bytes,
            Err(reason) => {
                self.fail(reason);
                return Admission::Closed;
            }
        };
        let mut q = c.queue.lock().unwrap_or_else(|e| e.into_inner());
        if c.terminal.load(Ordering::Acquire) {
            return Admission::Closed;
        }
        let mut entry = Entry {
            bytes,
            lane,
            at: Instant::now(),
            reservation: Weak::new(),
            charged: false,
            inflight: false,
        };
        let result = match lane {
            Lane::Critical => {
                let fault =
                    if entry.bytes.len() > CRITICAL_BYTE_CAP.saturating_sub(q.retained_bytes) {
                        Some("critical_bytes_full")
                    } else {
                        None
                    };
                if let Some(reason) = fault {
                    drop(q);
                    self.fail(reason);
                    return Admission::Closed;
                }
                entry.reservation = Arc::downgrade(c);
                entry.charged = true;
                permit.as_mut().expect("critical count reserved").armed = false;
                q.retained_bytes += entry.bytes.len();
                c.stats.update(|s| {
                    s.critical_current_count = q.retained_count as u64;
                    s.critical_current_bytes = q.retained_bytes as u64;
                    s.critical_high_water_count =
                        s.critical_high_water_count.max(s.critical_current_count);
                    s.critical_high_water_bytes =
                        s.critical_high_water_bytes.max(s.critical_current_bytes);
                });
                q.critical.push_back(entry);
                Admission::Admitted
            }
            Lane::Health | Lane::Presence => {
                let slot = if lane == Lane::Health {
                    &mut q.health
                } else {
                    &mut q.presence
                };
                if let Some(old) = slot.as_ref() {
                    c.stats.age(old.at);
                }
                let pending_bytes = entry.bytes.len() as u64;
                let replaced = slot.replace(entry).is_some();
                if replaced {
                    c.stats.update(|s| {
                        if lane == Lane::Health {
                            s.health_coalesced = s.health_coalesced.saturating_add(1)
                        } else {
                            s.presence_coalesced = s.presence_coalesced.saturating_add(1)
                        }
                    });
                }
                let mut p = c.stats.probe.lock().unwrap_or_else(|e| e.into_inner());
                if lane == Lane::Health {
                    p.health_pending = 1;
                    p.health_pending_bytes = pending_bytes;
                } else {
                    p.presence_pending = 1;
                    p.presence_pending_bytes = pending_bytes;
                }
                if replaced {
                    Admission::Coalesced
                } else {
                    Admission::Admitted
                }
            }
            Lane::Audio => {
                if q.audio.len() >= UPLINK_AUDIO_CAP {
                    let _ = c
                        .dropped_audio
                        .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                            Some(v.saturating_add(1))
                        });
                    c.stats
                        .update(|s| s.audio_dropped = s.audio_dropped.saturating_add(1));
                    Admission::DroppedAudio
                } else {
                    q.audio.push_back(entry);
                    c.stats.update(|s| {
                        s.audio_current_count = q.audio.len() as u64;
                        s.audio_high_water_count =
                            s.audio_high_water_count.max(s.audio_current_count);
                    });
                    Admission::Admitted
                }
            }
            Lane::Handshake => unreachable!(),
        };
        drop(q);
        c.ready.notify_one();
        result
    }
    pub(super) fn fail(&self, reason: &'static str) {
        self.connection.fail(reason);
    }
    pub(super) fn is_terminal(&self) -> bool {
        self.connection.terminal.load(Ordering::Acquire)
    }
    pub(super) async fn failed(&self) {
        loop {
            let wake = self.connection.fatal.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            if self.is_terminal() {
                return;
            }
            wake.await;
        }
    }
    pub(super) fn reason(&self) -> &'static str {
        self.connection
            .reason
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unwrap_or("peer_closed")
    }
    pub(super) fn same_connection(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.connection, &other.connection)
    }
    pub(super) async fn write_to<W: AsyncWrite + Unpin>(&self, mut writer: W) -> io::Result<()> {
        let _activity = self.connection.stats.activity(ActivityKind::Writer);
        let _panic_guard = WriterPanicGuard(self.clone());
        loop {
            let entry = loop {
                let wake = self.connection.ready.notified();
                tokio::pin!(wake);
                wake.as_mut().enable();
                if self.is_terminal() {
                    return Err(io::Error::other(self.reason()));
                }
                if let Some(entry) = self.connection.take() {
                    break entry;
                }
                wake.await;
            };
            let result = tokio::select! {
                result=write_frame(&mut writer,&entry.bytes,&self.connection.stats)=>result,
                _=self.failed()=>Err(io::Error::other(self.reason())),
            };
            self.connection.finished();
            drop(entry);
            if let Err(error) = result {
                self.fail(if error.kind() == io::ErrorKind::TimedOut {
                    "writer_timeout"
                } else {
                    "writer_error"
                });
                return Err(error);
            }
        }
    }
}
impl Connection {
    fn fail(&self, reason: &'static str) {
        {
            let mut recorded = self.reason.lock().unwrap_or_else(|e| e.into_inner());
            if self.terminal.load(Ordering::Acquire) {
                return;
            }
            *recorded = Some(reason);
            self.terminal.store(true, Ordering::Release);
        }
        if let Some(at) = self.oldest() {
            self.stats.age(at);
        }
        self.stats.update(|s| {
            s.transport_disconnects = s.transport_disconnects.saturating_add(1);
            if reason.starts_with("critical_") {
                s.critical_full = s.critical_full.saturating_add(1);
            }
            if matches!(
                reason,
                "invalid_frame" | "invalid_source" | "oversized_frame" | "encode_failed"
            ) {
                s.invalid_frames = s.invalid_frames.saturating_add(1);
            }
            if reason == "writer_timeout" {
                s.writer_timeouts = s.writer_timeouts.saturating_add(1);
            }
            if matches!(reason, "writer_error" | "writer_panic") {
                s.writer_errors = s.writer_errors.saturating_add(1);
            }
            s.last_fault = Some(crate::protocol::NorthboundFault {
                code: reason.into(),
                uptime_ms: self.stats.uptime(),
            });
        });
        {
            let generation = self
                .stats
                .status
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .link_generation;
            let mut p = self.stats.probe.lock().unwrap_or_else(|e| e.into_inner());
            p.last_fault_us = self.stats.micros();
            p.last_fault_generation = generation;
            p.last_fault_write_started_us = p.write_started_us;
            p.last_fault_write_deadline_us = p.write_deadline_us;
        }
        // Drop reservations outside the queue lock; the in-flight one stays charged.
        let (critical, health, presence, audio) = {
            let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
            (
                std::mem::take(&mut q.critical),
                q.health.take(),
                q.presence.take(),
                std::mem::take(&mut q.audio),
            )
        };
        drop((critical, health, presence, audio));
        self.stats.update(|s| s.audio_current_count = 0);
        {
            let mut p = self.stats.probe.lock().unwrap_or_else(|e| e.into_inner());
            p.health_pending = 0;
            p.presence_pending = 0;
            p.health_pending_bytes = 0;
            p.presence_pending_bytes = 0;
        }
        self.ready.notify_waiters();
        self.fatal.notify_waiters();
    }
    fn oldest(&self) -> Option<Instant> {
        let q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        [
            q.critical.front().map(|e| e.at),
            q.health.as_ref().map(|e| e.at),
            q.presence.as_ref().map(|e| e.at),
            q.audio.front().map(|e| e.at),
            q.inflight_at,
        ]
        .into_iter()
        .flatten()
        .min()
    }
    fn take(self: &Arc<Self>) -> Option<Entry> {
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if self.terminal.load(Ordering::Acquire) {
            return None;
        }
        let mut entry = loop {
            if q.round_lane == 0 && q.critical_burst < 8 {
                if let Some(e) = q.critical.pop_front() {
                    q.critical_burst += 1;
                    break e;
                }
            }
            q.round_lane = q.round_lane.max(1);
            let next = match q.round_lane {
                1 => q.health.take(),
                2 => q.presence.take(),
                _ => q.audio.pop_front(),
            };
            q.round_lane += 1;
            if q.round_lane > 3 {
                q.round_lane = 0;
                q.critical_burst = 0;
            }
            if let Some(e) = next {
                break e;
            }
            if q.critical.is_empty()
                && q.health.is_none()
                && q.presence.is_none()
                && q.audio.is_empty()
            {
                return None;
            }
        };
        self.stats.age(entry.at);
        entry.inflight = true;
        entry.reservation = Arc::downgrade(self);
        q.inflight_at = Some(entry.at);
        self.stats
            .update(|s| s.audio_current_count = q.audio.len() as u64);
        {
            let mut p = self.stats.probe.lock().unwrap_or_else(|e| e.into_inner());
            p.health_pending = q.health.is_some() as u64;
            p.presence_pending = q.presence.is_some() as u64;
            p.health_pending_bytes = q.health.as_ref().map_or(0, |e| e.bytes.len() as u64);
            p.presence_pending_bytes = q.presence.as_ref().map_or(0, |e| e.bytes.len() as u64);
            p.inflight_bytes = entry.bytes.len() as u64;
        }
        Some(entry)
    }
    fn finished(&self) {
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(at) = q.inflight_at.take() {
            self.stats.age(at);
        }
        self.stats
            .probe
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .inflight_bytes = 0;
    }
}

pub(super) async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
    stats: &NorthboundStats,
) -> io::Result<()> {
    {
        let mut p = stats.probe.lock().unwrap_or_else(|e| e.into_inner());
        p.write_started_us = stats.micros();
        p.write_deadline_us = p.write_started_us.saturating_add(WRITE_DEADLINE_MS * 1000);
    }
    tokio::time::timeout(Duration::from_millis(WRITE_DEADLINE_MS), async {
        let mut position = 0;
        while position < bytes.len() {
            let n = writer.write(&bytes[position..]).await?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "north writer made zero progress",
                ));
            }
            position += n;
            stats.update(|s| {
                s.successful_write_bytes = s.successful_write_bytes.saturating_add(n as u64)
            });
            stats
                .probe
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .last_progress_uptime_ms = stats.uptime();
        }
        Ok(())
    })
    .await
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "north frame write exceeded 1000 ms",
        )
    })?
}

pub(super) async fn write_handshake<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &GatewayFrame,
    stats: &NorthboundStats,
) -> io::Result<()> {
    let bytes = encode(frame, CRITICAL_BYTE_CAP)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    write_frame(writer, &bytes, stats).await
}

#[cfg(test)]
mod tests;

pub(super) fn write_error_reason(error: &io::Error) -> &'static str {
    if error.kind() == io::ErrorKind::TimedOut {
        "writer_timeout"
    } else if error.kind() == io::ErrorKind::InvalidData && error.to_string() == "oversized_frame" {
        "oversized_frame"
    } else if error.kind() == io::ErrorKind::InvalidData && error.to_string() == "encode_failed" {
        "encode_failed"
    } else {
        "writer_error"
    }
}
