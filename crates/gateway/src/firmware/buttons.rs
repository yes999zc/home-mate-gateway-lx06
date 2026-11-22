//! Button handling, two modes by platform (G6, 2026-11-08).
//!
//! FORWARD mode (FireOS, P1 fix): evdev is single-reader — the firmware
//! reading event1/event2 starves Android's InputReader. Full forwarder:
//! EVIOCGRAB the real devices, mirror every event (except the volume keys,
//! which we consume for our own gain law) into a uinput clone so Android
//! sees them again. Selected when /dev/uinput EXISTS (it does on FireOS —
//! P1 proved it).
//!
//! DIRECT mode (emOS): no Android input stack exists, so there is no second
//! reader to protect and no clone to feed — grab and read directly. Selected
//! when /dev/uinput is ABSENT (emOS init's node table has no uinput). The
//! Mute key gains real semantics here (onepager §3-3): short press toggles
//! the hardware mic mute; a long press is reported north as a hold event
//! with no local destructive action. Mode is a property of the platform,
//! not a flag — rolling the device back to FireOS restores the validated
//! press-everywhere behavior with the same binary.
//!
//! ABI note: `struct input_event` is 16 B on 32-bit tasks (timeval 8 B) and
//! 24 B on 64-bit — per-arch constants, or records misparse.

use super::ButtonEvent;
use crate::fwlog;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::io::AsRawFd;
use std::sync::Mutex;
use std::time::Instant;

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
/// The kernel's "I dropped events" marker (B-M4). After it, the batch's key
/// records cannot be trusted: a release may be missing, so a held key would
/// look pressed forever and a click could be invented. The reader resyncs
/// (clears the press state) instead of acting on partial input.
const SYN_DROPPED: u16 = 3;
/// EVIOCSCLOCKID = _IOW('E', 0xa0, int) — asm-generic encoding, same family
/// as EVIOCGRAB above. Setting CLOCK_MONOTONIC makes every timestamp we
/// forward comparable and immune to wall-clock steps (B-M4).
const EVIOCSCLOCKID: libc::c_ulong = 0x4004_45a0;
const CLOCK_MONOTONIC: libc::c_int = 1;
pub const KEY_MUTE: u16 = 113; // physical mic-mute on this board (dot2)
pub const KEY_VOLDOWN: u16 = 114;
pub const KEY_VOLUP: u16 = 115;
/// The top-center action button (pressed = stop/interrupt). It is the code
/// the G5-2 forwarder proved on the wire (Android received KEY_HELP when the
/// button was pressed); the direct form must map it too, or the physical
/// stop key does nothing once EchoMuse/uinput are gone (T4 found this: the
/// degradation path "action button still stops with a dead spotter" was
/// unreachable on emOS).
const KEY_HELP: u16 = 138;

/// A press held at least this long is a hold, not a click (direct mode).
const HOLD_MS: u128 = 1500;

/// Bytes per evdev record per task width.
#[cfg(target_arch = "arm")]
pub const INPUT_EVENT_SIZE: usize = 16;
#[cfg(not(target_arch = "arm"))]
pub const INPUT_EVENT_SIZE: usize = 24;

// uinput ioctls (asm-generic encoding — the only one this kernel matches).
const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564; // _IOW('U', 100, int)
const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565; // _IOW('U', 101, int)
const UI_DEV_CREATE: libc::c_ulong = 0x5501; // _IO('U', 1)
const EVIOC_GRAB: libc::c_ulong = 0x4004_4590; // _IOW('E', 0x90, int)

/// One parsed evdev record. The timestamp head is kept verbatim (B-M4): the
/// kernel's own clock is the only one a consumer can compare against, so it
/// is forwarded instead of being thrown away. 16 bytes covers the 64-bit
/// timeval; the 32-bit one uses the first 8.
#[derive(Debug, Clone, Copy, Default)]
struct EvRecord {
    kind: u16,
    code: u16,
    value: i32,
    stamp: [u8; 16],
}

fn parse_record(buf: &[u8]) -> EvRecord {
    // Trailing 8 bytes are type/code/value on BOTH ABIs (timeval differs
    // in size, not position of the tail).
    let n = buf.len();
    let mut stamp = [0u8; 16];
    let head = n.saturating_sub(8).min(stamp.len());
    stamp[..head].copy_from_slice(&buf[..head]);
    EvRecord {
        kind: u16::from_ne_bytes([buf[n - 8], buf[n - 7]]),
        code: u16::from_ne_bytes([buf[n - 6], buf[n - 5]]),
        value: i32::from_ne_bytes([buf[n - 4], buf[n - 3], buf[n - 2], buf[n - 1]]),
        stamp,
    }
}

