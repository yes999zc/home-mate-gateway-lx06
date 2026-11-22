use super::*;

/// Explicit RSS budget for `home-mate-device-gateway` on the 512 MB Dot2
/// (Phase 3 #21). Normal steady-state firmware RSS is ~4–8 MB; exceeding
/// `RSS_WARN_KB` indicates unexpected allocator/buffer retention, and
/// `RSS_LIMIT_KB` is the hard ceiling before OOM risk on the 512 MB device.
pub const RSS_WARN_KB: u64 = 16_384;
pub const RSS_LIMIT_KB: u64 = 32_768;

/// Process RSS in kB, read from /proc/self/status (open+read only — this
/// kernel's compat table has no statx, see clock32/access(2) precedent).
/// D-stage leak hunt: used by the [mem] trace lines only.
pub fn rss_kb() -> u64 {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open("/proc/self/status") else {
        return 0;
    };
    let mut s = String::new();
    let _ = f.read_to_string(&mut s);
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("VmRSS:") {
            return v
                .split_whitespace()
                .next()
                .and_then(|x| x.parse().ok())
                .unwrap_or(0);
        }
    }
    0
}

pub(super) fn check_rss_budget(rss: u64, context: &str) {
    if rss >= RSS_LIMIT_KB {
        fwlog!("[mem] WARNING: VmRSS={rss} kB exceeds RSS_LIMIT_KB={RSS_LIMIT_KB} kB ({context})");
    } else if rss >= RSS_WARN_KB {
        fwlog!("[mem] WARNING: VmRSS={rss} kB exceeds RSS_WARN_KB={RSS_WARN_KB} kB ({context})");
    }
}

/// Whether the device may cut its own voice playback on a near-end fact (R1d).
/// Every term mirrors a gate the core applies to the same fact, so the audio
/// can never be cut where the core would refuse to explain the stop:
/// a live voice play, not privacy-muted, a TRUSTED fact (reference locked +
/// residual margin), past the answer protection period, and only one stop per
/// episode (the detector's own latch covers per-play, this covers the flap
/// between a flush and the next play starting).
///
/// R1g: **the link's EFFECTIVE duplex is the first gate.** On a half-duplex
/// link the only command entry is the wake word, so a direct near-end fact
/// must not cut the audio — otherwise the story stops on the room's chatter,
/// the core (correctly) refuses the fact, and the ring is left on the amber
/// a stop implies. The fact still goes north (the core counts it); the audio
/// does not move. Full duplex restores the reflex: R3b turns it on by
/// negotiating, not by a code fork.
pub(super) fn local_stop_allowed(
    play_live: bool,
    duplex_full: bool,
    muted: bool,
    trusted: bool,
    voice_ms: u64,
    protect_ms: u64,
    already_stopped: bool,
) -> bool {
    play_live && duplex_full && !muted && trusted && voice_ms >= protect_ms && !already_stopped
}

/// Contract v2 §2.5 (#5, 2026-11-16): one local-exit episode — a retreat
/// word the spotter heard, the audio this device cut for it, and the fact
/// that will be reported when the last flush completes.
///
/// The episode is the device acting on a reflex, not a session decision: it
/// holds no phase, no credits and no wake state. The core's `close_session`
/// is still the only authority, and the `Keyword` frame sent north beside
/// this one is what closes the session — a second copy of the fact can never
/// produce a second answer.
#[derive(Debug, Clone)]
pub(super) struct LocalExit {
    /// The word as the spotter reported it (the `@text` from kw.txt).
    pub(super) keyword: String,
    /// When the hit was processed here (ms epoch) — the latency reference.
    pub(super) fired_ms: u64,
    /// Channels we asked the playback thread to flush (bit 0 voice, bit 1
    /// music). The fact is reported when `pending` reaches zero.
    pub(super) flushed: u8,
    /// Channels still owed a FlushDone.
    pub(super) pending: u8,
    /// The task each flushed channel still owned at hit time. Kept so a new
    /// play that took a slot in between is never mistaken for the one we cut.
    pub(super) voice_task: Option<String>,
    pub(super) music_task: Option<String>,
    /// First flush failure seen, if any (the audio may still be live).
    pub(super) flush_err: Option<String>,
}

