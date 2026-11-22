use super::*;

/// Bounded capacity of the uplink audio channel (~2 s at 80 ms/chunk).
pub const UPLINK_AUDIO_CAP: usize = 25;
/// Bounded capacity of the decode worker queue.
pub(super) const DECODE_QUEUE_CAP: usize = 64;

pub(super) enum DecodeCmd {
    ReserveStream {
        task_id: String,
        chan: Chan,
        stamp: PlayStamp,
        north: NorthTx,
        detail: String,
    },
    Play {
        task_id: String,
        chan: Chan,
        audio_b64: Option<String>,
        simulated_ms: Option<u64>,
        sample_rate: u32,
        channels: u16,
        stamp: PlayStamp,
        north: NorthTx,
    },
    PlayChunk {
        task_id: String,
        seq: u64,
        audio_b64: String,
        stamp: PlayStamp,
        north: NorthTx,
    },
    PlayEnd {
        task_id: String,
        stamp: PlayStamp,
    },
}

/// Poison-aware mutex lock: if a thread panicked while holding the lock,
/// record an `AudioFault` so the supervisor restarts cleanly (code 75) and
/// recover the inner guard instead of cascading panics.
pub(super) fn lock_or_fault<'a, T>(
    m: &'a Mutex<T>,
    fault: Option<&AudioFault>,
    label: &str,
) -> std::sync::MutexGuard<'a, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            if let Some(f) = fault {
                f.request(format!("mutex poisoned: {label}"));
            }
            poisoned.into_inner()
        }
    }
}

pub(super) type LedFadeState = ([(u8, u8, u8); leds::RING_LEDS], tokio::time::Instant);

pub(super) struct Shared {
    pub(super) vendor: Option<Arc<lx06_vendor::VendorMonitor>>,
    keyword_link: Option<u64>,
    pub(super) mute_revision: u64,
    pub(super) last_reported_mute: Option<bool>,
    /// Outbound north writer for the live connection (None between links).
    pub(super) north_tx: Option<NorthTx>,
    pub(super) mic_on: bool,
    pub(super) volume: u64,
    /// Device-owned hardware mic mute (privacy): gates uplink, forces the
    /// muted LED, persists across reboots (onepager §3-3).
    pub(super) muted: bool,
    /// Last AssistantState (repainted after reconnects).
    pub(super) led_state: Option<String>,
    /// LED B: green→black fade on window close (from-frame, started-at). The
    /// animation tick owns it; paint_led defers to it instead of painting the
    /// new state immediately.
    pub(super) led_fade: Option<LedFadeState>,
    /// Volume-arc suppression window (EchoMuse mechanism, stage C UX):
    /// while set, state frames are RECORDED into led_state but not painted;
    /// expiry repaints the latest recorded state (mute-aware). None = no arc.
    pub(super) arc_until: Option<tokio::time::Instant>,
    /// The level the live arc draws (v1 mechanism #12: the arc must survive
    /// any re-render, not only the instant the key was pressed).
    pub(super) arc_level: u64,
    /// v1 mechanism (#12): the machine-health lamp. `Reboot` until the first
    /// handshake verdict; `Protocol` sticks until a FULL handshake succeeds;
    /// `Offline` tracks the link. NEVER set from AssistantState — these are
    /// gateway-local facts, and the lamp outranks every semantic state except
    /// the mute latch.
    pub(super) link_fault: Option<leds::HealthLamp>,
    /// Wi-Fi probe verdict (v1 mechanism #12): true only after the debounced
    /// probe saw the interface unassociated twice in a row. A failed or
    /// timed-out probe leaves this value alone (three-state honesty: the
    /// probe never claims "down" it cannot prove).
    pub(super) wifi_down: bool,
    /// Local Wi-Fi provisioning notice state (#23): entered on mute hold,
    /// stops playback, rejects new Play/PlayBegin, lights provisioning lamp,
    /// and NEVER triggers factory reset.
    pub(super) wifi_provisioning: bool,
    /// Monotonic deadline (`kws::mono_ms()`) when local Wi-Fi provisioning
    /// expires and falls back to normal operation (#23).
    pub(super) wifi_provisioning_until_ms: Option<u64>,
    /// Night-mode brightness schedule (#35).
    pub(super) night_mode: leds::NightMode,
    /// LED ring physical orientation (#35).
    pub(super) led_orientation: leds::LedOrientation,
    pub(super) duplex_requested: String,
    /// What the link actually runs (R1g). The firmware's capability is
    /// honestly `half`, so this stays "half" until a verified full-duplex
    /// echo path exists; the near-end local stop reads THIS, not the request.
    pub(super) duplex_effective: String,
    /// Active play per channel: (task_id, expected_ms).
    pub(super) active: [Option<(String, u64)>; 2],
    /// 48k samples actually written to the DAC per channel (voice=0,
    /// music=1) — the played-side term of the v0.3 stream overflow budget.
    pub(super) played: [u64; 2],
    /// Loopback probe (task B): bumped when a playback stream starts, so the
    /// capture-side track restarts at the same moment the DAC-side track
    /// does (both files must describe the same window).
    pub(super) play_epoch: u64,
    /// 2026-11-14 (M6c-2): the output tone requested by the core. The
    /// playback thread rebuilds its stage when `eq_rev` changes (one lock per
    /// period, same pattern as `volume`), so a console save is heard on the
    /// next 42.7 ms period without a restart.
    pub(super) eq_curve: Option<loudness::Curve>,
    pub(super) eq_bands: Vec<loudness::CustomBand>,
    pub(super) eq_rev: u64,
    /// Channel-live truth from the playback thread ([voice, music]) — the
    /// writer's view of "audio is being written", updated on every
    /// transition. The barge-in detector arms on voice, the keyword guard
    /// needs "any playback" (D/T5).
    pub(super) play_live: [bool; 2],
    /// Keyword spotter status (D/T4): present when the sidecar was enabled.
    /// Replayed on reconnect so a false "running" is impossible.
    pub(super) kws_status: Option<kws::SharedStatus>,
    /// R1j/P1: deploy handle (restart the spotter after writing kw.txt).
    pub(super) kws_control: Option<kws::KwsControl>,
    /// Shared acoustic correlation source, also retained for KWS when
    /// semantic barge-in is disabled. None without AEC or both consumers.
    pub(super) barge: Option<Arc<std::sync::Mutex<bargein::Detector>>>,
    /// Task id a pending flush is answering (R0, 2026-11-13): a stop defers
    /// its receipt until FlushDone, and that receipt belongs to exactly this
    /// task — never to whatever play took the channel slot meanwhile.
    pub(super) pending_stop: [Option<String>; 2],
    /// R1d local stop: the capture thread's near-end reflex already flushed
    /// the voice channel. (task_id, fired_ms) lives here until FlushDone
    /// reports the hardware fact north, so the frame names the play that was
    /// actually cut. While set, further chunks for that task are dropped: the
    /// core is still synthesizing and would otherwise re-open the channel it
    /// just lost (a 3 s fact latency cannot deliver its Stop in time).
    pub(super) local_stop: Option<(String, u64)>,
    /// Contract v2 §2.5 (#5, 2026-11-16): a retreat word the spotter heard
    /// and this device is acting on NOW. One episode at a time; the audio is
    /// cut locally (both channels) and the fact is reported north when the
    /// hardware flush completes. This is NOT a second session state machine —
    /// the core's close_session remains the only authority.
    pub(super) local_exit: Option<LocalExit>,
    /// Sender into the playback thread for the local reflex. Set once the
    /// channel exists; the capture thread must not own a second one.
    pub(super) play_tx: Option<std::sync::mpsc::Sender<PlayCmd>>,
    /// v0.2.9 (wake arbitration): the loudest post-AEC block of the last
    /// `LEVEL_WINDOW_BLOCKS`, in dBFS. Written by the capture thread on every
    /// block, read when the spotter reports a hit — which happens up to
    /// `lag_ms` later. A window PEAK rather than the instantaneous level: by
    /// the time the model fires, the phrase may already be over.
    pub(super) mic_level: LevelWindow,
    /// Per-channel generation bumped on every Play/PlayBegin/Stop/teardown so
    /// an in-flight decode in the decode worker drops its stale output.
    pub(super) decode_gen: [u64; 2],
    /// Connection generation bumped on every north_connect_and_serve session.
    pub(super) link_gen: u64,
    /// Negotiated protocol version and `config_apply_v2` capability for the
    /// active north link.
    pub(super) negotiated_protocol: u32,
    pub(super) config_apply_v2: bool,
    pub(super) gateway_boot_id: String,
    pub(super) led_tz_offset_minutes: i16,
    pub(super) led_timezone: Option<String>,
    pub(super) volume_cap: f64,
    pub(super) mix_gain: f32,
    pub(super) vad_threshold: f32,
    pub(super) mic_gain_offset_db: Option<f32>,
    pub(super) config_journal: config_journal::ConfigJournal,
    pub(super) pending_eq_receipt: Option<PendingEqConfig>,
}

