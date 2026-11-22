//! Coordinate the two listeners on a shared LX06. Only the monitor worker runs
//! native commands. The gateway admits a Home wake after observing pns stopped.
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const LOG: &str = "/tmp/mico_aivs_lab/instruction.log";
const NATIVE_MUTE: &str = "/tmp/mipns/mute";
const MODE: &str = "/data/hm/mode";
const POLL: Duration = Duration::from_millis(200);
const FRESH: Duration = Duration::from_secs(2);
const RESERVATION: Duration = Duration::from_secs(5);
// Core closes the session before submitting its cached farewell on the same link.
const FINAL_CUE_WINDOW: Duration = Duration::from_millis(500);
const STOP_WAIT: Duration = Duration::from_secs(2);
const MAX_LOG: usize = 512 * 1024;

#[derive(Default)]
struct State {
    core_online: bool,
    managed_mute: bool,
    core_mute: bool,
    native_mute: Option<bool>,
    core_busy: bool,
    core_active: bool,
    candidate: Option<Instant>,
    playback: bool,
    reservation: Option<Instant>,
    closing_until: Option<Instant>,
    // Updated together, after both observers and pns have been checked.
    observed: Option<Instant>,
    native_busy: bool,
    pns_running: Option<bool>,
    pns_checked: Option<Instant>,
    // A stop followed by an idle transition cannot reuse pre-restart evidence.
    home_confirmed: bool,
}

impl State {
    fn muted(&self) -> bool {
        self.managed_mute || self.core_mute || self.native_mute != Some(false)
    }
    fn wants_home(&self, now: Instant) -> bool {
        self.core_active
            || self.playback
            || self.reservation.is_some_and(|end| now < end)
            || self.closing_until.is_some_and(|end| now < end)
    }
    fn native_clear(&self, now: Instant) -> bool {
        self.observed.is_some_and(|t| now.duration_since(t) < FRESH) && !self.native_busy
    }
    fn allowed(&self, now: Instant) -> bool {
        self.core_online && !self.muted() && self.native_clear(now)
    }
    fn owns(&self, now: Instant) -> bool {
        self.allowed(now)
            && self.wants_home(now)
            && self.home_confirmed
            && self.pns_running == Some(false)
    }
    fn idle(&self, now: Instant) -> bool {
        !self.core_busy && self.native_available(now)
    }
    fn may_capture(&self, now: Instant) -> bool {
        self.idle(now)
            || self.owns(now)
            || (self.allowed(now)
                && self.reservation.is_some_and(|until| now < until)
                && self.pns_running.is_some())
    }
    fn native_available(&self, now: Instant) -> bool {
        self.allowed(now) && !self.wants_home(now) && self.pns_running == Some(true)
    }
    fn candidate(&mut self, now: Instant) -> bool {
        if self.owns(now) {
            if self.closing_until.is_some() {
                self.candidate = Some(now + RESERVATION);
            }
            return true;
        }
        if !self.idle(now) {
            return false;
        }
        self.candidate = Some(now + RESERVATION);
        true
    }
    fn refresh(
        &mut self,
        now: Instant,
        busy: Option<bool>,
        pns: Option<bool>,
        muted: Option<bool>,
    ) {
        self.observed = busy.map(|_| now);
        self.native_busy = busy.unwrap_or(true);
        self.native_mute = muted;
        self.pns_running = pns;
        if pns == Some(true) && !self.playback && self.closing_until.is_some_and(|end| now >= end) {
            self.closing_until = None;
        }
        self.pns_checked = pns.map(|_| now);
        if !self.home_confirmed && self.reservation.is_some_and(|end| now >= end) {
            self.reservation = None;
            self.core_active = false;
        }
        if self.native_busy || self.muted() || pns.is_none() {
            self.closing_until = None;
            self.candidate = None;
            self.reservation = None;
            if !self.home_confirmed {
                self.core_active = false;
            }
        }
        if !self.wants_home(now) || self.muted() {
            self.home_confirmed = false;
        } else if self.allowed(now) && pns == Some(false) {
            self.home_confirmed = true;
        }
    }
    fn wants_pns(&self, now: Instant) -> bool {
        if self.managed_mute || self.wants_home(now) {
            return false;
        }
        // A self-muted native listener is already gated. Leaving it running
        // lets its own touchpad lift the mute. Never start it through that mute.
        self.native_mute == Some(false)
            || (self.native_mute == Some(true) && self.pns_running == Some(true))
    }
    fn core_state(&mut self, value: &str, now: Instant) {
        let finishing_home = value == "idle" && self.core_active && self.owns(now);
        let was_core_active = self.core_active;
        let held =
            self.closing_until.is_none() && (self.core_active || self.playback || self.owns(now));
        let active = matches!(
            value,
            "wake" | "listening" | "hearing" | "thinking" | "speaking" | "music" | "playing"
        );
        self.core_online = active || matches!(value, "idle" | "paused" | "muted" | "error");
        if self.core_online {
            self.core_mute = value == "muted";
        }
        let accepted = matches!(value, "wake" | "listening" | "hearing")
            && self.candidate.is_some_and(|end| now < end)
            && (self.native_available(now) || (self.closing_until.is_some() && self.owns(now)));
        // A spotted string is only a candidate. Its Core may ignore it, lose
        // multi-room arbitration or classify a local transport/retreat word.
        // Only fresh matching Core admission or an actual Play reserves pns.
        self.core_busy = (active || matches!(value, "paused" | "error"))
            && (self.closing_until.is_none() || accepted);
        self.core_active =
            (active && (held || accepted)) || (held && matches!(value, "paused" | "error"));
        if accepted {
            self.closing_until = None;
            self.candidate = None;
            self.reservation = Some(now + RESERVATION);
        } else if finishing_home {
            // A new idle frame alone never extends this deadline. Once the
            // writer accepts the cue, its normal EOS/drain holds ownership.
            self.closing_until = Some(now + FINAL_CUE_WINDOW);
            self.reservation = None;
        } else if (self.core_active && self.home_confirmed)
            || (was_core_active && value == "idle")
            || !self.core_online
            || self.core_mute
        {
            // wake -> hearing/listening may precede the service-stop receipt.
            // Keep the reservation so KWS and capture receive the first words.
            self.reservation = None;
        }
        if !self.core_online || self.core_mute {
            self.closing_until = None;
        }
        if !self.core_online || self.core_mute || (self.core_busy && !self.core_active) {
            self.candidate = None;
        }
        if !self.wants_home(now) || self.core_mute {
            self.home_confirmed = false;
        }
    }
}

