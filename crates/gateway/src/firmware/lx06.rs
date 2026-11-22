//! Xiaomi AI Speaker Pro (`LX06`) direct-occupation hardware port.
//!
//! Hardware baseline verified via live SSH probe on `LX06` (`ROM 1.94.13`,
//! `Linux 4.9.61 aarch64`):
//!   - Capture: `/dev/snd/pcmC0D3c` (`hw:0,3`, `PDM-dummy`), `S32_LE`, 8ch,
//!     48 kHz (`ch0..5` = 6 PDM mics, `ch6..7` = digital loopback).
//!   - Playback: `/dev/snd/pcmC0D2p` (`hw:0,2`, `TDM-C-tas5805`), `S16_LE`,
//!     2ch, 48 kHz.
//!   - LED ring: `AW20054` at `/sys/devices/i2c-0/0-003a/led_rgb` (18 LEDs,
//!     `"<idx> 0xBBGGRR"`).
//!   - Keypad: `/dev/input/event0` (`adc_keypad`, verified against stock
//!     `/etc/init.d/input` & `/usr/sbin/input.sh`: `102` Mute/Hold, `114`
//!     Play/Pause -> Action, `115` Vol-, `139` Vol+).

#[path = "lx06_pcm.rs"]
mod pcm;

use super::{
    alsa, buttons, leds, mixer, ButtonEvent, FirmwareConfig, Hardware, PLAY_CHANNELS,
    PLAY_FORMAT_S16_LE, PLAY_PERIOD, PLAY_RATE,
};
use crate::fwlog;
use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

pub const LX06_LED_PATH: &str = "/sys/devices/i2c-0/0-003a/led_rgb";
pub const LX06_LED_COUNT: usize = 18;
pub const LX06_BUTTON_DEV: &str = "/dev/input/event0";
pub const LX06_CAP_PCM: (u32, u32) = (0, 3);
pub const LX06_PLAY_PCM: (u32, u32) = (0, 2);

pub const LX06_CAP_RATE: u32 = 48_000;
pub const LX06_CAP_CHANNELS: usize = 8;
pub const LX06_CAP_FORMAT_S32_LE: u32 = 10; // SNDRV_PCM_FORMAT_S32_LE
pub const LX06_CAP_PERIOD: u32 = 512;
pub const LX06_CAP_BUFFER: u32 = 4096; // 4096 * 32B = 131072B (exact hardware max)

/// Right shift for ch0..5 (6 PDM mics) from S32_LE to i16 (~24 dB headroom,
/// verified not to clip at 80% volume where peak is 6.8e7).
pub const LX06_MIC_SHIFT: u32 = 12;
/// Right shift for ch6..7 (2-ch digital playback reference, full scale 2^28,
/// peak 8.6e8 at 80% volume).
pub const LX06_REF_SHIFT: u32 = 16;

/// Encode a calibrated `i16` mic sample (`ch0..7`) into `S24_3LE` so
/// `capture.rs`'s `decode_s24_block` (which shifts mic channels right by
/// `MIC_GAIN_SHIFT = 4`) recovers the exact `i16` value without loss or
/// clipping.
#[inline]
fn encode_i16_to_s24_3le(sample: i16, out: &mut [u8]) {
    let v = (sample as i32) << 4;
    out[0] = (v & 0xFF) as u8;
    out[1] = ((v >> 8) & 0xFF) as u8;
    out[2] = ((v >> 16) & 0xFF) as u8;
}

/// Encode a calibrated `i16` reference sample (`ch8`) into `S24_3LE` so
/// `capture.rs`'s `decode_s24_block` (which shifts `REF_CHANNEL = 8` right by
/// `8`, unlike mic channels' `>> 4`) recovers the exact `i16` value (`<< 8`
/// then `>> 8`). Without `<< 8`, `decode_s24_block` attenuated `ch8` by an
/// extra `16x` (`24 dB`), crippling SpeexDSP AEC during music playback.
#[inline]
fn encode_ref_i16_to_s24_3le(sample: i16, out: &mut [u8]) {
    let v = (sample as i32) << 8;
    out[0] = (v & 0xFF) as u8;
    out[1] = ((v >> 8) & 0xFF) as u8;
    out[2] = ((v >> 16) & 0xFF) as u8;
}

