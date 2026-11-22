//! LED ring via the is31fl3236 kernel driver sysfs (G5-2 design doc E4/E5).
//!
//! The driver exposes `frame` as **72 ASCII hex chars + CRLF = 36 bytes**, one
//! PWM byte per channel (12 ring LEDs × RGB). Default channel order assumes
//! `3k, 3k+1, 3k+2` = LED k (R, G, B); the on-device calibration sweep writes
//! the recorded mapping to `led-map.json` and it is loaded at startup.

use crate::fwlog;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::Path;

pub const RING_LEDS: usize = 12;
pub const CHANNELS: usize = 36;

/// sysfs node writer (e.g. `/sys/devices/soc/11007000.i2c/i2c-0/0-003f/frame`).
/// The fd is opened once and held — the ring is exclusively ours after the
/// supervisor stops the ledcontroller init service (P2 fix).
pub struct LedRing {
    frame_path: String,
    file: Option<std::fs::File>,
    /// ring index -> [r_idx, g_idx, b_idx] channel indices.
    map: [[usize; 3]; RING_LEDS],
}

impl LedRing {
    pub fn new(frame_path: &str) -> LedRing {
        LedRing {
            frame_path: frame_path.into(),
            file: None,
            map: default_map(),
        }
    }

    fn file(&mut self) -> io::Result<&std::fs::File> {
        if self.file.is_none() {
            self.file = Some(
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&self.frame_path)?,
            );
        }
        Ok(self.file.as_ref().unwrap())
    }

    pub fn with_map(frame_path: &str, map: [[usize; 3]; RING_LEDS]) -> LedRing {
        let mut l = LedRing::new(frame_path);
        l.map = map;
        l
    }

    /// D/T5 debt #6: load the physically-calibrated channel order from
    /// `led-map.json` (written by `tools/led_calibrate.py` after the
    /// per-LED visual sweep). Missing/invalid file = sequential default.
    ///
    /// Deliberately uses `access(2)` + `open/read` with a FIXED buffer:
    /// `Path::exists()`/`metadata()` go through statx (397 on armv7), which
    /// this 3.18 kernel's compat table lacks — the P0 watchdog root cause
    /// (docs/d/p0-watchdog-resets-20261109.md). No new syscalls here.
    pub fn with_calibrated_map(frame_path: &str, map_path: &Path) -> LedRing {
        let mut l = LedRing::new(frame_path);
        if let Some(map) = load_map(map_path) {
            fwlog!("[leds] calibrated map loaded from {}", map_path.display());
            l.map = map;
        } else {
            fwlog!(
                "[leds] no calibrated map at {} — sequential default",
                map_path.display()
            );
        }
        l
    }

    /// One LED lit white, all others dark — the per-LED visual calibration
    /// frame. `index` is the RING index (physical target), and the map
    /// decides which three PWM channels actually light up.
    pub fn calibration_frame(&self, index: usize) -> [(u8, u8, u8); RING_LEDS] {
        calibration_frame(index)
    }

    /// Encode 12 RGB values into the 72-hex-char frame line.
    pub fn encode(&self, rgb: &[(u8, u8, u8); RING_LEDS]) -> String {
        let mut ch = [0u8; CHANNELS];
        for (led, &(r, g, b)) in rgb.iter().enumerate() {
            let [ri, gi, bi] = self.map[led];
            ch[ri] = r;
            ch[gi] = g;
            ch[bi] = b;
        }
        let mut s = String::with_capacity(CHANNELS * 2 + 2);
        for b in ch {
            s.push_str(&format!("{b:02x}"));
        }
        s.push_str("\r\n");
        s
    }

    pub fn write(&mut self, rgb: &[(u8, u8, u8); RING_LEDS]) -> io::Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        let frame = self.encode(rgb).into_bytes();
        let f = self.file()?;
        let mut f = f;
        f.seek(SeekFrom::Start(0))?;
        f.write_all(&frame)
    }
}

/// Sequential triplets — the assumption before the calibration sweep; verified
/// (and corrected here) during the A/B window.
pub fn default_map() -> [[usize; 3]; RING_LEDS] {
    let mut m = [[0usize; 3]; RING_LEDS];
    for (k, row) in m.iter_mut().enumerate() {
        *row = [3 * k, 3 * k + 1, 3 * k + 2];
    }
    m
}

