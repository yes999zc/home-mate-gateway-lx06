//! Durable idempotent configuration journal and strict block validators
//! for `config_apply_v2` (#112 / #113).

use super::leds::{self, LedOrientation, NightMode};
use super::loudness;
use crate::protocol::{ConfigBlock, ConfigBlockState, ConfigReceiptStatus, GatewayFrame};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;

pub const MAX_RECENT_OPERATIONS: usize = 64;
const JOURNAL_FILE: &str = "config_journal.json";
const AUDIO_EQ_FILE: &str = "audio_eq.json";
const ACOUSTICS_FILE: &str = "acoustics_config.json";

/// Generate a per-boot identity string for `gateway_boot_id`.
pub fn new_gateway_boot_id() -> String {
    let pid = std::process::id();
    let ts = super::now_ms();
    format!("boot-{pid}-{ts:x}")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConfigReceiptData {
    pub device: String,
    pub gateway_boot_id: String,
    pub operation_id: String,
    pub application_id: String,
    pub data_epoch: u64,
    pub desired_revision: u64,
    pub block_generation: u64,
    pub block: ConfigBlock,
    pub block_hash: String,
    pub state_revision: u64,
    pub status: ConfigReceiptStatus,
    pub persisted_hash: Option<String>,
    pub active_hash: Option<String>,
    pub active_values: Option<serde_json::Value>,
    pub evidence: Option<serde_json::Value>,
    pub error_code: Option<String>,
    pub retryable: bool,
}

impl ConfigReceiptData {
    pub fn into_frame(self) -> GatewayFrame {
        GatewayFrame::ConfigReceipt {
            device: self.device,
            gateway_boot_id: self.gateway_boot_id,
            operation_id: self.operation_id,
            application_id: self.application_id,
            data_epoch: self.data_epoch,
            desired_revision: self.desired_revision,
            block_generation: self.block_generation,
            block: self.block,
            block_hash: self.block_hash,
            state_revision: self.state_revision,
            status: self.status,
            persisted_hash: self.persisted_hash,
            active_hash: self.active_hash,
            active_values: self.active_values,
            evidence: self.evidence,
            error_code: self.error_code,
            retryable: self.retryable,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BlockJournalEntry {
    pub block_generation: u64,
    pub data_epoch: u64,
    pub desired_revision: u64,
    pub operation_id: String,
    pub persisted_hash: Option<String>,
    pub persisted_values: Option<serde_json::Value>,
    pub active_hash: Option<String>,
    pub active_values: Option<serde_json::Value>,
    pub evidence: Option<serde_json::Value>,
    pub state: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpJournalEntry {
    pub operation_id: String,
    pub block: ConfigBlock,
    pub block_hash: String,
    pub receipt: ConfigReceiptData,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConfigJournal {
    pub state_revision: u64,
    pub blocks: BTreeMap<ConfigBlock, BlockJournalEntry>,
    pub recent_ops: VecDeque<OpJournalEntry>,
}

pub enum OpLookup {
    New,
    IdempotentReplay(Box<ConfigReceiptData>),
    Conflict,
}

impl ConfigJournal {
    /// Load `<state_dir>/config_journal.json`. A new boot preserves persisted
    /// records and high-water marks, while clearing `active_hash`/`active_values`
    /// until the real consumer reloads and reports.
    pub fn load(state_dir: &Path) -> Self {
        let Ok(raw) = std::fs::read_to_string(state_dir.join(JOURNAL_FILE)) else {
            return Self::default();
        };
        let mut journal: Self = serde_json::from_str(&raw).unwrap_or_default();
        for entry in journal.blocks.values_mut() {
            entry.active_hash = None;
            entry.active_values = None;
            if entry.state == "active" {
                entry.state = "persisted".into();
            }
        }
        while journal.recent_ops.len() > MAX_RECENT_OPERATIONS {
            journal.recent_ops.pop_front();
        }
        journal
    }

    pub fn save(&self, state_dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(state_dir)
            .map_err(|e| format!("create state_dir {}: {e}", state_dir.display()))?;
        let body =
            serde_json::to_string(self).map_err(|e| format!("serialize config_journal: {e}"))?;
        let target = state_dir.join(JOURNAL_FILE);
        let tmp = state_dir.join("config_journal.json.tmp");
        std::fs::write(&tmp, body).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &target).map_err(|e| format!("rename {}: {e}", target.display()))
    }

    pub fn lookup_operation(
        &self,
        operation_id: &str,
        block: ConfigBlock,
        block_hash: &str,
    ) -> OpLookup {
        if let Some(found) = self
            .recent_ops
            .iter()
            .rev()
            .find(|op| op.operation_id == operation_id)
        {
            if found.block == block && found.block_hash == block_hash {
                return OpLookup::IdempotentReplay(Box::new(found.receipt.clone()));
            }
            return OpLookup::Conflict;
        }
        OpLookup::New
    }

    pub fn check_high_water(
        &self,
        block: ConfigBlock,
        data_epoch: u64,
        block_generation: u64,
    ) -> Result<(), String> {
        let Some(entry) = self.blocks.get(&block) else {
            return Ok(());
        };
        if data_epoch < entry.data_epoch {
            return Err(format!(
                "stale data_epoch {data_epoch} < high-water {}",
                entry.data_epoch
            ));
        }
        if data_epoch == entry.data_epoch
            && block_generation > 0
            && block_generation < entry.block_generation
        {
            return Err(format!(
                "stale block_generation {block_generation} < high-water {}",
                entry.block_generation
            ));
        }
        Ok(())
    }

    pub fn record_terminal(
        &mut self,
        receipt: &ConfigReceiptData,
        persisted_values: Option<serde_json::Value>,
    ) {
        let entry = self.blocks.entry(receipt.block).or_default();
        if matches!(
            receipt.status,
            ConfigReceiptStatus::Persisted | ConfigReceiptStatus::Active
        ) {
            entry.data_epoch = entry.data_epoch.max(receipt.data_epoch);
            entry.block_generation = entry.block_generation.max(receipt.block_generation);
            entry.desired_revision = entry.desired_revision.max(receipt.desired_revision);
            entry.operation_id = receipt.operation_id.clone();
            if receipt.persisted_hash.is_some() {
                entry.persisted_hash = receipt.persisted_hash.clone();
            }
            if persisted_values.is_some() {
                entry.persisted_values = persisted_values;
            }
            if receipt.status == ConfigReceiptStatus::Active {
                entry.active_hash = receipt.active_hash.clone();
                entry.active_values = receipt.active_values.clone();
                entry.evidence = receipt.evidence.clone();
                entry.state = "active".into();
                entry.reason = None;
            } else {
                entry.state = "persisted".into();
                entry.reason = receipt.error_code.clone();
            }
        } else if receipt.status == ConfigReceiptStatus::Unsupported {
            entry.data_epoch = entry.data_epoch.max(receipt.data_epoch);
            entry.block_generation = entry.block_generation.max(receipt.block_generation);
            entry.operation_id = receipt.operation_id.clone();
            entry.active_values = receipt.active_values.clone();
            entry.state = "unsupported".into();
            entry.reason = receipt.error_code.clone();
        } else if receipt.status == ConfigReceiptStatus::Failed {
            entry.state = "failed".into();
            entry.reason = receipt.error_code.clone();
        }

        // Update existing op if present (e.g. Persisted -> Active), otherwise push.
        if let Some(existing) = self
            .recent_ops
            .iter_mut()
            .find(|op| op.operation_id == receipt.operation_id)
        {
            existing.receipt = receipt.clone();
        } else {
            if self.recent_ops.len() >= MAX_RECENT_OPERATIONS {
                self.recent_ops.pop_front();
            }
            self.recent_ops.push_back(OpJournalEntry {
                operation_id: receipt.operation_id.clone(),
                block: receipt.block,
                block_hash: receipt.block_hash.clone(),
                receipt: receipt.clone(),
            });
        }
    }

    pub fn block_states(&self) -> Vec<ConfigBlockState> {
        [
            ConfigBlock::Kws,
            ConfigBlock::AudioEq,
            ConfigBlock::Led,
            ConfigBlock::Duplex,
            ConfigBlock::Acoustics,
        ]
        .into_iter()
        .map(|block| {
            let entry = self.blocks.get(&block);
            ConfigBlockState {
                block,
                block_generation: entry.map(|e| e.block_generation).unwrap_or(0),
                operation_id: entry.map(|e| e.operation_id.clone()).unwrap_or_default(),
                data_epoch: entry.map(|e| e.data_epoch).unwrap_or(0),
                persisted_hash: entry.and_then(|e| e.persisted_hash.clone()),
                active_hash: entry.and_then(|e| e.active_hash.clone()),
                active_values: entry.and_then(|e| e.active_values.clone()),
                evidence: entry.and_then(|e| e.evidence.clone()),
                state: entry
                    .map(|e| e.state.clone())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "idle".into()),
                reason: entry.and_then(|e| e.reason.clone()),
            }
        })
        .collect()
    }
}

// --- Saved EQ & Acoustics persistence ---------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SavedAudioEq {
    pub curve: String,
    pub bands: Vec<crate::protocol::EqBand>,
}

pub fn load_audio_eq(state_dir: &Path) -> Option<SavedAudioEq> {
    let raw = std::fs::read_to_string(state_dir.join(AUDIO_EQ_FILE)).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn save_audio_eq(state_dir: &Path, doc: &SavedAudioEq) -> Result<(), String> {
    std::fs::create_dir_all(state_dir)
        .map_err(|e| format!("create state_dir {}: {e}", state_dir.display()))?;
    let body = serde_json::to_string(doc).map_err(|e| format!("serialize audio_eq: {e}"))?;
    let target = state_dir.join(AUDIO_EQ_FILE);
    let tmp = state_dir.join("audio_eq.json.tmp");
    std::fs::write(&tmp, body).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &target).map_err(|e| format!("rename {}: {e}", target.display()))
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct SavedAcoustics {
    pub volume_cap: f64,
    pub mix_gain: f32,
    pub onset_threshold_dbfs: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mic_gain_offset_db: Option<f32>,
}

pub fn load_acoustics(state_dir: &Path) -> Option<SavedAcoustics> {
    let raw = std::fs::read_to_string(state_dir.join(ACOUSTICS_FILE)).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn save_acoustics(state_dir: &Path, doc: &SavedAcoustics) -> Result<(), String> {
    std::fs::create_dir_all(state_dir)
        .map_err(|e| format!("create state_dir {}: {e}", state_dir.display()))?;
    let body = serde_json::to_string(doc).map_err(|e| format!("serialize acoustics: {e}"))?;
    let target = state_dir.join(ACOUSTICS_FILE);
    let tmp = state_dir.join("acoustics_config.json.tmp");
    std::fs::write(&tmp, body).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &target).map_err(|e| format!("rename {}: {e}", target.display()))
}

// --- Strict Block Validators ------------------------------------------------

#[derive(Debug, Clone)]
pub struct ValidatedAudioEq {
    pub curve_name: String,
    pub curve: loudness::Curve,
    pub bands: Vec<loudness::CustomBand>,
    pub raw_bands: Vec<crate::protocol::EqBand>,
    pub normalized: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AudioEqInput {
    curve: String,
    #[serde(default)]
    bands: Vec<crate::protocol::EqBand>,
}

pub fn validate_audio_eq(values: &serde_json::Value) -> Result<ValidatedAudioEq, Vec<String>> {
    let input: AudioEqInput = match serde_json::from_value(values.clone()) {
        Ok(v) => v,
        Err(e) => return Err(vec![format!("invalid audio_eq payload: {e}")]),
    };
    let mut errors = Vec::new();
    let curve = match input.curve.as_str() {
        "rui" => loudness::Curve::Rui,
        "official" => loudness::Curve::Official,
        "echomuse" => loudness::Curve::EchoMuse,
        "custom" => loudness::Curve::Custom,
        other => {
            errors.push(format!("unsupported audio_eq curve '{other}'"));
            loudness::Curve::Rui
        }
    };
    if input.bands.len() > 10 {
        errors.push(format!(
            "audio_eq bands count {} exceeds maximum 10",
            input.bands.len()
        ));
    }
    let mut bands = Vec::with_capacity(input.bands.len());
    for (idx, b) in input.bands.iter().enumerate() {
        if !b.freq.is_finite() || !(20.0..=16000.0).contains(&b.freq) {
            errors.push(format!(
                "band[{idx}].freq {} out of range 20..=16000 Hz",
                b.freq
            ));
        }
        if !b.gain_db.is_finite() || !(-18.0..=18.0).contains(&b.gain_db) {
            errors.push(format!(
                "band[{idx}].gain_db {} out of range -18..=18 dB",
                b.gain_db
            ));
        }
        if !b.q.is_finite() || !(0.3..=8.0).contains(&b.q) {
            errors.push(format!("band[{idx}].q {} out of range 0.3..=8.0", b.q));
        }
        bands.push(loudness::CustomBand {
            freq: b.freq,
            gain_db: b.gain_db,
            q: b.q,
        });
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let normalized = serde_json::json!({
        "curve": input.curve,
        "bands": input.bands,
    });
    Ok(ValidatedAudioEq {
        curve_name: input.curve,
        curve,
        bands,
        raw_bands: input.bands,
        normalized,
    })
}

#[derive(Debug, Clone)]
pub struct ValidatedLed {
    pub night_mode: NightMode,
    pub orientation: LedOrientation,
    pub timezone: String,
    pub tz_offset_minutes: i16,
    pub normalized: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LedInput {
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
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    tz_offset_minutes: Option<i16>,
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

pub fn resolve_timezone_offset(
    timezone: Option<&str>,
    explicit_offset: Option<i16>,
) -> Result<(String, i16), String> {
    if let Some(off) = explicit_offset {
        if !(-720..=840).contains(&off) {
            return Err(format!("tz_offset_minutes {off} out of range -720..=840"));
        }
    }
    match timezone {
        None => {
            let off = explicit_offset.unwrap_or(0);
            let label = if off == 0 {
                "UTC".to_string()
            } else {
                let sign = if off >= 0 { '+' } else { '-' };
                let abs = off.abs();
                format!("UTC{sign}{:02}:{:02}", abs / 60, abs % 60)
            };
            Ok((label, off))
        }
        Some(tz) => {
            let tz_offset = match tz {
                "UTC" | "Etc/UTC" => Some(0),
                "Asia/Shanghai" | "Asia/Hong_Kong" | "Asia/Taipei" | "Asia/Singapore" => Some(480),
                "Asia/Tokyo" | "Asia/Seoul" => Some(540),
                "Europe/London" => Some(0),
                "Europe/Berlin" | "Europe/Paris" => Some(60),
                "America/New_York" => Some(-300),
                "America/Los_Angeles" => Some(-480),
                other => parse_utc_offset_str(other),
            };
            let Some(resolved) = tz_offset else {
                return Err(format!("unsupported timezone '{tz}'"));
            };
            if let Some(exp) = explicit_offset {
                if exp != resolved {
                    return Err(format!(
                        "timezone '{tz}' offset {resolved} conflicts with tz_offset_minutes {exp}"
                    ));
                }
            }
            Ok((tz.to_string(), resolved))
        }
    }
}

fn parse_utc_offset_str(s: &str) -> Option<i16> {
    let rest = s.strip_prefix("UTC")?;
    if rest.is_empty() {
        return Some(0);
    }
    let sign = match rest.as_bytes().first().copied()? {
        b'+' => 1i16,
        b'-' => -1i16,
        _ => return None,
    };
    let hm = &rest[1..];
    let (h_str, m_str) = hm.split_once(':')?;
    let h: i16 = h_str.parse().ok()?;
    let m: i16 = m_str.parse().ok()?;
    if !(0..=14).contains(&h) || !(0..=59).contains(&m) {
        return None;
    }
    let total = sign * (h * 60 + m);
    (-720..=840).contains(&total).then_some(total)
}

pub fn validate_led(values: &serde_json::Value) -> Result<ValidatedLed, Vec<String>> {
    let input: LedInput = match serde_json::from_value(values.clone()) {
        Ok(v) => v,
        Err(e) => return Err(vec![format!("invalid led payload: {e}")]),
    };
    let mut errors = Vec::new();
    if input.night_start_minute >= 1440 {
        errors.push(format!(
            "night_start_minute {} must be < 1440",
            input.night_start_minute
        ));
    }
    if input.night_end_minute >= 1440 {
        errors.push(format!(
            "night_end_minute {} must be < 1440",
            input.night_end_minute
        ));
    }
    if input.night_brightness_pct > 100 {
        errors.push(format!(
            "night_brightness_pct {} must be <= 100",
            input.night_brightness_pct
        ));
    }
    if input.orientation_rotation as usize >= leds::RING_LEDS {
        errors.push(format!(
            "orientation_rotation {} must be < {}",
            input.orientation_rotation,
            leds::RING_LEDS
        ));
    }
    let tz_res = resolve_timezone_offset(input.timezone.as_deref(), input.tz_offset_minutes);
    if let Err(ref e) = tz_res {
        errors.push(e.clone());
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let (timezone, tz_offset_minutes) = tz_res.unwrap();
    let night_mode = NightMode {
        enabled: input.night_enabled,
        start_minute: input.night_start_minute,
        end_minute: input.night_end_minute,
        brightness_pct: input.night_brightness_pct,
    };
    let orientation = LedOrientation {
        rotation: input.orientation_rotation,
        reverse: input.orientation_reverse,
    };
    let normalized = serde_json::json!({
        "night_enabled": night_mode.enabled,
        "night_start_minute": night_mode.start_minute,
        "night_end_minute": night_mode.end_minute,
        "night_brightness_pct": night_mode.brightness_pct,
        "orientation_rotation": orientation.rotation,
        "orientation_reverse": orientation.reverse,
        "timezone": timezone,
        "tz_offset_minutes": tz_offset_minutes,
    });
    Ok(ValidatedLed {
        night_mode,
        orientation,
        timezone,
        tz_offset_minutes,
        normalized,
    })
}

#[derive(Debug, Clone)]
pub struct ValidatedKws {
    pub keywords: String,
    pub retreat_words: Vec<String>,
    pub intents_hash: String,
    pub threshold: Option<f32>,
    pub score: Option<f32>,
    pub normalized: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KwsInput {
    keywords: String,
    #[serde(default)]
    retreat_words: Vec<String>,
    #[serde(default)]
    intents_hash: String,
    #[serde(default)]
    threshold: Option<f32>,
    #[serde(default)]
    score: Option<f32>,
}

pub fn validate_kws(
    values: &serde_json::Value,
    kws_dir: &Path,
) -> Result<ValidatedKws, Vec<String>> {
    let input: KwsInput = match serde_json::from_value(values.clone()) {
        Ok(v) => v,
        Err(e) => return Err(vec![format!("invalid kws payload: {e}")]),
    };
    let mut errors = Vec::new();
    if let Some(t) = input.threshold {
        if !t.is_finite() || t <= 0.0 || t > 1.0 {
            errors.push(format!("threshold {t} out of range (0.0, 1.0]"));
        }
    }
    if let Some(s) = input.score {
        if !s.is_finite() || s <= 0.0 || s > 20.0 {
            errors.push(format!("score {s} out of range (0.0, 20.0]"));
        }
    }
    if let Err(e) = super::keywords::validate_keywords_only(kws_dir, &input.keywords) {
        errors.push(e);
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let normalized = serde_json::json!({
        "keywords": input.keywords,
        "retreat_words": input.retreat_words,
        "intents_hash": input.intents_hash,
        "threshold": input.threshold,
        "score": input.score,
    });
    Ok(ValidatedKws {
        keywords: input.keywords,
        retreat_words: input.retreat_words,
        intents_hash: input.intents_hash,
        threshold: input.threshold,
        score: input.score,
        normalized,
    })
}

#[derive(Debug, Clone)]
pub struct ValidatedDuplex {
    pub duplex_requested: String,
    pub normalized: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DuplexInput {
    duplex_requested: String,
}

pub fn validate_duplex(values: &serde_json::Value) -> Result<ValidatedDuplex, Vec<String>> {
    let input: DuplexInput = match serde_json::from_value(values.clone()) {
        Ok(v) => v,
        Err(e) => return Err(vec![format!("invalid duplex payload: {e}")]),
    };
    if input.duplex_requested != "half" && input.duplex_requested != "full" {
        return Err(vec![format!(
            "unsupported duplex_requested '{}'",
            input.duplex_requested
        )]);
    }
    let normalized = serde_json::json!({
        "duplex_requested": input.duplex_requested,
    });
    Ok(ValidatedDuplex {
        duplex_requested: input.duplex_requested,
        normalized,
    })
}

#[derive(Debug, Clone)]
pub struct ValidatedAcoustics {
    pub volume_cap: f64,
    pub mix_gain: f32,
    pub onset_threshold_dbfs: f32,
    pub mic_gain_offset_db: Option<f32>,
    pub normalized: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcousticsInput {
    volume_cap: f64,
    mix_gain: f32,
    onset_threshold_dbfs: f32,
    #[serde(default)]
    mic_gain_offset_db: Option<f32>,
}

pub fn validate_acoustics(values: &serde_json::Value) -> Result<ValidatedAcoustics, Vec<String>> {
    let input: AcousticsInput = match serde_json::from_value(values.clone()) {
        Ok(v) => v,
        Err(e) => return Err(vec![format!("invalid acoustics payload: {e}")]),
    };
    let mut errors = Vec::new();
    if !input.volume_cap.is_finite() || !(0.0..=1.0).contains(&input.volume_cap) {
        errors.push(format!(
            "volume_cap {} out of range 0.0..=1.0",
            input.volume_cap
        ));
    }
    if !input.mix_gain.is_finite() || !(0.05..=2.0).contains(&input.mix_gain) {
        errors.push(format!(
            "mix_gain {} out of range 0.05..=2.0",
            input.mix_gain
        ));
    }
    if !input.onset_threshold_dbfs.is_finite()
        || !(-120.0..=0.0).contains(&input.onset_threshold_dbfs)
    {
        errors.push(format!(
            "onset_threshold_dbfs {} out of range -120.0..=0.0",
            input.onset_threshold_dbfs
        ));
    }
    if let Some(off) = input.mic_gain_offset_db {
        if !off.is_finite() || !(-24.0..=24.0).contains(&off) {
            errors.push(format!(
                "mic_gain_offset_db {off} out of range -24.0..=24.0"
            ));
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let normalized = serde_json::json!({
        "volume_cap": input.volume_cap,
        "mix_gain": input.mix_gain,
        "onset_threshold_dbfs": input.onset_threshold_dbfs,
        "mic_gain_offset_db": input.mic_gain_offset_db,
    });
    Ok(ValidatedAcoustics {
        volume_cap: input.volume_cap,
        mix_gain: input.mix_gain,
        onset_threshold_dbfs: input.onset_threshold_dbfs,
        mic_gain_offset_db: input.mic_gain_offset_db,
        normalized,
    })
}

pub fn capability_fingerprint(capabilities: &[String]) -> String {
    let mut sorted = capabilities.to_vec();
    sorted.sort();
    super::fnv1a_hex(sorted.join(",").as_bytes())
}

pub fn kws_model_token_hashes(kws_dir: &Path) -> (Option<String>, Option<String>) {
    let token_hash = std::fs::read(kws_dir.join("model").join("tokens.txt"))
        .ok()
        .map(|b| super::fnv1a_hex(&b));
    let model_hash = std::fs::read(kws_dir.join("model").join("encoder.onnx"))
        .or_else(|_| std::fs::read(kws_dir.join("model").join("model.onnx")))
        .ok()
        .map(|b| super::fnv1a_hex(&b));
    (model_hash, token_hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn audio_eq_strict_validation_rejects_out_of_range_instead_of_clamping() {
        assert!(validate_audio_eq(&json!({
            "curve": "custom",
            "bands": [{ "freq": 1000.0, "gain_db": 3.0, "q": 1.2 }]
        }))
        .is_ok());

        // Out-of-range freq, gain_db, q, unknown curve, >10 bands
        assert!(validate_audio_eq(&json!({
            "curve": "custom",
            "bands": [{ "freq": 5.0, "gain_db": 3.0, "q": 1.2 }]
        }))
        .is_err());
        assert!(validate_audio_eq(&json!({
            "curve": "custom",
            "bands": [{ "freq": 1000.0, "gain_db": 25.0, "q": 1.2 }]
        }))
        .is_err());
        assert!(validate_audio_eq(&json!({
            "curve": "custom",
            "bands": [{ "freq": 1000.0, "gain_db": 3.0, "q": 0.1 }]
        }))
        .is_err());
        assert!(validate_audio_eq(&json!({
            "curve": "unknown_curve",
            "bands": []
        }))
        .is_err());
    }

    #[test]
    fn led_validation_checks_timezone_and_offsets() {
        let ok = validate_led(&json!({
            "night_enabled": true,
            "night_start_minute": 1320,
            "night_end_minute": 420,
            "night_brightness_pct": 15,
            "orientation_rotation": 3,
            "orientation_reverse": false,
            "timezone": "Asia/Shanghai"
        }))
        .expect("valid Asia/Shanghai");
        assert_eq!(ok.tz_offset_minutes, 480);

        assert!(validate_led(&json!({
            "timezone": "Mars/Olympus_Mons"
        }))
        .is_err());
        assert!(validate_led(&json!({
            "tz_offset_minutes": 999
        }))
        .is_err());
        assert!(validate_led(&json!({
            "timezone": "Asia/Shanghai",
            "tz_offset_minutes": 0
        }))
        .is_err());
    }

    #[test]
    fn acoustics_validation_enforces_per_unit_bounds() {
        assert!(validate_acoustics(&json!({
            "volume_cap": 0.85,
            "mix_gain": 1.0,
            "onset_threshold_dbfs": -42.0,
            "mic_gain_offset_db": -1.5
        }))
        .is_ok());

        assert!(validate_acoustics(&json!({
            "volume_cap": 1.2,
            "mix_gain": 1.0,
            "onset_threshold_dbfs": -42.0
        }))
        .is_err());
        assert!(validate_acoustics(&json!({
            "volume_cap": 0.8,
            "mix_gain": 0.01,
            "onset_threshold_dbfs": -42.0
        }))
        .is_err());
        assert!(validate_acoustics(&json!({
            "volume_cap": 0.8,
            "mix_gain": 1.0,
            "onset_threshold_dbfs": 5.0
        }))
        .is_err());
    }

    #[test]
    fn journal_bounds_recent_ops_and_clears_active_on_reboot() {
        let dir = std::env::temp_dir().join(format!("hm-cj-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut j = ConfigJournal::default();
        for i in 0..70u64 {
            let r = ConfigReceiptData {
                device: "dot2-d".into(),
                gateway_boot_id: "boot-1".into(),
                operation_id: format!("op-{i}"),
                application_id: "app-1".into(),
                data_epoch: 1,
                desired_revision: i + 1,
                block_generation: i + 1,
                block: ConfigBlock::Led,
                block_hash: format!("h-{i}"),
                state_revision: i + 1,
                status: ConfigReceiptStatus::Active,
                persisted_hash: Some(format!("h-{i}")),
                active_hash: Some(format!("h-{i}")),
                active_values: Some(json!({ "ok": true })),
                evidence: None,
                error_code: None,
                retryable: false,
            };
            j.record_terminal(&r, Some(json!({ "ok": true })));
        }
        assert_eq!(j.recent_ops.len(), MAX_RECENT_OPERATIONS);
        j.save(&dir).unwrap();

        let reloaded = ConfigJournal::load(&dir);
        let led = reloaded.blocks.get(&ConfigBlock::Led).unwrap();
        assert_eq!(led.block_generation, 70);
        assert_eq!(led.persisted_hash.as_deref(), Some("h-69"));
        assert_eq!(
            led.active_hash, None,
            "reboot clears active_hash until reloaded"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