/// Decimate 3×N frames of 48 kHz 8-channel `S32_LE` (LX06 `hw:0,3`) into N
/// frames of 16 kHz 9-channel `S24_3LE` expected by `capture_thread`:
///   - `ch0..5`: 6 anti-alias-averaged PDM mics (`>> 12`)
///   - `ch6`:    mean of `ch0..5` (fills the 7th mic slot for 7-mic fusion)
///   - `ch7`:    0
///   - `ch8`:    anti-alias-averaged stereo digital loopback `ch6..7` (`>> 16`)
pub fn decimate_lx06_s32_to_s24_9ch(raw_s32: &[u8], out_s24: &mut [u8]) -> usize {
    const IN_FRAME_BYTES: usize = LX06_CAP_CHANNELS * 4; // 32 B per 48k frame
    const TRIPLET_BYTES: usize = IN_FRAME_BYTES * 3; // 96 B per 16k output frame
    const OUT_FRAME_BYTES: usize = 9 * 3; // 27 B per 16k 9ch S24_3LE frame

    let out_frames = (raw_s32.len() / TRIPLET_BYTES).min(out_s24.len() / OUT_FRAME_BYTES);
    for f in 0..out_frames {
        let in_base = f * TRIPLET_BYTES;
        let out_base = f * OUT_FRAME_BYTES;

        let mut mic_sum = [0i64; 6];
        let mut ref_sum = 0i64;

        for sub in 0..3 {
            let f_base = in_base + sub * IN_FRAME_BYTES;
            for (c, m) in mic_sum.iter_mut().enumerate() {
                let off = f_base + c * 4;
                let s = i32::from_le_bytes([
                    raw_s32[off],
                    raw_s32[off + 1],
                    raw_s32[off + 2],
                    raw_s32[off + 3],
                ]);
                *m += s as i64;
            }
            for c in 6..8 {
                let off = f_base + c * 4;
                let s = i32::from_le_bytes([
                    raw_s32[off],
                    raw_s32[off + 1],
                    raw_s32[off + 2],
                    raw_s32[off + 3],
                ]);
                ref_sum += s as i64;
            }
        }

        let mut mic_i16 = [0i16; 6];
        let mut six_mic_acc = 0i32;
        for c in 0..6 {
            let avg = mic_sum[c] / 3;
            let scaled = (avg >> LX06_MIC_SHIFT).clamp(i16::MIN as i64, i16::MAX as i64) as i16;
            mic_i16[c] = scaled;
            six_mic_acc += scaled as i32;
            encode_i16_to_s24_3le(
                scaled,
                &mut out_s24[out_base + c * 3..out_base + (c + 1) * 3],
            );
        }

        // ch6: synthetic 7th mic = mean of the 6 perimeter mics.
        let ch6_i16 = (six_mic_acc / 6).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        encode_i16_to_s24_3le(ch6_i16, &mut out_s24[out_base + 6 * 3..out_base + 7 * 3]);

        // ch7: unused pad channel.
        out_s24[out_base + 7 * 3..out_base + 8 * 3].fill(0);

        // ch8: hardware digital reference (average of ch6+ch7 across 3 subframes).
        let ref_avg = ref_sum / 6;
        let ref_i16 = (ref_avg >> LX06_REF_SHIFT).clamp(i16::MIN as i64, i16::MAX as i64) as i16;
        encode_ref_i16_to_s24_3le(ref_i16, &mut out_s24[out_base + 8 * 3..out_base + 9 * 3]);
    }
    out_frames
}

/// AW20054 18-LED ring controller (`/sys/devices/i2c-0/0-003a/led_rgb`).
pub struct Aw20054Ring {
    led_rgb_path: String,
    led_fade_path: Option<std::path::PathBuf>,
    last_codes: [Option<u32>; LX06_LED_COUNT],
}

impl Aw20054Ring {
    pub fn new(led_rgb_path: &str) -> Self {
        let led_fade_path = std::path::Path::new(led_rgb_path).parent().map(|dir| {
            // Never write `led_hwen`: aw20054.ko's `led_hwen_store("1")`
            // pulses the HWEN reset pin WITHOUT re-running I2C init
            // (`0x02 = 0x01`), leaving the chip in hardware shutdown.
            // Boost AW20054 constant-current sink to maximum (0x07 = 160mA)
            // so the ring stays clearly visible through LX06's dark top diffuser.
            let _ = std::fs::write(dir.join("led_imax"), "0x07\n");
            let fade = dir.join("led_fade");
            let _ = std::fs::write(&fade, "r 0xff\n");
            let _ = std::fs::write(&fade, "g 0xff\n");
            let _ = std::fs::write(&fade, "b 0xff\n");
            fade
        });
        Self {
            led_rgb_path: led_rgb_path.into(),
            led_fade_path,
            last_codes: [None; LX06_LED_COUNT],
        }
    }