#[derive(Debug, Clone)]
pub(super) struct PendingEqConfig {
    pub(super) eq_rev: u64,
    pub(super) receipt: config_journal::ConfigReceiptData,
    pub(super) persisted_values: serde_json::Value,
}

impl Shared {
    pub(super) fn stamp(&self, chan: Chan) -> PlayStamp {
        PlayStamp {
            link: self.link_gen,
            channel: self.decode_gen[if chan == Chan::Voice { 0 } else { 1 }],
        }
    }
    pub(super) fn send_play(&self, tx: &std::sync::mpsc::Sender<PlayCmd>, action: PlayAction) {
        let stamp = self.stamp(action.chan());
        let _ = tx.send(PlayCmd { stamp, action });
    }

    pub(super) fn chan_slot(&mut self, chan: Chan) -> &mut Option<(String, u64)> {
        &mut self.active[if chan == Chan::Voice { 0 } else { 1 }]
    }
}

/// Playback-progress heartbeat for the watchdog (2026-11-13).
///
/// The 2026-11-13 hang: the device stopped writing to the DAC while the
/// process stayed up. Nothing in the firmware noticed, so the core waited for
/// a `play_finished` that could never come. The playback thread publishes
/// "a channel has live audio" plus "this write is outstanding, for THIS live
/// set" and the async pump compares the age. A starve (live stream, no
/// complete period yet, waiting for the next chunk) is deliberately NOT a
/// stall: an outstanding write that cannot complete is the only trip.
#[derive(Default)]
pub(crate) struct PlayProgress {
    live_mask: std::sync::atomic::AtomicU8,
    /// Which channels were live when the OUTSTANDING write began (0 = the
    /// idle silence clock). Recorded at the write site, never read from
    /// `live_mask` later: the loop-top `set_live` runs BEFORE the command
    /// queue is drained, so a stream whose very first period parks in
    /// `playback_write` still reads mask 0 there — the first-write blind
    /// spot that let a wedged play start go unseen (2026-11-13 M4).
    write_mask: std::sync::atomic::AtomicU8,
    want_write: std::sync::atomic::AtomicBool,
    at_ms: std::sync::atomic::AtomicU64,
}

impl PlayProgress {
    pub(super) fn set_live(&self, idx: usize, live: bool) {
        use std::sync::atomic::Ordering::Relaxed;
        let bit = 1u8 << idx;
        let cur = self.live_mask.load(Relaxed);
        let next = if live { cur | bit } else { cur & !bit };
        self.live_mask.store(next, Relaxed);
    }

    /// Called immediately before EVERY DAC write (data period or silence
    /// clock), carrying the live set computed at the write site. Stamping
    /// here (not only on completion) is what makes a blocked ioctl visible:
    /// the stamp ages while the thread cannot advance it.
    pub(super) fn begin_write(&self, live: u8) {
        use std::sync::atomic::Ordering::{Relaxed, Release};
        self.write_mask.store(live, Relaxed);
        self.at_ms.store(kws::mono_ms(), Relaxed);
        self.want_write.store(true, Release);
    }

    pub(super) fn end_write(&self) {
        use std::sync::atomic::Ordering::{Relaxed, Release};
        self.at_ms.store(kws::mono_ms(), Relaxed);
        self.want_write.store(false, Release);
    }

    pub(super) fn clear(&self) {
        use std::sync::atomic::Ordering::{Relaxed, Release};
        self.want_write.store(false, Release);
        self.write_mask.store(0, Relaxed);
        self.live_mask.store(0, Relaxed);
    }

    /// Channels blocked by a write that has not completed within `bound_ms`
    /// (0 = watchdog off). `Some(0)` means the DAC is wedged while nothing
    /// was live (the silence clock itself): no receipt is owed, but audio can
    /// never come back without a restart. `None` = healthy, including a plain
    /// starve (no outstanding write at all).
    pub(super) fn stalled(&self, now: u64, bound_ms: u64) -> Option<u8> {
        use std::sync::atomic::Ordering::{Acquire, Relaxed};
        if bound_ms == 0 || !self.want_write.load(Acquire) {
            return None;
        }
        if now.saturating_sub(self.at_ms.load(Relaxed)) >= bound_ms {
            Some(self.write_mask.load(Relaxed))
        } else {
            None
        }
    }
}

/// An unrecoverable audio fault (2026-11-13, M4).
///
/// The owed `failed` receipts go north, but the process cannot heal a wedged
/// DAC: the only recovery is a fresh codec bring-up, so the firmware exits
/// nonzero inside a bounded window and the supervisor starts it again. If the
/// kernel will not release the blocked write (D state), the process cannot
/// leave at all — that is an IMAGE-level fault and the supervisor cannot save
/// it; the contract says so instead of claiming coverage.
#[derive(Default)]
pub(super) struct AudioFault {
    reason: Mutex<Option<String>>,
    notify: tokio::sync::Notify,
}

