//! G5-2 direct-occupation audio component — the EchoMuse server's successor
//! with the controller role ABSORBED (user direction 2026-11-08: no
//! three-layer middleware; one process owns the hardware AND speaks the
//! northbound v0 protocol straight to the core — loopback once the core
//! itself moves on-device, protocol unchanged).
//!
//!   capture  /dev/snd/pcmC0D24c  S24_3LE 9ch 16k → downmix → AudioChunk north
//!   playback /dev/snd/pcmC0D23p  S16_LE 2ch 48k  ← Play WAVs, voice+music
//!            mixed, SOFTWARE gain (EchoMuse applies volume in DSP — mixer
//!            diff evidence 2026-11-08: zero control changes across levels)
//!   LED      is31fl3236 sysfs frame ← AssistantState (palette, reserved green)
//!   buttons  /dev/input/event1+2  → Button / device volume keys
//!   power    /sys/power/wake_lock
//!
//! Honesty rules: the descriptor advertises only implemented capabilities;
//! the firmware tag identifies this component northbound (console-visible);
//! receipts state facts (device_confirmed only after hardware wrote/dropped).
//!
//! Owner-confirmed long-press mute rule (#23, 2026-11-16):
//! Long-pressing the mute button enters local Wi-Fi provisioning mode: stops
//! any current playback, rejects incoming `Play` / `PlayBegin` with
//! `PlayRejected { reason: "wifi_provisioning", .. }`, lights the dedicated
//! `HealthLamp::WifiProvisioning` ring, and notifies the core with
//! `Button { action: "mute_hold_wifi_setup", .. }`. It NEVER triggers a
//! factory reset (factory reset is core/console-command only).

use crate::fwlog;
pub mod aec;
pub mod alsa;
pub mod bargein;
pub mod boot_health;
pub mod bringup;
pub mod buttons;
mod capture;
mod capture_read;
pub mod config_journal;
#[cfg(test)]
mod generation_tests;
pub mod jack;
mod keywords;
pub mod kws;
pub mod leds;
pub mod loudness;
pub mod lx06;
mod lx06_vendor;
pub mod mixer;
pub mod nearend;
mod north;
mod north_config;
mod north_link;
pub mod northbound;
mod playback;
pub mod profile;
pub mod resample;
mod role;
mod role_events;
#[cfg(test)]
mod tests;
mod wifi;
mod wire;

use crate::protocol::{
    now_ms, AecState, CoreFrame, DuplexState, GatewayFrame, HelloDevice, LineRead,
    PROTOCOL_ERROR_RETRY_MS, PROTOCOL_MIN_VERSION, PROTOCOL_VERSION,
};
use capture::capture_thread;
pub use capture::{decode_s24_3le, downmix_block};
use keywords::deploy_keywords;
use north::render_ring;
use north_link::{north_loop, ProtocolMismatch};
use northbound::NorthTx;
use playback::{
    decode_thread, playback_thread, progress_facts, PlayAction, PlayCmd, PlayEvent, PlayStamp,
};
pub use playback::{
    level_to_gain, mix_mono_f32, mix_to_stereo, mono_to_stereo_i16, MAX_SIMULATED_MS,
};
pub(crate) use role::PlayProgress;
use role::{lock_or_fault, AudioFault, DecodeCmd, Role, Shared, StreamCtx};
pub use role::{Firmware, AUDIO_FAULT_EXIT_CODE, UPLINK_AUDIO_CAP};
use serde_json::json;
use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::BufReader;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
#[cfg(test)]
pub(crate) use wifi::wifi_credentials_rejected;
use wifi::{load_state, save_state, spawn_wifi_probe};
pub use wire::{block_dbfs, rss_kb, RSS_LIMIT_KB, RSS_WARN_KB};
use wire::{
    check_rss_budget, flush_outcome, local_stop_allowed, pin_audio_thread, probe_armed,
    stream_task_of, FlushOutcome, LocalExit, ProbeSink,
};
pub(crate) use wire::{fnv1a_hex, LevelWindow};
#[cfg(test)]
pub(crate) use wire::{LEVEL_BLOCK_MS, LEVEL_WINDOW_BLOCKS};

