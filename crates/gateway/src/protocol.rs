//! Device protocol v0 frames (see the core repo's docs/contracts-v0.md —
//! the authoritative schema source) plus the newline-delimited JSON codec.
//!
//! G1 transport decision: TCP with newline-delimited JSON (LDJSON); the
//! gateway dials out, the core listens. Audio in v0 is inline base64 WAV
//! inside `play` frames; streaming PCM frames are a later protocol version.
//!
//! Every frame is internally tagged with `"type"`. Unknown frame types fail
//! decoding (v0 is strict — fail closed rather than guess semantics).

use serde::{Deserialize, Serialize};
mod northbound;
pub use northbound::{NorthboundFault, NorthboundStatus};

/// Protocol version this build speaks. Compatibility with the core is
/// INTERVAL intersection (`max(both min) <= min(both max)`, see
/// `protocol_operating_version`), not equality — so a mixed window (this
/// firmware restarted first, the OLD core still compares strictly) keeps
/// talking at 0. The 0 -> 1 bump is a separate paired release once the
/// whole fleet runs the mechanism (home-mate-b#8 / #11, 2026-11-16).
pub const PROTOCOL_VERSION: u32 = 2;

/// Lowest peer protocol version this build accepts (see PROTOCOL_VERSION).
pub const PROTOCOL_MIN_VERSION: u32 = 0;

/// Explicit protocol versions this build implements ([0, 2]; wire 1 is never invented).
pub const SUPPORTED_PROTOCOLS: &[u32] = &[0, 2];

/// Maximum encoded size (bytes) for config_apply_v2 control frames (64 KiB).
pub const MAX_CONFIG_FRAME_BYTES: usize = 64 * 1024;

/// Acoustic profile version advertised by the dot2 array post-AEC pipeline.
pub const PROFILE_VERSION: &str = "dot2-array-postaec-v1";

/// Effective configured echo tail at the fixed 16 kHz capture rate.
/// A disabled pipeline has no effective tail.
pub fn aec_tail_ms(enabled: bool, tail_samples: usize) -> Option<u32> {
    if !enabled {
        return None;
    }
    u64::try_from(tail_samples)
        .ok()?
        .checked_mul(1000)
        .and_then(|n| u32::try_from(n / 16_000).ok())
}

/// Echo path health state reported by the single acoustic evaluator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EchoPathHealth {
    Healthy,
    Degraded,
    Unknown,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// --- shared sub-structures ---------------------------------------------------

/// A device announced at handshake. `capabilities` uses the contract's
/// names: text_in / audio_in / audio_out / wake_event / volume / stop …
/// A device must not fabricate receipts for capabilities it lacks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HelloDevice {
    pub stable_id: String,
    pub model: String,
    #[serde(default)]
    pub firmware: Option<String>,
    pub capabilities: Vec<String>,
    /// Output/input path label from device-baseline evidence (e.g. dot2-d =
    /// "fireos_builtin_speaker"); the core displays it, never branches on it.
    pub audio_profile: String,
    /// v0.2.3: associated SSID (online WiFi change support). None on forms
    /// without a radio (virtual terminal) or before association.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wifi_ssid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AudioMeta {
    pub format: String,
    pub sample_rate: u32,
    pub channels: u16,
}

/// Duplex state reported per device: capability / requested / negotiated /
/// effective are kept apart (voice-interaction contract: the model never
/// gets to declare a mode "effective" by itself).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DuplexState {
    pub capability: String, // "half" | "full"
    pub requested: String,
    pub negotiated: String,
    pub effective: String,
}

/// AEC ownership facts. `owner` is the single owner of the AEC stage —
/// `"device"` (the dot2-d EchoMuse AEC runs in the firmware audio thread),
/// `"core"` (the core cancels echo itself; no such path ships today), or
/// `"none"` (a text-only terminal with no audio path at all). `reference`
/// tells whether a reference signal is available. The core never guesses:
/// an empty owner is reported as unknown.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AecState {
    pub owner: String,
    pub reference: String, // "available" | "absent"
}

/// Closed union of typed configuration blocks for `config_apply_v2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigBlock {
    Kws,
    AudioEq,
    Led,
    Duplex,
    Acoustics,
}

impl ConfigBlock {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Kws => "kws",
            Self::AudioEq => "audio_eq",
            Self::Led => "led",
            Self::Duplex => "duplex",
            Self::Acoustics => "acoustics",
        }
    }
}

/// Lifecycle status of a `ConfigReceipt` (`config_apply_v2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigReceiptStatus {
    Validated,
    Persisted,
    Active,
    Failed,
    Unsupported,
    Superseded,
}