    // Native ledserver retains control of current and fade for its own frames.
    fn shared(led_rgb_path: &str) -> Self {
        Self {
            led_rgb_path: led_rgb_path.to_owned(),
            led_fade_path: None,
            last_codes: [None; LX06_LED_COUNT],
        }
    }

    fn assert_full_fade(&self) {
        if let Some(fade) = &self.led_fade_path {
            let _ = std::fs::write(fade, "r 0xff\n");
            let _ = std::fs::write(fade, "g 0xff\n");
            let _ = std::fs::write(fade, "b 0xff\n");
        }
    }

    /// Boost per-channel PWM (`0..=255`) for LX06's dark top diffuser while
    /// preserving hue ratios across `(r, g, b)` and keeping `0` dark.
    ///
    /// Note: the AW20054 kernel driver extracts `val >> 2` (6-bit PWM `0..63`),
    /// so values `< 4` truncate to `0` in hardware. We scale the colour vector
    /// by `2.2x` (clamped so the dominant channel never exceeds `255`, preserving
    /// R:G:B hue balance) and enforce a minimum floor of `4` (`1` in 6-bit PWM)
    /// for any non-zero channel.
    #[inline]
    pub fn boost_rgb_for_diffuser(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
        let max_c = r.max(g).max(b);
        if max_c == 0 {
            return (0, 0, 0);
        }
        let target_max = ((max_c as u32 * 22 + 5) / 10).min(255);
        let scale_chan = |c: u8| -> u8 {
            if c == 0 {
                0
            } else {
                let s = (c as u32 * target_max + (max_c as u32 / 2)) / (max_c as u32);
                s.clamp(4, 255) as u8
            }
        };
        (scale_chan(r), scale_chan(g), scale_chan(b))
    }

    /// Pack `(r, g, b)` into AW20054's `0xBBGGRR` integer (`R | G<<8 | B<<16`).
    #[inline]
    pub fn pack_bbggrr(r: u8, g: u8, b: u8) -> u32 {
        (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
    }

    /// Map a 12-LED logical ring frame onto 18 physical LEDs (`0..18`),
    /// applying diffuser brightness compensation.
    pub fn map_12_to_18(rgb12: &[(u8, u8, u8); leds::RING_LEDS]) -> [u32; LX06_LED_COUNT] {
        let mut out = [0u32; LX06_LED_COUNT];
        for (i, slot) in out.iter_mut().enumerate() {
            let (r, g, b) = rgb12[(i * leds::RING_LEDS) / LX06_LED_COUNT];
            let (br, bg, bb) = Self::boost_rgb_for_diffuser(r, g, b);
            *slot = Self::pack_bbggrr(br, bg, bb);
        }
        out
    }

    pub fn write(&mut self, rgb12: &[(u8, u8, u8); leds::RING_LEDS]) -> io::Result<()> {
        let targets = Self::map_12_to_18(rgb12);
        if self.last_codes == targets.map(Some) {
            return Ok(());
        }
        // Re-assert full fade if this is the very first frame write.
        if self.last_codes.iter().all(|c| c.is_none()) {
            self.assert_full_fade();
        }
        // Open led_rgb per changed LED so sysfs kernfs_fop_write always sees
        // offset 0; keeping a single fd open advances ppos past 0 on subsequent writes.
        for (idx, &code) in targets.iter().enumerate() {
            if self.last_codes[idx] == Some(code) {
                continue;
            }
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(&self.led_rgb_path)?;
            let cmd = format!("{idx} 0x{code:06x}\n");
            f.write_all(cmd.as_bytes())?;
            self.last_codes[idx] = Some(code);
        }
        Ok(())
    }
}

/// Maximum buffered 16 kHz S24_3LE 9ch frames in the capture drain queue
/// (6 × 80 ms = 480 ms = 207,360 bytes). Decouples the 85.33 ms hardware DMA
/// ring (`4096 @ 48 kHz`) from SpeexDSP AEC + KWS processing latency so
/// `hw:0,3` never hits `EPIPE` overruns.
const CAPTURE_QUEUE_MAX_FRAMES: usize = 1280 * 6;
const OUT_FRAME_BYTES: usize = 9 * 3;
const CAPTURE_QUEUE_MAX_BYTES: usize = CAPTURE_QUEUE_MAX_FRAMES * OUT_FRAME_BYTES;

struct CaptureQueueState {
    buf: VecDeque<u8>,
    pending_err: Option<io::Error>,
    closed: bool,
}

struct CaptureQueue {
    state: Mutex<CaptureQueueState>,
    cv: Condvar,
}

impl CaptureQueue {
    fn new() -> Self {
        Self {
            state: Mutex::new(CaptureQueueState {
                buf: VecDeque::with_capacity(CAPTURE_QUEUE_MAX_BYTES),
                pending_err: None,
                closed: false,
            }),
            cv: Condvar::new(),
        }
    }