// --- live hardware facts (design doc §1 E1/E2, verified 2026-11-08) ---------------
const CAP_RATE: u32 = 16000;
const CAP_CHANNELS: usize = 9;
const CAP_FORMAT_S24_3LE: u32 = 6;
const CAP_PERIOD: u32 = 512;
const CAP_BUFFER: u32 = 2560;
/// One uplink block = 80 ms (v0.2 wire cadence): 1280 frames @16k.
const UPLINK_FRAMES: u32 = 1280;
/// Keyword-spotter feed granularity: 20 ms. The spotter cannot fire before
/// it has the samples, so feeding at the 80 ms uplink cadence would add up
/// to 80 ms of dead time to every interruption (T4).
const KWS_SLICE: usize = 320;

const PLAY_RATE: u32 = 48000;
const PLAY_FORMAT_S16_LE: u32 = 2;
const PLAY_CHANNELS: u32 = 2;
pub const PLAY_PERIOD: u32 = 2048;
/// Default playback buffer = 4 periods = 170.7 ms @48k (D/T5 debt #4:
/// "ALSA 播放缓冲 170ms"). Configurable via `--play-buffer` so the SAME
/// binary can A/B 4096 (85 ms) against 8192 without a rebuild — the
/// latency-vs-underrun trade-off must be measured, not guessed.
pub const PLAY_BUFFER: u32 = 8192;

const RECONNECT_MIN_MS: u64 = 1_000;
const RECONNECT_MAX_MS: u64 = 30_000;
/// Physical volume keys step on the device's own 0–127 grid (≈4 dB).
const VOLUME_KEY_STEP: u64 = 8;
const DEFAULT_VOLUME: u64 = 100;
/// R1d local stop: the answer protection period the core also enforces
/// (voice.rs `barge_protect_ms`). A near-end fact inside it is the fresh
/// answer's own TTS onset — the device must not cut there, or the audio it
/// kills would be audio the core then refuses to explain.
const DEFAULT_BARGE_PROTECT_MS: u64 = 800;
/// R1d: how long a locally stopped play keeps rejecting the core's chunks.
/// Must exceed the worst fact latency seen (3.078 s) with room for the
/// core's own handling, while staying far below a normal turn so a stale
/// marker cannot eat a legitimate later play.
const LOCAL_STOP_BLOCK_MS: u64 = 10_000;
/// Default bounded timeout for local Wi-Fi provisioning mode (#23): 180 s.
pub const DEFAULT_WIFI_PROVISIONING_TIMEOUT_MS: u64 = 180_000;