/// Per-block current/high-water state reported in `ConfigState`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigBlockState {
    pub block: ConfigBlock,
    #[serde(default)]
    pub block_generation: u64,
    #[serde(default)]
    pub operation_id: String,
    #[serde(default)]
    pub data_epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persisted_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_values: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<serde_json::Value>,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

// --- gateway → core -----------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GatewayFrame {
    Hello {
        protocol: u32,
        /// v1 mechanism: the LOWEST peer protocol version this gateway
        /// accepts. Absent (an older gateway) = exactly 0.
        #[serde(default)]
        min_protocol: u32,
        /// Explicit protocol versions supported by this peer (`[0, 2]`).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        supported_versions: Vec<u32>,
        gateway: String,
        /// v1 mechanism: the firmware PACKAGE version ("0.0.53"),
        /// machine-readable. Empty = an older gateway that does not say.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        gateway_version: String,
        /// Effective configured echo tail, not the compile-time default.
        /// None when AEC is disabled or the sender does not report it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        aec_tail_ms: Option<u32>,
        /// Acoustic profile version (e.g. "dot2-array-postaec-v1").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile_version: Option<String>,
        devices: Vec<HelloDevice>,
    },
    Wake {
        device: String,
        ts: u64,
        seq: u64,
    },
    Utterance {
        device: String,
        ts: u64,
        seq: u64,
        source: String, // "text" for virtual terminals, "asr" later
        text: String,
    },
    Button {
        device: String,
        ts: u64,
        seq: u64,
        action: String,
    },
    MuteState {
        device: String,
        ts: u64,
        seq: u64,
        muted: bool,
        source: String,
        /// v1 mechanism (#13): echo of the `MuteSet.task_id` that caused
        /// this state. None = a device fact (key press, boot report) —
        /// such a fact must never settle a remote command's waiter.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
    },
    PlayReceipt {
        task_id: String,
        stage: String, // accepted|submitted|sent|device_confirmed|failed|unknown
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        ts: u64,
    },
    /// #23: Incoming Play or PlayBegin rejected while the device cannot accept
    /// new playback (e.g. `reason: "wifi_provisioning"`).
    PlayRejected {
        task_id: String,
        device: String,
        reason: String,
        ts: u64,
    },
    StopReceipt {
        task_id: String,
        stage: String, // sent|device_confirmed|unknown
        /// Hardware truth (R0, 2026-11-13): 48k mono samples the DAC actually
        /// took for this play before the stop, in ms. Present only when a
        /// flush answered a stop (`None` = nothing was playing under that
        /// task id, or the stop asked for no flush). Additive and optional —
        /// an older core ignores it, so the protocol version stays v0.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        played_ms: Option<u64>,
        /// M6a: `played_ms` minus the backlog the hardware still held when
        /// the drop was issued. Absent = not observable.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        consumed_ms: Option<u64>,
        /// "hw_delay" | "written" (see PlayFinished).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        progress_basis: Option<String>,
        /// M6a: the flush FAILED — the channel may still be audible. Stage is
        /// then "unknown", never "device_confirmed".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    VolumeReceipt {
        task_id: String,
        effective: f64,
        unit: String,
        stage: String,
        /// R2 maintenance (2026-11-13): which device this fact belongs to.
        /// Additive and optional — an older core ignores it, an older
        /// firmware omits it and the core falls back to its pending map.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device: Option<String>,
    },
    ModeReport {
        device: String,
        duplex: DuplexState,
        aec: AecState,
    },
    /// v0.1 backfill: the device finished consuming a playback (its own
    /// observation — not a promise the user heard it all).
    PlayFinished {
        device: String,
        task_id: String,
        played_ms: u64,
        /// M6a (2026-11-13): how much of `played_ms` the DAC is estimated to
        /// have CONSUMED when the stream drained — `played_ms` minus the
        /// hardware backlog reported by SNDRV_PCM_IOCTL_DELAY. Absent when
        /// the platform cannot report the backlog (then `progress_basis` is
        /// "written" and the caller must not treat played_ms as heard).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        consumed_ms: Option<u64>,
        /// "hw_delay" (consumed_ms measured against the hardware pointer) or
        /// "written" (only the write count is known).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        progress_basis: Option<String>,
        ts: u64,
    },
    /// v0.2.3 (stage C): outcome of a WifiConfig change, reported north
    /// once the join watchdog decides. `state` is "applied" (associated
    /// with the new SSID) or "reverted" (watchdog timeout — backup
    /// restored, old network re-joined); `detail` carries diagnostics.
    WifiState {
        device: String,
        ts: u64,
        ssid: String,
        state: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// v0.2 (G3): one block of uplink PCM (s16le, 80 ms cadence from the
    /// dot2 adapter). `layout` "mono" | "mic_ref" (2ch [mic|ref]).
    /// Optional `vad_speech` / `vad_prob` carry gateway-side neural Silero VAD
    /// decisions (Stage 2 remote VAD pass-through; omitted when unavailable so
    /// core falls back to local/energy VAD).
    AudioChunk {
        device: String,
        ts: u64,
        seq: u64,
        pcm_b64: String,
        format: String,
        sample_rate: u32,
        channels: u16,
        layout: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        vad_speech: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        vad_prob: Option<f32>,
    },
    /// v0.2: achieved mic-session state (answer to mic_session + honest
    /// transitions when the southbound device drops).
    MicReport {
        device: String,
        active: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        layout: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sample_rate: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channels: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// v0.2 (mapping gap G6): per-device online/offline INSIDE a live
    /// gateway connection. `descriptor` present when introducing a device.
    DeviceStatus {
        device: String,
        online: bool,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        descriptor: Option<HelloDevice>,
        /// Optional diagnostics schema 1; public authority is Core contracts-v0.md.
        /// Value preserves additive unknown-schema decoding at negotiated wire 0.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        northbound: Option<serde_json::Value>,
    },
    /// v0.2.4 (D/T4): the on-device keyword spotter recognized a fixed phrase
    /// in the post-AEC stream. A FACT, not a decision — whether a live
    /// playback stops is the core's call for every word EXCEPT the retreat
    /// set (v0.5 local exit: a deployed retreat word cuts the audio on the
    /// device BEFORE this frame's twin `local_exit_flushed`, then closes the
    /// session north). A false WAKE-word positive therefore cannot stop or
    /// start anything on its own; a false retreat-word positive costs one
    /// silence, never a semantics execution. `fed_ms` is the stream position
    /// at detection (diagnostics only; the wall clock is `ts`).
    Keyword {
        device: String,
        ts: u64,
        seq: u64,
        keyword: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fed_ms: Option<u64>,
        /// v0.2.7 (R1g): the device's echo guard believes the microphone
        /// heard the SPEAKERS, not a person. It is a HINT, not a verdict: the
        /// guard cannot tell "our own answer said the phrase" from "a person
        /// said it over loud playback", and suppressing it here left the user
        /// unable to interrupt their own story (device 2026-11-12). The core
        /// decides — it knows whether OUR live text contains the phrase.
        #[serde(default)]
        self_heard: bool,
        /// v0.2.9 (wake arbitration, 2026-11-15): the acoustic strength of
        /// the microphone at this hit, in dBFS. THE SPATIAL EVIDENCE: when
        /// one phrase wakes five speakers, the one that heard it loudest is
        /// usually the one in the room with the person. Measured over the
        /// RAW post-AEC mono block the spotter itself is fed (Core alone
        /// applies the per-unit arbitration calibration), kept as a short
        /// window PEAK so a detection reported `lag_ms` later still describes
        /// the phrase rather than the silence after it. None = not measured
        /// (never 0.0, which is a real and very loud level).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rms_dbfs: Option<f32>,
        /// v0.2.9: how stale the newest fed sample was when the model
        /// produced the result (the driver's `lag_ms`). `ts - lag_ms` is when
        /// the LAST audio of the phrase reached this device — comparable
        /// across nodes with NTP-synced clocks, which `fed_ms` (a per-boot
        /// stream position) is not.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lag_ms: Option<u64>,
        /// Active KWS bundle identity (hash/generation) for `config_apply_v2` (#112).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bundle_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kws_generation: Option<u64>,
    },
    /// v0.2.5 (D/T5): the device heard GENUINE near-end speech while a
    /// voice-channel playback was live (onset + echo cross-correlation
    /// guard), and it kept the audio from before the detection so the
    /// first syllable survives. A fact from the reflex layer; whether the
    /// playback stops and the speech becomes a turn is the CORE's decision
    /// (voice.rs::on_barge_in — same shape as the keyword path).
    BargeIn {
        device: String,
        ts: u64,
        seq: u64,
        /// Onset pre-roll: the last ≤400 ms of the post-AEC mono stream,
        /// ending at the detection block. S16LE @16k mono, base64.
        pcm_b64: String,
        sample_rate: u32,
        channels: u16,
        /// The guard's own value: max normalized cross-correlation of the
        /// pre-AEC mic against the DAC reference at the fire moment.
        /// Below the device's guard threshold — that is why this fired.
        corr: f32,
        /// Onset slice level (dBFS RMS) the detector saw.
        energy_db: f32,
        /// The DAC reference level (dBFS RMS) in the same block. The core
        /// re-checks the relative near-end criterion with this.
        #[serde(default)]
        ref_db: f32,
        /// Near-end EXCESS over the reference: `energy_db - ref_db` (dB).
        /// A certified fact sits within the detector's 15 dB margin
        /// (`residual_db > -15`); echo-only blocks are far below it. The
        /// sign is positive when the person is louder than the playback —
        /// reporting suppression here made a loud person look like echo
        /// (fixed 2026-11-12, R1d).
        #[serde(default)]
        residual_db: f32,
        /// Every 20 ms reference frame in the detection block was verified
        /// against the output ring. False = the fact is not trustworthy.
        #[serde(default)]
        ref_locked: bool,
    },
    /// Echo path health transition or sustained degradation telemetry.
    EchoHealth {
        device: String,
        health: EchoPathHealth,
        degraded_ms: u64,
        reason: String,
    },
    /// Near-end speech presence telemetry (loopback only, <= 1 per 80 ms block).
    NearEndPresence {
        device: String,
        ts_ms: u64,
        duration_ms: u64,
        rms_dbfs: f32,
        noise_floor_dbfs: f32,
        snr_db: f32,
        clipped: bool,
        echo_health: EchoPathHealth,
        profile_version: String,
    },
    /// R1d (2026-11-12 night): the device stopped its own voice channel on
    /// the near-end reflex. A hardware FACT, not a request — the audio is
    /// already gone when this leaves. The semantic decision ("does this count
    /// as a turn") still rides the following `BargeIn`. `latency_ms` is the
    /// device's own fire→flush time, which a slow north link cannot inflate;
    /// `played_ms` is what the DAC actually took before the flush.
    StoppedLocally {
        device: String,
        task_id: String,
        /// "near_end" today; the field exists so a future reflex (keyword,
        /// button) can reuse the frame without another protocol change.
        reason: String,
        played_ms: u64,
        latency_ms: u64,
        /// Read-ahead left in the channel when the reflex cut it: proof (or
        /// not) that synthesis pacing holds on this answer. Diagnostic only.
        ahead_ms: u64,
        ts: u64,
    },
    /// v0.2.4 (D/T4): keyword-spotter lifecycle. Sent on every state change
    /// (starting → running → down) and on connect, so the core and console
    /// can say "keyword interrupt unavailable" instead of pretending the
    /// reflex exists.
    KwsReport {
        device: String,
        /// "starting" | "running" | "down"
        state: String,
        detail: String,
        restarts: u64,
        detections: u64,
        /// v0.2.5 (D/T5): self-heard hits the echo guard suppressed. It was
        /// only in the console JSON, never on the wire, so the core's
        /// `guarded` counter read 0 by construction (R1g/P7).
        #[serde(default)]
        guarded: u64,
        /// v0.2.6 (R1g/P7): this process's epoch. Lifetime counters outlive a
        /// core restart, so two numbers are only comparable inside one epoch.
        #[serde(default)]
        epoch: u64,
        /// M6b (v0.13, 2026-11-13): the FNV-1a of the keyword file the
        /// SPOTTER actually loaded (empty until its first `ready`). The
        /// deploy receipt's `hash` only proves what the device INSTALLED;
        /// this is the load proof.
        #[serde(default)]
        loaded_hash: String,
        /// When that load was confirmed (ms epoch, 0 = never).
        #[serde(default)]
        loaded_ms: u64,
        /// Contract v2 §2.5 (#5): retreat words armed for a LOCAL exit on this
        /// device right now. A device fact that survives core restarts, so a
        /// console reading only the core's deploy record cannot show a false
        /// "not armed" after a restart.
        #[serde(default)]
        local_exit: u32,
        /// Contract v2 #4: the sensitivity the LOADED model reported in its
        /// own `ready` line (0.0 = an older driver).
        #[serde(default)]
        threshold: f32,
        #[serde(default)]
        score: f32,
    },
    /// R1j/P1: outcome of [`CoreFrame::KwsDeploy`]. `hash` is the FNV-1a of
    /// the file the device holds now (empty when the deploy was rejected).
    KwsDeployReceipt {
        device: String,
        accepted: bool,
        hash: String,
        keywords: u64,
        detail: String,
        /// Contract v2 §2.5 (#5, 2026-11-16): how many retreat words this
        /// device armed for LOCAL exit (0 = nothing can be cut locally).
        #[serde(default)]
        local_exit: u32,
        /// The word→intent map version it stored (empty = none installed).
        #[serde(default)]
        intents_hash: String,
        /// Contract v2 #4: the sensitivity now installed (0 = not tuned).
        #[serde(default)]
        threshold: f32,
        #[serde(default)]
        score: f32,
    },
    /// Contract v2 §2.5 (#5): this device recognised a retreat word and cut
    /// its own audio before the core answered. A fact, not a decision: the
    /// core's `close_session` stays the only authority over the session, and
    /// the `Keyword` frame that rides with this one is what closes it.
    LocalExitFlushed {
        device: String,
        keyword: String,
        ts: u64,
        /// Hit → silence, measured when the hardware flush completed.
        latency_ms: u64,
        /// "voice" / "music" — the channels this device actually cleared.
        channels: Vec<String>,
        /// Set when a channel's flush failed (the audio may still be live).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// v1 mechanism (#11, 2026-11-16): this gateway refused the link — a
    /// version interval with no intersection, or a frame whose capability
    /// the device does not declare. Sent BEFORE the disconnect so the core
    /// records the reason. Decode reality: an OLD core logs and skips a
    /// runtime frame it cannot decode; a HELLO-stage frame IS fatal there.
    /// Old peers are never promised to understand this frame — EOF
    /// degradation accepted.
    ProtocolError {
        protocol: u32,
        #[serde(default)]
        min_protocol: u32,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        gateway_version: String,
        reason: String,
    },
    /// `config_apply_v2` current gateway configuration state & capabilities.
    ConfigState {
        request_id: String,
        device: String,
        gateway_boot_id: String,
        state_revision: u64,
        capabilities: Vec<String>,
        block_schemas: serde_json::Value,
        bounds: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_hash: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token_hash: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        time_basis: Option<serde_json::Value>,
        blocks: Vec<ConfigBlockState>,
    },
    /// `config_apply_v2` preflight validation result (no state mutation).
    ConfigValidated {
        request_id: String,
        device: String,
        gateway_boot_id: String,
        state_revision: u64,
        block: ConfigBlock,
        block_hash: String,
        supported: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        normalized_values: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capability_fingerprint: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_hash: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token_hash: Option<String>,
        #[serde(default)]
        blockers: Vec<String>,
    },
    /// `config_apply_v2` effective configuration receipt correlated by
    /// `operation_id` + `block_hash`.
    ConfigReceipt {
        device: String,
        gateway_boot_id: String,
        operation_id: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        application_id: String,
        #[serde(default)]
        data_epoch: u64,
        #[serde(default)]
        desired_revision: u64,
        #[serde(default)]
        block_generation: u64,
        block: ConfigBlock,
        block_hash: String,
        state_revision: u64,
        status: ConfigReceiptStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        persisted_hash: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_hash: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_values: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_code: Option<String>,
        #[serde(default)]
        retryable: bool,
    },
}

// --- core → gateway -------------------------------------------------------------

/// One peaking band of the user's EQ (`AudioEq`). Values are clamped by the
/// receiver: freq 20..16000 Hz, gain ±18 dB, Q 0.3..8.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct EqBand {
    pub freq: f32,
    pub gain_db: f32,
    pub q: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoreFrame {
    HelloAck {
        protocol: u32,
        /// v1 mechanism: lowest peer version this core accepts (see Hello).
        #[serde(default)]
        min_protocol: u32,
        /// Explicit protocol versions supported by the core (`[0, 2]`).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        supported_versions: Vec<u32>,
        /// Optional core capabilities (e.g. `config_apply_v2`).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        capabilities: Vec<String>,
        /// v1 mechanism: machine-readable core version. Empty = older core.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        core_version: String,
        generation: u64,
    },
    /// v1 mechanism (#11): the core refused the handshake — no version
    /// intersection, a capability gap, or duplicate device ids. Sent over
    /// the handshake socket before the connection closes so the lamp can be
    /// a protocol-error lamp with a recorded reason; best-effort (an old
    /// gateway that cannot decode it just sees the disconnect).
    HelloReject {
        protocol: u32,
        #[serde(default)]
        min_protocol: u32,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        core_version: String,
        reason: String,
    },
    Play {
        task_id: String,
        device: String,
        content_ref: String,
        audio: AudioMeta,
        /// Inline audio payload (base64 WAV in v0). Absent for simulated
        /// playback (music from the mock library), where `simulated_ms` runs
        /// the media channel without real samples.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio_b64: Option<String>,
        /// v0.1: output channel. "voice" (TTS/acks) and "media" (music) may
        /// run concurrently; barge-in/wake/stop halts both.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel: Option<String>,
        /// Transcript/label for terminals that display rather than speak.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        simulated_ms: Option<u64>,
        /// What a wake event during playback should do; v0: "stop_current".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        barge_in: Option<String>,
    },
    /// v0.3 (D/T3): streamed playback — begin, incremental raw-PCM chunks,
    /// end. The gateway advertises `play_stream` in its hello descriptor
    /// when it implements these; whole-WAV `play` stays forever as the
    /// fallback. Chunks carry bare S16LE PCM at the rate `play_begin`
    /// declares (no WAV headers); `seq` starts at 0 and must increase by
    /// exactly one — a gap or repeat is a poisoned stream (disconnect).
    PlayBegin {
        task_id: String,
        device: String,
        content_ref: String,
        audio: AudioMeta,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        barge_in: Option<String>,
    },
    PlayChunk {
        task_id: String,
        seq: u64,
        audio_b64: String,
    },
    PlayEnd {
        task_id: String,
    },
    /// R1j/P1 (v0.4): replace the device-side keyword vocabulary. The core
    /// compiles the file from the reflex configuration; THIS side owns the
    /// device assets — validate every token against `tokens.txt`, back up
    /// the previous file, write the new one atomically and restart the
    /// spotter. A token outside the table makes sherpa reject the WHOLE
    /// file while the driver still reports ready (measured 2026-11-12), so
    /// the receipt is a device fact, not an assumption.
    KwsDeploy {
        device: String,
        keywords: String,
        /// Contract v2 §2.5 (#5, 2026-11-16): the words in this file whose
        /// intent is `retreat`. They are kept as the LOCAL EXIT set, so a hit
        /// can silence the device without waiting for the core. Absent/empty
        /// (an older core) = no local exit; the hit is only reported north.
        #[serde(default)]
        retreat_words: Vec<String>,
        /// FNV-1a of the word→intent map these words came from, echoed back
        /// in the receipt so "installed" names the meaning, not just the file.
        #[serde(default)]
        intents_hash: String,
        /// Contract v2 #4 (2026-11-16): the spotter's sensitivity, passed to
        /// the launcher as `KWS_THRESHOLD`/`KWS_SCORE` (run.sh already reads
        /// them). Absent = leave the bundle's own default.
        #[serde(default)]
        threshold: Option<f32>,
        #[serde(default)]
        score: Option<f32>,
    },
    Stop {
        task_id: String,
        flush: bool,
    },
    /// 2026-11-14 (M6c-2): the output tone, applied live to the playback
    /// thread. `curve` picks the fixed voicing ("rui" | "official" |
    /// "echomuse" | "custom"); `bands` is the user's own peaking EQ on top
    /// of it (Custom: the bands ARE the curve). Empty bands = none.
    AudioEq {
        device: String,
        curve: String,
        #[serde(default)]
        bands: Vec<EqBand>,
    },
    VolumeSet {
        task_id: String,
        device: String,
        value: f64,
        unit: String, // core speaks linear 0..1; device scale mapping is here
    },
    /// v0.1 backfill: core requests a duplex mode; the gateway answers with
    /// a ModeReport carrying the negotiated/effective truth.
    ModeSet {
        device: String,
        duplex_requested: String,
    },
    /// v0.2 (G3): start/stop a device mic session (continuous mode only).
    MicSession {
        device: String,
        action: String, // "start" | "stop"
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<String>,
    },
    /// v0.2 (G4, mapping gap G8): assistant interaction state per device.
    /// Semantics are core-owned; THIS side maps the state to device
    /// presentation (dot2-d: LED ring, palette per forensics §5).
    AssistantState {
        device: String,
        state: String, // idle|listening|wake|thinking|speaking|music|muted|error
    },
    /// v0.2 (G6, replaced-capability #2): host clock projection right
    /// after hello (emOS has no time service; its clock sits in 2010 and
    /// C-stage TLS depends on sane time). The firmware sets the system
    /// clock (root) and logs the outcome; no reply is expected.
    TimeSet {
        ts_ms: u64,
    },
    /// v0.2.3 (stage C, replaced-capability #1-2): online WiFi change. The
    /// firmware backs up the active wpa config, applies the new network,
    /// and runs a join watchdog: if the new SSID does not associate within
    /// WIFI_JOIN_TIMEOUT the backup is restored and reconfigured (the
    /// device stays reachable — an unattended change can never strand it).
    /// Result returns north as WifiState.
    WifiConfig {
        ssid: String,
        psk: String,
    },
    /// v1 mechanism (#13, 2026-11-16): set the mute LATCH to the given
    /// value — the same latch the physical key toggles, no timeout, no
    /// auto-unmute, persisted like a key press. This is the ONE frame that
    /// may write the latch; nothing implicit ever may. The core sends it
    /// ONLY to devices that declare `remote_mute`; the answer is a
    /// `MuteState` fact echoing `task_id`, so a key press (no task id)
    /// cannot settle someone else's command.
    MuteSet {
        task_id: String,
        device: String,
        muted: bool,
    },
    /// #35: Night-mode LED dimming schedule and physical ring orientation.
    LedConfig {
        device: String,
        #[serde(default)]
        night_enabled: bool,
        #[serde(default = "default_night_start")]
        night_start_minute: u16,
        #[serde(default = "default_night_end")]
        night_end_minute: u16,
        #[serde(default = "default_night_brightness")]
        night_brightness_pct: u8,
        #[serde(default)]
        orientation_rotation: u8,
        #[serde(default)]
        orientation_reverse: bool,
    },
    /// `config_apply_v2`: query current gateway config state & capabilities.
    ConfigStateGet {
        request_id: String,
        device: String,
    },
    /// `config_apply_v2`: preflight validation (no file writes or sidecar restarts).
    ConfigValidate {
        request_id: String,
        device: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_gateway_boot_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_state_revision: Option<u64>,
        block: ConfigBlock,
        #[serde(default = "default_schema_version")]
        schema_version: u32,
        block_hash: String,
        values: serde_json::Value,
    },
    /// `config_apply_v2`: durable idempotent apply of one typed config block.
    ConfigApply {
        device: String,
        #[serde(default)]
        core_node_id: String,
        #[serde(default)]
        data_epoch: u64,
        #[serde(default)]
        application_id: String,
        operation_id: String,
        #[serde(default)]
        desired_revision: u64,
        #[serde(default)]
        block_generation: u64,
        block: ConfigBlock,
        #[serde(default = "default_schema_version")]
        schema_version: u32,
        block_hash: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_gateway_boot_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_state_revision: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capability_fingerprint: Option<String>,
        values: serde_json::Value,
    },
}

fn default_schema_version() -> u32 {
    1
}

fn default_night_start() -> u16 {
    1320
}

fn default_night_end() -> u16 {
    420
}

fn default_night_brightness() -> u8 {
    10
}

// --- v1 handshake mechanism (#11, 2026-11-16) ---------------------------------
//
// Compatibility is INTERVAL INTERSECTION, not "peer version inside my
// range": checked on both sides, that naive rule degenerates into
// "highest versions equal" (core [0,1] accepts firmware 0 but firmware
// [0,0] rejects core 1) and would fail the activation's firmware-confirm
// stage on every bump. The intersection rule lets [0,0] talk to [0,1] at 0.
// Handshake truth only — no replay/downgrade protection is intended
// (loopback trust domain).

/// `Some(operating)` when both accepted-intervals intersect; both sides
/// encode at this version. Behavior is driven by capabilities and optional
/// fields, never by branching on this number.
pub fn protocol_operating_version(mine: (u32, u32), peer: (u32, u32)) -> Option<u32> {
    let (my_min, my_max) = mine;
    let (peer_min, peer_max) = peer;
    if my_min > my_max || peer_min > peer_max {
        return None; // an illegal interval rejects, never sweeps under the rug
    }
    let lo = my_min.max(peer_min);
    let hi = my_max.min(peer_max);
    (lo <= hi).then_some(hi)
}

/// Negotiate operating protocol version using both intervals and explicit
/// `supported_versions` lists (`[0, 2]`). When `peer_supported` is empty
/// (a legacy peer), falls back to the highest version in `my_supported`
/// inside the interval intersection (so `[0, 1]` lands on `0`, and `[1, 1]`
/// is rejected rather than inventing wire 1).
pub fn protocol_operating_version_with_supported(
    mine: (u32, u32),
    my_supported: &[u32],
    peer: (u32, u32),
    peer_supported: &[u32],
) -> Option<u32> {
    let (my_min, my_max) = mine;
    let (peer_min, peer_max) = peer;
    if my_min > my_max || peer_min > peer_max {
        return None;
    }
    let lo = my_min.max(peer_min);
    let hi = my_max.min(peer_max);
    if lo > hi {
        return None;
    }
    my_supported
        .iter()
        .copied()
        .filter(|v| *v >= lo && *v <= hi)
        .filter(|v| peer_supported.is_empty() || peer_supported.contains(v))
        .max()
}

pub const REASON_PROTOCOL_NO_INTERSECTION: &str = "protocol_no_intersection";

/// `ProtocolError.reason` for a version interval with no overlap, both
/// intervals spelled out for the log and the lamp.
pub fn protocol_no_intersection_reason(mine: (u32, u32), peer: (u32, u32)) -> String {
    format!(
        "{REASON_PROTOCOL_NO_INTERSECTION}: peer accepts [{},{}], this side accepts [{},{}]",
        peer.0, peer.1, mine.0, mine.1
    )
}

/// Disconnect budget after an incompatible handshake (lamp first, then the
/// socket closes). The lower-level timeouts live beside their users:
/// `CONNECT_TIMEOUT_MS`/`HELLO_TIMEOUT_MS` in firmware/mod.rs.
pub const PROTOCOL_FAIL_DISCONNECT_MS: u64 = 3000;

/// Reconnect period while the lamp shows a protocol error — slow, deliberate,
/// and it is what makes the lamp survivable: when the peer is rolled back to
/// a compatible version, the next attempt succeeds and the lamp clears.
pub const PROTOCOL_ERROR_RETRY_MS: u64 = 30_000;

// --- codec ----------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("frame exceeds {max} bytes")]
    TooLarge { max: usize },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Encode a frame as one LDJSON line (no trailing newline — the writer adds
/// it, mirroring tokio's write_all of `line` + `\n`).
pub fn encode_gateway(f: &GatewayFrame) -> Result<String, CodecError> {
    Ok(serde_json::to_string(f)?)
}

pub fn encode_core(f: &CoreFrame) -> Result<String, CodecError> {
    Ok(serde_json::to_string(f)?)
}

pub fn decode_gateway(line: &str) -> Result<GatewayFrame, CodecError> {
    Ok(serde_json::from_str(line)?)
}

pub fn decode_core(line: &str) -> Result<CoreFrame, CodecError> {
    Ok(serde_json::from_str(line)?)
}

/// Maximum accepted frame size on the wire. Play frames carry inline WAV,
/// so this bounds a long spoken answer (≈ tens of seconds of 16 kHz audio).
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineRead {
    Eof,
    Line(String),
    Oversized { task_id: Option<String> },
}

fn extract_task_id_from_prefix(prefix: &[u8]) -> Option<String> {
    let head = std::str::from_utf8(&prefix[..prefix.len().min(4096)]).ok()?;
    let key_pos = head.find("\"task_id\"")?;
    let rest = &head[key_pos + "\"task_id\"".len()..];
    let colon = rest.find(':')?;
    let after_colon = rest[colon + 1..].trim_start();
    if !after_colon.starts_with('"') {
        return None;
    }
    let val_rest = &after_colon[1..];
    let end = val_rest.find('"')?;
    Some(val_rest[..end].to_string())
}

/// Read one LDJSON line from the core (capped at `MAX_FRAME_BYTES`).
/// When a line exceeds `MAX_FRAME_BYTES`, drains until `\n` without growing
/// memory and returns `LineRead::Oversized` so the caller can either skip it
/// and keep the link alive (`firmware`) or report a bounded error (`client`).
pub async fn read_line_capped<R: tokio::io::AsyncRead + Unpin>(
    lines: &mut tokio::io::BufReader<R>,
) -> Result<LineRead, Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::AsyncBufReadExt;
    let mut buf = Vec::with_capacity(512);
    let mut oversized = false;
    loop {
        let avail = lines.fill_buf().await?;
        if avail.is_empty() {
            if oversized || buf.is_empty() {
                return Ok(LineRead::Eof);
            }
            break;
        }
        if let Some(pos) = avail.iter().position(|&b| b == b'\n') {
            if oversized || buf.len() + pos > MAX_FRAME_BYTES {
                if !oversized && buf.len() < 4096 {
                    let take = (4096 - buf.len()).min(pos);
                    buf.extend_from_slice(&avail[..take]);
                }
                lines.consume(pos + 1);
                return Ok(LineRead::Oversized {
                    task_id: extract_task_id_from_prefix(&buf),
                });
            }
            buf.extend_from_slice(&avail[..pos]);
            lines.consume(pos + 1);
            break;
        }
        if !oversized {
            if buf.len() + avail.len() > MAX_FRAME_BYTES {
                oversized = true;
            } else {
                buf.extend_from_slice(avail);
            }
        }
        let len = avail.len();
        lines.consume(len);
    }
    while buf.last() == Some(&b'\r') {
        buf.pop();
    }
    Ok(LineRead::Line(String::from_utf8_lossy(&buf).into_owned()))
}

#[cfg(test)]
mod tests;