pub struct VendorMonitor {
    native: Native,
    state: Mutex<State>,
    changed: Condvar,
}

impl VendorMonitor {
    pub fn start(initial_mute: bool) -> Arc<Self> {
        Self::start_with(initial_mute, Native::device())
    }
    fn new(initial_mute: bool, native: Native) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                managed_mute: initial_mute,
                native_mute: native.mute_marker(),
                ..State::default()
            }),
            native,
            changed: Condvar::new(),
        })
    }
    fn start_with(initial_mute: bool, native: Native) -> Arc<Self> {
        let monitor = Self::new(initial_mute, native);
        let weak = Arc::downgrade(&monitor);
        std::thread::spawn(move || {
            let mut journal = Journal::default();
            let mut last_service = None;
            while let Some(monitor) = weak.upgrade() {
                if !monitor.native.dual_mode() {
                    monitor.invalidate();
                    break;
                }
                let busy = monitor.native.busy(&mut journal);
                let mut pns = monitor.native.pid("mipns-xiaomi").map(|pid| pid.is_some());
                let now = Instant::now();
                let mut state = monitor.state.lock().unwrap();
                state.refresh(now, busy, pns, monitor.native.mute_marker());
                let want = state.wants_pns(now);
                let needs_service = pns != Some(want)
                    && last_service.is_none_or(|(direction, when)| {
                        direction != want || now.duration_since(when) >= Duration::from_secs(1)
                    });
                monitor.changed.notify_all();
                drop(state);
                if needs_service && monitor.native.dual_mode() {
                    // Recheck the decision immediately before issuing the service command.
                    let current = monitor.state.lock().unwrap().wants_pns(Instant::now());
                    if current == want && (!want || !monitor.native_muted_now()) {
                        last_service = Some((want, now));
                        let _ = run(
                            &monitor.native.pns,
                            &[if want { "start" } else { "stop" }],
                            false,
                        );
                        pns = monitor.native.pid("mipns-xiaomi").map(|pid| pid.is_some());
                        // Recheck the log AND the player after stopping. A native
                        // StartCapture could have arrived during the service call.
                        let busy = monitor.native.busy(&mut journal);
                        let mut state = monitor.state.lock().unwrap();
                        state.refresh(Instant::now(), busy, pns, monitor.native.mute_marker());
                        monitor.changed.notify_all();
                    }
                }
                let guard = monitor.state.lock().unwrap();
                drop(monitor.changed.wait_timeout(guard, POLL).unwrap());
            }
        });
        monitor
    }

    fn invalidate(&self) {
        let mut s = self.state.lock().unwrap();
        s.refresh(Instant::now(), None, None, None);
        self.changed.notify_all();
    }

    pub fn observe_home_candidate(&self) -> bool {
        if !self.native.dual_mode() || self.native_muted_now() {
            return false;
        }
        self.state.lock().unwrap().candidate(Instant::now())
    }

    /// This can wait for a bounded native-service acknowledgement. Never call it
    /// from the microphone drain or the LED repaint loop.
    pub fn try_home_wake(&self) -> bool {
        if !self.native.dual_mode() || self.native_muted_now() {
            return false;
        }
        let now = Instant::now();
        let mut state = self.state.lock().unwrap();
        if state.owns(now) {
            return true;
        }
        let created = if state.native_available(now) {
            state.closing_until = None;
            state.candidate = None;
            state.reservation = Some(now + RESERVATION);
            self.changed.notify_all();
            state.reservation
        } else if state.allowed(now) && state.wants_home(now) && state.pns_running.is_some() {
            // Programmatic playback may arrive while the keyword thread waits
            // for the very same claim. Join it without extending its lifetime.
            None
        } else {
            return false;
        };
        let (mut state, _) = self
            .changed
            .wait_timeout_while(state, STOP_WAIT, |s| {
                let t = Instant::now();
                s.allowed(t) && s.wants_home(t) && !s.owns(t)
            })
            .unwrap();
        if state.owns(Instant::now()) && !self.native_muted_now() && self.native.dual_mode() {
            return true;
        }
        if created.is_some() && state.reservation == created {
            state.reservation = None;
            self.changed.notify_all();
        }
        false
    }

    /// Also gates non-wake words: a Home retreat or stop is legitimate while
    /// Home owns the device, whereas native activity and unavailable facts veto it.
    pub fn vendor_owns(&self) -> bool {
        if self.native_muted_now() {
            return true;
        }
        let state = self.state.lock().unwrap();
        let now = Instant::now();
        !(state.idle(now) || state.owns(now))
    }
    pub fn home_owns(&self) -> bool {
        self.state.lock().unwrap().owns(Instant::now()) && !self.native_muted_now()
    }
    pub fn indicator_owner(&self) -> super::lx06::IndicatorOwner {
        use super::lx06::IndicatorOwner;
        let s = self.state.lock().unwrap();
        let now = Instant::now();
        if self.native_muted_now() || !s.allowed(now) || s.pns_running.is_none() {
            IndicatorOwner::Native
        } else if s.owns(now) {
            IndicatorOwner::Home
        } else {
            IndicatorOwner::Idle
        }
    }
    pub fn capture_allowed(&self) -> bool {
        let s = self.state.lock().unwrap();
        s.may_capture(Instant::now()) && !self.native_muted_now()
    }
    pub fn native_muted_now(&self) -> bool {
        self.native.mute_marker() != Some(false)
    }
    pub fn native_blocks_play(&self) -> bool {
        let s = self.state.lock().unwrap();
        !s.allowed(Instant::now()) || s.pns_running.is_none() || self.native_muted_now()
    }
    /// Confirm the requested native privacy gate with a fresh process readback.
    /// Caller must first release any firmware-state lock.
    pub fn wait_privacy_applied(&self) -> bool {
        let s = self.state.lock().unwrap();
        let verified = |s: &State| {
            s.managed_mute
                && s.pns_running == Some(false)
                && s.pns_checked.is_some_and(|t| t.elapsed() < FRESH)
        };
        let (s, _) = self
            .changed
            .wait_timeout_while(s, STOP_WAIT, |s| s.managed_mute && !verified(s))
            .unwrap();
        verified(&s)
    }
    pub fn set_privacy(&self, muted: bool) {
        let mut s = self.state.lock().unwrap();
        if muted && !s.managed_mute {
            s.pns_checked = None;
        }
        s.managed_mute = muted;
        if muted {
            s.closing_until = None;
            s.candidate = None;
            s.reservation = None;
            s.home_confirmed = false;
        }
        self.changed.notify_all();
    }
    pub fn assistant_state(&self, value: &str) {
        let mut s = self.state.lock().unwrap();
        s.core_state(value, Instant::now());
        self.changed.notify_all();
    }
    pub fn set_playback_active(&self, active: bool) {
        let mut s = self.state.lock().unwrap();
        if s.playback != active {
            s.playback = active && s.owns(Instant::now());
            if s.playback {
                // The accepted writer now owns the tail through drain. Retain
                // the closing marker so a presentation echo cannot reopen it.
                if s.closing_until.is_some() {
                    s.closing_until = Some(Instant::now());
                }
                s.reservation = None;
            }
            if !s.wants_home(Instant::now()) {
                s.home_confirmed = false;
            }
            self.changed.notify_all();
        }
    }
}