#[derive(Debug, Clone)]
pub struct FirmwareConfig {
    /// Northbound core LDJSON address (loopback once the core is on-device).
    pub core_addr: String,
    /// v1 mechanism (#11/#12): does THIS build declare the `wifi`
    /// capability (a radio is present)? A form without one must NOT execute
    /// a WifiConfig frame — the privileged-frame gate turns it into a
    /// protocol refusal instead of a silent network change. The probe task
    /// (Wi-Fi health lamp) runs only when this is set.
    pub wifi: bool,
    /// v0 stable id this component hosts.
    pub device_id: String,
    pub model: String,
    /// Descriptor firmware tag — identifies the form northbound.
    pub version: String,
    /// Persistent state directory (volume survives reboots; eMMC-light).
    pub state_dir: PathBuf,
    /// Deployment volume ceiling 0..=1 (every path through it, like the
    /// adapter's DOT2_VOLUME_CAP heritage).
    pub volume_cap: f64,
    /// 2ch [mic|ref] uplink (AecState.reference "available").
    pub hw_ref: bool,
    /// Capture channels mixed into the mono uplink (0-based, of 9).
    pub mic_channels: Vec<usize>,
    /// Reference channel for hw_ref (ch8 semantics pending A/B measurement).
    pub ref_channel: usize,
    /// Device-side AEC pipeline (G6): per-mic speex -> fusion -> mono.
    pub aec: bool,
    /// Echo tail in samples (multiple of aec::FRAME). Default aec::TAIL;
    /// overridable via --aec-tail so the tail→CPU table (pre-freeze
    /// account ②, 2026-11-08) measures points on ONE binary.
    pub aec_tail: usize,
    /// AEC measurement mode: dual-track pre/post PCM to the state dir
    /// (bounded) for the ERLE procedure (acceptance doc §0).
    pub aec_measure: bool,
    /// Window length of the dual-track measurement (default 120 s). T4
    /// acceptance needs several minutes of pre/post audio for a batch of
    /// keyword attempts; ~64 KB/s of /data writes is still negligible.
    pub aec_measure_secs: u64,
    /// Loopback self-check (D/hardware task B, 2026-11-10): while set, the
    /// playback thread writes what it actually sent to the DAC (16k mono
    /// downmix, post gain/loudness) and the capture thread writes ch8 to
    /// two tmpfs files, so the core can cross-correlate them after a known
    /// test tone. Off by default — /run writes never happen in normal runs.
    pub loopback_probe: bool,
    /// Output loudness stage (D stage 2026-11-10, RUI parameter
    /// migration): volume-tiered bass/presence/gain ladder + look-ahead
    /// limiter, applied to the mixed signal BEFORE the dealias reference
    /// feed. Default ON; `--loudness off` A/Bs against the flat chain.
    pub loudness: bool,
    /// D 1.1 A/B: scale on the loudness bass shelf (1.0 = RUI ladder).
    pub loudness_bass: f32,
    /// M6c A/B: which curve the loudness stage applies. `None` = the env
    /// seam (`HM_LOUDNESS_CURVE`, default RUI); `--loudness-curve official`
    /// selects the curve fitted from the stock EQ_50 FIR on the device.
    pub loudness_curve: Option<loudness::Curve>,
    /// M6c A/B: the stock 4-band MBCL replaces the broadband limiter.
    /// `None` = the env seam (`HM_LOUDNESS_MBCL`), default off. Only exists
    /// with the official curve.
    pub loudness_mbcl: Option<bool>,
    /// A user-defined peaking chain (`--loudness-curve custom`). Only used
    /// when the curve is Custom; the same list the console editor will send.
    pub eq_bands: Vec<loudness::CustomBand>,
    /// Playback ALSA buffer in frames (D/T5 debt #4). Default 8192 =
    /// 170 ms at 48k; `--play-buffer 4096` halves it for the A/B. Must be
    /// a multiple of PLAY_PERIOD and >= 2 periods (enforced by the CLI).
    pub play_buffer: u32,
    /// Boot-time capture health sentinel (D/T5 debt #1): classify the first
    /// ~2 s of 9ch capture and correlate ch8 against the DAC reference on
    /// the first playback. Off = no classification at all.
    pub boot_health: bool,
    /// Keyword spotting sidecar (D/T4, 2026-11-10): the firmware feeds its
    /// post-AEC mono block to `hm-kws` on its own core and forwards
    /// detections north. Off = no sidecar process at all.
    pub kws: bool,
    /// Sidecar bundle (driver + loader + libs + model + run.sh).
    pub kws_dir: PathBuf,
    /// Core the sidecar/external worker is pinned to (cpu3: cpu0 kernel/net,
    /// cpu1 AEC/playback, cpu2 core — the approved pinning map).
    pub kws_cpu: usize,
    /// Semantic barge-in detector (D/T5, 2026-11-10): near-end speech
    /// onset while the voice channel plays, guarded by the near-end vs
    /// DAC-reference cross-correlation (RUI Local defense ①) and reported
    /// north with onset pre-roll (defense ②). Requires AEC — without it
    /// the uplink is pure echo and the detector would fire on our own
    /// playback; `barge_in && aec` is the effective enable.
    pub barge_in: bool,
    /// Absolute silence floor (dBFS) for a candidate slice. Not the
    /// near-end criterion (that is `barge_in_margin` below the reference).
    pub barge_in_db: f32,
    /// Near-end margin below the DAC reference (dB). A slice within this
    /// many dB of the reference is a candidate; 15 dB is the R1 Step-3
    /// acceptance number.
    pub barge_in_margin: f32,
    /// Consecutive echo-only blocks with >= the margin of measured
    /// suppression required before a near-end fact is allowed (5 = 400 ms).
    pub barge_in_converged: u32,
    /// Fire requires the mic-vs-reference correlation BELOW this.
    pub barge_in_corr: f32,
    /// Playback settle window in ms after the voice channel starts (D/T5
    /// R1): candidate onsets inside it are the answer's own TTS onset
    /// transient while the AEC relocks — suppressed, never reported.
    pub barge_in_settle: u64,
    /// R1d local stop: minimum age of the live voice play (ms) before the
    /// device may cut it on its own reflex. Mirrors the core's answer
    /// protection period — inside it a "near-end" fact is the TTS onset
    /// transient, and cutting there is exactly the self-hearing regression
    /// the 2026-11-09 acceptance caught. Gateway and core must agree or the
    /// device would stop audio the core then refuses to explain.
    pub barge_in_protect: u64,
    /// Calibration telemetry interval in ms (0 = off): while voice plays,
    /// log `[barge-in] stat corr=… energy=… ref=… voice_ms=…` for the
    /// three-group correlation distributions. Off in normal operation.
    pub barge_in_stats: u64,
    /// v0.3 §4 (amended 2026-11-10): per-stream ahead-of-playback cap in
    /// 48k samples before `stream_overflow` fails the task (default 60s;
    /// the original 10s chopped long answers mid-word — see
    /// STREAM_AHEAD_LIMIT).
    pub stream_ahead: u64,
    /// 2026-11-13: fail a stream whose data write has made no progress for
    /// this long (ms). 0 = off. A live-but-starved stream (waiting for the
    /// next chunk) is not a stall; a blocked/frozen DAC write is. The receipt
    /// is `failed` and — M4 — the process then exits nonzero so the
    /// supervisor can restart it into a fresh codec bring-up (a dead DAC
    /// path cannot be healed in place).
    pub play_watchdog_ms: u64,
    /// M4: no successful capture block for this long (ms) is an audio fault
    /// (panicked/dead capture thread: a box that cannot hear is dead even if
    /// it can still talk). 0 = off.
    pub capture_watchdog_ms: u64,
    /// M4: leave the process on an unrecoverable audio fault. Off only for
    /// contract tests that assert the receipts in-process; production keeps
    /// this on or the box never recovers.
    pub exit_on_audio_fault: bool,
    /// M4 test hook (0 = off): request an audio fault this many ms after
    /// boot, through the REAL exit path. It exercises the receipt → bounded
    /// exit → supervisor restart loop on-device; it does NOT simulate a
    /// wedged kernel (that would be unrecoverable by design).
    pub fault_inject_ms: u64,
    pub capture_pcm: (u32, u32),
    pub playback_pcm: (u32, u32),
    pub led_frame_path: String,
    pub button_devices: Vec<String>,
    /// Hardware & per-unit acoustic profile (#25).
    pub profile: profile::DeviceProfile,
    /// Night-mode LED dimming schedule (#35).
    pub night_mode: leds::NightMode,
    /// LED ring physical orientation (#35).
    pub led_orientation: leds::LedOrientation,
    /// Bounded timeout for local Wi-Fi provisioning mode (#23).
    pub wifi_provisioning_timeout_ms: u64,
}