/// Forward one record verbatim: the source timestamp (same arch, same
/// timeval layout) plus type/code/value.
fn emit_record(buf: &mut [u8], rec: &EvRecord) {
    let n = buf.len();
    let head = n.saturating_sub(8);
    buf[..head].copy_from_slice(&rec.stamp[..head]);
    buf[n - 8..n - 6].copy_from_slice(&rec.kind.to_ne_bytes());
    buf[n - 6..n - 4].copy_from_slice(&rec.code.to_ne_bytes());
    buf[n - 4..n].copy_from_slice(&rec.value.to_ne_bytes());
}

/// Is this the kernel's dropped-events marker? (B-M4)
fn is_sync_dropped(rec: &EvRecord) -> bool {
    rec.kind == EV_SYN && rec.code == SYN_DROPPED
}

#[cfg(target_os = "macos")]
fn raw_ioctl(fd: libc::c_int, req: libc::c_ulong, arg: libc::c_int) -> libc::c_int {
    unsafe { libc::ioctl(fd, req, arg) }
}
#[cfg(not(target_os = "macos"))]
fn raw_ioctl(fd: libc::c_int, req: libc::c_ulong, arg: libc::c_int) -> libc::c_int {
    #[cfg(target_env = "musl")]
    let request = req as libc::c_int;
    #[cfg(not(target_env = "musl"))]
    let request = req;
    unsafe { libc::ioctl(fd, request, arg) }
}

#[cfg(target_os = "macos")]
fn raw_ioctl_ptr(fd: libc::c_int, req: libc::c_ulong, arg: *const libc::c_int) -> libc::c_int {
    unsafe { libc::ioctl(fd, req, arg) }
}
#[cfg(not(target_os = "macos"))]
fn raw_ioctl_ptr(fd: libc::c_int, req: libc::c_ulong, arg: *const libc::c_int) -> libc::c_int {
    #[cfg(target_env = "musl")]
    let request = req as libc::c_int;
    #[cfg(not(target_env = "musl"))]
    let request = req;
    unsafe { libc::ioctl(fd, request, arg) }
}

/// Press-state machine, pure so the click/hold split is unit-testable
/// without device nodes. In direct mode the Mute key resolves on RELEASE
/// (click vs hold); everything else reports on the press edge like before.
#[derive(Default)]
pub struct PressTracker {
    pressed: HashMap<u16, Instant>,
    resolve_mute_on_release: bool,
    track_action_hold: bool,
    key_map: super::profile::KeyMap,
}

impl PressTracker {
    pub fn new(resolve_mute_on_release: bool) -> PressTracker {
        Self {
            resolve_mute_on_release,
            ..Default::default()
        }
    }

    pub fn with_keymap(
        resolve_mute_on_release: bool,
        key_map: super::profile::KeyMap,
    ) -> PressTracker {
        PressTracker {
            pressed: Default::default(),
            resolve_mute_on_release,
            track_action_hold: true,
            key_map,
        }
    }

    /// Forget every in-flight press (B-M4 resync after SYN_DROPPED).
    pub fn reset(&mut self) {
        self.pressed.clear();
    }