    fn push_s24(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let overflow = st
            .buf
            .len()
            .saturating_add(bytes.len())
            .saturating_sub(CAPTURE_QUEUE_MAX_BYTES);
        if overflow > 0 {
            // Drop whole 16 kHz frames from the front so frame alignment stays intact.
            let drop_bytes = overflow.div_ceil(OUT_FRAME_BYTES) * OUT_FRAME_BYTES;
            let actual = drop_bytes.min(st.buf.len());
            st.buf.drain(..actual);
        }
        st.buf.extend(bytes);
        self.cv.notify_all();
    }

    fn push_err(&self, err: io::Error) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.buf.clear();
        st.pending_err = Some(err);
        self.cv.notify_all();
    }

    fn pop_frames(&self, dst: &mut [u8], frames: u32) -> io::Result<u32> {
        let want_bytes = (frames as usize) * OUT_FRAME_BYTES;
        if want_bytes == 0 {
            return Ok(0);
        }
        if dst.len() < want_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "capture destination buffer smaller than requested frames",
            ));
        }
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(err) = st.pending_err.take() {
                return Err(err);
            }
            if st.buf.len() >= want_bytes {
                let (a, b) = st.buf.as_slices();
                if a.len() >= want_bytes {
                    dst[..want_bytes].copy_from_slice(&a[..want_bytes]);
                } else {
                    dst[..a.len()].copy_from_slice(a);
                    let rem = want_bytes - a.len();
                    dst[a.len()..want_bytes].copy_from_slice(&b[..rem]);
                }
                st.buf.drain(..want_bytes);
                return Ok(frames);
            }
            if st.closed {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "lx06 capture drain thread exited",
                ));
            }
            st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }
}

fn pin_capture_drain_thread() {
    #[cfg(target_os = "linux")]
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(0, &mut set);
        let _ = libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
        let mut param: libc::sched_param = std::mem::zeroed();
        param.sched_priority = 60;
        let _ = libc::sched_setscheduler(0, libc::SCHED_FIFO, &param);
    }
}