impl Default for FirmwareConfig {
    fn default() -> Self {
        let profile = profile::DeviceProfile::default();
        Self {
            core_addr: "127.0.0.1:8932".into(),
            // dot2-direct carries a radio; a radio-less form flips this off.
            wifi: true,
            device_id: profile.unit_id.clone(),
            model: profile.model.clone(),
            // <version> <build-id> (flavor): the build id lets a release
            // manifest prove the firmware came from a committed tree and lets
            // health confirm the binary that is actually running.
            version: format!(
                "hm-firmware {} {} {}",
                env!("CARGO_PKG_VERSION"),
                env!("HM_FIRMWARE_BUILD"),
                os_flavor()
            ),
            state_dir: "/data/local/tmp/hm-direct".into(),
            volume_cap: profile.volume_cap,
            hw_ref: false,
            aec: false,
            aec_tail: aec::TAIL,
            aec_measure: false,
            aec_measure_secs: 120,
            loopback_probe: false,
            loudness: true,
            loudness_bass: 1.0,
            loudness_curve: None,
            loudness_mbcl: None,
            eq_bands: Vec::new(),
            play_buffer: PLAY_BUFFER,
            boot_health: true,
            kws: false,
            kws_dir: "/data/local/tmp/hm-kws".into(),
            kws_cpu: 3,
            barge_in: true,
            barge_in_db: profile.vad_threshold,
            barge_in_margin: bargein::Config::default().near_end_margin_db,
            barge_in_converged: bargein::Config::default().converged_blocks,
            barge_in_corr: bargein::Config::default().corr_max,
            barge_in_settle: bargein::Config::default().settle_ms,
            barge_in_protect: DEFAULT_BARGE_PROTECT_MS,
            barge_in_stats: 0,
            stream_ahead: STREAM_AHEAD_LIMIT,
            play_watchdog_ms: 10_000,
            capture_watchdog_ms: 10_000,
            exit_on_audio_fault: true,
            fault_inject_ms: 0,
            // v1 assumption: ch0..6 = mic array, ch8 = playback reference.
            // Corrected from live channel energies during the A/B window.
            mic_channels: vec![0, 1, 2, 3, 4, 5, 6],
            ref_channel: 8,
            capture_pcm: (0, 24),
            playback_pcm: (0, 23),
            led_frame_path: "/sys/devices/soc/11007000.i2c/i2c-0/0-003f/frame".into(),
            button_devices: vec!["/dev/input/event1".into(), "/dev/input/event2".into()],
            profile,
            night_mode: leds::NightMode::default(),
            led_orientation: leds::LedOrientation::default(),
            wifi_provisioning_timeout_ms: DEFAULT_WIFI_PROVISIONING_TIMEOUT_MS,
        }
    }
}