    /// Feed one EV_KEY record; return the semantic event it completes.
    pub fn feed(&mut self, code: u16, value: i32) -> Option<ButtonEvent> {
        let mute_key = self.key_map.mute;
        let action_key = self.key_map.action;
        let vol_up_key = self.key_map.vol_up;
        let vol_down_key = self.key_map.vol_down;
        match value {
            1 => {
                // KEY_DOWN. In forward mode the Mute key reports immediately
                // (validated G5-2 behavior: press = stop/interrupt); in
                // direct mode its click/hold split resolves on release.
                if code == mute_key && self.resolve_mute_on_release {
                    self.pressed.insert(code, Instant::now());
                    return None;
                }
                if code == action_key && self.track_action_hold {
                    self.pressed.insert(code, Instant::now());
                }
                self.key_event(code)
            }
            0 => {
                // KEY_UP: completes a tracked Mute or Action hold press.
                if code == mute_key && self.resolve_mute_on_release {
                    if let Some(start) = self.pressed.remove(&code) {
                        let held = start.elapsed().as_millis();
                        return Some(if held >= HOLD_MS {
                            ButtonEvent::Hold(code, held as u64)
                        } else {
                            ButtonEvent::MuteToggle
                        });
                    }
                    return None;
                }
                if code == action_key && self.track_action_hold {
                    if let Some(start) = self.pressed.remove(&code) {
                        let held = start.elapsed().as_millis();
                        if held >= HOLD_MS {
                            return Some(ButtonEvent::Hold(code, held as u64));
                        }
                    }
                    return None;
                }
                None
            }
            // 2 = autorepeat: the key is still held. Volume honours it (the
            // firmware steps finely and throttles); every other key keeps the
            // old "ignore repeats" rule — a held Action key must not fire a
            // stream of stops.
            _ => {
                if value == 2 && code == vol_up_key {
                    Some(ButtonEvent::VolumeUpRepeat)
                } else if value == 2 && code == vol_down_key {
                    Some(ButtonEvent::VolumeDownRepeat)
                } else {
                    None
                }
            }
        }
    }

    fn key_event(&self, code: u16) -> Option<ButtonEvent> {
        if code == self.key_map.vol_up {
            Some(ButtonEvent::VolumeUp)
        } else if code == self.key_map.vol_down {
            Some(ButtonEvent::VolumeDown)
        } else if code == self.key_map.mute || code == self.key_map.action || code == KEY_HELP {
            Some(ButtonEvent::Action)
        } else {
            Some(ButtonEvent::Unknown(code))
        }
    }
}

/// The grabbed real devices + (on FireOS) the uinput clone they feed.
pub struct ButtonHub {
    real: Vec<File>,
    uinput: Option<File>,
    tracker: PressTracker,
}