impl AudioFault {
    /// First request wins: a second wedged channel must not overwrite the
    /// first reason (both channels fail through the same write gate).
    pub(super) fn request(&self, reason: String) -> bool {
        let mut slot = self.reason.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return false;
        }
        *slot = Some(reason);
        drop(slot);
        self.notify.notify_one();
        true
    }

    pub(super) fn take(&self) -> Option<String> {
        self.reason.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    pub(super) fn is_requested(&self) -> bool {
        self.reason
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    pub(super) async fn notified(&self) {
        self.notify.notified().await;
    }
}

/// Exit code for an unrecoverable audio fault (sysexits EX_TEMPFAIL — the
/// supervisor restart is exactly the remedy). 0 stays reserved for an
/// intentional TERM; any nonzero exit is already "failure" to the supervisor.
pub const AUDIO_FAULT_EXIT_CODE: i32 = 75;

/// Bounded drain before the fault exit. The runtime is single-threaded, so
/// this MUST be an async sleep: the north writer needs the scheduler to put
/// the owed `failed` receipts on the socket first.
pub(super) const AUDIO_FAULT_DRAIN_MS: u64 = 1_500;

/// One live v0.3 stream (between play_begin and play_end/stop).
#[derive(Clone)]
pub(super) struct StreamCtx {
    pub(super) stamp: PlayStamp,
    pub(super) chan: Chan,
    pub(super) state: Arc<Mutex<StreamDecodeState>>,
}
pub(super) struct StreamDecodeState {
    pub(super) next_seq: u64,
    pub(super) resampler: resample::StreamResampler,
    pub(super) pushed: u64,
    pub(super) played_base: u64,
    pub(super) streaming_receipt_sent: bool,
}

/// Everything the async side needs; cloned into tasks freely.
#[derive(Clone)]
pub(super) struct Role {
    pub(super) cfg: FirmwareConfig,
    pub(super) hw: Arc<dyn Hardware>,
    pub(super) shared: Arc<Mutex<Shared>>,
    pub(super) play_tx: std::sync::mpsc::Sender<PlayCmd>,
    pub(super) decode_tx: mpsc::Sender<DecodeCmd>,
    /// Volume-key autorepeat throttle (2026-11-15): the kernel repeats at
    /// ~10/s, which is too fast to paint a ring and too fast to hear.
    pub(super) volume_repeat_at: Arc<Mutex<Option<tokio::time::Instant>>>,
    pub(super) seq: Arc<AtomicU64>,
    /// Live v0.3 streams by task_id (begin..end). Single-connection role;
    /// shared via Arc so the Role itself stays Clone for spawned tasks.
    pub(super) streams: Arc<Mutex<std::collections::HashMap<String, StreamCtx>>>,
    /// Playback heartbeat for the stall watchdog (2026-11-13).
    pub(super) progress: Arc<PlayProgress>,
    /// Unrecoverable-audio-fault latch (M4): the pump drains the owed
    /// receipts and then exits nonzero for the supervisor to restart.
    pub(super) fault: Arc<AudioFault>,
    /// True while `playback_thread` is running; cleared on panic/exit.
    pub(super) playback_alive: Arc<AtomicBool>,
    /// Last successful capture block (monotonic ms). A capture thread that
    /// panicked or sits in a blocked read stops advancing this; the pump
    /// treats it as an audio fault (a box that cannot hear is dead even if
    /// it can still talk).
    pub(super) capture_ok_ms: Arc<AtomicU64>,
    /// Total uplink `AudioChunk` frames dropped due to bounded queue backpressure.
    pub(super) dropped_audio: Arc<AtomicU64>,
    pub(super) northbound_stats: Arc<northbound::NorthboundStats>,
}

pub struct Firmware {
    cfg: FirmwareConfig,
    hw: Arc<dyn Hardware>,
    shared: Arc<Mutex<Shared>>,
    seq: Arc<AtomicU64>,
    dropped_audio: Arc<AtomicU64>,
    northbound_stats: Arc<northbound::NorthboundStats>,
}

fn keyword_level_source(shared: Arc<Mutex<Shared>>) -> kws::LevelSource {
    Arc::new(move |window_ms| {
        shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mic_level
            .peak_over_ms(window_ms)
    })
}

/// The gateway only receives the retreat subset of the Core reflex map.
/// Pass a clean idle candidate through immediately; only Core can confirm
/// that its mapping opened a Home session. Media claims use actual Play.
pub(super) fn lx06_keyword_admission(shared: Arc<Mutex<Shared>>) -> kws::HitAdmission {
    Arc::new(move |word, suspected, retreat| {
        let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
        if sh.muted || sh.north_tx.is_none() {
            return false;
        }
        if word.trim() == "切换小爱" {
            if sh.vendor.is_none() && !suspected {
                drop(sh);
                if let Err(error) =
                    lx06::select_mode(std::path::Path::new("/data/hm/mode"), "xiaoai-pinned")
                {
                    fwlog!("[lx06] mode switch failed: {error}");
                } else {
                    std::process::exit(0);
                }
            }
            return false;
        }
        let Some(vendor) = &sh.vendor else {
            return true;
        };
        let accepted = if suspected || retreat {
            !vendor.vendor_owns() && (!suspected || vendor.home_owns())
        } else {
            vendor.observe_home_candidate()
        };
        if accepted {
            sh.keyword_link = Some(sh.link_gen);
        }
        accepted
    })
}

impl Firmware {
    pub fn new(cfg: FirmwareConfig, hw: Arc<dyn Hardware>) -> Firmware {
        let saved_led = leds::load_led_config(&cfg.state_dir);
        let night_mode = saved_led
            .as_ref()
            .map(|s| s.night_mode)
            .unwrap_or(cfg.night_mode);
        let led_orientation = saved_led
            .as_ref()
            .map(|s| s.led_orientation)
            .unwrap_or(cfg.led_orientation);
        let led_tz_offset_minutes = saved_led.as_ref().map(|s| s.tz_offset_minutes).unwrap_or(0);
        let led_timezone = saved_led.and_then(|s| s.timezone);

        let saved_eq = config_journal::load_audio_eq(&cfg.state_dir);
        let (eq_curve, eq_bands, eq_rev) = match saved_eq {
            Some(eq) => {
                let curve = match eq.curve.as_str() {
                    "official" => loudness::Curve::Official,
                    "echomuse" => loudness::Curve::EchoMuse,
                    "custom" => loudness::Curve::Custom,
                    _ => loudness::Curve::Rui,
                };
                let bands = eq
                    .bands
                    .into_iter()
                    .map(|b| loudness::CustomBand {
                        freq: b.freq,
                        gain_db: b.gain_db,
                        q: b.q,
                    })
                    .collect();
                (Some(curve), bands, 1)
            }
            None => (None, Vec::new(), 0),
        };

        let saved_acoustics = config_journal::load_acoustics(&cfg.state_dir);
        let (volume_cap, mix_gain, vad_threshold, mic_gain_offset_db) = match saved_acoustics {
            Some(ac) => (
                ac.volume_cap,
                ac.mix_gain,
                ac.onset_threshold_dbfs,
                ac.mic_gain_offset_db,
            ),
            None => (
                cfg.volume_cap,
                cfg.profile.mix_gain,
                cfg.barge_in_db,
                Some(cfg.profile.mic_gain_offset_db),
            ),
        };

        let journal = config_journal::ConfigJournal::load(&cfg.state_dir);
        let gateway_boot_id = config_journal::new_gateway_boot_id();

        Firmware {
            cfg,
            hw,
            shared: Arc::new(Mutex::new(Shared {
                vendor: None,
                keyword_link: None,
                mute_revision: 0,
                last_reported_mute: None,
                north_tx: None,
                mic_on: false,
                volume: DEFAULT_VOLUME,
                eq_curve,
                eq_bands,
                eq_rev,
                led_state: None,
                led_fade: None,
                arc_until: None,
                arc_level: 0,
                link_fault: Some(leds::HealthLamp::Reboot),
                wifi_down: false,
                wifi_provisioning: false,
                wifi_provisioning_until_ms: None,
                night_mode,
                led_orientation,
                muted: false,
                duplex_requested: String::new(),
                duplex_effective: "half".into(),
                active: [None, None],
                played: [0, 0],
                play_epoch: 0,
                play_live: [false, false],
                kws_status: None,
                kws_control: None,
                barge: None,
                pending_stop: [None, None],
                local_stop: None,
                local_exit: None,
                play_tx: None,
                mic_level: LevelWindow::default(),
                decode_gen: [0, 0],
                link_gen: 0,
                negotiated_protocol: 0,
                config_apply_v2: false,
                gateway_boot_id,
                led_tz_offset_minutes,
                led_timezone,
                volume_cap,
                mix_gain,
                vad_threshold,
                mic_gain_offset_db,
                config_journal: journal,
                pending_eq_receipt: None,
            })),
            seq: Arc::new(AtomicU64::new(0)),
            dropped_audio: Arc::new(AtomicU64::new(0)),
            northbound_stats: Arc::new(northbound::NorthboundStats::default()),
        }
    }

    #[doc(hidden)]
    pub fn test_dropped_audio_counter(&self) -> Arc<AtomicU64> {
        self.dropped_audio.clone()
    }

    #[doc(hidden)]
    pub fn test_wifi_down_setter(&self) -> Box<dyn Fn(bool) + Send + Sync> {
        let shared = self.shared.clone();
        let hw = self.hw.clone();
        Box::new(move |down: bool| {
            {
                let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
                sh.wifi_down = down;
            }
            render_ring(&shared, &hw);
        })
    }

    /// Read-only bounded observer of the actual production transport counters.
    pub fn northbound_observer(&self) -> Arc<northbound::NorthboundStats> {
        self.northbound_stats.clone()
    }

    pub fn run(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let Firmware {
            cfg,
            hw,
            shared,
            seq,
            dropped_audio,
            northbound_stats,
        } = self;
        // 2026-11-13 stall watchdog: the playback thread publishes progress
        // here; the async pump reads it and fails the stream honestly.
        let progress = Arc::new(PlayProgress::default());
        // M4: the unrecoverable-fault latch and the capture heartbeat. The
        // heartbeat starts at boot so a capture thread that never manages one
        // successful read trips it too (PCM open can block).
        let fault = Arc::new(AudioFault::default());
        let playback_alive = Arc::new(AtomicBool::new(true));
        let capture_ok_ms = Arc::new(AtomicU64::new(kws::mono_ms()));

        let (vol, muted) = load_state(&cfg.state_dir);
        let vendor = (cfg.profile.model == "LX06"
            && std::env::var("HM_LX06_DUAL_WAKE").as_deref() == Ok("1"))
        .then(|| lx06_vendor::VendorMonitor::start(muted));
        {
            let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
            sh.volume = vol;
            sh.muted = muted;
            sh.vendor.clone_from(&vendor);
        }
        let _ = hw.wake_lock(); // init context succeeds; adb context degrades
        if muted {
            // A muted device must not come up hot: the hardware gate is
            // restored before anything else, and the ring says so. (Mute
            // outranks the Reboot lamp, so this IS what ring_frame gives.)
            let _ = hw.set_adc_mute(true);
            let _ = hw.led_write(&leds::state_rgb("muted"));
        } else {
            // v1 mechanism #12: the boot lamp is `Reboot` until the first
            // handshake verdict, not a dark ring.
            render_ring(&shared, &hw);
        }

        // Hardware threads (std blocking I/O). The playback thread keeps a
        // private volume Arc so gain reads never contend with north frames.
        let (play_tx, play_rx) = std::sync::mpsc::channel::<PlayCmd>();
        // R1d: the capture thread owns the near-end reflex and must be able
        // to flush without a round trip through the core (measured 3.078 s
        // fact latency, plus 1.9 s for the Stop behind TTS chunks).
        shared.lock().unwrap_or_else(|e| e.into_inner()).play_tx = Some(play_tx.clone());
        let (events_tx, mut events_rx) = mpsc::unbounded_channel::<PlayEvent>();
        let (btn_tx, mut btn_rx) = mpsc::unbounded_channel::<ButtonEvent>();
        let (pipeline, ref_feed) = if cfg.aec {
            let (mut p, r) = aec::Pipeline::new(cfg.aec_tail);
            let feed = if cfg.profile.model == "LX06" {
                p.set_direct_clean_ref(true);
                None
            } else {
                Some(r)
            };
            // State the rate each canceller REPORTS, not a constant: the boot
            // line must be the device's own fact (R0 — an A/B build with the
            // set-call skipped still printed the constant, which was a lie).
            let rate = p.sampling_rates().first().copied().unwrap_or(-1);
            fwlog!(
                "[aec] on: {} cancellers frame={} tail={} ({}ms) speex_rate={}Hz, fusion 33t/1.4kHz, dealias {}",
                aec::MIC_CHANNELS, aec::FRAME, cfg.aec_tail,
                cfg.aec_tail * 1000 / CAP_RATE as usize, rate,
                if cfg.profile.model == "LX06" { "direct-hw-ref" } else { "armed" }
            );
            (Some(p), feed)
        } else {
            (None, None)
        };
        // No eager open: tracks are armed per diagnostic play (see
        // probe_armed) so the steady path writes nothing to /run.
        let (probe_play, probe_cap, probe_mics): (
            Option<ProbeSink>,
            Option<ProbeSink>,
            Option<ProbeSink>,
        ) = (None, None, None);
        let loopback_probe = cfg.loopback_probe;
        {
            let hw = hw.clone();
            let shared = shared.clone();
            let mix_gain = cfg.profile.mix_gain;
            let loudness_on = cfg.loudness;
            let loudness_bass = cfg.loudness_bass;
            let loudness_curve = cfg.loudness_curve;
            let loudness_mbcl = cfg.loudness_mbcl;
            let eq_bands = cfg.eq_bands.clone();
            let progress = progress.clone();
            if loudness_on {
                let (curve, mbcl) = loudness::resolve_chain(loudness_curve, loudness_mbcl);
                fwlog!(
                    "[loudness] on: curve={} mbcl={} bands={} bass_scale={} (pre-ref-feed)",
                    match curve {
                        loudness::Curve::Official => "official",
                        loudness::Curve::Custom => "custom",
                        loudness::Curve::EchoMuse => "echomuse",
                        loudness::Curve::Rui => "rui",
                    },
                    mbcl,
                    eq_bands.len(),
                    loudness_bass
                );
                if curve == loudness::Curve::Custom {
                    fwlog!(
                        "[loudness] custom bands: {}",
                        eq_bands
                            .iter()
                            .map(|b| format!("{}Hz{:+.1}dB Q{:.2}", b.freq, b.gain_db, b.q))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
            let fault = fault.clone();
            let playback_alive = playback_alive.clone();
            std::thread::spawn(move || {
                playback_thread(
                    hw,
                    play_rx,
                    events_tx,
                    shared,
                    ref_feed,
                    mix_gain,
                    loudness_on,
                    loudness_bass,
                    loudness_curve,
                    loudness_mbcl,
                    eq_bands,
                    loopback_probe,
                    probe_play,
                    progress,
                    fault,
                    playback_alive,
                )
            });
        }
        {
            let hw = hw.clone();
            std::thread::spawn(move || loop {
                match hw.button_next() {
                    Ok(ev) => {
                        if btn_tx.send(ev).is_err() {
                            return;
                        }
                    }
                    Err(_) => std::thread::sleep(std::time::Duration::from_secs(1)),
                }
            });
        }
        // v1 mechanism (#12): the Wi-Fi health probe. A DEDICATED std thread
        // — the query is a blocking subprocess and must never sit on the
        // async runtime's 1 s tick (the audit flagged exactly that). Each
        // query is additionally bounded: the answer must arrive within
        // WIFI_PROBE_TIMEOUT_MS or it counts as "no answer", which never
        // claims "down". Entering `wifi_down` needs TWO consecutive
        // unassociated answers (debounce, no lamp flapping); leaving it
        // needs ONE associated answer. A run of failed queries clears the
        // claim instead of freezing it.
        if cfg.wifi {
            spawn_wifi_probe(shared.clone(), hw.clone());
        }
        let nearend = Arc::new(Mutex::new(nearend::NearEndEvaluator::new(
            cfg.device_id.clone(),
            cfg.aec,
            super::core_addr_is_loopback(&cfg.core_addr),
        )));
        // KWS echo integrity outlives the optional semantic barge-in feature.
        // Capture keeps the same correlation source fresh without arming a
        // semantic onset when cfg.barge_in is false.
        let vad_threshold = lock_or_fault(&shared, Some(&fault), "start.shared").vad_threshold;
        let barge = if cfg.aec && (cfg.barge_in || cfg.kws) {
            let detector = Arc::new(std::sync::Mutex::new(bargein::Detector::new(
                bargein::Config {
                    onset_db: vad_threshold,
                    near_end_margin_db: cfg.barge_in_margin,
                    converged_blocks: cfg.barge_in_converged,
                    corr_max: cfg.barge_in_corr,
                    settle_ms: cfg.barge_in_settle,
                    ..bargein::Config::default()
                },
            )));
            fwlog!(
                "[acoustic-guard] on: floor {} dBFS / near-end within {} dB of ref / converged {} blocks / guard corr < {} / preroll {}ms / settle {}ms{}",
                cfg.barge_in_db,
                cfg.barge_in_margin,
                cfg.barge_in_converged,
                cfg.barge_in_corr,
                bargein::PREROLL_MS,
                cfg.barge_in_settle,
                if cfg.barge_in_stats > 0 {
                    format!(" / stats {}ms", cfg.barge_in_stats)
                } else {
                    String::new()
                }
            );
            if !cfg.barge_in {
                fwlog!("[barge-in] semantic triggering off; KWS correlation guard remains active");
            }
            shared.lock().unwrap_or_else(|e| e.into_inner()).barge = Some(detector.clone());
            Some(detector)
        } else {
            let why = if cfg.aec {
                "--barge-in off"
            } else {
                "no AEC: the uplink would be our own echo"
            };
            fwlog!("[barge-in] disabled ({why})");
            None
        };
        // Keyword spotter (D/T4): its own process, its own core, fed from
        // the post-AEC block. The north sink follows reconnects through
        // Shared; a sidecar failure never blocks or degrades the audio path.
        // The echo guard (D/T5) suppresses hits the microphone heard FROM
        // THE SPEAKERS: playback live + mic-vs-reference correlation high.
        let kws_feed = if cfg.kws {
            let shared_north = shared.clone();
            let north: kws::NorthSink = Arc::new(move |frame: GatewayFrame| {
                let mut sh = shared_north.lock().unwrap_or_else(|e| e.into_inner());
                let Some(tx) = sh.north_tx.clone() else {
                    return;
                };
                if let Some(vendor) = sh
                    .vendor
                    .as_ref()
                    .filter(|_| matches!(frame, GatewayFrame::Keyword { .. }))
                {
                    let allowed = !sh.muted && vendor.capture_allowed();
                    let same_link = sh.keyword_link.take() == Some(sh.link_gen);
                    if !allowed || !same_link {
                        return;
                    }
                    tx.send_ctrl(frame);
                } else {
                    drop(sh);
                    tx.send_ctrl(frame);
                }
            });
            let guard_shared = shared.clone();
            let guard_detector = barge.clone();
            let guard_nearend = nearend.clone();
            let guard_threshold = cfg.barge_in_corr;
            let guard: kws::Guard = Arc::new(move || {
                let playing = {
                    let sh = guard_shared.lock().unwrap_or_else(|e| e.into_inner());
                    sh.play_live[0] || sh.play_live[1]
                };
                nearend::kws_guard_should_suppress(
                    playing,
                    &guard_nearend,
                    guard_detector.as_ref(),
                    guard_threshold,
                    kws::mono_ms(),
                )
            });
            // v0.2.9 / #47: the claim's acoustic evidence, read from the capture
            // thread's own window (same samples the spotter was fed), aligned to
            // the wake phrase span. Transport RAW dBFS, just like presence;
            // Core alone applies per-unit arbitration calibration.
            let level = keyword_level_source(shared.clone());
            // Contract v2 §2.5 (#5): a retreat word cuts the audio HERE, in
            // the spotter's reader thread, before the core is asked. The
            // decision to close the session is still the core's; this only
            // stops what is playing and starts the fact that proves it.
            // Self-heard hits never reach this closure — the reader only
            // calls it for hits the echo guard did NOT suspect.
            let exit_shared = shared.clone();
            let on_retreat: kws::RetreatSink = Arc::new(move |keyword: &str, _fed_ms: u64| {
                let now = now_ms();
                let mut sh = exit_shared.lock().unwrap_or_else(|e| e.into_inner());
                if sh.vendor.as_ref().is_some_and(|v| v.vendor_owns()) {
                    return false;
                }
                if sh.local_exit.is_some() {
                    // One episode at a time: a double detection must not
                    // become two flushes or two facts.
                    return false;
                }
                let voice_task = sh.active[0].as_ref().map(|(t, _)| t.clone());
                let music_task = sh.active[1].as_ref().map(|(t, _)| t.clone());
                let mask = LocalExit::channel_mask(
                    sh.play_live,
                    [voice_task.as_deref(), music_task.as_deref()],
                );
                if mask == 0 {
                    // Nothing is playing: the exit is only a Report north,
                    // and the core still closes the window from the keyword.
                    fwlog!(
                        "[firmware] local_exit_idle keyword={keyword} — nothing to flush (the keyword still goes north)"
                    );
                    return false;
                }
                let tx = sh.play_tx.clone();
                sh.local_exit = Some(LocalExit {
                    keyword: keyword.to_string(),
                    fired_ms: now,
                    flushed: mask,
                    pending: mask,
                    voice_task,
                    music_task,
                    flush_err: None,
                });
                if let Some(tx) = tx {
                    if mask & 1 != 0 {
                        sh.send_play(&tx, PlayAction::Flush(Chan::Voice));
                    }
                    if mask & 2 != 0 {
                        sh.send_play(&tx, PlayAction::Flush(Chan::Music));
                    }
                }
                drop(sh);
                fwlog!(
                    "[firmware] local_exit_start keyword={keyword} channels={:?} (core not consulted yet)",
                    if mask == 1 { "voice" } else if mask == 2 { "music" } else { "voice+music" }
                );
                true
            });
            let admit =
                (cfg.profile.model == "LX06").then(|| lx06_keyword_admission(shared.clone()));
            let feed = kws::spawn_with_admission(
                cfg.kws_dir.clone(),
                cfg.kws_cpu,
                cfg.device_id.clone(),
                north,
                Some(guard),
                Some(level),
                Some(on_retreat),
                admit,
            );
            fwlog!(
                "[kws] sidecar enabled: {} (cpu{})",
                cfg.kws_dir.display(),
                cfg.kws_cpu
            );
            {
                let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
                sh.kws_status = Some(feed.status());
                sh.kws_control = Some(feed.control());
            }
            Some(feed)
        } else {
            fwlog!("[kws] sidecar disabled (--kws to enable keyword interruption)");
            None
        };
        // D/T5 debt #1: boot capture health sentinel (clock-free, observes
        // the first ~2 s + first playback). Off via --boot-health off.
        let boot = if cfg.boot_health && cfg.aec {
            fwlog!(
                "[boot-health] on: {} blocks warm-up, ch8/ref correlation on first play",
                boot_health::WARMUP_BLOCKS
            );
            Some(boot_health::BootSentinel::new())
        } else {
            fwlog!(
                "[boot-health] disabled ({})",
                if cfg.aec {
                    "--boot-health off"
                } else {
                    "requires --aec"
                }
            );
            None
        };
        {
            let hw = hw.clone();
            let shared = shared.clone();
            let cfg = cfg.clone();
            let seq = seq.clone();
            let capture_ok = capture_ok_ms.clone();
            std::thread::spawn(move || {
                capture_thread(
                    hw,
                    shared,
                    cfg,
                    seq,
                    pipeline,
                    loopback_probe,
                    probe_cap,
                    probe_mics,
                    kws_feed,
                    barge,
                    nearend,
                    boot,
                    capture_ok,
                )
            });
        }

        let (decode_tx, decode_rx) = mpsc::channel::<DecodeCmd>(DECODE_QUEUE_CAP);
        let role = Role {
            cfg,
            hw,
            shared,
            play_tx,
            decode_tx,
            seq,
            volume_repeat_at: Arc::new(Mutex::new(None)),
            streams: Arc::new(Mutex::new(std::collections::HashMap::new())),
            progress,
            fault,
            playback_alive,
            capture_ok_ms,
            dropped_audio,
            northbound_stats,
        };
        role.northbound_stats
            .set_decode_queue(role.decode_tx.downgrade());
        {
            let role_decode = role.clone();
            std::thread::Builder::new()
                .name("decode".into())
                .spawn(move || decode_thread(role_decode, decode_rx))?;
        }

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(async move {
            let role2 = role.clone();
            let mut north = tokio::spawn(north_loop(role));
            let mut pump = tokio::spawn(async move {
                // 2026-11-13: 500 ms is 12 periods; the watchdog itself is a
                // multi-second bound, so this only decides how quickly a
                // frozen DAC becomes a named failure.
                let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                // LED B: the breath needs a fine cadence (10 Hz, ~9% of a
                // 3.5 s cycle per step — smooth to the eye, negligible CPU).
                let mut led = tokio::time::interval(std::time::Duration::from_millis(100));
                led.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                // M4 test hook: a boot-relative deadline realized once. The
                // ${state_dir}/fault-inject file is the ON-DEVICE form and is
                // removed as it is read, so the supervisor's restart cannot
                // re-inject — the next boot is the recovery half of the test.
                let inject_ms = std::fs::read_to_string(role2.cfg.state_dir.join("fault-inject"))
                    .ok()
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .inspect(|&v| {
                        let _ = std::fs::remove_file(role2.cfg.state_dir.join("fault-inject"));
                        fwlog!(
                            "[firmware] fault injection armed from fault-inject ({v}ms, one-shot)"
                        );
                    })
                    .unwrap_or(role2.cfg.fault_inject_ms);
                let inject_at = (inject_ms > 0).then(|| kws::mono_ms() + inject_ms);
                loop {
                    tokio::select! {
                        Some(ev) = events_rx.recv() => role2.on_play_event(ev),
                        Some(b) = btn_rx.recv() => role2.on_button(b),
                        // Volume-arc expiry: sleeps while the window is open
                        // (re-read on every pump wake, so extended presses
                        // push the deadline), parks when no arc is showing.
                        _ = arc_window(&role2) => role2.expire_volume_arc(),
                        // LED B: breathing / close fade.
                        _ = led.tick() => role2.led_tick(),
                        // Playback stall watchdog + capture heartbeat.
                        _ = tick.tick() => {
                            role2.watch_audio();
                            if let Some(at) = inject_at {
                                if kws::mono_ms() >= at {
                                    role2.fault.request(
                                        "injected audio fault (--fault-inject-ms test hook)".into(),
                                    );
                                }
                            }
                        }
                        // Immediate wakeup when playback thread exits/panics or mutex poisons.
                        _ = role2.fault.notified() => {}
                        else => break,
                    }
                    // Drain any already-queued playback events (e.g. WriteFailed
                    // sent right before playback_thread exited) so owed `failed`
                    // receipts reach the north writer before the fault drain.
                    while let Ok(ev) = events_rx.try_recv() {
                        role2.on_play_event(ev);
                    }
                    // M4: an unrecoverable audio fault leaves the process.
                    // The latched reason is drained HERE, on the one task
                    // that can also let the north writer drain: the runtime
                    // is single-threaded, so the wait must be an async one.
                    // Exit 75 makes the supervisor restart into a fresh
                    // codec bring-up (0 stays "intentional TERM").
                    if let Some(reason) = role2.fault.take() {
                        fwlog!("[firmware] unrecoverable audio fault: {reason}");
                        if role2.cfg.exit_on_audio_fault {
                            fwlog!(
                                "[firmware] {}ms drain, then exit {} for the supervisor restart",
                                AUDIO_FAULT_DRAIN_MS,
                                AUDIO_FAULT_EXIT_CODE
                            );
                            tokio::time::sleep(std::time::Duration::from_millis(
                                AUDIO_FAULT_DRAIN_MS,
                            ))
                            .await;
                            std::process::exit(AUDIO_FAULT_EXIT_CODE);
                        }
                        // Tests only (--audio-fault-exit off): stay up so the
                        // receipts already sent remain observable in-process.
                        fwlog!("[firmware] --audio-fault-exit off: staying up after the fault");
                    }
                }
            });
            tokio::select! {
                _ = &mut north => {},
                _ = &mut pump => {},
            }
        });
        Ok(())
    }
}

pub(super) async fn arc_window(role: &Role) {
    loop {
        let until = { lock_or_fault(&role.shared, Some(&role.fault), "role.shared").arc_until };
        match until {
            None => std::future::pending::<()>().await,
            Some(deadline) => {
                tokio::time::sleep_until(deadline).await;
                let now = tokio::time::Instant::now();
                let current =
                    { lock_or_fault(&role.shared, Some(&role.fault), "role.shared").arc_until };
                match current {
                    Some(d) if d > now => continue, // extended by a newer press
                    _ => return,                    // expired (or cleared)
                }
            }
        }
    }
}

impl Role {
    pub(super) fn flush_play(&self, chan: Chan) {
        let sh = lock_or_fault(&self.shared, Some(&self.fault), "flush.shared");
        sh.send_play(&self.play_tx, PlayAction::Flush(chan));
    }
    pub(super) fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// End-of-connection teardown (M6a). Called once the north loop returns,
    /// on the runtime thread — the playback thread may still be parked in a
    /// write, so this only QUEUES the flushes; the watchdog owns the wedged
    /// case. Without it a new connection inherited a Role that was still
    /// mid-stream and reported an old task's terminal facts into a new
    /// generation.
    pub(super) fn teardown_link(&self) {
        let (had_audio, cleared) = {
            let mut sh = lock_or_fault(&self.shared, Some(&self.fault), "teardown.shared");
            let cleared = (sh.active[0].is_some() as u8)
                + (sh.active[1].is_some() as u8)
                + (sh.play_live[0] as u8)
                + (sh.play_live[1] as u8);
            sh.send_play(&self.play_tx, PlayAction::Flush(Chan::Voice));
            sh.send_play(&self.play_tx, PlayAction::Flush(Chan::Music));
            sh.active = [None, None];
            sh.pending_stop = [None, None];
            sh.local_stop = None;
            // A local exit that lost its link keeps no marker: its flushes
            // are still owed but the fact has nowhere to go, and the keyword
            // reached the core (or will be retried by the next connection).
            sh.local_exit = None;
            sh.play_live = [false, false];
            sh.decode_gen[0] = sh.decode_gen[0].wrapping_add(1);
            sh.decode_gen[1] = sh.decode_gen[1].wrapping_add(1);
            sh.last_reported_mute = None;
            if let Some(vendor) = &sh.vendor {
                vendor.set_playback_active(false);
                vendor.assistant_state("offline");
                self.hw
                    .set_home_indicator(super::lx06::IndicatorOwner::Native);
            }
            sh.link_gen = sh.link_gen.wrapping_add(1);
            (cleared > 0, cleared)
        };
        let streams = std::mem::take(&mut *lock_or_fault(
            &self.streams,
            Some(&self.fault),
            "teardown.streams",
        ));
        self.progress.clear();
        if had_audio || !streams.is_empty() {
            // Counts CHANNELS (a whole-WAV play has a slot but no streaming
            // session), plus any v0.3 streams that were mid-flight.
            fwlog!(
                "[firmware] north link down — cleared {cleared} channel fact(s) and {} stream(s); both channels flushed",
                streams.len()
            );
        }
    }

    pub(super) fn north(&self, f: GatewayFrame) {
        let tx = lock_or_fault(&self.shared, Some(&self.fault), "north.shared")
            .north_tx
            .clone();
        if let Some(tx) = tx {
            tx.send_ctrl(f);
        }
    }

    pub(super) fn descriptor(&self) -> HelloDevice {
        let mut capabilities = vec![
            "audio_in".into(),
            "audio_out".into(),
            "stop".into(),
            "volume".into(),
            // v0.3: streamed playback (PlayBegin/PlayChunk/PlayEnd).
            "play_stream".into(),
            // v1 mechanism (#13, 2026-11-16): MuteSet writes the latch —
            // declared so the core sends it (and old cores, which never
            // send it, stay window-safe either way).
            "remote_mute".into(),
            // v1 mechanism (#14, 2026-11-16): the interrupt contract of THIS
            // build, as formal capability tokens — wake words + the retreat
            // set's LOCAL exit. Nothing else may cut audio (half duplex).
            "local_exit".into(),
            "interrupt_wake_word".into(),
            "interrupt_retreat_local_exit".into(),
            // Stage 2 M2.1 (`config_apply_v2` / #112 / #113): typed effective
            // configuration protocol with durable idempotent receipts.
            "config_apply_v2".into(),
            "config_block_kws".into(),
            "config_block_audio_eq".into(),
            "config_block_led".into(),
            "config_block_duplex".into(),
            "config_block_acoustics".into(),
        ];
        // v0.2.3: online wifi change (WifiConfig frame + watchdog) — only a
        // radio-carrying form declares it; a radio-less build refuses the
        // frame as a protocol error instead of executing it.
        if self.cfg.wifi {
            capabilities.push("wifi".into());
        }
        HelloDevice {
            stable_id: self.cfg.device_id.clone(),
            model: self.cfg.model.clone(),
            firmware: Some(self.cfg.version.clone()),
            capabilities,
            audio_profile: "fireos_builtin_speaker".into(),
            wifi_ssid: Self::wifi_current_ssid(),
        }
    }

    pub(super) fn mic_layout(&self) -> (&'static str, u16) {
        if self.cfg.hw_ref {
            ("mic_ref", 2)
        } else {
            ("mono", 1)
        }
    }

    // --- events from hardware threads ------------------------------------------

    /// One watchdog tick: the DAC write gate plus the capture heartbeat.
    /// Both are "audio is gone" facts; the pump turns either into an exit.
    pub(super) fn watch_audio(&self) {
        self.watch_wifi_provisioning();
        self.watch_playback();
        if self.cfg.capture_watchdog_ms == 0 {
            return;
        }
        let now = kws::mono_ms();
        let last = self
            .capture_ok_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        let stale = now.saturating_sub(last);
        if stale >= self.cfg.capture_watchdog_ms
            && self.fault.request(format!(
                "capture: no successful read for {stale}ms (bound {}ms)",
                self.cfg.capture_watchdog_ms
            ))
        {
            fwlog!(
                "[firmware] capture watchdog: no successful read for {stale}ms (bound {}ms)",
                self.cfg.capture_watchdog_ms
            );
        }
    }

    fn watch_wifi_provisioning(&self) {
        let now = kws::mono_ms();
        let expired = {
            let mut sh = lock_or_fault(&self.shared, Some(&self.fault), "role.shared");
            if sh.wifi_provisioning {
                if let Some(until) = sh.wifi_provisioning_until_ms {
                    if now >= until {
                        sh.wifi_provisioning = false;
                        sh.wifi_provisioning_until_ms = None;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            }
        };
        if expired {
            render_ring(&self.shared, &self.hw);
            let ssid = Self::wifi_current_ssid().unwrap_or_default();
            let detail = format!(
                "provisioning timeout after {}ms; kept existing network",
                self.cfg.wifi_provisioning_timeout_ms
            );
            fwlog!("[hm-firmware] wifi reverted ssid={ssid} {detail}");
            self.north(GatewayFrame::WifiState {
                device: self.cfg.device_id.clone(),
                ts: now_ms(),
                ssid,
                state: "reverted".into(),
                detail: Some(detail),
            });
        }
    }

    /// 2026-11-13 stall watchdog. The playback thread publishes the live set
    /// the outstanding write blocks plus its monotonic stamp; a channel whose
    /// write has not completed inside `cfg.play_watchdog_ms` is failed BY NAME
    /// here. A starve (waiting for the next chunk) never trips it — that is
    /// the difference this test pins.
    ///
    /// M4 change of contract: the receipt is still owed, but the process no
    /// longer pretends it can recover. A wedged DAC (write blocked while
    /// nothing was live — the silence clock — owes no receipt at all) is an
    /// unrecoverable audio fault: the firmware exits nonzero and the
    /// supervisor restarts it into a fresh codec bring-up.
    fn watch_playback(&self) {
        if !self.playback_alive.load(Ordering::Acquire) {
            self.fault
                .request("playback thread exited unexpectedly".into());
            return;
        }
        let Some(mask) = self
            .progress
            .stalled(kws::mono_ms(), self.cfg.play_watchdog_ms)
        else {
            return;
        };
        if mask == 0 {
            // Nothing live: no play to fail, but audio can never come back.
            if self.fault.request(format!(
                "play_watchdog: silence clock write blocked for {}ms",
                self.cfg.play_watchdog_ms
            )) {
                fwlog!(
                    "[firmware] play watchdog: the idle silence clock has made no progress for {}ms",
                    self.cfg.play_watchdog_ms
                );
            }
            return;
        }
        for (idx, chan) in [(0usize, Chan::Voice), (1, Chan::Music)] {
            if mask & (1u8 << idx) == 0 {
                continue;
            }
            let (slot, played) = {
                let mut sh = lock_or_fault(&self.shared, Some(&self.fault), "role.shared");
                let slot = sh.active[idx].take();
                sh.play_live[idx] = false;
                sh.pending_stop[idx] = None;
                (slot, sh.played[idx])
            };
            self.progress.set_live(idx, false);
            let Some((task_id, expected_ms)) = slot else {
                continue;
            };
            lock_or_fault(&self.streams, Some(&self.fault), "role.streams").remove(&task_id);
            let played_ms = played * 1000 / PLAY_RATE as u64;
            fwlog!(
                "[firmware] play watchdog: task={task_id} no DAC progress for {}ms played_ms={played_ms} pushed_ms={expected_ms}",
                self.cfg.play_watchdog_ms
            );
            self.north(GatewayFrame::PlayReceipt {
                task_id,
                stage: "failed".into(),
                detail: Some(format!(
                    "play_watchdog: no DAC progress for {}ms played_ms={played_ms} pushed_ms={expected_ms}",
                    self.cfg.play_watchdog_ms
                )),
                ts: now_ms(),
            });
            // Best effort: if the write is truly wedged this command waits in
            // the channel until the thread recovers; the receipt above has
            // already told the core what happened.
            self.flush_play(chan);
        }
        self.fault.request(format!(
            "play_watchdog: no DAC progress for {}ms (channels {}), played_ms/pushed_ms reported",
            self.cfg.play_watchdog_ms, mask
        ));
    }
}

#[cfg(test)]
pub(super) fn isolated_role(hw: Arc<dyn Hardware>) -> (Role, std::sync::mpsc::Receiver<PlayCmd>) {
    let firmware = Firmware::new(
        FirmwareConfig {
            device_id: "fixture-dot".into(),
            wifi: false,
            aec: false,
            kws: false,
            ..Default::default()
        },
        hw,
    );
    let (play_tx, rx) = std::sync::mpsc::channel();
    let (decode_tx, _) = mpsc::channel(DECODE_QUEUE_CAP);
    (
        Role {
            cfg: firmware.cfg,
            hw: firmware.hw,
            shared: firmware.shared,
            play_tx,
            decode_tx,
            seq: firmware.seq,
            volume_repeat_at: Arc::new(Mutex::new(None)),
            streams: Arc::new(Mutex::new(Default::default())),
            progress: Arc::new(PlayProgress::default()),
            fault: Arc::new(AudioFault::default()),
            playback_alive: Arc::new(AtomicBool::new(true)),
            capture_ok_ms: Arc::new(AtomicU64::new(0)),
            dropped_audio: firmware.dropped_audio,
            northbound_stats: firmware.northbound_stats,
        },
        rx,
    )
}