fn capture_drain_loop(mut cap: pcm::Capture, queue: Arc<CaptureQueue>, muted: Arc<AtomicBool>) {
    pin_capture_drain_thread();
    const IN_FRAME_BYTES: usize = LX06_CAP_CHANNELS * 4;
    const TRIPLET_BYTES: usize = IN_FRAME_BYTES * 3;
    let period_bytes = (LX06_CAP_PERIOD as usize) * IN_FRAME_BYTES;
    // Carry buffer holds up to 2 leftover 48 kHz frames + 1 full period.
    let mut raw = vec![0u8; 2 * IN_FRAME_BYTES + period_bytes];
    let mut carry_bytes = 0usize;
    let mut out_s24 = vec![0u8; (LX06_CAP_PERIOD as usize).div_ceil(3) * OUT_FRAME_BYTES];
    let mut consecutive_xruns = 0u32;

    loop {
        if queue.state.lock().unwrap_or_else(|e| e.into_inner()).closed {
            return;
        }
        let dst = &mut raw[carry_bytes..carry_bytes + period_bytes];
        match cap.read_period(dst) {
            Ok(0) => continue,
            Ok(n) => {
                consecutive_xruns = 0;
                let total_bytes = carry_bytes + (n as usize) * IN_FRAME_BYTES;
                let complete_triplets = total_bytes / TRIPLET_BYTES;
                let consumed_bytes = complete_triplets * TRIPLET_BYTES;
                if complete_triplets > 0 {
                    let produced = decimate_lx06_s32_to_s24_9ch(
                        &raw[..consumed_bytes],
                        &mut out_s24[..complete_triplets * OUT_FRAME_BYTES],
                    );
                    if muted.load(Ordering::Relaxed) {
                        // Zero all mic channels (ch0..7, bytes 0..24) at the
                        // Hardware boundary while keeping ch8 (bytes 24..27) intact.
                        for f in 0..produced {
                            let base = f * OUT_FRAME_BYTES;
                            out_s24[base..base + 24].fill(0);
                        }
                    }
                    queue.push_s24(&out_s24[..produced * OUT_FRAME_BYTES]);
                }
                let rem = total_bytes - consumed_bytes;
                if rem > 0 {
                    raw.copy_within(consumed_bytes..total_bytes, 0);
                }
                carry_bytes = rem;
            }
            Err(e) => {
                carry_bytes = 0;
                if matches!(cap, pcm::Capture::Shared { .. }) {
                    let mut state = queue.state.lock().unwrap_or_else(|e| e.into_inner());
                    state.closed = true;
                    state.buf.clear();
                    state.pending_err = Some(e);
                    queue.cv.notify_all();
                    return;
                }
                let is_xrun = matches!(
                    e.raw_os_error(),
                    Some(code) if code == libc::EPIPE || code == alsa::ESTRPIPE
                );
                if is_xrun {
                    consecutive_xruns = consecutive_xruns.saturating_add(1);
                    // alsa::Pcm::readi already ran prepare_and_start(). Do NOT
                    // clear the capture queue or propagate transient xruns to
                    // capture_thread (which would drop buffered speech onset!).
                    if consecutive_xruns <= 16 {
                        continue;
                    }
                }
                consecutive_xruns = 0;
                queue.push_err(e);
            }
        }
    }
}

enum Playback {
    Direct(alsa::Pcm),
    Shared(pcm::SharedPlayback),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IndicatorOwner {
    Home,
    Idle,
    Native,
}

struct Indicator {
    path: String,
    active: bool,
    ring: Option<Aw20054Ring>,
}

impl Indicator {
    fn set_active(&mut self, owner: IndicatorOwner) {
        self.active = owner == IndicatorOwner::Home;
        if !self.active {
            if let Some(mut ring) = self.ring.take().filter(|_| owner == IndicatorOwner::Idle) {
                if let Err(error) = ring.write(&[(0, 0, 0); leds::RING_LEDS]) {
                    fwlog!("[lx06] could not clear the Home indicator on release: {error}");
                }
            }
        }
    }

    fn write(&mut self, rgb: &[(u8, u8, u8); leds::RING_LEDS]) -> io::Result<()> {
        if self.active {
            self.ring
                .get_or_insert_with(|| Aw20054Ring::shared(&self.path))
                .write(rgb)?;
        }
        Ok(())
    }
}

/// Atomically publish a manual mode selection to the supervisor.
pub fn select_mode(path: &std::path::Path, mode: &str) -> io::Result<()> {
    let pending = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&pending, format!("{mode}\n"))?;
    std::fs::rename(pending, path)
}

pub struct Lx06Hardware {
    capture_queue: Arc<CaptureQueue>,
    muted: Arc<AtomicBool>,
    playback: Mutex<Playback>,
    indicator: Mutex<Indicator>,
    passive_action: Option<u16>,
    buttons: buttons::SharedButtonHub,
}

impl Lx06Hardware {
    pub fn open(cfg: &FirmwareConfig) -> io::Result<Self> {
        let shared = std::env::var("HM_LX06_DUAL_WAKE").as_deref() == Ok("1");
        let hub = buttons::SharedButtonHub::open_with_keymap(
            &cfg.button_devices,
            cfg.profile.key_map,
            !shared,
        )?;
        let (cap, playback) = if shared {
            (
                pcm::Capture::shared()?,
                Playback::Shared(pcm::SharedPlayback::default()),
            )
        } else {
            Self::open_direct_pcm(cfg)?
        };

        let capture_queue = Arc::new(CaptureQueue::new());
        let muted = Arc::new(AtomicBool::new(false));
        let q = capture_queue.clone();
        let m = muted.clone();
        std::thread::Builder::new()
            .name("lx06-cap-drain".into())
            .spawn(move || capture_drain_loop(cap, q, m))?;

        Ok(Self {
            capture_queue,
            muted,
            playback: Mutex::new(playback),
            indicator: Mutex::new(Indicator {
                path: cfg.led_frame_path.clone(),
                active: !shared,
                ring: (!shared).then(|| Aw20054Ring::new(&cfg.led_frame_path)),
            }),
            buttons: hub,
            passive_action: shared.then_some(cfg.profile.key_map.action),
        })
    }