impl FirmwareConfig {
    /// Apply a loaded per-unit `DeviceProfile` (#25), filling `model`,
    /// `volume_cap`, and `barge_in_db` (`vad_threshold`) unless explicitly
    /// overridden via CLI flags.
    pub fn apply_profile(
        &mut self,
        profile: profile::DeviceProfile,
        explicit_model: bool,
        explicit_volume_cap: bool,
        explicit_barge_in_db: bool,
    ) {
        self.profile = profile;
        if !explicit_model {
            self.model = self.profile.model.clone();
        }
        if !explicit_volume_cap {
            self.volume_cap = self.profile.volume_cap;
        }
        if !explicit_barge_in_db {
            self.barge_in_db = self.profile.vad_threshold;
        }
        if self.profile.model == "LX06" {
            self.capture_pcm = lx06::LX06_CAP_PCM;
            self.playback_pcm = lx06::LX06_PLAY_PCM;
            self.led_frame_path = lx06::LX06_LED_PATH.into();
            self.button_devices = vec![lx06::LX06_BUTTON_DEV.into()];
            self.mic_channels = vec![0, 1, 2, 3, 4, 5];
            self.ref_channel = 8;
        }
    }
}

/// Dispatch to the board-appropriate `Hardware` implementation based on
/// `cfg.profile.model` (`"LX06"` -> `Lx06Hardware`, default -> `DeviceHardware`).
pub fn open_hardware(cfg: &FirmwareConfig) -> io::Result<Arc<dyn Hardware>> {
    if cfg.profile.model == "LX06" {
        Ok(Arc::new(lx06::Lx06Hardware::open(cfg)?))
    } else {
        Ok(Arc::new(DeviceHardware::open(cfg)?))
    }
}

