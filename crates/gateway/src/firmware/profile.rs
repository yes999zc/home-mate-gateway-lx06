//! Device Profile layer (#25): 3-layer separation (Session / Profile / Port).
//!
//! `DeviceProfile` holds hardware-family capabilities and per-unit measured
//! acoustic parameters (volume cap, mix gain, VAD threshold) so the Session
//! layer (`north.rs`, `playback.rs`) stays free of hardcoded `"AEOBC"` /
//! `"dot2-d"` constants, while Dot2-specific ioctls and 12-LED layout stay
//! in the Port layer (`alsa.rs`, `buttons.rs`, `leds.rs`).
//!
//! Per owner scope (2026-11-16): acoustic parameters are measured per unit
//! and are NOT copied blindly across units.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// AEC pipeline scheme supported by a device profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AecScheme {
    None,
    SpeexMimo,
    HardwareRef,
}

/// Physical evdev key mapping for a unit's button board.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyMap {
    pub mute: u16,
    pub vol_down: u16,
    pub vol_up: u16,
    pub action: u16,
}

impl Default for KeyMap {
    fn default() -> Self {
        Self {
            mute: super::buttons::KEY_MUTE,
            vol_down: super::buttons::KEY_VOLDOWN,
            vol_up: super::buttons::KEY_VOLUP,
            action: 138, // KEY_HELP on Dot2
        }
    }
}

/// Device volume grid parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeScale {
    pub grid_max: u64,
    pub key_step: u64,
    pub default_level: u64,
}

impl Default for VolumeScale {
    fn default() -> Self {
        Self {
            grid_max: 127,
            key_step: super::VOLUME_KEY_STEP,
            default_level: super::DEFAULT_VOLUME,
        }
    }
}

/// Complete device profile for a physical unit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceProfile {
    pub unit_id: String,
    pub model: String,
    pub gateway_tag: String,
    pub capture_rate: u32,
    pub capture_channels: usize,
    pub playback_rate: u32,
    pub playback_channels: u32,
    pub aec_scheme: AecScheme,
    pub led_count: usize,
    pub key_map: KeyMap,
    pub volume_scale: VolumeScale,
    /// Per-unit measured output volume ceiling (0.0..=1.0).
    pub volume_cap: f64,
    /// Per-unit measured playback mix gain multiplier (0.0..=2.0).
    pub mix_gain: f32,
    /// Per-unit measured VAD / barge-in onset threshold (dBFS).
    pub vad_threshold: f32,
    /// Legacy stored calibration metadata, retained for profile compatibility.
    /// Wire RMS is raw; only Core applies its own per-unit arbitration offset.
    #[serde(default)]
    pub mic_gain_offset_db: f32,
}

impl Default for DeviceProfile {
    fn default() -> Self {
        Self::dot2_d()
    }
}

impl DeviceProfile {
    fn base_dot2(unit_id: &str, volume_cap: f64, mix_gain: f32, vad_threshold: f32) -> Self {
        Self {
            unit_id: unit_id.into(),
            model: "AEOBC".into(),
            gateway_tag: "dot2-direct".into(),
            capture_rate: super::CAP_RATE,
            capture_channels: super::CAP_CHANNELS,
            playback_rate: super::PLAY_RATE,
            playback_channels: super::PLAY_CHANNELS,
            aec_scheme: AecScheme::SpeexMimo,
            led_count: super::leds::RING_LEDS,
            key_map: KeyMap::default(),
            volume_scale: VolumeScale::default(),
            volume_cap,
            mix_gain,
            vad_threshold,
            mic_gain_offset_db: 0.0,
        }
    }

    /// Preset for unit `dot2-a` (distinct per-unit measured acoustics).
    pub fn dot2_a() -> Self {
        Self::base_dot2("dot2-a", 0.90, 0.92, -40.0)
    }

    /// Preset for unit `dot2-b` (distinct per-unit measured acoustics).
    pub fn dot2_b() -> Self {
        Self::base_dot2("dot2-b", 0.88, 0.85, -39.0)
    }