    fn open_direct_pcm(cfg: &FirmwareConfig) -> io::Result<(pcm::Capture, Playback)> {
        if let Ok(ctl) = mixer::Ctl::open("/dev/snd/controlC0") {
            fwlog!("[lx06] controlC0: {} controls enumerated", ctl.known());
            // Ensure digital loopback (ch6/ch7) is enabled for AEC reference.
            if let Err(e) = ctl.set("Loopback Enable", &["Enable"]) {
                fwlog!("[lx06] Loopback Enable = Enable failed (non-fatal): {e}");
            }
            if let Err(e) = ctl.set("Loopback datain source", &["PDMIN"]) {
                fwlog!("[lx06] Loopback datain source = PDMIN failed (non-fatal): {e}");
            }
            // Route TDMIN_C (the internal digital loopback of TDM-C / TAS5805
            // playback on hw:0,2; DTS `datalb_src = 5`) into ch6..7 of hw:0,3.
            // Stock `/etc/asound.state` overrides this to TDMOUT_B (`hw:0,1`
            // dummy), which leaves ch6..7 completely silent during hw:0,2
            // playback and starves SpeexDSP AEC of its reference signal.
            if let Err(e) = ctl.set("Loopback tmdin lb source", &["TDMIN_C"]) {
                fwlog!("[lx06] Loopback tmdin lb source = TDMIN_C failed (non-fatal): {e}");
            }
        }

        let mut cap = alsa::Pcm::open(cfg.capture_pcm.0, cfg.capture_pcm.1, true)?;
        cap.configure(
            true,
            LX06_CAP_FORMAT_S32_LE,
            LX06_CAP_CHANNELS as u32,
            LX06_CAP_RATE,
            LX06_CAP_PERIOD,
            LX06_CAP_BUFFER,
        )?;
        fwlog!(
            "[lx06] capture hw:{},{} configured: S32_LE {}ch {}Hz period={} buffer={} (async drain queue {}ms)",
            cfg.capture_pcm.0,
            cfg.capture_pcm.1,
            LX06_CAP_CHANNELS,
            LX06_CAP_RATE,
            LX06_CAP_PERIOD,
            LX06_CAP_BUFFER,
            CAPTURE_QUEUE_MAX_FRAMES * 1000 / 16_000
        );

        let mut play = alsa::Pcm::open(cfg.playback_pcm.0, cfg.playback_pcm.1, false)?;
        let play_configured = play.configure(
            false,
            PLAY_FORMAT_S16_LE,
            PLAY_CHANNELS,
            PLAY_RATE,
            PLAY_PERIOD,
            cfg.play_buffer,
        );
        if let Err(first_err) = play_configured {
            fwlog!(
                "[lx06] playback configure(period={}, buf={}) rejected ({first_err}); retrying 1920/7680",
                PLAY_PERIOD,
                cfg.play_buffer
            );
            play.configure(
                false,
                PLAY_FORMAT_S16_LE,
                PLAY_CHANNELS,
                PLAY_RATE,
                1920,
                7680,
            )?;
        }
        // Clock two silent periods into TAS5805 DAC on startup.
        let silence = vec![0u8; PLAY_PERIOD as usize * PLAY_CHANNELS as usize * 2];
        let _ = play.writei(&silence, PLAY_PERIOD);
        let _ = play.writei(&silence, PLAY_PERIOD);

        Ok((pcm::Capture::Direct(cap), Playback::Direct(play)))
    }
}

impl Drop for Lx06Hardware {
    fn drop(&mut self) {
        self.capture_queue
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .closed = true;
        self.capture_queue.cv.notify_all();
    }
}

impl Hardware for Lx06Hardware {
    fn capture_read(&self, buf: &mut [u8], frames: u32) -> io::Result<u32> {
        self.capture_queue.pop_frames(buf, frames)
    }