impl LocalExit {
    /// Which channels this episode must flush, from the slot owners and the
    /// playback thread's live flags. Bit 0 = voice, bit 1 = music; 0 = there
    /// is nothing to cut (the exit is then only a reported fact).
    pub(super) fn channel_mask(live: [bool; 2], slot: [Option<&str>; 2]) -> u8 {
        let mut mask = 0u8;
        for i in 0..2 {
            if live[i] || slot[i].is_some() {
                mask |= 1 << i;
            }
        }
        mask
    }

    /// Did this flush clear the slot we cut? Only when the channel still owns
    /// the task we recorded: a newer play that took the slot keeps it (its
    /// own StreamDone has nowhere else to go).
    pub(super) fn clears_slot(&self, chan_index: usize, current: Option<&str>) -> bool {
        let ours = if chan_index == 0 {
            &self.voice_task
        } else {
            &self.music_task
        };
        match (ours.as_deref(), current) {
            (Some(mine), Some(cur)) => mine == cur,
            _ => false,
        }
    }

    pub(super) fn channels(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.flushed & 1 != 0 {
            out.push("voice".to_string());
        }
        if self.flushed & 2 != 0 {
            out.push("music".to_string());
        }
        out
    }
}

/// The task id a play/stream frame belongs to, when it carries one. Used by
/// the R1d stale-chunk guard: a locally stopped play must not be re-opened by
/// samples the core was already synthesizing when the reflex fired.
pub(super) fn stream_task_of(frame: &CoreFrame) -> Option<&str> {
    match frame {
        CoreFrame::Play { task_id, .. }
        | CoreFrame::PlayBegin { task_id, .. }
        | CoreFrame::PlayChunk { task_id, .. }
        | CoreFrame::PlayEnd { task_id } => Some(task_id.as_str()),
        _ => None,
    }
}

/// Who a hardware flush answers (R0 + R1d). A flush is one event with three
/// possible owners, and getting it wrong is how "the answer finished after
/// 0 s" happened: the receipt must name the play the stop NAMED, a local
/// reflex stop names its own play, and a flush that answers neither belongs
/// to nobody (the channel's current owner keeps its slot and its own facts).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum FlushOutcome {
    /// The stop the core asked for: StopReceipt `device_confirmed`.
    NamedStop { task_id: String, takes_slot: bool },
    /// The device's own near-end reflex cut this play: StoppedLocally.
    LocallyStopped {
        task_id: String,
        fired_ms: u64,
        takes_slot: bool,
    },
    /// Nobody named this flush.
    Unclaimed,
}

impl FlushOutcome {
    /// Whether the flush ends the channel's current slot. Only when the named
    /// (or local) play is still the slot's owner: a newer play that took the
    /// slot in between keeps it, because its StreamDone has nowhere else to go
    /// (device log 2026-11-12: the answer's finish went missing/mislabelled).
    pub(super) fn takes_slot(&self) -> bool {
        match self {
            FlushOutcome::NamedStop { takes_slot, .. }
            | FlushOutcome::LocallyStopped { takes_slot, .. } => *takes_slot,
            FlushOutcome::Unclaimed => false,
        }
    }
}

/// Decide a flush's owner from the pending stop, the local reflex marker and
/// the channel's current slot. The local marker wins: the core's own Stop for
/// the same task arrives later and finds nothing left to confirm.
pub(super) fn flush_outcome(
    stop_task: Option<&str>,
    local: Option<(&str, u64)>,
    cur: Option<&str>,
) -> FlushOutcome {
    if let Some((task_id, fired_ms)) = local {
        return FlushOutcome::LocallyStopped {
            task_id: task_id.to_string(),
            fired_ms,
            takes_slot: cur == Some(task_id),
        };
    }
    match (stop_task, cur) {
        (Some(stop), Some(cur)) if stop == cur => FlushOutcome::NamedStop {
            task_id: stop.to_string(),
            takes_slot: true,
        },
        _ => FlushOutcome::Unclaimed,
    }
}