/// Resolve the production `stable_id` (#22): requires either an explicit
/// `--device-id` flag or a non-empty `<state_dir>/device-id` file. Production
/// startup must never silently fall back to `"dot2-d"`.
pub fn resolve_production_device_id(
    explicit: Option<&str>,
    state_dir: &std::path::Path,
) -> Result<String, String> {
    if let Some(id) = explicit {
        let trimmed = id.trim();
        if trimmed.is_empty() {
            return Err(
                "explicit --device-id is empty; refusing to start without a device identity".into(),
            );
        }
        return Ok(trimmed.to_string());
    }
    let id_path = state_dir.join("device-id");
    match std::fs::read_to_string(&id_path) {
        Ok(contents) => {
            let trimmed = contents.trim();
            if trimmed.is_empty() {
                Err(format!(
                    "device identity file {} is empty; refusing to fall back to a default stable_id",
                    id_path.display()
                ))
            } else {
                Ok(trimmed.to_string())
            }
        }
        Err(e) => Err(format!(
            "missing device identity: neither --device-id nor {} is available ({e}); refusing to fall back to a default stable_id",
            id_path.display()
        )),
    }
}

/// A physical button transition (key press edge only makes it north).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonEvent {
    Action,
    VolumeDown,
    VolumeUp,
    /// Autorepeat while the key is HELD (value 2). Separate from the first
    /// press because the two want different steps: a tap moves a coarse step,
    /// a held key walks finely. Dropping these (the pre-2026-11-15 behaviour:
    /// `_ => None`) made the volume key useless in practice — 16 taps from
    /// silence to full, which is what "调整音量不顺畅" turned out to be.
    VolumeDownRepeat,
    VolumeUpRepeat,
    /// Direct-mode Mute key short press: toggle the hardware mic mute.
    MuteToggle,
    /// Direct-mode long press: reported north, no local destructive action.
    Hold(u16, u64),
    Unknown(u16),
}

/// Deployment form label for the descriptor: emOS when its ramdisk
/// /etc/os-release is visible, FireOS direct otherwise. Same binary rolls
/// between platforms; the label keeps the console honest about which form
/// answered hello.
pub fn os_flavor() -> &'static str {
    match std::fs::read_to_string("/etc/os-release") {
        Ok(s) if s.lines().any(|l| l.trim() == "ID=emos") => "(emos)",
        _ => "(direct)",
    }
}

// --- hardware abstraction -------------------------------------------------------

pub trait Hardware: Send + Sync + 'static {
    /// Let a shared LED ring follow the active assistant.
    fn set_home_indicator(&self, _owner: lx06::IndicatorOwner) {}

    /// Blocking interleaved capture read of `frames` frames (S24_3LE 9ch).
    fn capture_read(&self, buf: &mut [u8], frames: u32) -> io::Result<u32>;
    /// Blocking interleaved playback write (`frames` stereo S16 frames).
    fn playback_write(&self, stereo: &[i16], frames: u32) -> io::Result<u32>;
    fn playback_flush(&self) -> io::Result<()>;
    fn playback_drain(&self) -> io::Result<()>;
    /// A shared playback client can close its own writer at final EOS. Direct
    /// DACs keep the silence clock to avoid their codec's power-cycle pop.
    fn playback_releases_when_idle(&self) -> bool {
        false
    }
    fn playback_underruns(&self) -> u64;
    /// Frames still queued between the application pointer and the DAC
    /// (M6a). `None` = the platform cannot say — the receipt then reports
    /// `progress_basis: "written"` instead of inventing a consumption count.
    fn playback_delay_frames(&self) -> Option<u64> {
        None
    }
    fn led_write(&self, rgb: &[(u8, u8, u8); leds::RING_LEDS]) -> io::Result<()>;
    /// Blocking wait for the next button press.
    fn button_next(&self) -> io::Result<ButtonEvent>;
    /// Hardware mic mute across all four ADC chips (privacy gate).
    fn set_adc_mute(&self, _on: bool) -> io::Result<()> {
        Ok(())
    }
    /// Set the system clock (G6 replaced-capability #2: emOS ships no
    /// time service). Returns success; callers log, never fatal.
    fn set_system_time(&self, _ts_ms: u64) -> bool {
        false
    }
    fn wake_lock(&self) -> io::Result<()>;
}