/// Parse a calibrated map: 12 rows of [r,g,b] channel indices, each a
/// permutation of 0..36. Pure, so the validation is unit-testable off-device.
pub fn parse_map_json(text: &str) -> Option<[[usize; 3]; RING_LEDS]> {
    let rows: Vec<Vec<usize>> = serde_json::from_str(text).ok()?;
    if rows.len() != RING_LEDS {
        return None;
    }
    let mut map = [[0usize; 3]; RING_LEDS];
    let mut seen = [false; CHANNELS];
    for (k, row) in rows.iter().enumerate() {
        if row.len() != 3 {
            return None;
        }
        for (j, &ch) in row.iter().enumerate() {
            if ch >= CHANNELS || seen[ch] {
                return None; // out of range or duplicated PWM channel
            }
            seen[ch] = true;
            map[k][j] = ch;
        }
    }
    Some(map)
}

/// Read the map file WITHOUT stat/metadata (P0 discipline: statx=397 traps
/// on this 3.18 kernel). `access(2)` then a fixed-size `open/read` — the
/// same pattern as `firmware::probe_armed`. Returns None on any problem so
/// the ring always falls back to the sequential default.
pub fn load_map(path: &Path) -> Option<[[usize; 3]; RING_LEDS]> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    if unsafe { libc::access(c.as_ptr(), libc::F_OK) } != 0 {
        return None;
    }
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY) };
    if fd < 0 {
        return None;
    }
    let mut buf = [0u8; 4096];
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    unsafe { libc::close(fd) };
    if n <= 0 {
        return None;
    }
    let text = std::str::from_utf8(&buf[..n as usize]).ok()?;
    parse_map_json(text)
}

/// Standalone one-LED calibration frame (used by the `led-cal` CLI mode,
/// which must not open the whole audio hardware just to light a lamp).
pub fn calibration_frame(index: usize) -> [(u8, u8, u8); RING_LEDS] {
    let mut rgb = [(0u8, 0u8, 0u8); RING_LEDS];
    if index < RING_LEDS {
        rgb[index] = (0x60, 0x60, 0x60);
    }
    rgb
}

/// Palette by semantic state (G2 forensics §5; identical law to the adapter's
/// led_rgb). #00A050 (listening green) is the system-wide reserved
/// "receive gate open" color: this table is the ONLY way the ring gets
/// painted and "listening" is the only row carrying it — no other state can
/// impersonate it by construction. Unknown states paint dark, never stale.
pub fn state_rgb(state: &str) -> [(u8, u8, u8); RING_LEDS] {
    let (r, g, b) = match state {
        "listening" => (0x00, 0xA0, 0x50), // reserved green (breathing, see below)
        "hearing" => (0x00, 0xA0, 0x50),   // same green, held solid
        "wake" => (0x78, 0x78, 0x78),
        "thinking" => (0x00, 0x28, 0xA0),
        "speaking" => (0x00, 0x6E, 0x96),
        "music" => (0x64, 0x00, 0x8C),
        // R1e: a program is on hold (「暂停」, waiting for 继续). RUI-local's
        // `music_paused` is #602800; this is the same amber family raised to
        // our brightness law, and it must not read as error/muted red — one
        // signal, one meaning ([docs/led-states.md] in the core repo).
        "paused" => (0xB0, 0x80, 0x00),
        "muted" => (0x78, 0x00, 0x00),
        "error" => (0xAA, 0x00, 0x00),
        _ => (0x00, 0x00, 0x00), // "idle" + unknown: dark, never stale
    };
    [(r, g, b); RING_LEDS]
}

// --- LED B: breathing window vs solid "being heard" (2026-11-15) ----------------
//
// User decision: green means "the conversation window is open", and the ring
// must make the two situations inside it tell apart — a window that merely
// stays open (breathing) and a mic segment being captured right now (solid).
// The breath is deliberately slow and CONSTANT-RATE: the window can be
// extended by a follow-up or cut by 退下, so a rate that implied a countdown
// would state something the core cannot promise.

/// One breath, peak to peak.
pub const BREATH_PERIOD_MS: u64 = 3_500;
/// Trough brightness: dim enough to read as "waiting", never dark enough to
/// look closed.
pub const BREATH_MIN_K: f32 = 0.45;
/// Window-close fade: green ramps to black over this (a visual full stop; the
/// spoken close cue is separate).
pub const CLOSE_FADE_MS: u64 = 300;

/// 0.45…1.0, sine, one cycle per BREATH_PERIOD_MS.
pub fn breath_factor(t_ms: u64) -> f32 {
    let phase = (t_ms % BREATH_PERIOD_MS) as f32 / BREATH_PERIOD_MS as f32;
    let s = (phase * std::f32::consts::TAU).sin();
    BREATH_MIN_K + (1.0 - BREATH_MIN_K) * (s * 0.5 + 0.5)
}