/// Pin the calling audio thread to one CPU (D/CPU task, 2026-11-10).
/// `HM_FW_AUDIO_CPU` names the default audio core (supervisor sets 1: AEC on
/// cpu1, cpu0 stays for kernel/network, hm-core owns cpu2, KWS takes cpu3);
/// `HM_FW_PLAY_CPU` optionally overrides the playback thread's core so 48 kHz
/// ALSA period writes never wait behind 7-channel SpeexDSP AEC blocks on the
/// same core. Unset = no pinning (Mac/NAS dev and the A/B rollback form).
/// Failure is logged and swallowed: a scheduler quirk must never take the
/// audio chain down (honesty rule — degrade, never panic).
#[cfg(target_os = "linux")]
pub(super) fn pin_audio_thread(thread: &str) {
    let env_key = if thread == "playback" && std::env::var_os("HM_FW_PLAY_CPU").is_some() {
        "HM_FW_PLAY_CPU"
    } else {
        "HM_FW_AUDIO_CPU"
    };
    let Ok(spec) = std::env::var(env_key) else {
        return;
    };
    let Ok(cpu) = spec.trim().parse::<usize>() else {
        fwlog!("[cpu] {env_key}={spec:?} is not a CPU index — {thread} stays unpinned");
        return;
    };
    let mut set = [0u8; 128]; // cpu_set_t: 1024 bits, identical on musl
    if cpu >= set.len() * 8 {
        fwlog!("[cpu] {env_key}={cpu} out of range — {thread} stays unpinned");
        return;
    };
    set[cpu / 8] |= 1 << (cpu % 8);
    let rc =
        unsafe { libc::sched_setaffinity(0, set.len(), set.as_ptr() as *const libc::cpu_set_t) };
    if rc == 0 {
        fwlog!("[cpu] {thread} pinned to cpu{cpu}");
    } else {
        fwlog!(
            "[cpu] {thread} pin to cpu{cpu} failed: {} — running unpinned",
            std::io::Error::last_os_error()
        );
    }
}

#[cfg(not(target_os = "linux"))]
pub(super) fn pin_audio_thread(_thread: &str) {}

/// Loopback self-check sink (D/hardware task B, 2026-11-10): a bounded
/// append-only tmpfs file. /run is 4MB on emOS and the firmware log shares it.
/// 2026-11-13 (the hang): the log lived on the same filesystem, a full /run
/// made a `println!` write partial and panicked an audio thread. Three tracks
/// at 320 KB (10 s of 16k mono each; the 2 s probe asset is 64 KB) plus a
/// 512 KB reserve can no longer crowd out the log.
const PROBE_MAX_BYTES: u64 = 320_000;
const PROBE_FREE_RESERVE: u64 = 512 * 1024;

/// Free bytes on the filesystem holding `dir` (0 = unknown, treated as "no").
fn tmpfs_free(dir: &str) -> u64 {
    let Ok(c) = std::ffi::CString::new(dir) else {
        return 0;
    };
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
    if rc != 0 {
        return 0;
    }
    (st.f_bavail as u64).saturating_mul(st.f_frsize as u64)
}

/// Probe tracks are armed on demand: the core touches
/// `/run/hm-loop/arm` before a diagnostic play, so normal operation does
/// ZERO tmpfs writes (the steady-on probe measurably dipped chunks/s).
///
/// Deliberately `access(2)`, not `Path::exists()`: musl answers `stat` with
/// `statx`, which this 3.18 kernel does not implement in its AArch32 compat
/// table, so every check costs a full register dump on the serial console.
/// That is the same amplifier that made the clock_gettime64 traps reset the
/// board (src/clock32.rs, docs/d/p0-watchdog-resets-20261109.md). This runs
/// twice per playback (play + capture thread), so it is worth keeping off.
pub(super) fn probe_armed() -> bool {
    let dir = std::env::var("HM_LOOP_DIR").unwrap_or_else(|_| "/run/hm-loop".into());
    let path = format!("{dir}/arm");
    match std::ffi::CString::new(path) {
        Ok(c) => unsafe { libc::access(c.as_ptr(), libc::F_OK) == 0 },
        Err(_) => false,
    }
}

pub(super) struct ProbeSink {
    name: String,
    file: std::fs::File,
    written: u64,
}