/// Production hardware: exclusive ALSA PCM pair + is31fl3236 sysfs + evdev
/// buttons + wake_lock. Opening capture while the EchoMuse server still
/// holds it returns EBUSY — the supervisor counts that as a fast-exit and
/// rolls back (design doc §2.4).
pub struct DeviceHardware {
    capture: Mutex<alsa::Pcm>,
    playback: Mutex<alsa::Pcm>,
    ring: std::sync::Mutex<leds::LedRing>,
    buttons: buttons::SharedButtonHub,
    ctl: Arc<Mutex<mixer::Ctl>>,
}

impl DeviceHardware {
    pub fn open(cfg: &FirmwareConfig) -> io::Result<DeviceHardware> {
        // Codec bring-up FIRST (user direction 2026-11-08): emOS has no
        // Android HAL, DAPM leaves both converters powered down, and DAPM
        // decides what to power at stream open — routes must close before
        // the PCMs do. On FireOS the values match what the HAL would set.
        let ctl = mixer::Ctl::open("/dev/snd/controlC0")
            .map_err(|e| io::Error::new(e.kind(), format!("controlC0 open for bring-up: {e}")))?;
        fwlog!("[bringup] controlC0: {} controls enumerated", ctl.known());
        let (failed, total) = bringup::pre_pcm(&ctl);
        if failed == total && total > 0 {
            return Err(io::Error::other(
                "every pre-PCM mixer write failed — is the codec present?",
            ));
        }
        let mut cap = alsa::Pcm::open(cfg.capture_pcm.0, cfg.capture_pcm.1, true)?;
        cap.configure(
            true,
            CAP_FORMAT_S24_3LE,
            CAP_CHANNELS as u32,
            CAP_RATE,
            CAP_PERIOD,
            CAP_BUFFER,
        )?;
        let mut play = alsa::Pcm::open(cfg.playback_pcm.0, cfg.playback_pcm.1, false)?;
        // D/T5 debt #4: the 170 ms buffer is a knob, not a constant. The
        // A/B (4096 vs 8192) runs on ONE binary with --play-buffer.
        fwlog!(
            "[alsa] playback buffer={} frames ({:.1}ms) period={} frames ({:.1}ms)",
            cfg.play_buffer,
            cfg.play_buffer as f64 * 1000.0 / PLAY_RATE as f64,
            PLAY_PERIOD,
            PLAY_PERIOD as f64 * 1000.0 / PLAY_RATE as f64,
        );
        play.configure(
            false,
            PLAY_FORMAT_S16_LE,
            PLAY_CHANNELS,
            PLAY_RATE,
            PLAY_PERIOD,
            cfg.play_buffer,
        )?;
        // Click-free amp bring-up: the DAC must clock silence before the amp
        // sees it (EchoMuse pcm_speaker.go 2026-07-10). Two periods of zeros
        // ≈ 85 ms at 48k/2048, then amp On, settle, DAC volume to unity.
        {
            let silence = vec![0u8; PLAY_PERIOD as usize * PLAY_CHANNELS as usize * 2];
            bringup::amp_up(&ctl, || {
                let _ = play.writei(&silence, PLAY_PERIOD);
                let _ = play.writei(&silence, PLAY_PERIOD);
            });
        }
        // AUX/headphone jack (user request 2026-11-15): amp_up just turned the
        // built-in amp On unconditionally, so the jack state has to be applied
        // AFTER it — and the boot-time reading counts even without an edge.
        let ctl = Arc::new(Mutex::new(ctl));
        let boot_jack = jack::read_state();
        jack::apply_now(&ctl.lock().unwrap_or_else(|e| e.into_inner()), boot_jack);
        jack::spawn_watcher(ctl.clone(), boot_jack);
        let hub = buttons::SharedButtonHub::open(&cfg.button_devices)?;
        Ok(DeviceHardware {
            capture: Mutex::new(cap),
            playback: Mutex::new(play),
            // D/T5 debt #6: use the physically-calibrated LED order when a
            // `led-map.json` exists (see tools/led_calibrate.py + runbook).
            ring: std::sync::Mutex::new(leds::LedRing::with_calibrated_map(
                &cfg.led_frame_path,
                &cfg.state_dir.join("led-map.json"),
            )),
            buttons: hub,
            ctl,
        })
    }
}