impl ButtonHub {
    /// Open the real nodes and pick the mode from the platform: /dev/uinput
    /// exists → forward mode (clone + forward, current behavior); absent →
    /// direct mode (grab + read, nothing to feed). A uinput that exists but
    /// fails to open stays degraded (reals NOT grabbed and NOT read) — never
    /// the half-state that starves the system.
    pub fn open(paths: &[String]) -> io::Result<ButtonHub> {
        let mut real = Vec::new();
        for p in paths {
            match OpenOptions::new().read(true).open(p) {
                Ok(f) => real.push(f),
                Err(e) => fwlog!("[firmware] button node {p}: {e}"),
            }
        }
        if real.is_empty() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no button devices"));
        }
        // B-M4: a known event clock. Monotonic survives the 2010 device clock
        // and any later time projection; failure is logged, never fatal (the
        // press logic itself uses Instant, which is monotonic regardless).
        for f in &real {
            let r = raw_ioctl(f.as_raw_fd(), EVIOCSCLOCKID, CLOCK_MONOTONIC);
            if r != 0 {
                fwlog!(
                    "[firmware] EVIOCSCLOCKID failed: {}",
                    io::Error::last_os_error()
                );
            }
        }
        let node_exists = std::path::Path::new("/dev/uinput").exists();
        let uinput = if node_exists {
            match setup_uinput() {
                Ok(u) => Some(u),
                Err(e) => {
                    fwlog!(
                        "[firmware] uinput present but unusable ({e}) — leaving buttons to Android"
                    );
                    None
                }
            }
        } else {
            fwlog!("[firmware] no /dev/uinput — direct button mode (emOS)");
            None
        };
        if uinput.is_some() {
            for f in &real {
                let r = raw_ioctl(f.as_raw_fd(), EVIOC_GRAB, 1);
                if r != 0 {
                    fwlog!(
                        "[firmware] EVIOCGRAB failed: {}",
                        io::Error::last_os_error()
                    );
                }
            }
            Ok(ButtonHub {
                real,
                uinput,
                tracker: PressTracker::new(false),
            })
        } else if node_exists {
            // Degraded: do not read — the fds stay open solely as markers.
            real.clear();
            Ok(ButtonHub {
                real: Vec::new(),
                uinput: None,
                tracker: PressTracker::new(false),
            })
        } else {
            // Direct mode: no other consumer exists; grab for discipline.
            for f in &real {
                let r = raw_ioctl(f.as_raw_fd(), EVIOC_GRAB, 1);
                if r != 0 {
                    fwlog!(
                        "[firmware] EVIOCGRAB failed: {}",
                        io::Error::last_os_error()
                    );
                }
            }
            Ok(ButtonHub {
                real,
                uinput: None,
                tracker: PressTracker::new(true),
            })
        }
    }

    /// Direct-mode opener with an explicit board `KeyMap` (e.g. LX06
    /// `adc_keypad`, where no Android `InputReader` exists).
    pub fn open_with_keymap(
        paths: &[String],
        key_map: super::profile::KeyMap,
        grab: bool,
    ) -> io::Result<ButtonHub> {
        let mut real = Vec::new();
        for p in paths {
            match OpenOptions::new().read(true).open(p) {
                Ok(f) => real.push(f),
                Err(e) => fwlog!("[firmware] button node {p}: {e}"),
            }
        }
        if real.is_empty() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no button devices"));
        }
        for f in &real {
            let clk = CLOCK_MONOTONIC;
            let r = raw_ioctl_ptr(f.as_raw_fd(), EVIOCSCLOCKID, &clk);
            if r != 0 {
                fwlog!(
                    "[firmware] EVIOCSCLOCKID failed: {}",
                    io::Error::last_os_error()
                );
            }
            if !grab {
                continue;
            }
            let r = raw_ioctl(f.as_raw_fd(), EVIOC_GRAB, 1);
            if r != 0 {
                fwlog!(
                    "[firmware] EVIOCGRAB failed: {}",
                    io::Error::last_os_error()
                );
            }
        }
        Ok(ButtonHub {
            real,
            uinput: None,
            tracker: PressTracker::with_keymap(true, key_map),
        })
    }

    /// Blocking wait for the next semantic press; in forward mode every read
    /// record that is not a consumed volume key is mirrored to the clone.
    pub fn next_event(&mut self) -> io::Result<ButtonEvent> {
        self.next_matching(|_| true)
    }

    fn next_matching(&mut self, accepts: impl Fn(&ButtonEvent) -> bool) -> io::Result<ButtonEvent> {
        loop {
            let (batch, dropped) = self.read_batch()?;
            if dropped {
                // B-M4: resync. A key released while we were behind would
                // otherwise stay in the pressed map and, after HOLD_MS, turn
                // into a hold that never happened.
                self.tracker.reset();
                fwlog!("[firmware] button events dropped by the kernel; press state reset");
            }
            for ev in batch {
                if ev.kind != EV_KEY {
                    continue; // SYN and friends: forwarded inside read_batch
                }
                if let Some(ev) = self.tracker.feed(ev.code, ev.value) {
                    if accepts(&ev) {
                        return Ok(ev);
                    }
                }
            }
        }
    }

    /// Read every currently queued record; forward all non-volume events.
    /// Returns (records, dropped): `dropped` means the kernel's queue
    /// overflowed during this batch, so the caller must resync its press
    /// state and ignore these key records (B-M4).
    fn read_batch(&self) -> io::Result<(Vec<EvRecord>, bool)> {
        let mut fds: Vec<libc::pollfd> = self
            .real
            .iter()
            .map(|f| libc::pollfd {
                fd: f.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let r = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut out = Vec::new();
        let mut dropped = false;
        let mut fwd = [0u8; INPUT_EVENT_SIZE];
        for (i, fd) in fds.iter().enumerate() {
            if fd.revents & libc::POLLIN == 0 {
                continue;
            }
            // Drain the node; each record is forwarded unless consumed.
            let mut buf = [0u8; INPUT_EVENT_SIZE * 8];
            loop {
                let n = (&self.real[i]).read(&mut buf)?;
                if n == 0 {
                    break;
                }
                let mut off = 0usize;
                while off + INPUT_EVENT_SIZE <= n {
                    let rec = parse_record(&buf[off..off + INPUT_EVENT_SIZE]);
                    off += INPUT_EVENT_SIZE;
                    if is_sync_dropped(&rec) {
                        // The queue overflowed: whatever key edges were in
                        // flight are gone. Do not forward and do not act —
                        // the caller clears its pressed map so a missing
                        // release cannot become a phantom hold.
                        dropped = true;
                        out.clear();
                        continue;
                    }
                    if dropped {
                        // Still draining this batch: keep the queue moving so
                        // the next poll starts clean, but trust nothing.
                        continue;
                    }
                    let consumed =
                        rec.kind == EV_KEY && (rec.code == KEY_VOLUP || rec.code == KEY_VOLDOWN);
                    if !consumed {
                        if let Some(u) = &self.uinput {
                            emit_record(&mut fwd, &rec);
                            let r = unsafe {
                                libc::write(
                                    u.as_raw_fd(),
                                    fwd.as_ptr() as *const libc::c_void,
                                    fwd.len(),
                                )
                            };
                            if r < 0 {
                                fwlog!(
                                    "[firmware] uinput forward failed: {}",
                                    io::Error::last_os_error()
                                );
                            }
                        }
                    }
                    if rec.kind == EV_KEY {
                        out.push(rec);
                    }
                }
                if n < buf.len() {
                    break;
                }
            }
        }
        Ok((out, dropped))
    }
}

fn setup_uinput() -> io::Result<File> {
    let u = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/uinput")?;
    if raw_ioctl(u.as_raw_fd(), UI_SET_EVBIT, EV_KEY as libc::c_int) != 0 {
        return Err(io::Error::last_os_error());
    }
    if raw_ioctl(u.as_raw_fd(), UI_SET_EVBIT, EV_SYN as libc::c_int) != 0 {
        return Err(io::Error::last_os_error());
    }
    // Register every key this board can emit (forwarding must not be
    // limited to the ones we recognize).
    for code in [KEY_MUTE, KEY_VOLDOWN, KEY_VOLUP, KEY_HELP] {
        if raw_ioctl(u.as_raw_fd(), UI_SET_KEYBIT, code as libc::c_int) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    // struct uinput_user_dev: name[80] + input_id(4×u16) + ff_effects_max
    // + absmax/min/fuzz/flat (4 × 64 × i32) = 1116 B, zeroed except identity.
    let mut dev = vec![0u8; 1116];
    let name = b"hm-direct-pass-through";
    dev[..name.len()].copy_from_slice(name);
    // input_id: bustag/bus/product/version — leave zeros (virtual).
    let mut u = u;
    {
        use std::io::Write;
        u.write_all(&dev)?;
    }
    let r = raw_ioctl(u.as_raw_fd(), UI_DEV_CREATE, 0);
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(u)
}

/// Shared handle for the Hardware impl (interior mutability not needed —
/// the hub itself only reads; forwarding happens internally).
pub struct SharedButtonHub(Mutex<ButtonHub>);

impl SharedButtonHub {
    pub fn open(paths: &[String]) -> io::Result<SharedButtonHub> {
        Ok(SharedButtonHub(Mutex::new(ButtonHub::open(paths)?)))
    }
    pub fn open_with_keymap(
        paths: &[String],
        key_map: super::profile::KeyMap,
        grab: bool,
    ) -> io::Result<SharedButtonHub> {
        Ok(SharedButtonHub(Mutex::new(ButtonHub::open_with_keymap(
            paths, key_map, grab,
        )?)))
    }
    pub fn next_action_hold(&self, action: u16) -> io::Result<ButtonEvent> {
        self.0
            .lock()
            .unwrap()
            .next_matching(|event| matches!(event, ButtonEvent::Hold(code, _) if *code == action))
    }
    pub fn next(&self) -> io::Result<ButtonEvent> {
        self.0.lock().unwrap().next_event()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_mode_mute_reports_on_press() {
        let mut t = PressTracker::new(false);
        assert_eq!(t.feed(KEY_MUTE, 1), Some(ButtonEvent::Action));
        // Action key: KEY_HELP, in forward AND direct mode (direct mode only
        // intercepts KEY_MUTE's click/hold split).
        assert_eq!(t.feed(KEY_HELP, 1), Some(ButtonEvent::Action));
        assert_eq!(
            t.feed(KEY_MUTE, 0),
            None,
            "release is inert in forward mode"
        );
    }

    #[test]
    fn a_dropped_batch_is_recognized_and_resynced() {
        // B-M4: the marker itself is identified from a real record shape.
        let mut raw = [0u8; INPUT_EVENT_SIZE];
        raw[INPUT_EVENT_SIZE - 8] = EV_SYN as u8;
        raw[INPUT_EVENT_SIZE - 6] = SYN_DROPPED as u8;
        let rec = parse_record(&raw);
        assert!(is_sync_dropped(&rec));
        let mut key = [0u8; INPUT_EVENT_SIZE];
        key[INPUT_EVENT_SIZE - 8] = EV_KEY as u8;
        key[INPUT_EVENT_SIZE - 6] = KEY_MUTE as u8;
        key[INPUT_EVENT_SIZE - 1] = 1;
        assert!(!is_sync_dropped(&parse_record(&key)));

        // A press in flight, then a drop: the release may be gone, so the
        // press must NOT survive as a phantom hold after the resync.
        let mut t = PressTracker::new(true);
        assert_eq!(t.feed(KEY_MUTE, 1), None, "press arms");
        t.reset();
        assert_eq!(t.feed(KEY_MUTE, 0), None, "a release after resync is inert");
    }

    #[test]
    fn a_forwarded_record_keeps_the_kernel_timestamp() {
        // B-M4: the timestamp head is forwarded verbatim (the clock id is
        // set to CLOCK_MONOTONIC on the real nodes), not zeroed.
        let mut raw = [0u8; INPUT_EVENT_SIZE];
        for (i, b) in raw.iter_mut().enumerate().take(INPUT_EVENT_SIZE - 8) {
            *b = (i as u8) + 1;
        }
        raw[INPUT_EVENT_SIZE - 8] = EV_KEY as u8;
        raw[INPUT_EVENT_SIZE - 6] = KEY_HELP as u8;
        raw[INPUT_EVENT_SIZE - 4] = 1; // value=1, little-endian i32
        let rec = parse_record(&raw);
        let mut out = [0u8; INPUT_EVENT_SIZE];
        emit_record(&mut out, &rec);
        assert_eq!(&out[..INPUT_EVENT_SIZE - 8], &raw[..INPUT_EVENT_SIZE - 8]);
        assert_eq!(parse_record(&out).code, KEY_HELP);
        assert_eq!(parse_record(&out).value, 1);
    }

    #[test]
    fn direct_mode_short_press_toggles_mute() {
        let mut t = PressTracker::new(true);
        assert_eq!(t.feed(KEY_MUTE, 1), None, "press arms, resolves on release");
        assert_eq!(t.feed(KEY_MUTE, 0), Some(ButtonEvent::MuteToggle));
    }

    #[test]
    fn direct_mode_long_press_is_hold() {
        let mut t = PressTracker::new(true);
        assert_eq!(t.feed(KEY_MUTE, 1), None);
        std::thread::sleep(Duration::from_millis(HOLD_MS as u64 + 20));
        match t.feed(KEY_MUTE, 0) {
            Some(ButtonEvent::Hold(code, ms)) => {
                assert_eq!(code, KEY_MUTE);
                assert!(ms >= HOLD_MS as u64);
            }
            other => panic!("expected hold, got {other:?}"),
        }
    }

    #[test]
    fn volume_keys_never_hold_resolve() {
        let mut t = PressTracker::new(true);
        assert_eq!(t.feed(KEY_VOLUP, 1), Some(ButtonEvent::VolumeUp));
        assert_eq!(t.feed(KEY_VOLDOWN, 1), Some(ButtonEvent::VolumeDown));
        assert_eq!(t.feed(KEY_VOLUP, 0), None);
    }

    use std::time::Duration;
}

#[cfg(test)]
mod repeat_tests {
    use super::*;

    /// 2026-11-15 (user report "调整音量不顺畅"): the kernel's autorepeat was
    /// dropped, so the only way to move the volume was to tap the key 16
    /// times. Volume keys now report repeats; every other key keeps ignoring
    /// them, because a held Action key must not fire a stream of stops.
    #[test]
    fn volume_keys_report_autorepeat_and_other_keys_do_not() {
        let mut t = PressTracker::new(false);
        assert_eq!(t.feed(KEY_VOLUP, 1), Some(ButtonEvent::VolumeUp));
        assert_eq!(t.feed(KEY_VOLUP, 2), Some(ButtonEvent::VolumeUpRepeat));
        assert_eq!(t.feed(KEY_VOLUP, 2), Some(ButtonEvent::VolumeUpRepeat));
        assert_eq!(t.feed(KEY_VOLUP, 0), None);
        assert_eq!(t.feed(KEY_VOLDOWN, 1), Some(ButtonEvent::VolumeDown));
        assert_eq!(t.feed(KEY_VOLDOWN, 2), Some(ButtonEvent::VolumeDownRepeat));
        // Hold/repeat of every other key stays silent after the first press.
        assert_eq!(t.feed(KEY_HELP, 1), Some(ButtonEvent::Action));
        assert_eq!(t.feed(KEY_HELP, 2), None);
        assert_eq!(t.feed(KEY_HELP, 2), None);
    }
}