impl ProbeSink {
    pub(super) fn open(name: &str) -> Option<Self> {
        let dir = std::env::var("HM_LOOP_DIR").unwrap_or_else(|_| "/run/hm-loop".into());
        let _ = std::fs::create_dir_all(&dir);
        // Never take the space the firmware log needs: a diagnostic that fills
        // /run would cost the audio path its logging (the 2026-11-13 hang).
        if tmpfs_free(&dir) < PROBE_FREE_RESERVE + PROBE_MAX_BYTES {
            fwlog!(
                "[loopback] probe skipped: {dir} has less than {} bytes free",
                PROBE_FREE_RESERVE + PROBE_MAX_BYTES
            );
            return None;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(format!("{dir}/{name}"))
            .ok()?;
        fwlog!("[loopback] probe track {dir}/{name} open");
        Some(Self {
            name: name.into(),
            file,
            written: 0,
        })
    }

    /// Start a fresh track (called when a new playback begins, so a
    /// diagnostic run minutes after boot still compares the right window —
    /// the bounded file would otherwise be full of pre-test audio).
    #[allow(dead_code)]
    pub(super) fn reset(&mut self) {
        if let Some(f) = Self::open(&self.name) {
            *self = f;
        }
    }

    pub(super) fn append_bytes(&mut self, bytes: &[u8]) {
        use std::io::Write;
        if self.written >= PROBE_MAX_BYTES {
            return;
        }
        let take = (PROBE_MAX_BYTES - self.written).min(bytes.len() as u64) as usize;
        if self.file.write_all(&bytes[..take]).is_ok() {
            self.written += take as u64;
        }
    }

    pub(super) fn append(&mut self, samples: &[i16]) {
        use std::io::Write;
        if self.written >= PROBE_MAX_BYTES {
            return;
        }
        let mut bytes = Vec::with_capacity(samples.len() * 2);
        for s in samples {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        let take = (PROBE_MAX_BYTES - self.written).min(bytes.len() as u64) as usize;
        if self.file.write_all(&bytes[..take]).is_ok() {
            self.written += take as u64;
        }
    }
}

pub(crate) fn fnv1a_hex(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Duration of one capture block pushed into `LevelWindow` (1280 frames @ 16 kHz = 80 ms).
pub(crate) const LEVEL_BLOCK_MS: u64 = 80;

/// How many capture blocks the keyword claim's level covers. 20 blocks ×
/// 80 ms = 1.6 s, comfortably longer than the observed reporting lag (90 ms
/// on dot2-d) and about one utterance of the wake phrase.
pub(crate) const LEVEL_WINDOW_BLOCKS: usize = 20;

/// A tiny ring of per-block dBFS with a running peak read.
#[derive(Debug, Clone)]
pub(crate) struct LevelWindow {
    blocks: [f32; LEVEL_WINDOW_BLOCKS],
    next: usize,
}

impl Default for LevelWindow {
    fn default() -> Self {
        Self {
            // -120 dBFS is the empty slot. A real block is never that quiet
            // unless the microphone is dead, and a peak over empty slots must
            // not win an arbitration by being "loud".
            blocks: [-120.0; LEVEL_WINDOW_BLOCKS],
            next: 0,
        }
    }
}

impl LevelWindow {
    pub fn push(&mut self, dbfs: f32) {
        if dbfs.is_finite() {
            self.blocks[self.next] = dbfs;
            self.next = (self.next + 1) % LEVEL_WINDOW_BLOCKS;
        }
    }

    /// Loudest block in the most recent `window_ms` span (clamped to
    /// `[1, LEVEL_WINDOW_BLOCKS]` blocks). `None` while nothing has been
    /// measured: "no meter" and "quiet room" are different facts, and 0.0 dBFS
    /// is a real (very loud) level, so it cannot double as the absent value.
    pub fn peak_over_ms(&self, window_ms: u64) -> Option<f32> {
        let count = (window_ms.div_ceil(LEVEL_BLOCK_MS) as usize).clamp(1, LEVEL_WINDOW_BLOCKS);
        let mut best = f32::NEG_INFINITY;
        for i in 1..=count {
            let idx = (self.next + LEVEL_WINDOW_BLOCKS - i) % LEVEL_WINDOW_BLOCKS;
            best = best.max(self.blocks[idx]);
        }
        if best <= -120.0 {
            None
        } else {
            Some(best)
        }
    }

    /// Loudest block across the full `LEVEL_WINDOW_BLOCKS` history.
    #[cfg(test)]
    pub fn peak(&self) -> Option<f32> {
        self.peak_over_ms(LEVEL_WINDOW_BLOCKS as u64 * LEVEL_BLOCK_MS)
    }
}

/// RMS of one S16 block in dBFS (0 dBFS = full scale). A silent block is
/// -120.0, the window's empty value.
pub fn block_dbfs(block: &[i16]) -> f32 {
    if block.is_empty() {
        return -120.0;
    }
    let sum: f64 = block
        .iter()
        .map(|s| {
            let v = *s as f64 / 32768.0;
            v * v
        })
        .sum();
    let rms = (sum / block.len() as f64).sqrt();
    if rms <= 1e-6 {
        -120.0
    } else {
        (20.0 * rms.log10()) as f32
    }
}