    fn playback_write(&self, stereo: &[i16], frames: u32) -> io::Result<u32> {
        let bytes =
            unsafe { std::slice::from_raw_parts(stereo.as_ptr().cast::<u8>(), stereo.len() * 2) };
        match &mut *self.playback.lock().unwrap_or_else(|e| e.into_inner()) {
            Playback::Direct(pcm) => pcm.writei(bytes, frames),
            Playback::Shared(pcm) => pcm.write(bytes, frames),
        }
    }

    fn playback_flush(&self) -> io::Result<()> {
        match &mut *self.playback.lock().unwrap_or_else(|e| e.into_inner()) {
            Playback::Direct(pcm) => pcm.flush(),
            Playback::Shared(pcm) => pcm.flush(),
        }
    }

    fn playback_releases_when_idle(&self) -> bool {
        matches!(
            *self.playback.lock().unwrap_or_else(|e| e.into_inner()),
            Playback::Shared(_)
        )
    }

    fn playback_drain(&self) -> io::Result<()> {
        match &mut *self.playback.lock().unwrap_or_else(|e| e.into_inner()) {
            Playback::Direct(pcm) => pcm.drain(),
            Playback::Shared(pcm) => pcm.drain(),
        }
    }

    fn playback_underruns(&self) -> u64 {
        match &*self.playback.lock().unwrap_or_else(|e| e.into_inner()) {
            Playback::Direct(pcm) => pcm.underruns(),
            Playback::Shared(_) => 0, // aplay does not export an underrun counter
        }
    }

    fn playback_delay_frames(&self) -> Option<u64> {
        match &*self.playback.lock().unwrap_or_else(|e| e.into_inner()) {
            Playback::Direct(pcm) => pcm.delay_frames().ok(),
            Playback::Shared(_) => None,
        }
    }

    fn set_home_indicator(&self, owner: IndicatorOwner) {
        if self.passive_action.is_some() {
            self.indicator
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .set_active(owner);
        }
    }

    fn led_write(&self, rgb: &[(u8, u8, u8); leds::RING_LEDS]) -> io::Result<()> {
        self.indicator
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write(rgb)
    }

    fn button_next(&self) -> io::Result<ButtonEvent> {
        match self.passive_action {
            Some(code) => self.buttons.next_action_hold(code),
            None => self.buttons.next(),
        }
    }

    fn set_adc_mute(&self, on: bool) -> io::Result<()> {
        self.muted.store(on, Ordering::Relaxed);
        fwlog!("[lx06] hardware-boundary/uplink mic mute set to {on}");
        Ok(())
    }

    fn set_system_time(&self, ts_ms: u64) -> bool {
        let tv = libc::timeval {
            tv_sec: (ts_ms / 1000) as libc::time_t,
            tv_usec: ((ts_ms % 1000) * 1000) as libc::suseconds_t,
        };
        unsafe { libc::settimeofday(&tv, std::ptr::null()) == 0 }
    }

