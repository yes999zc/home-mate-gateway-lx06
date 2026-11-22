use super::*;

/// Device volume law (design doc §1 E7, corrected by live evidence): the
/// EchoMuse firmware applies volume in DSP — a full mixer dump diffed at
/// levels 114 vs 38 shows zero control changes — so the law is the
/// documented 0.5 dB/step scale applied in software: 127 = 0 dB, 0 = −63.5 dB.
pub fn level_to_gain(level: u64) -> f32 {
    10f32.powf((level as f32 - 127.0) * 0.5 / 20.0)
}

/// Mix two mono streams with clamping (voice + media wire channels), apply
/// the software volume gain, duplicate to interleaved stereo for the PCM.
pub fn mix_to_stereo(a: &[i16], b: &[i16], out_len: usize, gain: f32) -> Vec<i16> {
    let mut stereo = Vec::with_capacity(out_len * 2);
    for i in 0..out_len {
        let sa = a.get(i).copied().unwrap_or(0) as i32;
        let sb = b.get(i).copied().unwrap_or(0) as i32;
        let mixed = ((sa.saturating_add(sb)) as f32 * gain) as i32;
        let s = mixed.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        stereo.push(s);
        stereo.push(s);
    }
    stereo
}

/// Edge fades for one channel's period (RUI dot2-plug parameter
/// migration: their `fade_in_pcm`/`fade_out_pcm`, 2026-10 field-tuned).
/// A stream that starts or ENDS on a non-zero sample steps to/from the
/// always-clocking silence wire — heard as a tick or a power-cut pop
/// after the last word (user report 2026-11-10: answer tails AND the
/// follow-window tone). A short linear ramp makes the step inaudible.
/// Applied per channel BEFORE mixing so an ending voice does not dip
/// still-playing music.
const FADE_IN_SAMPLES: usize = (PLAY_RATE as f32 * 0.005) as usize; // 240 ≈ 5 ms
const FADE_OUT_SAMPLES: usize = (PLAY_RATE as f32 * 0.010) as usize; // 480 = 10 ms

pub(super) fn fade_edges(samples: &mut [i16], fade_in: bool, fade_out: bool) {
    if samples.is_empty() {
        return;
    }
    if fade_in {
        // linspace 0→1: the FIRST sample lands exactly on zero.
        let n = FADE_IN_SAMPLES.min(samples.len()).max(1);
        for (i, sample) in samples.iter_mut().enumerate().take(n) {
            let g = i as f32 / (n - 1) as f32;
            *sample = (*sample as f32 * g) as i16;
        }
    }
    if fade_out {
        // linspace 1→0 indexed from the end: the LAST sample lands
        // exactly on zero (a 0.998 × full-scale last sample is still an
        // audible step — caught by the tail-fade contract test).
        let n = FADE_OUT_SAMPLES.min(samples.len()).max(1);
        let len = samples.len();
        for i in 0..n {
            // i counts BACK from the end: i=0 IS the last sample and must
            // land on zero; the ramp start (i=n-1) keeps full gain.
            let g = if n == 1 {
                0.0
            } else {
                i as f32 / (n - 1) as f32
            };
            samples[len - 1 - i] = (samples[len - 1 - i] as f32 * g) as i16;
        }
    }
}

pub fn mix_mono_f32(a: &[i16], b: &[i16], out_len: usize, gain: f32) -> Vec<f32> {
    let mut mono = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let sa = a.get(i).copied().unwrap_or(0) as f32 / 32768.0;
        let sb = b.get(i).copied().unwrap_or(0) as f32 / 32768.0;
        mono.push((sa + sb) * gain);
    }
    mono
}

/// Loudness output → interleaved stereo S16 (the mix is mono, duplicated;
/// the limiter already bounded |x| ≤ −0.1 dBFS).
pub fn mono_to_stereo_i16(mono: &[f32]) -> Vec<i16> {
    let mut stereo = Vec::with_capacity(mono.len() * 2);
    for v in mono {
        let s = (v.clamp(-1.0, 1.0) * 32767.0) as i16;
        stereo.push(s);
        stereo.push(s);
    }
    stereo
}

#[derive(Debug)]
pub(super) enum PlayAction {
    Samples { chan: Chan, samples: Vec<i16> },
    Eos(Chan),
    Flush(Chan),
}

/// Internal execution identity, never taken from a wire task ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PlayStamp {
    pub link: u64,
    pub channel: u64,
}
#[derive(Debug)]
pub(super) struct PlayCmd {
    pub stamp: PlayStamp,
    pub action: PlayAction,
}
impl PlayAction {
    pub(super) fn chan(&self) -> Chan {
        match self {
            Self::Samples { chan, .. } | Self::Eos(chan) | Self::Flush(chan) => *chan,
        }
    }
}

/// Turn "written N ms" plus the hardware backlog into the pair every terminal
/// receipt carries (M6a). No backlog = no consumption claim: the caller gets
/// `(None, "written")` and must not read `played_ms` as "heard".
pub(super) fn progress_facts(
    played_ms: u64,
    delay_frames: Option<u64>,
) -> (Option<u64>, Option<String>) {
    match delay_frames {
        Some(frames) => {
            let delay_ms = frames.saturating_mul(1000) / PLAY_RATE as u64;
            (
                Some(played_ms.saturating_sub(delay_ms)),
                Some("hw_delay".to_string()),
            )
        }
        None => (None, Some("written".to_string())),
    }
}