impl Hardware for DeviceHardware {
    fn capture_read(&self, buf: &mut [u8], frames: u32) -> io::Result<u32> {
        self.capture
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .readi(buf, frames)
    }
    fn playback_write(&self, stereo: &[i16], frames: u32) -> io::Result<u32> {
        let bytes =
            unsafe { std::slice::from_raw_parts(stereo.as_ptr().cast::<u8>(), stereo.len() * 2) };
        self.playback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .writei(bytes, frames)
    }
    fn playback_flush(&self) -> io::Result<()> {
        self.playback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .flush()
    }
    fn playback_drain(&self) -> io::Result<()> {
        self.playback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
    }
    fn playback_underruns(&self) -> u64 {
        self.playback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .underruns()
    }
    fn playback_delay_frames(&self) -> Option<u64> {
        self.playback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .delay_frames()
            .ok()
    }
    fn led_write(&self, rgb: &[(u8, u8, u8); leds::RING_LEDS]) -> io::Result<()> {
        self.ring
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write(rgb)
    }
    fn set_adc_mute(&self, on: bool) -> io::Result<()> {
        bringup::adc_mute(&self.ctl.lock().unwrap_or_else(|e| e.into_inner()), on)
    }
    fn button_next(&self) -> io::Result<ButtonEvent> {
        self.buttons.next()
    }
    fn set_system_time(&self, ts_ms: u64) -> bool {
        // The libc crate carries the correct timeval layout per target
        // (musl armv7: 32-bit fields). Failure is reported, not fatal.
        let tv = libc::timeval {
            tv_sec: (ts_ms / 1000) as libc::time_t,
            tv_usec: ((ts_ms % 1000) * 1000) as libc::suseconds_t,
        };
        unsafe { libc::settimeofday(&tv, std::ptr::null()) == 0 }
    }
    fn wake_lock(&self) -> io::Result<()> {
        std::fs::write("/sys/power/wake_lock", "hm-direct")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chan {
    Voice,
    Music,
}

/// v0.3 §4 (amended 2026-11-10): pending-ahead cap before a stream is
/// failed with `stream_overflow`. Was 10s — a "tell me a story" answer
/// (synthesis ~2x realtime) hit it at ~12s of playback and got chopped
/// mid-word: an unfaded hard cut, audible as a loud click (forensic run3,
/// −13 dBFS → digital zero in <5 ms). The durable fix is core-side send
/// pacing (T3.5); this raises the ceiling so real-length answers play.
///
/// R1d measurement (2026-11-12, device log): a real long story synthesized at
/// ~4x realtime — `pushed_ms=81136 played_ms=20821 ahead_ms=60314` — so 60 s
/// of AHEAD is only ~20 s of playback and the answer was cut there. The
/// number that matters is the answer length, not the wall clock, so the cap
/// now admits a 3-minute answer: 180 s at 48k mono i16 ≈ 17 MB pending, still
/// small against this device's memory and bounded per channel.
const STREAM_AHEAD_LIMIT: u64 = 180 * 48_000;

/// The shipping link is the core on this same speaker. Accept only a
/// loopback socket (`127.0.0.1`, `::1`, or the name `localhost`).
pub fn core_addr_is_loopback(addr: &str) -> bool {
    let addr = addr.trim();
    if let Ok(sa) = addr.parse::<std::net::SocketAddr>() {
        return sa.ip().is_loopback();
    }
    let Some((host, port)) = addr.rsplit_once(':') else {
        return false;
    };
    if port.parse::<u16>().is_err() {
        return false;
    }
    let host = host.trim_matches(|c| c == '[' || c == ']');
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}