struct Native {
    mode: PathBuf,
    mute: PathBuf,
    log: PathBuf,
    pidof: PathBuf,
    pns: PathBuf,
    mphelper: PathBuf,
    #[cfg(test)]
    scratch: Option<PathBuf>,
}

impl Native {
    fn device() -> Self {
        Self {
            mode: MODE.into(),
            mute: NATIVE_MUTE.into(),
            log: LOG.into(),
            pidof: "pidof".into(),
            pns: "/etc/init.d/pns".into(),
            mphelper: "mphelper".into(),
            #[cfg(test)]
            scratch: None,
        }
    }
    fn mute_marker(&self) -> Option<bool> {
        match fs::metadata(&self.mute) {
            Ok(_) => Some(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Some(false),
            Err(_) => None,
        }
    }

    fn dual_mode(&self) -> bool {
        fs::read_to_string(&self.mode).is_ok_and(|s| s.trim() == "homemate")
    }

    // A bounded status query. pidof status 1 with empty output means absent;
    // failing to start the helper, a timeout, or ambiguous output is unknown.
    fn pid(&self, name: &str) -> Option<Option<u32>> {
        let (code, out) = run(&self.pidof, &[name], true)?;
        match (code, out.trim()) {
            (1, "") => Some(None),
            (0, value) => value.split_whitespace().next()?.parse().ok().map(Some),
            _ => None,
        }
    }

    fn busy(&self, journal: &mut Journal) -> Option<bool> {
        let mico = self.pid("mico_aivs_lab")??;
        let playing = self.player_busy()?;
        let dialog = journal.read(&self.log, mico).ok()?;
        Some(playing || dialog)
    }

    fn player_busy(&self) -> Option<bool> {
        let (code, out) = run(&self.mphelper, &["mute_stat"], true)?;
        if code != 0 {
            return None;
        }
        match out.trim() {
            "0" | "2" => Some(false),
            "1" | "3" => Some(true),
            _ => None,
        }
    }
}

fn run(bin: &Path, args: &[&str], output: bool) -> Option<(i32, String)> {
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(if output {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .process_group(0)
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_millis(if output { 300 } else { 800 });
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                if let Some(pipe) = child.stdout.take() {
                    // A helper's descendant can retain stdout after it exits.
                    let fd = pipe.as_raw_fd();
                    if unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
                        return None;
                    }
                    pipe.take(256).read_to_string(&mut out).ok()?;
                }
                return Some((status.code()?, out));
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                // The init script can be a shell with foreground children.
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[derive(Default)]
struct Journal {
    cursor: Option<(u32, u64, u64, u64)>,
    dialog: Option<String>,
    pending: Vec<u8>,
    // Some firmware versions truncate and replace the last instruction on the
    // same inode; file length alone cannot distinguish that from an append.
    tail: Vec<u8>,
}

impl Journal {
    fn read(&mut self, path: &Path, mico: u32) -> io::Result<bool> {
        let mut f = File::open(path)?;
        let meta = f.metadata()?;
        let key = (mico, meta.dev(), meta.ino());
        if !meta.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a vendor journal",
            ));
        }
        let mut continuity = false;
        if let Some((pid, dev, ino, pos)) = self.cursor {
            if key == (pid, dev, ino) && meta.len() >= pos {
                f.seek(SeekFrom::Start(pos.saturating_sub(self.tail.len() as u64)))?;
                let mut bytes = vec![0; self.tail.len()];
                f.read_exact(&mut bytes)?;
                continuity = bytes == self.tail;
            }
        }
        let offset = match self.cursor {
            Some((pid, dev, ino, pos))
                if continuity
                    && key == (pid, dev, ino)
                    && meta.len() >= pos
                    && meta.len() - pos <= MAX_LOG as u64 =>
            {
                pos
            }
            _ => {
                // A new or rotated journal is a snapshot, not a new wake. Fold
                // its bounded tail to recover an already active dialog and allow
                // only its matching finish to release it.
                let start = meta.len().saturating_sub(MAX_LOG as u64);
                if self.cursor.is_none_or(|c| c.0 != mico) {
                    self.dialog = None;
                }
                self.pending.clear();
                if start > 0 {
                    self.dialog = Some(String::new());
                }
                start
            }
        };
        f.seek(SeekFrom::Start(offset))?;
        let mut data = Vec::new();
        (&mut f).take(MAX_LOG as u64 + 1).read_to_end(&mut data)?;
        if self.pending.len() + data.len() > MAX_LOG {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "vendor journal overrun",
            ));
        }
        self.pending.extend_from_slice(&data);
        let mut done = 0;
        for line in self.pending.split_inclusive(|b| *b == b'\n') {
            if line.last() != Some(&b'\n') {
                break;
            }
            done += line.len();
            if !line.iter().all(u8::is_ascii_whitespace) {
                match serde_json::from_slice::<serde_json::Value>(line) {
                    Ok(v) if v.get("header").is_some_and(|h| h.is_object()) => {
                        apply_instruction(&mut self.dialog, &v)
                    }
                    // A damaged line may have been StartCapture. Keep the
                    // microphone with the vendor until a fresh completion.
                    _ => self.dialog = Some(String::new()),
                }
            }
        }
        self.pending.drain(..done);
        // The vendor may replace its current line without writing a final
        // newline. A complete JSON value is a complete instruction at EOF.
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&self.pending) {
            if v.get("header").is_some_and(|h| h.is_object()) {
                apply_instruction(&mut self.dialog, &v);
                self.pending.clear();
            }
        }
        let end = offset + data.len() as u64;
        self.tail.resize(end.min(64) as usize, 0);
        f.seek(SeekFrom::Start(end - self.tail.len() as u64))?;
        f.read_exact(&mut self.tail)?;
        self.cursor = Some((mico, key.1, key.2, end));
        // Partial JSON can already be a new StartCapture. Do not admit a wake
        // until the line terminator arrives.
        Ok(self.dialog.is_some() || !self.pending.is_empty())
    }
}