/// Playback thread → role loop: a channel's stream drained after EOS, or a
/// Flush completed (the hardware fact behind device_confirmed receipts).
#[derive(Debug)]
pub(super) enum PlayEvent {
    StreamDone {
        stamp: PlayStamp,
        chan: Chan,
        bytes: u64,
        periods: u64,
        underruns: u64,
        /// Hardware backlog sampled the moment the stream drained (M6a).
        /// `None` = the platform cannot report it (the receipt then says
        /// `progress_basis: "written"`).
        delay_frames: Option<u64>,
    },
    /// The flush completed in hardware. `played_ms` is what the DAC actually
    /// took for this run before the stop — the fact the StopReceipt carries
    /// (R0, 2026-11-13: the receipt used to leave BEFORE this).
    FlushDone {
        stamp: PlayStamp,
        chan: Chan,
        played_ms: u64,
        /// Backlog sampled immediately BEFORE the drop (M6a).
        delay_frames: Option<u64>,
        /// The hardware flush itself failed — the channel may still be
        /// audible, so the stop must never be confirmed (M6a).
        flush_err: Option<String>,
        /// When the DAC was actually dropped (ms epoch), stamped HERE in the
        /// playback thread. The local-exit fact's latency must be this minus
        /// the hit: measuring it when the ROLE LOOP eventually reads the event
        /// charges the device for a core-side stall it did not cause (live
        /// 2026-11-16: a real flush was reported as 2959 ms because the role
        /// loop was 2.7 s behind).
        at_ms: u64,
    },
    WriteFailed {
        stamps: [PlayStamp; 2],
        detail: String,
    },
    /// Emitted at the 42 ms period boundary after `Loudness` has been rebuilt
    /// for `eq_rev`.
    EqApplied { eq_rev: u64 },
}

pub(super) struct ChanStream {
    stamp: Option<PlayStamp>,
    pending: VecDeque<i16>,
    eos: bool,
    live: bool,
    bytes_recv: u64,
    underruns_at_start: u64,
    periods: u64,
    /// 48k mono samples of THIS run that reached the DAC (periods in which
    /// the channel had data). Reset when the run ends — a stop/flush reports
    /// it as played_ms and starts the next run from zero.
    written: u64,
}

impl ChanStream {
    fn new() -> Self {
        ChanStream {
            stamp: None,
            pending: VecDeque::new(),
            eos: false,
            live: false,
            bytes_recv: 0,
            underruns_at_start: 0,
            periods: 0,
            written: 0,
        }
    }
}

/// samples would sit unwritten forever (caught by the G5-2 contract test).
struct PlaybackExitGuard {
    alive: Arc<AtomicBool>,
    fault: Arc<AudioFault>,
}

impl Drop for PlaybackExitGuard {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        let reason = if std::thread::panicking() {
            "playback thread panicked".to_string()
        } else {
            "playback thread exited unexpectedly".to_string()
        };
        self.fault.request(reason);
    }
}