/// The states that paint the reserved green.
pub fn is_green_family(state: &str) -> bool {
    matches!(state, "listening" | "hearing")
}

/// Brightness scale, round-to-nearest so a dim frame still lights the ring.
pub fn scaled(rgb: &[(u8, u8, u8); RING_LEDS], k: f32) -> [(u8, u8, u8); RING_LEDS] {
    let k = k.clamp(0.0, 1.0);
    let mut out = *rgb;
    for slot in out.iter_mut() {
        slot.0 = (slot.0 as f32 * k).round() as u8;
        slot.1 = (slot.1 as f32 * k).round() as u8;
        slot.2 = (slot.2 as f32 * k).round() as u8;
    }
    out
}

/// Link-down indicator (P4): a DIM WHITE ring — alive but disconnected.
/// Deliberately distinct from every semantic state (and from reserved
/// green, which stays unreachable except via "listening"). In the v1
/// mechanism this is the `core_offline` lamp; see [`HealthLamp`].
pub const OFFLINE_RGB: (u8, u8, u8) = (0x20, 0x20, 0x20);

// --- v1 mechanism (#12, 2026-11-16): machine-health lamps ---------------------
//
// Four gateway-LOCAL facts, never AssistantState: the core cannot report its
// own absence, and a Wi-Fi drop is visible precisely when the loopback core
// is still fine. Values are PROPOSALS locked by unit tests (pairwise distinct
// + distinct from muted/error red and reserved green); the owner's on-device
// visual acceptance is the real gate (led-states.md §色值提案).
/// Protocol refusal: version interval with no intersection, or a frame whose
/// capability this device does not declare. Alarm red-orange — the brightest
/// fault on purpose.
pub const FAULT_PROTOCOL_RGB: (u8, u8, u8) = (0xFF, 0x20, 0x00);
/// Wireless interface not associated (the loopback core may still be up).
pub const FAULT_WIFI_RGB: (u8, u8, u8) = (0xA0, 0x00, 0x60);
/// Process up, first handshake verdict pending (`Starting`). One brightness
/// step above `core_offline`; the two never show together (the state machine
/// exits Starting at the first verdict).
pub const FAULT_REBOOT_RGB: (u8, u8, u8) = (0x40, 0x40, 0x40);
/// Local Wi-Fi provisioning mode (#23, entered on long-press mute): amber-gold
/// ring distinct from `WifiDown`, `Offline`, `Reboot`, `Protocol`, `muted`,
/// and `listening`.
pub const WIFI_PROVISIONING_RGB: (u8, u8, u8) = (0xE0, 0x70, 0x00);

/// The machine-health lamp this build shows. NOT a semantic assistant state:
/// it survives `AssistantState` frames (they are recorded, not painted) and
/// is overridden only by the mute latch (privacy) and — for everything but
/// `Protocol` — by the volume arc (physical feedback).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthLamp {
    /// Process up, first handshake verdict pending.
    Reboot,
    /// Loopback link down / handshake lost — not a protocol error.
    Offline,
    /// Wireless not associated.
    WifiDown,
    /// Local Wi-Fi provisioning mode (#23: mute-hold setup).
    WifiProvisioning,
    /// Version/capability refusal. Sticky: only a FULL successful handshake
    /// clears it, and reconnects pace at PROTOCOL_ERROR_RETRY_MS.
    Protocol,
}

pub type FadeWindow = ([(u8, u8, u8); RING_LEDS], u64);

/// The ring's ONE composition point (v1 mechanism #12). Every event mutates
/// `Shared` and re-renders through here; no other code decides what the ring
/// shows — that is what makes the priority ladder enforceable (the audit
/// found volume arcs, animation ticks and reconnect repaints all writing the
/// ring directly, so no ladder could hold).
///
/// Ladder: muted > protocol_error > volume arc > wifi_provisioning > core_offline >
/// wifi_down > reboot(Starting) > close-fade > breath(listening) > semantic state.
pub fn ring_frame(
    muted: bool,
    lamp: Option<HealthLamp>,
    arc_level: Option<u64>,
    fade: Option<FadeWindow>,
    state: &str,
    now_ms: u64,
) -> [(u8, u8, u8); RING_LEDS] {
    if muted {
        return state_rgb("muted"); // privacy outranks everything, unchanged
    }
    if lamp == Some(HealthLamp::Protocol) {
        return [FAULT_PROTOCOL_RGB; RING_LEDS];
    }
    if let Some(level) = arc_level {
        return volume_arc_rgb(level);
    }
    if lamp == Some(HealthLamp::WifiProvisioning) {
        return [WIFI_PROVISIONING_RGB; RING_LEDS];
    }
    if lamp == Some(HealthLamp::Offline) {
        return [OFFLINE_RGB; RING_LEDS];
    }
    if lamp == Some(HealthLamp::WifiDown) {
        return [FAULT_WIFI_RGB; RING_LEDS];
    }
    if lamp == Some(HealthLamp::Reboot) {
        return [FAULT_REBOOT_RGB; RING_LEDS];
    }
    if let Some((from, elapsed_ms)) = fade {
        if elapsed_ms < CLOSE_FADE_MS {
            let k = 1.0 - elapsed_ms as f32 / CLOSE_FADE_MS as f32;
            return scaled(&from, k);
        }
    }
    if state == "listening" {
        return scaled(&state_rgb("listening"), breath_factor(now_ms));
    }
    state_rgb(state)
}