fn apply_instruction(dialog: &mut Option<String>, v: &serde_json::Value) {
    let Some(header) = v.get("header") else {
        return;
    };
    let name = header.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let namespace = header
        .get("namespace")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let id = header
        .get("dialog_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    match (namespace, name) {
        ("SpeechRecognizer", "StartCapture" | "RecognizeResult" | "StopCapture")
        | ("SpeechSynthesizer", "Speak") => *dialog = Some(id.to_owned()),
        ("Dialog", "Finish")
            if dialog
                .as_deref()
                .is_none_or(|open| open.is_empty() || open == id) =>
        {
            *dialog = None
        }
        _ => {}
    }
}

#[cfg(test)]
impl Native {
    fn fixture() -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "hm-lx06-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let native = Self {
            mode: root.join("mode"),
            mute: root.join("mute"),
            log: root.join("instruction.log"),
            pidof: root.join("pidof"),
            pns: root.join("pns"),
            mphelper: root.join("mphelper"),
            scratch: Some(root),
        };
        fs::write(&native.mode, "homemate").unwrap();
        native
    }
}

#[cfg(test)]
impl Drop for Native {
    fn drop(&mut self) {
        if let Some(root) = self.scratch.as_ref() {
            let _ = fs::remove_dir_all(root);
        }
    }
}