/// Per-period playback gain combining the volume grid curve and the unit
/// profile's measured `mix_gain` multiplier (#25).
pub(crate) fn effective_playback_gain(level: u64, mix_gain: f32) -> f32 {
    level_to_gain(level) * mix_gain.clamp(0.0, 2.0)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn playback_thread(
    hw: Arc<dyn Hardware>,
    rx: std::sync::mpsc::Receiver<PlayCmd>,
    events: mpsc::UnboundedSender<PlayEvent>,
    shared: Arc<Mutex<Shared>>,
    ref_feed: Option<Arc<aec::dealias::Dealias>>,
    mix_gain: f32,
    loudness_on: bool,
    loudness_bass: f32,
    loudness_curve: Option<loudness::Curve>,
    loudness_mbcl: Option<bool>,
    eq_bands: Vec<loudness::CustomBand>,
    loopback_probe: bool,
    mut probe: Option<ProbeSink>,
    progress: Arc<PlayProgress>,
    fault: Arc<AudioFault>,
    playback_alive: Arc<AtomicBool>,
) {
    let _exit_guard = PlaybackExitGuard {
        alive: playback_alive,
        fault: fault.clone(),
    };
    pin_audio_thread("playback");
    let release_idle = hw.playback_releases_when_idle();
    let mut voice = ChanStream::new();
    let mut music = ChanStream::new();
    // Output loudness stage (D stage): tier keyed on the CURRENT level
    // each period (rebuilds its biquads only when the tier moves).
    let mut loudness =
        loudness::Loudness::from_config(loudness_on, loudness_curve, loudness_mbcl, &eq_bands);
    loudness.set_bass_scale(loudness_bass);
    // The revision the running stage was built from (0 = the supervisor line
    // config; any core push bumps it).
    let mut eq_applied: u64 = 0;

    fn stream<'a>(c: Chan, v: &'a mut ChanStream, m: &'a mut ChanStream) -> &'a mut ChanStream {
        if c == Chan::Voice {
            v
        } else {
            m
        }
    }

    /// Allocation-free variant: drain into a caller-owned scratch buffer.
    /// The playback hot path runs this 23 times a second for hours; every
    /// `Vec` it used to allocate here (take/mix/stereo) was churn the
    /// musl heap answered by growing — measured as ~12 MB of RSS per 28 s
    /// play, ending in an OOM kill after ~57 min (D-stage soak, 2026-11-10).
    fn take_into(s: &mut ChanStream, n: usize, out: &mut Vec<i16>) {
        out.clear();
        let k = n.min(s.pending.len());
        out.extend(s.pending.drain(..k));
    }

    // EchoMuse-parity silence loop: the DAC is clocked CONTINUOUSLY —
    // data periods when streams are live, silence periods otherwise. The
    // codec's playback loopback (ch8) rails at full scale when the stream
    // underruns (measured on-device: bursty TTS period writes → ref
    // 32767 flat, quiet tail shows a +65 DC bias), which poisons the AEC
    // reference; a tight-loop tone (no underruns) matched exactly 512/512.
    // Continuous clocking also keeps the de-alias ring advancing in
    // lockstep with the DAC — silence feeds it zeros, ch8 shows zeros.
    let silence = vec![0i16; PLAY_PERIOD as usize * 2];
    // Audio hot-path scratch (see take_into): reused for every period so the
    // steady path performs no heap allocation at all.
    let per = PLAY_PERIOD as usize;
    let mut a_buf: Vec<i16> = Vec::with_capacity(per);
    let mut b_buf: Vec<i16> = Vec::with_capacity(per);
    let mut mono_buf: Vec<f32> = Vec::with_capacity(per);
    let mut stereo_buf: Vec<i16> = Vec::with_capacity(per * 2);
    // Channel-live truth for Shared (D/T5): written on every transition,
    // never per period — the steady loop pays one comparison.
    let mut live_prev = [false, false];

    loop {
        // Watchdog truth: "this channel has audio" (a live stream or queued
        // frames), independent of whether a period is complete yet. The copy
        // that MATTERS is re-read at the write site below — this one is one
        // command drain stale (the first Samples of a play land after it).
        progress.set_live(0, voice.live || !voice.pending.is_empty());
        progress.set_live(1, music.live || !music.pending.is_empty());
        let quiet =
            !voice.live && !music.live && voice.pending.is_empty() && music.pending.is_empty();
        // B-M3: a live stream holding less than one period is NOT idle — it
        // is waiting for its next chunk. The old loop busy-spun with a
        // non-blocking poll and no write, so a gap longer than the 170 ms
        // ALSA buffer emptied the DAC (XRUN at a chunk boundary = the
        // "power-cut" at a sentence edge). Waiting here, bounded by half the
        // buffer, lets a normal inter-chunk gap pass untouched (no silence
        // inserted into the stream — the sample-equivalence contract test
        // pins that) while a genuinely stalled stream still gets zeros
        // before it can starve.
        let writable_now = voice.pending.len() >= per
            || music.pending.len() >= per
            || (voice.eos && !voice.pending.is_empty())
            || (music.eos && !music.pending.is_empty());
        let starving = !writable_now
            && (voice.live || music.live)
            && !(voice.pending.is_empty() && music.pending.is_empty());
        // True only when the wait below expired WITHOUT data: that is the
        // moment a starved stream must be given zeros. A chunk that merely
        // does not complete a period yet is not starvation — keep waiting
        // (this is what keeps the stream gap-free at chunk boundaries).
        let mut starved_out = false;
        let cmd = if quiet || starving {
            let wait_ms = if starving {
                PLAY_BUFFER as u64 * 500 / PLAY_RATE as u64
            } else {
                20
            };
            match rx.recv_timeout(std::time::Duration::from_millis(wait_ms)) {
                Ok(c) => Some(c),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    starved_out = starving;
                    None
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match rx.try_recv() {
                Ok(c) => Some(c),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
            }
        };
        // Drain EVERY queued command each iteration (one blocking read when
        // idle, then non-blocking). A whole-WAV play arrives as one Samples
        // command per period — 117 of them for a 5 s asset — and a
        // one-command-per-iteration poll left a Stop sitting behind all of
        // them, so the flush only landed after the whole asset had played
        // (T4 live evidence; regression test whole_wav_play_is_interruptible).
        let mut cmd = cmd;
        while let Some(c) = cmd.take() {
            let stamp = c.stamp;
            let chan = c.action.chan();
            let mut sh = lock_or_fault(&shared, Some(&fault), "playback.command");
            let current = sh.stamp(chan);
            let local = stream(chan, &mut voice, &mut music).stamp;
            let valid = match &c.action {
                PlayAction::Flush(_) => stamp == current || local == Some(stamp),
                _ => stamp == current,
            };
            if !valid {
                drop(sh);
                cmd = rx.try_recv().ok();
                continue;
            }
            sh.play_epoch = sh.play_epoch.wrapping_add(u64::from(
                matches!(&c.action, PlayAction::Samples { .. })
                    && !stream(chan, &mut voice, &mut music).live,
            ));
            drop(sh);
            // Local PCM is owned exclusively by this playback thread. A teardown
            // queues cleanup before fresh commands; it cannot mutate this buffer.
            // A new execution never inherits queued PCM from an older run.
            if !matches!(&c.action, PlayAction::Flush(_)) && local != Some(stamp) {
                *stream(chan, &mut voice, &mut music) = ChanStream::new();
                stream(chan, &mut voice, &mut music).stamp = Some(stamp);
            }
            match c.action {
                PlayAction::Samples { chan, samples } => {
                    let s = stream(chan, &mut voice, &mut music);
                    if !s.live {
                        s.underruns_at_start = 0;
                        s.live = true;
                        // Loopback probe: a new play = a new comparison
                        // window, on BOTH tracks (capture resets when it
                        // sees this epoch change).
                    }
                    s.bytes_recv += (samples.len() * 2) as u64;
                    s.pending.extend(samples);
                    if s.periods == 0 {
                        s.underruns_at_start = hw.playback_underruns();
                        // Probe tracks: open only when the core armed this
                        // play (zero steady-state I/O). The sink lives on
                        // the same thread that just saw the stream start.
                        if loopback_probe {
                            probe = if probe_armed() {
                                ProbeSink::open("write.pcm")
                            } else {
                                None
                            };
                        }
                    }
                }
                PlayAction::Eos(chan) => {
                    stream(chan, &mut voice, &mut music).eos = true;
                }
                PlayAction::Flush(chan) => {
                    let s = stream(chan, &mut voice, &mut music);
                    let played_ms = s.written * 1000 / PLAY_RATE as u64;
                    if s.live {
                        s.pending.clear();
                        s.eos = false;
                        s.live = false;
                    }
                    *s = ChanStream::new();
                    let mut flush_err = None;
                    // M6a: the backlog is sampled BEFORE the drop — after it
                    // there is nothing left to ask, and the stop receipt
                    // needs to distinguish "consumed" from "written".
                    // #18: only claim a DAC drop (delay_frames / dealias drop)
                    // when hardware `playback_flush()` actually ran; if the
                    // other channel is still live, the DAC buffer was NOT
                    // dropped.
                    let mut delay_frames = None;
                    if !voice.live && !music.live {
                        delay_frames = hw.playback_delay_frames();
                        if let Err(e) = hw.playback_flush() {
                            // The channel may still be audible. Never let the
                            // stop be confirmed on a failed flush (M6a).
                            flush_err = Some(format!("{e}"));
                            delay_frames = None;
                        } else if let Some(feed) = &ref_feed {
                            feed.on_dac_drop();
                        }
                    }
                    // AFTER the hardware flush: the receipt the role loop
                    // sends from here states a hardware fact, not an intent.
                    let _ = events.send(PlayEvent::FlushDone {
                        stamp,
                        chan,
                        played_ms,
                        delay_frames,
                        flush_err,
                        at_ms: now_ms(),
                    });
                }
            }
            cmd = match rx.try_recv() {
                Ok(c) => Some(c),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
            };
        }

        // Nothing queued right now: clock one silence period so the DAC
        // never underruns (see the silence-loop note above). This also
        // paces the loop; real periods take the write path below.
        //
        // B-M3 (§3.1): the clock is driven by "no channel has a writable
        // period", NOT by "both channels are empty". A live stream holding
        // 1..period-1 frames used to make BOTH branches skip that iteration
        // — no write at all, so the DAC starved (XRUN) exactly at the chunk
        // boundaries of a streamed answer. Zeros keep the clock running; the
        // buffered frames stay queued and play once a period is complete.
        // Watchdog truth AT THE WRITE SITE (2026-11-13 M4): the top-of-loop
        // copy predates the command drain, so the first period of a play —
        // the one that parks if the DAC is already wedged — would publish
        // mask 0 and the stall would be invisible. This is the set the
        // outstanding write actually blocks.
        let live_now: u8 = u8::from(voice.live || !voice.pending.is_empty())
            | (u8::from(music.live || !music.pending.is_empty()) << 1);
        let writable = voice.pending.len() >= per
            || music.pending.len() >= per
            || (voice.eos && !voice.pending.is_empty())
            || (music.eos && !music.pending.is_empty());
        // A shared sink already has a clock. Starting its private writer
        // for pure idle silence would keep the device occupied indefinitely.
        let clock_silence =
            !release_idle || (voice.live && !voice.eos) || (music.live && !music.eos);
        if clock_silence && !writable && (!starving || starved_out) {
            if let Some(feed) = &ref_feed {
                feed.feed(&silence);
            }
            // 2026-11-13 (M4): the silence clock is a DAC write like any
            // other. It used to skip the write gate, so a blocked silence
            // write froze the clock invisibly (no receipt, no watchdog).
            progress.begin_write(live_now);
            let write_stamps = execution_stamps(&shared, &voice, &music);
            if hw.playback_write(&silence, PLAY_PERIOD).is_err() {
                progress.end_write();
                let _ = events.send(PlayEvent::WriteFailed {
                    stamps: write_stamps,
                    detail: "silence clock write failed".into(),
                });
                let _ = hw.playback_flush();
            } else {
                progress.end_write();
            }
            // Fall through: the EOS check below must still run so a stream
            // that just drained reports StreamDone (the contract test
            // caught the `continue` skipping it).
        }

        // Write whole periods while any channel has a COMPLETE period
        // queued, or is at EOS with a partial tail (padded once, then
        // done). Writing a partial mid-stream (v0.3 caught by the
        // equivalence contract test) chops the stream into zero-padded
        // islands — a live stream below one period WAITS for more chunks
        // instead. A live-but-starved stream still must not be mistaken
        // for idle: the loop keeps draining the command channel, so later
        // samples are consumed the moment they land (G5-2 lesson kept).
        // Read the authoritative level from `shared` (the contract test
        // caught a stale private copy ignoring volume_set — one source of
        // truth, one lock per 42 ms period).
        let (level, eq_rev, eq_curve, eq_bands, live_mix_gain) = {
            let sh = lock_or_fault(&shared, Some(&fault), "playback.shared");
            (
                sh.volume,
                sh.eq_rev,
                sh.eq_curve,
                sh.eq_bands.clone(),
                sh.mix_gain,
            )
        };
        if eq_rev != eq_applied {
            // Rebuild the stage at a period boundary: a config change lands
            // between periods, never inside one.
            loudness =
                loudness::Loudness::from_config(loudness_on, eq_curve, loudness_mbcl, &eq_bands);
            loudness.set_bass_scale(loudness_bass);
            loudness.set_level(level);
            eq_applied = eq_rev;
            let _ = events.send(PlayEvent::EqApplied { eq_rev });
        }
        let _ = mix_gain;
        let gain = effective_playback_gain(level, live_mix_gain);
        loudness.set_level(level);
        let per = PLAY_PERIOD as usize;
        // ONE period per outer-loop iteration, not a burst. A whole-WAV play
        // queues all of its samples up front, and the old burst loop drained
        // that entire queue (28 s of blocked ALSA writes) before returning to
        // the command channel — Stop/Flush could only land after the asset had
        // finished (T4 live evidence: the device reported played_ms=28032 for
        // a play stopped 4 s in, and the DAC never went quiet). Checking
        // commands every period bounds stop latency to ~one period (42.7 ms)
        // plus the ALSA flush, for whole-WAV and streamed plays alike.
        if voice.pending.len() >= per
            || music.pending.len() >= per
            || (voice.eos && !voice.pending.is_empty())
            || (music.eos && !music.pending.is_empty())
        {
            take_into(&mut voice, per, &mut a_buf);
            take_into(&mut music, per, &mut b_buf);
            let a = &mut a_buf;
            let b = &mut b_buf;

            // Edge fades on the channel's FIRST and LAST period: the last
            // period is this take emptying an EOS stream (a full-period
            // take that empties it counts too — the boundary case a
            // partial-only rule would miss).
            let a_first = voice.periods == 0;
            let a_last = voice.eos && voice.pending.is_empty();
            fade_edges(a, a_first, a_last);
            let b_first = music.periods == 0;
            let b_last = music.eos && music.pending.is_empty();
            fade_edges(b, b_first, b_last);
            // Mix → loudness ladder (bass/presence/gain by volume tier +
            // limiter) → stereo. The loudness stage sits BEFORE the
            // de-alias feed so the AEC reference is the curve that
            // actually plays — the canceller adapts to the new spectrum
            // automatically (same contract as any playback change).
            // Same arithmetic as mix_mono_f32 (which pads short channel
            // buffers to a whole period with zeros) but into a reused
            // buffer: the circuit-equivalence contract test pins both.
            mono_buf.clear();
            mono_buf.reserve(per);
            for i in 0..per {
                let sa = a.get(i).copied().unwrap_or(0) as f32 / 32768.0;
                let sb = b.get(i).copied().unwrap_or(0) as f32 / 32768.0;
                mono_buf.push((sa + sb) * gain);
            }
            loudness.process(&mut mono_buf);
            stereo_buf.clear();
            for v in mono_buf.iter() {
                let s = (v.clamp(-1.0, 1.0) * 32767.0) as i16;
                stereo_buf.push(s);
                stereo_buf.push(s);
            }
            let stereo = &stereo_buf[..];
            // Every written period feeds the de-alias reference ring — the
            // AEC's clean reference is rebuilt from what we ACTUALLY wrote
            // (post-gain, post-loudness; DAC volume sits at unity), ch8
            // only aligns it.
            if let Some(feed) = &ref_feed {
                feed.feed(stereo);
            }
            progress.begin_write(live_now);
            let write_stamps = execution_stamps(&shared, &voice, &music);
            if let Err(e) = hw.playback_write(stereo, PLAY_PERIOD) {
                let _ = events.send(PlayEvent::WriteFailed {
                    stamps: write_stamps,
                    detail: format!("{e}"),
                });
                progress.clear();
                let _ = hw.playback_flush();
                // Thread exits here; leave the channel-live truth honest.
                lock_or_fault(&shared, Some(&fault), "playback.shared").play_live = [false, false];
                break;
            }
            progress.end_write();
            // Loopback self-check (task B): the samples this period ACTUALLY
            // sent to the DAC, decimated 48k→16k (every 3rd, same ratio the
            // ch8 reference path uses). Capture side appends ch8 in
            // capture_thread; the core aligns the two files.
            if let Some(p) = probe.as_mut() {
                let mono16: Vec<i16> = mono_buf
                    .iter()
                    .step_by(3)
                    .map(|v| (v.clamp(-1.0, 1.0) * 32767.0) as i16)
                    .collect();
                p.append(&mono16);
            }
            voice.periods += (a.len() / PLAY_PERIOD as usize) as u64;
            music.periods += (b.len() / PLAY_PERIOD as usize) as u64;
            // played_ms truth: a written period is PLAY_PERIOD frames at the
            // DAC for every channel that contributed samples to it.
            if !a.is_empty() {
                voice.written += PLAY_PERIOD as u64;
            }
            if !b.is_empty() {
                music.written += PLAY_PERIOD as u64;
            }
            // v0.3 overflow budget: the played-side term (per-channel 48k
            // samples that reached the DAC; silence-clock periods do not
            // count — only real stream data advances playback progress).
            {
                let mut sh = lock_or_fault(&shared, Some(&fault), "playback.shared");
                if voice.stamp == Some(sh.stamp(Chan::Voice)) {
                    sh.played[0] += a.len() as u64;
                }
                if music.stamp == Some(sh.stamp(Chan::Music)) {
                    sh.played[1] += b.len() as u64;
                }
            }
        }

        // Drain the shared writer only at the last natural EOS. Another live
        // channel must continue independently. Direct DACs retain their
        // continuous silence clock and never drain between utterances.
        let finished = |s: &ChanStream| s.live && s.eos && s.pending.is_empty();
        if finished(&voice) || finished(&music) {
            if release_idle
                && (!voice.live || finished(&voice))
                && (!music.live || finished(&music))
            {
                let stamps = execution_stamps(&shared, &voice, &music);
                progress.begin_write(live_now);
                let drained = hw.playback_drain();
                progress.end_write();
                if let Err(error) = drained {
                    let _ = events.send(PlayEvent::WriteFailed {
                        stamps,
                        detail: format!("shared playback drain failed: {error}"),
                    });
                    lock_or_fault(&shared, Some(&fault), "playback.shared").play_live =
                        [false, false];
                    break;
                }
            }
            // D-stage OOM forensics: `live` (allocated - freed) is the
            // program's own retained bytes; rss is what the kernel gave us.
            // They diverge exactly by the allocator's retention, which is the
            // measured cause of the hourly OOM (see docs/d/d-stage-close).
            let (al, fr, live) = crate::alloc_live();
            let rss = rss_kb();
            fwlog!(
                "[mem] play end rss={rss} live={live} alloc={al} free={fr} vcap={}",
                voice.pending.capacity()
            );
            check_rss_budget(rss, "play end");
        }
        for (chan, s) in [(Chan::Voice, &mut voice), (Chan::Music, &mut music)] {
            if s.eos && s.pending.is_empty() && s.live {
                // Only a backend with actual delay telemetry can report
                // consumed frames (a drained shared writer has none).
                let delay_frames = hw.playback_delay_frames();
                let _ = events.send(PlayEvent::StreamDone {
                    stamp: s.stamp.expect("live stream has execution stamp"),
                    chan,
                    bytes: s.bytes_recv,
                    periods: s.periods,
                    underruns: hw.playback_underruns().saturating_sub(s.underruns_at_start),
                    delay_frames,
                });
                *s = ChanStream::new();
            }
        }
        // Loudness look-ahead delay line: when nothing is left to play in
        // either channel the stage still holds the last few ms of the run.
        // Dropping it here (inside the end fade) stops one utterance's tail
        // from reappearing at the head of the next — the old code carried it
        // over, a real if quiet distortion source at every utterance edge.
        //
        // B-M3 (§3.4): only at a TRUE end. Both pending queues are also empty
        // while a live stream waits for its next chunk, and resetting there
        // dropped the delay line in the middle of a sentence (audible as a
        // hole/step whenever the ladder is enabled).
        if loudness.pending() > 0
            && voice.pending.is_empty()
            && music.pending.is_empty()
            && !voice.live
            && !music.live
        {
            loudness.reset_stream();
        }

        // Channel-live truth (D/T5): the barge-in detector arms on the
        // writer's view of "voice is playing" and the keyword guard needs
        // "any playback". Diffed per iteration, written only on change.
        let live_now = [voice.live, music.live];
        if live_now != live_prev {
            let mut sh = lock_or_fault(&shared, Some(&fault), "playback.shared");
            sh.play_live = [
                live_now[0] && voice.stamp == Some(sh.stamp(Chan::Voice)),
                live_now[1] && music.stamp == Some(sh.stamp(Chan::Music)),
            ];
            live_prev = live_now;
        }
    }
}

/// R1j/P1: validate + install a keyword file. The device is the only place
/// that can check tokens against the MODEL's table, and it is the place that
/// must never leave a half-valid file behind (sherpa rejects the whole file
/// when one token is unknown, and the driver still reports ready).
///
/// Dedicated blocking worker thread for base64 decoding and WAV/PCM resampling
/// (#15). Keeps CPU-heavy decode off the single-threaded tokio runtime so
/// `Stop`, watchdog ticks, LED ticks, and the north writer stay responsive.
fn execution_stamps(
    shared: &Mutex<Shared>,
    voice: &ChanStream,
    music: &ChanStream,
) -> [PlayStamp; 2] {
    let sh = lock_or_fault(shared, None, "playback.stamps");
    [
        voice.stamp.unwrap_or_else(|| sh.stamp(Chan::Voice)),
        music.stamp.unwrap_or_else(|| sh.stamp(Chan::Music)),
    ]
}

pub(super) fn decode_thread(role: Role, mut rx: mpsc::Receiver<DecodeCmd>) {
    while let Some(cmd) = rx.blocking_recv() {
        decode_one(&role, cmd);
    }
}

/// A local audio claim may wait for native-engine readback. Keep that work
/// off the northbound runtime and revalidate the channel after the wait.
fn reserve_decoded_play(
    role: &Role,
    task_id: &str,
    chan: Chan,
    stamp: PlayStamp,
    north: &NorthTx,
) -> bool {
    let vendor = {
        let sh = lock_or_fault(&role.shared, Some(&role.fault), "reserve.shared");
        if sh.stamp(chan) != stamp || north.is_terminal() {
            return false;
        }
        sh.vendor.clone()
    };
    let Some(vendor) = vendor else { return true };
    let admitted = vendor.try_home_wake();
    let mut sh = lock_or_fault(&role.shared, Some(&role.fault), "reserve.shared");
    if sh.stamp(chan) != stamp || north.is_terminal() {
        return false;
    }
    if admitted {
        vendor.set_playback_active(true);
        role.hw.set_home_indicator(vendor.indicator_owner());
        return true;
    }
    sh.chan_slot(chan).take();
    lock_or_fault(&role.streams, Some(&role.fault), "reserve.streams").remove(task_id);
    drop(sh);
    north.send_ctrl(GatewayFrame::PlayReceipt {
        task_id: task_id.to_string(),
        stage: "failed".into(),
        detail: Some("native_audio_busy".into()),
        ts: now_ms(),
    });
    false
}

/// Every mutation uses Shared → streams ordering. CPU-heavy whole-file decode
/// happens outside both locks, then the original execution is checked again.
pub(super) fn decode_one(role: &Role, cmd: DecodeCmd) {
    let _activity = role
        .northbound_stats
        .activity(northbound::ActivityKind::Decode);
    match cmd {
        DecodeCmd::ReserveStream {
            task_id,
            chan,
            stamp,
            north,
            detail,
        } => {
            if reserve_decoded_play(role, &task_id, chan, stamp, &north) {
                north.send_ctrl(GatewayFrame::PlayReceipt {
                    task_id,
                    stage: "accepted".into(),
                    detail: Some(detail),
                    ts: now_ms(),
                });
            }
        }
        DecodeCmd::Play {
            task_id,
            chan,
            audio_b64,
            simulated_ms,
            sample_rate,
            channels,
            stamp,
            north,
        } => {
            {
                let sh = lock_or_fault(&role.shared, Some(&role.fault), "decode.shared");
                if sh.stamp(chan) != stamp || north.is_terminal() {
                    return;
                }
            }
            let wav = if let Some(b64) = audio_b64 {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .map_err(|e| format!("bad audio_b64: {e}"))
            } else if let Some(ms) = simulated_ms {
                Ok(silence_wav(ms))
            } else {
                return;
            };
            let decoded = wav.and_then(|wav| {
                resample::wav_to_pcm16(&wav).ok_or_else(|| {
                    format!("wav not parseable (fmt says rate={sample_rate} ch={channels})")
                })
            });
            let mut sh = lock_or_fault(&role.shared, Some(&role.fault), "decode.shared");
            if sh.stamp(chan) != stamp || north.is_terminal() {
                return;
            }
            match decoded {
                Err(detail) => {
                    if sh
                        .chan_slot(chan)
                        .as_ref()
                        .is_some_and(|(task, _)| task == &task_id)
                    {
                        sh.chan_slot(chan).take();
                    }
                    drop(sh);
                    north.send_ctrl(GatewayFrame::PlayReceipt {
                        task_id,
                        stage: "failed".into(),
                        detail: Some(detail),
                        ts: now_ms(),
                    });
                }
                Ok((pcm, expected_ms)) => {
                    drop(sh);
                    if !reserve_decoded_play(role, &task_id, chan, stamp, &north) {
                        return;
                    }
                    north.send_ctrl(GatewayFrame::PlayReceipt {
                        task_id: task_id.clone(),
                        stage: "accepted".into(),
                        detail: None,
                        ts: now_ms(),
                    });
                    let mut sh = lock_or_fault(&role.shared, Some(&role.fault), "decode.shared");
                    if sh.stamp(chan) != stamp || north.is_terminal() {
                        return;
                    }
                    *sh.chan_slot(chan) = Some((task_id.clone(), expected_ms));
                    let _ = role.play_tx.send(PlayCmd {
                        stamp,
                        action: PlayAction::Samples { chan, samples: pcm },
                    });
                    let _ = role.play_tx.send(PlayCmd {
                        stamp,
                        action: PlayAction::Eos(chan),
                    });
                    drop(sh);
                    north.send_ctrl(GatewayFrame::PlayReceipt {
                        task_id,
                        stage: "sent".into(),
                        detail: Some(format!(
                            "~{expected_ms} ms at 48k mono [{}]",
                            if chan == Chan::Voice {
                                "voice"
                            } else {
                                "music"
                            }
                        )),
                        ts: now_ms(),
                    });
                    check_rss_budget(rss_kb(), "decode");
                }
            }
        }
        DecodeCmd::PlayChunk {
            task_id,
            seq,
            audio_b64,
            stamp,
            north,
        } => {
            let ctx = {
                let sh = lock_or_fault(&role.shared, Some(&role.fault), "decode.shared");
                let streams = lock_or_fault(&role.streams, Some(&role.fault), "decode.streams");
                let Some(ctx) = streams.get(&task_id) else {
                    return;
                };
                if ctx.stamp != stamp || sh.stamp(ctx.chan) != stamp || north.is_terminal() {
                    return;
                }
                ctx.clone()
            };
            use base64::Engine;
            // Only the decoder takes this state lock; runtime admission,
            // capture, teardown and watchdog never wait on decode work.
            let mut state = lock_or_fault(&ctx.state, Some(&role.fault), "decode.state");
            let pcm = base64::engine::general_purpose::STANDARD.decode(audio_b64);
            let sequence_bad = seq != state.next_seq;
            let mut failure = None;
            let mut out = Vec::new();
            let first = !state.streaming_receipt_sent;
            let mut decode_error = false;
            match pcm {
                Err(_) => decode_error = true,
                Ok(_) if sequence_bad => decode_error = true,
                Ok(pcm) => {
                    state.next_seq += 1;
                    if pcm.len() % 2 != 0 {
                        failure = Some("stream chunk not whole s16 samples".to_string());
                    } else {
                        let samples: Vec<i16> = pcm
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|c| i16::from_le_bytes([c[0], c[1]]))
                            .collect();
                        out = state.resampler.push(&samples);
                        state.pushed += out.len() as u64;
                        state.streaming_receipt_sent = true;
                    }
                }
            }
            let sh = lock_or_fault(&role.shared, Some(&role.fault), "decode.shared");
            let mut streams = lock_or_fault(&role.streams, Some(&role.fault), "decode.streams");
            if north.is_terminal()
                || sh.stamp(ctx.chan) != stamp
                || !streams
                    .get(&task_id)
                    .is_some_and(|live| Arc::ptr_eq(&live.state, &ctx.state))
            {
                return;
            }
            if decode_error {
                drop(streams);
                drop(sh);
                drop(state);
                north.fail("decode_error");
                return;
            }
            let played = sh.played[if ctx.chan == Chan::Voice { 0 } else { 1 }]
                .saturating_sub(state.played_base);
            let ahead = state.pushed.saturating_sub(played);
            if ahead > role.cfg.stream_ahead {
                failure = Some(format!(
                    "stream_overflow pushed_ms={} played_ms={} ahead_ms={} cap_ms={}",
                    state.pushed * 1000 / PLAY_RATE as u64,
                    played * 1000 / PLAY_RATE as u64,
                    ahead * 1000 / PLAY_RATE as u64,
                    role.cfg.stream_ahead * 1000 / PLAY_RATE as u64
                ));
            }
            if let Some(detail) = failure {
                let removed = streams.remove(&task_id);
                let _ = role.play_tx.send(PlayCmd {
                    stamp,
                    action: PlayAction::Flush(ctx.chan),
                });
                drop(streams);
                drop(sh);
                drop(state);
                drop(removed);
                north.send_ctrl(GatewayFrame::PlayReceipt {
                    task_id,
                    stage: "failed".into(),
                    detail: Some(detail),
                    ts: now_ms(),
                });
            } else {
                if !out.is_empty() {
                    let _ = role.play_tx.send(PlayCmd {
                        stamp,
                        action: PlayAction::Samples {
                            chan: ctx.chan,
                            samples: out,
                        },
                    });
                }
                drop(streams);
                drop(sh);
                drop(state);
                if first {
                    north.send_ctrl(GatewayFrame::PlayReceipt {
                        task_id,
                        stage: "streaming".into(),
                        detail: None,
                        ts: now_ms(),
                    });
                }
            }
        }
        DecodeCmd::PlayEnd { task_id, stamp } => {
            let ctx = {
                let sh = lock_or_fault(&role.shared, Some(&role.fault), "decode.shared");
                let mut streams = lock_or_fault(&role.streams, Some(&role.fault), "decode.streams");
                let Some(ctx) = streams.get(&task_id) else {
                    return;
                };
                if ctx.stamp != stamp
                    || sh.stamp(ctx.chan) != stamp
                    || sh.north_tx.as_ref().is_some_and(|tx| tx.is_terminal())
                {
                    return;
                }
                streams.remove(&task_id).expect("checked stream")
            };
            let mut state = lock_or_fault(&ctx.state, Some(&role.fault), "decode.state");
            let tail = state.resampler.finish();
            let chan = ctx.chan;
            let pushed = state.pushed + tail.len() as u64;
            drop(state);
            let mut sh = lock_or_fault(&role.shared, Some(&role.fault), "decode.shared");
            if sh.stamp(chan) != stamp || sh.north_tx.as_ref().is_some_and(|tx| tx.is_terminal()) {
                return;
            }
            if !tail.is_empty() {
                let _ = role.play_tx.send(PlayCmd {
                    stamp,
                    action: PlayAction::Samples {
                        chan,
                        samples: tail,
                    },
                });
            }
            let _ = role.play_tx.send(PlayCmd {
                stamp,
                action: PlayAction::Eos(chan),
            });
            *sh.chan_slot(chan) = Some((task_id, pushed * 1000 / PLAY_RATE as u64));
        }
    }
}

/// Upper bound on `simulated_ms` in `silence_wav` so a huge value cannot OOM
/// the 512 MB board (#17).
pub const MAX_SIMULATED_MS: u64 = 60_000;

/// 48 kHz silence for simulated plays (mock-library music has no samples).
pub(super) fn silence_wav(ms: u64) -> Vec<u8> {
    let ms = ms.min(MAX_SIMULATED_MS);
    let samples = (ms * PLAY_RATE as u64 / 1000) as usize;
    let mut w = Vec::with_capacity(44 + samples * 2);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + samples as u32 * 2).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&PLAY_RATE.to_le_bytes());
    w.extend_from_slice(&(PLAY_RATE * 2).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(samples as u32 * 2).to_le_bytes());
    w.resize(44 + samples * 2, 0);
    w
}

impl Role {
    pub(super) fn supersede(&self, chan: Chan, reason: &str, direct_tx: &NorthTx) {
        let old = {
            let mut sh = lock_or_fault(&self.shared, Some(&self.fault), "role.shared");
            let old = sh.chan_slot(chan).take();
            if old.is_some() {
                sh.send_play(&self.play_tx, PlayAction::Flush(chan));
            }
            old
        };
        if let Some((task_id, _)) = old {
            let _ = direct_tx.send_ctrl(GatewayFrame::PlayReceipt {
                task_id,
                stage: "unknown".into(),
                detail: Some(reason.into()),
                ts: now_ms(),
            });
        }
    }
}