/// Night-mode brightness window (#35, inspired by `home-mate-device-gateway/src/control.rs`).
/// `start_minute` and `end_minute` are minutes since midnight (`0..1440`);
/// when `start_minute > end_minute` (e.g. 1320..420 = 22:00..07:00), the window
/// wraps across midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NightMode {
    pub enabled: bool,
    pub start_minute: u16,
    pub end_minute: u16,
    pub brightness_pct: u8,
}

impl Default for NightMode {
    fn default() -> Self {
        Self {
            enabled: false,
            start_minute: 1320, // 22:00
            end_minute: 420,    // 07:00
            brightness_pct: 10,
        }
    }
}

impl NightMode {
    /// Returns true if `minute_of_day` falls inside the active night window.
    pub fn is_active_at(&self, minute_of_day: u16) -> bool {
        if !self.enabled {
            return false;
        }
        let m = minute_of_day % 1440;
        let start = self.start_minute % 1440;
        let end = self.end_minute % 1440;
        if start < end {
            m >= start && m < end
        } else if start > end {
            m >= start || m < end
        } else {
            true
        }
    }

    /// Returns the brightness multiplier (`0.0..=1.0`) at `minute_of_day`.
    pub fn brightness_scale(&self, minute_of_day: u16) -> f32 {
        if self.is_active_at(minute_of_day) {
            (self.brightness_pct.min(100) as f32) / 100.0
        } else {
            1.0
        }
    }
}

/// LED ring physical orientation mapping (#35).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LedOrientation {
    /// Clockwise slot offset (`0..RING_LEDS`).
    pub rotation: u8,
    /// Reverse direction around the ring before/after applying rotation.
    pub reverse: bool,
}

impl LedOrientation {
    /// Remap logical ring indices (`0..RING_LEDS`) to physical ring positions.
    pub fn apply(&self, frame: &[(u8, u8, u8); RING_LEDS]) -> [(u8, u8, u8); RING_LEDS] {
        let rot = (self.rotation as usize) % RING_LEDS;
        let mut out = [(0u8, 0u8, 0u8); RING_LEDS];
        for (i, &color) in frame.iter().enumerate() {
            let phys = if self.reverse {
                (rot + RING_LEDS - (i % RING_LEDS)) % RING_LEDS
            } else {
                (i + rot) % RING_LEDS
            };
            out[phys] = color;
        }
        out
    }
}

/// Convert UNIX epoch seconds to minute-of-day (`0..1440`).
pub fn minute_of_day_from_unix_secs(secs: u64) -> u16 {
    ((secs / 60) % 1440) as u16
}

/// Current UTC minute-of-day (`0..1440`), safe on all targets without extra syscalls.
pub fn minute_of_day_now() -> u16 {
    minute_of_day_now_with_offset(0)
}

/// Current local minute-of-day (`0..1440`) using an explicit UTC offset in minutes.
pub fn minute_of_day_now_with_offset(tz_offset_minutes: i16) -> u16 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let utc_min = minute_of_day_from_unix_secs(secs) as i32;
    (utc_min + tz_offset_minutes as i32).rem_euclid(1440) as u16
}

/// Saved LED preferences (`<state_dir>/led_config.json`, #35).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SavedLedConfig {
    pub night_mode: NightMode,
    pub led_orientation: LedOrientation,
    #[serde(default)]
    pub tz_offset_minutes: i16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