    fn wake_lock(&self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_indicator_is_untouched_until_home_turn_and_released_afterward() {
        let path = std::env::temp_dir().join(format!("lx06-led-release-{}", std::process::id()));
        std::fs::write(&path, "native").unwrap();
        let mut indicator = Indicator {
            path: path.to_str().unwrap().to_string(),
            active: false,
            ring: None,
        };
        let frame = [(10, 10, 10); leds::RING_LEDS];
        assert!(indicator.write(&frame).is_ok());
        assert!(indicator.ring.is_none());
        indicator.set_active(IndicatorOwner::Home);
        indicator.write(&frame).unwrap();
        assert!(!std::fs::read_to_string(&path)
            .unwrap()
            .starts_with("17 0x000000"));
        assert!(indicator.ring.as_ref().unwrap().led_fade_path.is_none());
        indicator.set_active(IndicatorOwner::Idle);
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .starts_with("17 0x000000"));
        std::fs::write(&path, "native").unwrap();
        indicator.set_active(IndicatorOwner::Idle);
        assert!(indicator.write(&frame).is_ok());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "native");
        indicator.set_active(IndicatorOwner::Home);
        indicator.write(&frame).unwrap();
        std::fs::write(&path, "native mute").unwrap();
        indicator.set_active(IndicatorOwner::Native);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "native mute");
        std::fs::remove_file(path).unwrap();
        assert!(indicator.ring.is_none());
    }

    #[test]
    fn decimate_lx06_scales_mics_and_reference_without_clipping() {
        // 3 frames at 48 kHz (8 channels × i32) -> 1 frame at 16 kHz (9 channels × S24_3LE).
        // Measured peak at 80% volume: mic 6.8e7, ref 8.6e8.
        let frame_48k: [i32; 8] = [
            68_000_000,
            -68_000_000,
            1 << 22,
            1 << 22,
            1 << 22,
            1 << 22,
            860_000_000,
            860_000_000,
        ];
        let mut raw = Vec::with_capacity(3 * 8 * 4);
        for _ in 0..3 {
            for s in frame_48k {
                raw.extend_from_slice(&s.to_le_bytes());
            }
        }
        let mut out_s24 = vec![0u8; 27];
        let n = decimate_lx06_s32_to_s24_9ch(&raw, &mut out_s24);
        assert_eq!(n, 1);

        let mut decoded = Vec::new();
        super::super::capture::decode_s24_block(&out_s24, &mut decoded);
        assert_eq!(decoded.len(), 1);
        let row = decoded[0];
        let ch0 = row[0];
        let ch1 = row[1];
        let ch2 = row[2];
        let ch8 = row[8];

        assert_eq!(ch0, (68_000_000i32 >> 12) as i16);
        assert_eq!(ch1, (-68_000_000i32 >> 12) as i16);
        assert_eq!(ch2, 1 << 10); // 2^22 >> 12 = 2^10
        assert_eq!(ch8, (860_000_000i32 >> 16) as i16);
        assert!(ch0 < i16::MAX && ch1 > i16::MIN && ch8 < i16::MAX);
    }

    #[test]
    fn aw20054_packs_bbggrr_and_maps_12_to_18() {
        assert_eq!(Aw20054Ring::pack_bbggrr(0xFF, 0xB0, 0x60), 0x60B0FF);
        assert_eq!(Aw20054Ring::boost_rgb_for_diffuser(0, 0, 0), (0, 0, 0));
        assert_eq!(
            Aw20054Ring::boost_rgb_for_diffuser(0x78, 0x78, 0x78),
            (0xFF, 0xFF, 0xFF)
        );
        let mut ring12 = [(0u8, 0u8, 0u8); 12];
        ring12[0] = (0, 72, 0);
        let mapped = Aw20054Ring::map_12_to_18(&ring12);
        // 72 * 2.2 = 158 (0x9E)
        assert_eq!(mapped[0], 0x009E00);
        assert_eq!(mapped[1], 0x009E00);
        assert_eq!(mapped[2], 0x000000);
    }

    #[test]
    fn lx06_keymap_tracks_four_physical_keys() {
        let prof = super::super::profile::DeviceProfile::lx06();
        let mut tracker = buttons::PressTracker::with_keymap(true, prof.key_map);
        assert_eq!(tracker.feed(139, 1), Some(ButtonEvent::VolumeUp));
        assert_eq!(tracker.feed(115, 1), Some(ButtonEvent::VolumeDown));
        assert_eq!(tracker.feed(114, 1), Some(ButtonEvent::Action));
        // Short press on KEY_HOME (102, physical Mic Mute on LX06) toggles mute on release.
        assert_eq!(tracker.feed(102, 1), None);
        assert_eq!(tracker.feed(102, 0), Some(ButtonEvent::MuteToggle));
    }

    #[test]
    fn capture_queue_preserves_frame_alignment_and_propagates_errors() {
        let q = CaptureQueue::new();
        let mut f1 = vec![0x11u8; OUT_FRAME_BYTES];
        f1[0] = 0xAA;
        let mut f2 = vec![0x22u8; OUT_FRAME_BYTES];
        f2[0] = 0xBB;
        q.push_s24(&f1);
        q.push_s24(&f2);

        let mut out = vec![0u8; 2 * OUT_FRAME_BYTES];
        assert_eq!(q.pop_frames(&mut out, 2).unwrap(), 2);
        assert_eq!(out[0], 0xAA);
        assert_eq!(out[OUT_FRAME_BYTES], 0xBB);

        q.push_err(io::Error::from_raw_os_error(libc::EPIPE));
        let err = q.pop_frames(&mut out, 1).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EPIPE));
    }
}