    /// Preset for unit `dot2-c` (distinct per-unit measured acoustics).
    pub fn dot2_c() -> Self {
        Self::base_dot2("dot2-c", 0.85, 0.80, -38.0)
    }

    /// Preset for unit `dot2-d` (baseline Dot2 direct-occupation unit).
    pub fn dot2_d() -> Self {
        Self::base_dot2("dot2-d", 1.00, 1.00, -42.0)
    }

    /// Preset for unit `dot2-e` (distinct per-unit measured acoustics).
    pub fn dot2_e() -> Self {
        Self::base_dot2("dot2-e", 0.95, 0.90, -41.0)
    }

    /// Base profile for Xiaomi AI Speaker Pro (`LX06`): 8ch S32_LE @ 48 kHz
    /// (ch0..5 PDM mics, ch6..7 digital loopback ref), 18-LED AW20054 ring,
    /// 4-button `adc_keypad` (`event0`).
    pub fn base_lx06(unit_id: &str, volume_cap: f64, mix_gain: f32, vad_threshold: f32) -> Self {
        Self {
            unit_id: unit_id.into(),
            model: "LX06".into(),
            gateway_tag: "lx06-direct".into(),
            capture_rate: 48_000,
            capture_channels: 8,
            playback_rate: super::PLAY_RATE,
            playback_channels: super::PLAY_CHANNELS,
            aec_scheme: AecScheme::SpeexMimo,
            led_count: 18,
            key_map: KeyMap {
                mute: 102,     // KEY_HOME -> physical Mic Mute button on LX06 adc_keypad
                vol_down: 115, // KEY_VOLUMEUP -> physical Volume Down button on LX06 adc_keypad
                vol_up: 139,   // KEY_MENU -> physical Volume Up button on LX06 adc_keypad
                action: 114, // KEY_VOLUMEDOWN -> physical Play/Pause (Action) button on LX06 adc_keypad
            },
            volume_scale: VolumeScale::default(),
            volume_cap,
            mix_gain,
            vad_threshold,
            mic_gain_offset_db: 0.0,
        }
    }

    /// Preset for unit `lx06` (Xiaomi AI Speaker Pro baseline).
    pub fn lx06() -> Self {
        Self::base_lx06("lx06", 1.00, 1.00, -42.0)
    }

    /// Select the built-in preset for `unit_id`, or construct a Dot2-family
    /// profile tagged with `unit_id` if no dedicated preset exists yet.
    pub fn for_unit(unit_id: &str) -> Self {
        let trimmed = unit_id.trim();
        match trimmed {
            "dot2-a" => Self::dot2_a(),
            "dot2-b" => Self::dot2_b(),
            "dot2-c" => Self::dot2_c(),
            "dot2-d" => Self::dot2_d(),
            "dot2-e" => Self::dot2_e(),
            "lx06" => Self::lx06(),
            other if other.starts_with("lx06-") => Self::base_lx06(other, 1.00, 1.00, -42.0),
            other => {
                let mut p = Self::dot2_d();
                p.unit_id = other.to_string();
                p
            }
        }
    }

    /// Load a unit profile from `<state_dir>/profile.json` if present and
    /// valid; otherwise fall back to `DeviceProfile::for_unit(unit_id)`.
    ///
    /// Uses `std::fs::read_to_string` directly (no `Path::exists()`/`statx`).
    pub fn load_from_dir(state_dir: &Path, unit_id: &str) -> Self {
        let path = state_dir.join("profile.json");
        if let Ok(raw) = std::fs::read_to_string(&path) {
            if let Ok(mut parsed) = serde_json::from_str::<DeviceProfile>(&raw) {
                if parsed.unit_id.trim().is_empty() {
                    parsed.unit_id = unit_id.to_string();
                }
                return parsed;
            }
        }
        Self::for_unit(unit_id)
    }
}