/// Load `<state_dir>/led_config.json` without `statx` (direct `read_to_string`).
pub fn load_led_config(state_dir: &Path) -> Option<SavedLedConfig> {
    let raw = std::fs::read_to_string(state_dir.join("led_config.json")).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Persist `<state_dir>/led_config.json` atomically (`tmp` + `rename`),
/// propagating any filesystem/serialization error.
pub fn save_led_config(
    state_dir: &Path,
    night_mode: NightMode,
    led_orientation: LedOrientation,
    tz_offset_minutes: i16,
    timezone: Option<String>,
) -> Result<(), String> {
    std::fs::create_dir_all(state_dir)
        .map_err(|e| format!("create state_dir {}: {e}", state_dir.display()))?;
    let doc = SavedLedConfig {
        night_mode,
        led_orientation,
        tz_offset_minutes,
        timezone,
    };
    let body = serde_json::to_string(&doc).map_err(|e| format!("serialize led_config: {e}"))?;
    let target = state_dir.join("led_config.json");
    let tmp = state_dir.join("led_config.json.tmp");
    std::fs::write(&tmp, body).map_err(|e| {
        fwlog!("[leds] led_config.json tmp write failed: {e}");
        format!("write {}: {e}", tmp.display())
    })?;
    std::fs::rename(&tmp, &target).map_err(|e| {
        fwlog!("[leds] led_config.json rename failed: {e}");
        format!("rename {}: {e}", target.display())
    })
}

/// Compose a ring frame with the full priority ladder (`ring_frame`), then
/// apply night-mode dimming (exempting `muted` and `HealthLamp` so privacy and
/// fault indicators remain legible) and physical LED ring orientation.
#[allow(clippy::too_many_arguments)]
pub fn ring_frame_with_prefs(
    muted: bool,
    lamp: Option<HealthLamp>,
    arc_level: Option<u64>,
    fade: Option<FadeWindow>,
    state: &str,
    now_ms: u64,
    night: NightMode,
    minute_of_day: u16,
    orientation: LedOrientation,
) -> [(u8, u8, u8); RING_LEDS] {
    let raw = ring_frame(muted, lamp, arc_level, fade, state, now_ms);
    let dimmed = if muted || lamp.is_some() {
        raw
    } else {
        scaled(&raw, night.brightness_scale(minute_of_day))
    };
    orientation.apply(&dimmed)
}

// --- volume arc (EchoMuse led-ring-states, stage C UX) ------------------------------

/// Arc segment color: cyan, brightness matched to the palette's mid law.
pub const ARC_RGB: (u8, u8, u8) = (0x00, 0x9E, 0x9E);

/// Arc window in ms (upstream `volumeLEDSecs` = 2 s). Physical-button
/// feedback only — core-initiated volume changes never light the arc.
pub const VOLUME_ARC_MS: u64 = 2000;

/// Volume arc frame: N of RING_LEDS lit ∝ level/127 (0 → fully dark),
/// growing from ring index 0 (physical order). Round-to-nearest so the
/// first key step (127/12 ≈ 11) already lights one segment.
pub fn volume_arc_rgb(level: u64) -> [(u8, u8, u8); RING_LEDS] {
    let lit = (((level as usize) * RING_LEDS) + 127 / 2) / 127;
    let mut rgb = [(0u8, 0u8, 0u8); RING_LEDS];
    for slot in rgb.iter_mut().take(lit.min(RING_LEDS)) {
        *slot = ARC_RGB;
    }
    rgb
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_breath_stays_in_its_band_and_never_goes_dark() {
        for t in (0..7000).step_by(25) {
            let k = breath_factor(t);
            assert!(
                (BREATH_MIN_K - 1e-6..=1.0 + 1e-6).contains(&k),
                "breath out of band at {t}ms: {k}"
            );
        }
        // One cycle: the trough is a trough, the peak is the peak.
        assert!(breath_factor(BREATH_PERIOD_MS * 3 / 4) < 0.55);
        assert!(breath_factor(BREATH_PERIOD_MS / 4) > 0.95);
    }

    #[test]
    fn scaling_preserves_the_hue_and_hits_exact_zero() {
        let green = state_rgb("listening");
        let dim = scaled(&green, BREATH_MIN_K);
        assert_eq!(dim[0].0, 0, "red stays off at any breath level");
        assert!(dim[0].1 > 0 && dim[0].2 > 0, "a dim frame still lights");
        assert_eq!(scaled(&green, 0.0)[0], (0, 0, 0), "fade end is true black");
        assert_eq!(scaled(&green, 1.0)[0], green[0]);
    }

    #[test]
    fn hearing_and_listening_are_the_same_green() {
        assert_eq!(state_rgb("hearing")[0], state_rgb("listening")[0]);
        assert!(is_green_family("hearing") && is_green_family("listening"));
        assert!(!is_green_family("idle") && !is_green_family("speaking"));
    }

    #[test]
    fn encode_is_36_hex_bytes_plus_crlf() {
        let l = LedRing::new("/dev/null");
        let mut rgb = [(0u8, 0u8, 0u8); RING_LEDS];
        rgb[0] = (0xAB, 0x0C, 0x10);
        let s = l.encode(&rgb);
        assert_eq!(s.len(), CHANNELS * 2 + 2);
        assert!(s.ends_with("\r\n"));
        assert!(s.starts_with("ab0c10"));
        let tail = "0".repeat(66) + "\r\n";
        assert_eq!(&s[6..], tail);
    }

    #[test]
    fn map_routes_channels() {
        // LED 5's blue on channel 16 under the default sequential map.
        let mut map = default_map();
        map[5] = [30, 31, 16];
        let l = LedRing::with_map("/dev/null", map);
        let mut rgb = [(0u8, 0u8, 0u8); RING_LEDS];
        rgb[5] = (0, 0, 0xFF);
        let s = l.encode(&rgb);
        let bytes = hex_bytes(&s[..CHANNELS * 2]);
        assert_eq!(bytes[16], 0xFF);
        assert_eq!(bytes[30], 0);
    }

    /// One signal, one meaning: every semantic state owns its own color, and
    /// the reserved "receive gate open" green is reachable ONLY via
    /// `listening`. `paused` must also be distinguishable from the two reds
    /// (a paused story is not an error).
    #[test]
    fn every_state_has_its_own_color() {
        let states = [
            "idle",
            "listening",
            "wake",
            "thinking",
            "speaking",
            "music",
            "paused",
            "muted",
            "error",
        ];
        for (i, a) in states.iter().enumerate() {
            for b in states.iter().skip(i + 1) {
                assert_ne!(
                    state_rgb(a)[0],
                    state_rgb(b)[0],
                    "{a} and {b} must not share a color"
                );
            }
            if *a != "listening" {
                assert_ne!(
                    state_rgb(a)[0],
                    (0x00, 0xA0, 0x50),
                    "{a} must not use reserved green"
                );
            }
        }
        // Hue check for the amber: red channel dominant over green, no blue —
        // and far enough from both reds to be told apart at a glance.
        let (pr, pg, pb) = state_rgb("paused")[0];
        assert!(pr > pg && pb == 0, "paused must be amber");
        assert!(
            pr.saturating_sub(state_rgb("error")[0].0) < 0x20 || pg > 0x40,
            "distinct from error"
        );
        // Unknown states paint dark, never the previous color.
        assert_eq!(state_rgb("nonsense"), [(0, 0, 0); RING_LEDS]);
    }

    #[test]
    fn volume_arc_lights_proportionally() {
        let zero = volume_arc_rgb(0);
        assert!(zero.iter().all(|&c| c == (0, 0, 0)), "level 0 paints dark");
        let full = volume_arc_rgb(127);
        assert!(
            full.iter().all(|&c| c == ARC_RGB),
            "level 127 lights all 12"
        );
        let mid = volume_arc_rgb(64);
        let lit = mid.iter().filter(|&&c| c == ARC_RGB).count();
        // 64*12/127 rounds to 6 segments — and they are the leading ones.
        assert_eq!(lit, 6, "got {lit}");
        assert!(mid[..6].iter().all(|&c| c == ARC_RGB));
        assert_eq!(mid[6], (0, 0, 0));
    }

    /// v1 mechanism (#12): the four health lamps are pairwise distinct AND
    /// distinct from the privacy reds, the semantic error red and the
    /// reserved green. Numeric inequality is the unit gate; the owner's
    /// on-device eye is the acceptance gate (values are proposals).
    #[test]
    fn health_lamps_are_pairwise_distinct_and_clear_of_reds_and_green() {
        let lamps = [
            ("reboot", FAULT_REBOOT_RGB),
            ("offline", OFFLINE_RGB),
            ("wifi_down", FAULT_WIFI_RGB),
            ("wifi_provisioning", WIFI_PROVISIONING_RGB),
            ("protocol", FAULT_PROTOCOL_RGB),
        ];
        for (i, (an, av)) in lamps.iter().enumerate() {
            for (bn, bv) in lamps.iter().skip(i + 1) {
                assert_ne!(av, bv, "{an} and {bn} must not share a color");
            }
            assert_ne!(
                *av,
                state_rgb("muted")[0],
                "{an} must not read as the privacy red"
            );
            assert_ne!(
                *av,
                state_rgb("error")[0],
                "{an} must not read as the semantic error red"
            );
            assert_ne!(
                *av,
                state_rgb("listening")[0],
                "{an} must not use the reserved green"
            );
            assert_ne!(
                *av,
                state_rgb("wake")[0],
                "{an} must not read as the wake gray"
            );
        }
        // reboot is exactly one brightness step above offline (never shown
        // together — the state machine guarantees it — but the two greys
        // must still not collide).
        assert!(FAULT_REBOOT_RGB.0 > OFFLINE_RGB.0);
    }

    /// The ladder, verbatim: muted > protocol > arc > offline > wifi >
    /// reboot > fade > breath > semantic state. A reordering is a product
    /// decision, not a refactor.
    #[test]
    fn the_ring_ladder_is_the_ladder() {
        let all = [
            None,
            Some(HealthLamp::Reboot),
            Some(HealthLamp::Offline),
            Some(HealthLamp::WifiDown),
            Some(HealthLamp::WifiProvisioning),
            Some(HealthLamp::Protocol),
        ];
        // 1. Mute beats every lamp (privacy is local).
        for lamp in all {
            assert_eq!(
                ring_frame(true, lamp, Some(64), None, "listening", 0),
                state_rgb("muted")
            );
            assert_eq!(
                ring_frame(true, lamp, None, None, "speaking", 0),
                state_rgb("muted")
            );
        }
        // 2. Protocol beats the arc; 3. arc beats the other lamps.
        assert_eq!(
            ring_frame(
                false,
                Some(HealthLamp::Protocol),
                Some(64),
                None,
                "speaking",
                0
            ),
            [FAULT_PROTOCOL_RGB; RING_LEDS]
        );
        assert_eq!(
            ring_frame(
                false,
                Some(HealthLamp::Offline),
                Some(64),
                None,
                "speaking",
                0
            ),
            volume_arc_rgb(64)
        );
        assert_eq!(
            ring_frame(
                false,
                Some(HealthLamp::WifiDown),
                Some(64),
                None,
                "speaking",
                0
            ),
            volume_arc_rgb(64)
        );
        // 4/5/6. wifi_provisioning > offline > wifi > reboot.
        assert_eq!(
            ring_frame(
                false,
                Some(HealthLamp::WifiProvisioning),
                None,
                None,
                "speaking",
                0
            ),
            [WIFI_PROVISIONING_RGB; RING_LEDS]
        );
        assert_eq!(
            ring_frame(false, Some(HealthLamp::Offline), None, None, "speaking", 0),
            [OFFLINE_RGB; RING_LEDS]
        );
        assert_eq!(
            ring_frame(false, Some(HealthLamp::WifiDown), None, None, "speaking", 0),
            [FAULT_WIFI_RGB; RING_LEDS]
        );
        assert_eq!(
            ring_frame(false, Some(HealthLamp::Reboot), None, None, "speaking", 0),
            [FAULT_REBOOT_RGB; RING_LEDS]
        );
        // 7. A mid-fade green loses to every lamp, wins over the semantic state.
        let green = state_rgb("listening");
        let fading = ring_frame(false, None, None, Some((green, 100)), "speaking", 0);
        assert_eq!(fading, scaled(&green, 1.0 - 100.0 / CLOSE_FADE_MS as f32));
        // 8/9. breath on listening, static otherwise. A quarter period is
        // the sine peak, so the frame is exactly the full reserved green.
        let breath = ring_frame(false, None, None, None, "listening", BREATH_PERIOD_MS / 4);
        assert_eq!(
            breath,
            state_rgb("listening"),
            "a breath peak is the full green"
        );
        assert_eq!(
            ring_frame(false, None, None, None, "speaking", 0),
            state_rgb("speaking")
        );
        // A finished fade shows the new state, not a stale ramp frame.
        assert_eq!(
            ring_frame(
                false,
                None,
                None,
                Some((green, CLOSE_FADE_MS + 1)),
                "speaking",
                0
            ),
            state_rgb("speaking")
        );
    }

    #[test]
    fn parse_map_rejects_bad_shapes_and_duplicates() {
        let good = serde_json::to_string(&default_map().to_vec()).unwrap();
        assert_eq!(parse_map_json(&good), Some(default_map()));
        assert!(parse_map_json("[[0,1,2]]").is_none(), "12 rows required");
        assert!(parse_map_json("not json").is_none());
        let dup = serde_json::to_string(&vec![vec![0, 0, 1]; RING_LEDS]).unwrap();
        assert!(parse_map_json(&dup).is_none(), "duplicate PWM channel");
        let mut oor = default_map();
        oor[0] = [36, 1, 2];
        assert!(parse_map_json(&serde_json::to_string(&oor).unwrap()).is_none());
    }

    #[test]
    fn missing_map_file_falls_back_to_default() {
        let p = std::path::Path::new("/definitely/not/a/real/led-map.json");
        let l = LedRing::with_calibrated_map("/dev/null", p);
        assert_eq!(l.map, default_map());
    }

    #[test]
    fn calibration_frame_lights_exactly_one_lamp() {
        let f = calibration_frame(7);
        assert_eq!(f[7], (0x60, 0x60, 0x60));
        assert!(f.iter().enumerate().all(|(k, &c)| k == 7 || c == (0, 0, 0)));
        // Out-of-range index is a dark ring, never a panic.
        assert!(calibration_frame(99).iter().all(|&c| c == (0, 0, 0)));
    }

    fn hex_bytes(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn night_mode_window_wrapping_midnight() {
        let night = NightMode {
            enabled: true,
            start_minute: 1320, // 22:00
            end_minute: 420,    // 07:00
            brightness_pct: 10,
        };
        assert!(!night.is_active_at(1319)); // 21:59
        assert!(night.is_active_at(1320)); // 22:00 inclusive
        assert!(night.is_active_at(1439)); // 23:59
        assert!(night.is_active_at(0)); // 00:00
        assert!(night.is_active_at(419)); // 06:59
        assert!(!night.is_active_at(420)); // 07:00 exclusive
        assert!(!night.is_active_at(720)); // 12:00

        let disabled = NightMode {
            enabled: false,
            ..night
        };
        assert!(!disabled.is_active_at(0));
        assert_eq!(disabled.brightness_scale(0), 1.0);
    }

    #[test]
    fn night_mode_brightness_scaling_and_ladder_preservation() {
        let night = NightMode {
            enabled: true,
            start_minute: 1320,
            end_minute: 420,
            brightness_pct: 25,
        };
        let orient = LedOrientation::default();
        // Normal semantic state dims to 25% inside night window.
        let speaking_night = ring_frame_with_prefs(
            false, None, None, None, "speaking", 0, night, 60, // 01:00
            orient,
        );
        assert_eq!(speaking_night, scaled(&state_rgb("speaking"), 0.25));

        // Outside night window stays at 100%.
        let speaking_day = ring_frame_with_prefs(
            false, None, None, None, "speaking", 0, night, 720, // 12:00
            orient,
        );
        assert_eq!(speaking_day, state_rgb("speaking"));

        // Muted & HealthLamp preserve full priority and brightness even at night.
        let muted_night =
            ring_frame_with_prefs(true, None, None, None, "speaking", 0, night, 60, orient);
        assert_eq!(muted_night, state_rgb("muted"));

        let proto_night = ring_frame_with_prefs(
            false,
            Some(HealthLamp::Protocol),
            Some(64),
            None,
            "speaking",
            0,
            night,
            60,
            orient,
        );
        assert_eq!(proto_night, [FAULT_PROTOCOL_RGB; RING_LEDS]);

        let offline_night = ring_frame_with_prefs(
            false,
            Some(HealthLamp::Offline),
            None,
            None,
            "speaking",
            0,
            night,
            60,
            orient,
        );
        assert_eq!(offline_night, [OFFLINE_RGB; RING_LEDS]);
    }

    #[test]
    fn orientation_rotation_and_reverse_mapping() {
        let mut frame = [(0u8, 0u8, 0u8); RING_LEDS];
        frame[0] = (10, 0, 0);
        frame[1] = (0, 20, 0);
        frame[2] = (0, 0, 30);

        let rot3 = LedOrientation {
            rotation: 3,
            reverse: false,
        };
        let r3 = rot3.apply(&frame);
        assert_eq!(r3[3], (10, 0, 0));
        assert_eq!(r3[4], (0, 20, 0));
        assert_eq!(r3[5], (0, 0, 30));

        let rev_only = LedOrientation {
            rotation: 0,
            reverse: true,
        };
        let rv = rev_only.apply(&frame);
        assert_eq!(rv[0], (10, 0, 0));
        assert_eq!(rv[11], (0, 20, 0));
        assert_eq!(rv[10], (0, 0, 30));

        let rot3_rev = LedOrientation {
            rotation: 3,
            reverse: true,
        };
        let r3v = rot3_rev.apply(&frame);
        assert_eq!(r3v[3], (10, 0, 0));
        assert_eq!(r3v[2], (0, 20, 0));
        assert_eq!(r3v[1], (0, 0, 30));
    }
}