#[cfg(test)]
impl VendorMonitor {
    pub(super) fn test_wants_pns(&self) -> bool {
        self.state.lock().unwrap().wants_pns(Instant::now())
    }
    pub(super) fn test_observer(native_busy: bool, home_owns: bool) -> Arc<Self> {
        let monitor = Self::new(false, Native::fixture());
        {
            let mut state = monitor.state.lock().unwrap();
            let now = Instant::now();
            state.core_online = true;
            state.core_active = home_owns;
            state.refresh(now, Some(native_busy), Some(!home_owns), Some(false));
        }
        monitor
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn idle_admission_requires_live_native_observers_and_pns_stop_readback() {
        let now = Instant::now();
        let mut s = State {
            core_online: true,
            ..State::default()
        };
        for (busy, pns) in [
            (None, Some(true)),
            (Some(false), None),
            (Some(true), Some(true)),
        ] {
            s.refresh(now, busy, pns, Some(false));
            assert!(!s.idle(now));
        }
        s.refresh(now, Some(false), Some(true), Some(false));
        assert!(s.idle(now));
        assert!(!s.idle(now + FRESH));
        s.reservation = Some(now + RESERVATION);
        assert!(!s.owns(now));
        assert!(!s.wants_pns(now));
        s.refresh(now, Some(false), Some(false), Some(false));
        assert!(s.owns(now));
        s.refresh(now, Some(false), Some(true), Some(false));
        assert!(!s.owns(now), "native unexpectedly respawned");
        assert!(s.wants_pns(now + RESERVATION));
        s.refresh(now + RESERVATION, Some(false), Some(true), Some(false));
        assert!(s.idle(now + RESERVATION));
    }

    #[test]
    fn privacy_busy_and_long_running_media() {
        let now = Instant::now();
        let mut s = State {
            core_online: true,
            playback: true,
            ..State::default()
        };
        s.refresh(now, Some(false), Some(false), Some(false));
        assert!(s.owns(now));
        assert!(!s.wants_pns(now + Duration::from_secs(600)));
        s.refresh(now, Some(true), Some(false), Some(false));
        assert!(!s.owns(now));
        s.playback = false;
        s.managed_mute = true;
        s.refresh(now, Some(false), Some(false), Some(false));
        assert!(!s.wants_pns(now));
        s.managed_mute = false;
        s.refresh(now, Some(false), Some(false), Some(true));
        assert!(!s.wants_pns(now));
        s.refresh(now, Some(false), Some(false), Some(false));
        assert!(s.wants_pns(now));
        assert!(
            !s.idle(now),
            "pns must be running again before another Home claim"
        );
    }

    #[test]
    fn retain_native_mute_owner_and_ignore_unsolicited_core_activity() {
        let now = Instant::now();
        let mut s = State::default();
        s.core_state("idle", now);
        s.refresh(now, Some(true), Some(true), Some(false));
        s.core_state("thinking", now);
        assert!(s.wants_pns(now));
        assert!(
            !s.wants_home(now),
            "a Core presentation update must not preempt native"
        );
        s.refresh(now, Some(false), Some(true), Some(true));
        s.core_state("muted", now);
        assert!(s.wants_pns(now), "self-muted PNS handles its own unmute");
        assert!(!s.idle(now));
        s.managed_mute = true;
        assert!(
            !s.wants_pns(now),
            "explicit privacy latch overrides the physical state"
        );
        s.managed_mute = false;
        s.refresh(now, Some(false), Some(false), Some(true));
        assert!(
            !s.wants_pns(now),
            "never start a stopped PNS through native privacy"
        );
        s.refresh(now, Some(false), Some(false), Some(false));
        assert!(s.wants_pns(now), "resume only after original mute released");
    }

    #[test]
    fn candidate_requires_fresh_positive_core_admission_and_explicit_idle_releases() {
        let now = Instant::now();
        let mut s = State::default();
        s.core_state("idle", now);
        s.refresh(now, Some(false), Some(true), Some(false));
        assert!(s.candidate(now));
        assert!(s.wants_pns(now));
        // A new native utterance arrives before Core confirms.
        s.refresh(now, Some(true), Some(true), Some(false));
        s.core_state("wake", now);
        assert!(s.wants_pns(now));
        s.refresh(now, Some(false), Some(true), Some(false));
        assert!(!s.candidate(now), "unsolicited active Core is still busy");
        s.core_state("idle", now);
        assert!(s.candidate(now));
        let late = now + RESERVATION;
        s.refresh(late, Some(false), Some(true), Some(false));
        s.core_state("wake", late);
        assert!(
            s.wants_pns(late),
            "stale candidates cannot claim a later wake"
        );
        s.core_state("idle", late);
        assert!(s.candidate(late));
        s.core_state("wake", late);
        assert!(!s.wants_pns(late));
        s.refresh(late, Some(false), Some(false), Some(false));
        assert!(s.owns(late));
        for presentation in ["paused", "error", "thinking", "hearing"] {
            s.core_state(presentation, late);
            assert!(s.owns(late));
        }
        s.core_state("idle", late);
        assert!(s.owns(late));
        s.core_state("idle", late + FINAL_CUE_WINDOW / 2);
        s.core_state("speaking", late + FINAL_CUE_WINDOW / 2);
        assert!(s.wants_pns(late + FINAL_CUE_WINDOW));
        assert!(!s.core_busy);
    }

    #[test]
    fn new_confirmed_home_wake_during_close_keeps_its_own_turn() {
        let monitor = VendorMonitor::test_observer(false, true);
        let mut s = monitor.state.lock().unwrap();
        let now = Instant::now();
        s.core_state("idle", now);
        assert!(s.candidate(now));
        s.core_state("wake", now);
        assert!(s.owns(now + FINAL_CUE_WINDOW));
        s.core_state("idle", now);
        s.refresh(now + FINAL_CUE_WINDOW, Some(false), Some(true), Some(false));
        assert!(s.closing_until.is_none());
        s.core_state("thinking", now + FINAL_CUE_WINDOW);
        assert!(s.core_busy);
    }

    #[test]
    fn farewell_joins_closing_turn_and_drains_before_return_to_native() {
        let monitor = VendorMonitor::test_observer(false, true);
        monitor.assistant_state("idle");
        assert!(!monitor.test_wants_pns());
        assert!(monitor.try_home_wake());
        monitor.set_playback_active(true);
        monitor.assistant_state("speaking");
        let after_window = Instant::now() + FINAL_CUE_WINDOW;
        assert!(!monitor.state.lock().unwrap().wants_pns(after_window));
        monitor.set_playback_active(false);
        assert!(monitor.test_wants_pns());
        monitor.assistant_state("idle");
        assert!(monitor.test_wants_pns());
    }

    #[test]
    fn keep_inline_speech_through_pending_stop_but_yield_if_native_started() {
        let now = Instant::now();
        for native_started in [true, false] {
            let mut s = State::default();
            s.core_state("idle", now);
            s.refresh(now, Some(false), Some(true), Some(false));
            assert!(s.candidate(now));
            for projection in ["wake", "hearing", "listening"] {
                s.core_state(projection, now);
                assert!(s.may_capture(now), "dropped speech onset at {projection}");
                assert!(!s.owns(now), "stop not yet confirmed");
            }
            s.refresh(now, Some(native_started), Some(false), Some(false));
            assert_eq!(s.owns(now), !native_started);
            assert_eq!(s.may_capture(now), !native_started);
            assert_eq!(s.wants_pns(now), native_started);
        }
    }

    #[test]
    fn worker_coordinates_real_helpers_two_listeners_mute_and_stop_timeout() {
        use std::os::unix::fs::PermissionsExt;
        let n = Native::fixture();
        let root = n.scratch.as_ref().unwrap().clone();
        let write_helper = |path: &Path, body: &str| {
            fs::write(
                path,
                format!("#!/bin/sh\ncd \"$(dirname \"$0\")\" || exit 2\n{body}\n"),
            )
            .unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        };
        write_helper(
            &n.pidof,
            r#"case "$1" in mico_aivs_lab) echo 101;; mipns-xiaomi) [ -f running ] || exit 1; echo 202;; *) exit 2;; esac"#,
        );
        write_helper(&n.mphelper, r#"[ "$1" = mute_stat ] && cat player"#);
        write_helper(
            &n.pns,
            r#"echo "$1" >> calls
case "$1" in
    start) : > running;;
    stop) [ ! -f hung ] || sleep 5; [ ! -f delay ] || sleep 0.3; rm -f running;;
    *) exit 2;;
esac"#,
        );
        let running = root.join("running");
        let muted = n.mute.clone();
        let journal = n.log.clone();
        let player = root.join("player");
        let calls = root.join("calls");
        let wait = |condition: &dyn Fn() -> bool| {
            let end = Instant::now() + Duration::from_secs(5);
            while !condition() {
                assert!(
                    Instant::now() < end,
                    "worker did not converge; calls = {:?}",
                    fs::read_to_string(&calls)
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        fs::write(&running, "").unwrap();
        fs::write(&player, "0").unwrap();
        let monitor = VendorMonitor::start_with(false, n);
        monitor.assistant_state("idle");
        assert!(monitor.vendor_owns(), "missing journal is unknown");
        fs::write(&journal, "").unwrap();
        wait(&|| !monitor.vendor_owns());
        // First the vendor accepts one utterance. StopCapture does not end
        // native playback or an in-flight native dialog.
        fs::write(&journal, event("SpeechRecognizer", "StartCapture", "first")).unwrap();
        wait(&|| monitor.vendor_owns());
        assert!(!monitor.observe_home_candidate());
        monitor.assistant_state("thinking");
        fs::write(&journal, event("SpeechRecognizer", "StopCapture", "first")).unwrap();
        fs::write(&player, "1").unwrap();
        fs::write(&journal, event("Dialog", "Finish", "first")).unwrap();
        assert!(!monitor.try_home_wake());
        assert!(running.exists(), "home must not stop native mid-dialog");
        monitor.assistant_state("idle");
        fs::write(&player, "0").unwrap();
        wait(&|| !monitor.vendor_owns());
        // A lost/unclassified keyword can be forwarded without changing PNS.
        assert!(monitor.observe_home_candidate());
        assert!(!calls.exists());
        // Second the Home wake wins Core approval; native readback must stop.
        fs::write(root.join("delay"), "").unwrap();
        monitor.assistant_state("wake");
        wait(&|| calls.exists());
        monitor.assistant_state("hearing");
        assert!(monitor.capture_allowed());
        monitor.assistant_state("listening");
        assert!(monitor.capture_allowed());
        wait(&|| monitor.home_owns());
        assert!(!running.exists());
        fs::remove_file(root.join("delay")).unwrap();
        monitor.assistant_state("speaking");
        monitor.assistant_state("idle");
        wait(&|| !monitor.vendor_owns() && running.exists());
        // Physical privacy remains under the real stock touchpad's control.
        fs::write(&muted, "").unwrap();
        assert!(monitor.vendor_owns());
        std::thread::sleep(POLL + POLL);
        assert!(running.exists());
        fs::remove_file(&muted).unwrap();
        wait(&|| !monitor.vendor_owns());
        monitor.set_privacy(true);
        assert!(monitor.wait_privacy_applied());
        assert!(!running.exists());
        monitor.set_privacy(false);
        wait(&|| !monitor.vendor_owns() && running.exists());
        // A hung stop script cannot grant a successful Home takeover.
        fs::write(root.join("hung"), "").unwrap();
        assert!(!monitor.try_home_wake());
        assert!(!monitor.home_owns());
        assert!(running.exists());
        fs::remove_file(root.join("hung")).unwrap();
        wait(&|| !monitor.vendor_owns());
        assert!(monitor.try_home_wake());
        assert!(!running.exists());
    }

    fn event(namespace: &str, name: &str, id: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({"header":{"namespace":namespace,"name":name,"dialog_id":id}})
        )
    }

    #[test]
    fn observe_in_progress_dialog_across_rotation_and_partial_appends() {
        let path = std::env::temp_dir().join(format!("hm-vendor-journal-{}", std::process::id()));
        let rotated = path.with_extension("old");
        let mut journal = Journal::default();
        fs::write(
            &path,
            [
                event("SpeechRecognizer", "StartCapture", "old"),
                event("Dialog", "Finish", "old"),
            ]
            .concat(),
        )
        .unwrap();
        assert!(
            !journal.read(&path, 50).unwrap(),
            "no replay of completed dialogs"
        );
        let append = |s: &str| {
            fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(s.as_bytes())
                .unwrap()
        };
        let start = event("SpeechRecognizer", "StartCapture", "live");
        append(&start[..20]);
        assert!(journal.read(&path, 50).unwrap(), "partial event is unknown");
        append(&start[20..]);
        append(&event("SpeechRecognizer", "StopCapture", "live"));
        assert!(
            journal.read(&path, 50).unwrap(),
            "recognition ended but answer is pending"
        );
        fs::rename(&path, &rotated).unwrap();
        assert!(journal.read(&path, 50).is_err());
        fs::write(&path, event("Dialog", "Finish", "old")).unwrap();
        assert!(
            journal.read(&path, 50).unwrap(),
            "stale finish on new inode"
        );
        append(&event("Dialog", "Finish", "live"));
        assert!(!journal.read(&path, 50).unwrap());
        append("malformed\n");
        assert!(journal.read(&path, 50).unwrap());
        append(&event("Dialog", "Finish", "next"));
        assert!(!journal.read(&path, 50).unwrap());
        let pending = event("SpeechRecognizer", "StartCapture", "z");
        fs::write(&path, pending.trim_end()).unwrap();
        assert!(journal.read(&path, 50).unwrap());
        let ended = format!(
            "{}{}",
            event("Dialog", "Finish", "z").trim_end(),
            " ".repeat(pending.len())
        );
        fs::write(&path, ended).unwrap();
        assert!(
            !journal.read(&path, 50).unwrap(),
            "same-inode replacement without newline"
        );
        fs::remove_file(&path).unwrap();
        fs::remove_file(rotated).unwrap();
    }
}
