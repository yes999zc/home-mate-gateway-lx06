use super::*;

pub fn decode_s24_3le(bytes: &[u8], channels: usize) -> Vec<i32> {
    let mut out = Vec::with_capacity(bytes.len() / 3);
    for f in bytes.chunks_exact(channels * 3) {
        for c in 0..channels {
            let b = &f[c * 3..c * 3 + 3];
            let v = (b[0] as i32) | ((b[1] as i32) << 8) | ((b[2] as i32) << 16);
            out.push((v << 8) >> 8); // sign-extend from bit 23
        }
    }
    out
}

/// Mean of the configured mic channels per frame; ref channel interleaved
/// after it when hw_ref is on (`[mic|ref]` 2ch uplink, v0.2 layout).
pub fn downmix_block(
    raw: &[u8],
    channels: usize,
    mic_channels: &[usize],
    ref_channel: usize,
    hw_ref: bool,
) -> Vec<i16> {
    let decoded = decode_s24_3le(raw, channels);
    let frames = decoded.len() / channels;
    let live: Vec<usize> = mic_channels
        .iter()
        .copied()
        .filter(|&c| c < channels)
        .collect();
    let n = live.len().max(1) as i64;
    // Uplink scale ≈ EchoMuse parity: the 24-bit mean is scaled ×16 over
    // the plain top-16-bits conversion (>>4 of the 24-bit mean, saturating).
    // Calibrated live 2026-11-08: raw top-16 mapping put room noise at
    // −73 dB (VAD −42/−47 deaf); full-range mapping hit −19 dB. >>4 lands
    // the noise floor back at ≈ −50 dBFS where the G4 calibration lives.
    const MIC_GAIN_SHIFT: u32 = 4;
    let sat = |v: i64| (v >> MIC_GAIN_SHIFT).clamp(i16::MIN as i64, i16::MAX as i64) as i16;
    let mut out = Vec::with_capacity(if hw_ref { frames * 2 } else { frames });
    for f in 0..frames {
        let base = f * channels;
        let acc: i64 = live.iter().map(|&c| decoded[base + c] as i64).sum();
        let mix = sat(acc / n);
        if hw_ref {
            let r = if ref_channel < channels {
                sat(decoded[base + ref_channel] as i64)
            } else {
                0
            };
            out.push(mix);
            out.push(r);
        } else {
            out.push(mix);
        }
    }
    out
}

/// The sidecar may return immediately once fed, so publication is a
/// sequencing boundary shared by the production worker and barrier tests.
#[allow(clippy::too_many_arguments)]
fn publish_before_kws<F: FnOnce()>(
    nearend: &Mutex<nearend::NearEndEvaluator>,
    block: &[i16],
    playing: bool,
    locked: bool,
    corr: Option<f32>,
    fired: bool,
    mono_ms: u64,
    wall_ms: u64,
    feed: F,
) -> nearend::EvalOutput {
    let out = nearend
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .evaluate(block, playing, locked, corr, fired, mono_ms, wall_ms);
    feed();
    out
}

const EPIPE_IMMEDIATE_RETRIES: u32 = 3;
const EPIPE_BACKOFF_MS: u64 = 15;
const EPIPE_LOG_INTERVAL_MS: u64 = 1_000;

/// Returns `true` if `err` is an ALSA overrun (`EPIPE`) or suspended stream (`ESTRPIPE`),
/// for which `alsa::readi` already called `prepare_and_start()`.
pub(super) fn is_xrun_error(err: &std::io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(code) if code == libc::EPIPE || code == alsa::ESTRPIPE
    )
}

/// Bounded backoff + log rate-limiter for consecutive `EPIPE`/`ESTRPIPE` errors (#59).
///
/// The first [`EPIPE_IMMEDIATE_RETRIES`] consecutive xruns retry with zero sleep so
/// healthy ring-buffer recovery stays immediate; persistent xruns back off to
/// [`EPIPE_BACKOFF_MS`] and rate-limit `fwlog!` to once per [`EPIPE_LOG_INTERVAL_MS`].
#[derive(Default)]
pub(super) struct XrunBackoff {
    consecutive: u32,
    last_log_ms: Option<u64>,
    suppressed_logs: u64,
}

impl XrunBackoff {
    pub(super) fn reset(&mut self) {
        self.consecutive = 0;
        self.suppressed_logs = 0;
    }

    /// Record one capture error and return `(sleep_duration, suppressed_before_this_log)`.
    /// `None` for `suppressed_before_this_log` means this log line should be suppressed.
    pub(super) fn on_error(
        &mut self,
        err: &std::io::Error,
        now_ms: u64,
    ) -> (Option<std::time::Duration>, Option<u64>) {
        if !is_xrun_error(err) {
            self.reset();
            return (Some(std::time::Duration::from_millis(200)), Some(0));
        }
        self.consecutive = self.consecutive.saturating_add(1);
        let sleep = (self.consecutive > EPIPE_IMMEDIATE_RETRIES)
            .then(|| std::time::Duration::from_millis(EPIPE_BACKOFF_MS));
        let should_log = self
            .last_log_ms
            .is_none_or(|prev| now_ms.saturating_sub(prev) >= EPIPE_LOG_INTERVAL_MS);
        if should_log {
            self.last_log_ms = Some(now_ms);
            let suppressed = std::mem::take(&mut self.suppressed_logs);
            (sleep, Some(suppressed))
        } else {
            self.suppressed_logs = self.suppressed_logs.saturating_add(1);
            (sleep, None)
        }
    }
}

/// Capture thread: one 80 ms blocking read per uplink block. Always reading
/// keeps the stream warm and paced even with the mic session off.
#[allow(clippy::too_many_arguments)]
pub(super) fn capture_thread(
    hw: Arc<dyn Hardware>,
    shared: Arc<Mutex<Shared>>,
    cfg: FirmwareConfig,
    seq: Arc<AtomicU64>,
    mut pipeline: Option<aec::Pipeline>,
    loopback_probe: bool,
    mut probe: Option<ProbeSink>,
    mut probe_mics: Option<ProbeSink>,
    kws_feed: Option<kws::KwsFeed>,
    barge: Option<Arc<std::sync::Mutex<bargein::Detector>>>,
    nearend: Arc<std::sync::Mutex<nearend::NearEndEvaluator>>,
    boot: Option<boot_health::BootSentinel>,
    ok_ms: Arc<AtomicU64>,
) {
    pin_audio_thread("capture/aec");
    let mut boot = boot;
    let frame_bytes = UPLINK_FRAMES as usize * CAP_CHANNELS * 3;
    let mut buf = vec![0u8; frame_bytes];
    // AEC scratch: the decoded 9ch block (established >>4 scale) and the
    // fused output. In measure mode a second fusion tracks the pre-AEC
    // signal for the ERLE dual-track (acceptance doc §0).
    let mut decoded: Vec<[i16; 9]> = Vec::with_capacity(UPLINK_FRAMES as usize);
    let mut fused = vec![0i16; UPLINK_FRAMES as usize];
    let mut pre_aec = pipeline.is_some().then(aec::fusion::ArrayMixer::new);
    let mut pre_out = vec![0i16; UPLINK_FRAMES as usize];
    let mut measure_files: Option<MeasureTracks> = if cfg.aec_measure && cfg.aec {
        MeasureTracks::open(&cfg.state_dir, cfg.aec_measure_secs)
    } else {
        None
    };
    let mut stats_at = std::time::Instant::now();
    let mut probe_epoch = 0u64;
    // R1 self-hearing: count 80 ms blocks with at least one degraded 20 ms
    // reference frame. This is the honest "reference was not fully locked"
    // signal the detector consumes (the old code sampled the lock state
    // after the block and could miss a loss+relock inside it).
    let mut degraded_ref_blocks: u64 = 0;
    // D/T5 calibration telemetry: voice-live rising edge (mono ms) and the
    // last stat-line stamp. Only used when --barge-in-stats is on.
    let mut barge_voice_since: Option<u64> = None;
    let mut barge_stat_at: u64 = 0;
    // R1 self-hearing: the near-end guard must see the SAME array-processed
    // pre-AEC signal the uplink is built from, not a naive 7-mic average.
    // The average comb-filters the echo across the ring and decorrelates it
    // from the DAC reference (on-device 2026-11-12: echo windows read
    // corr 0.1-0.4 with the average, 0.4-0.9 with the fusion) — which is
    // precisely the "echo looks like a person" failure. Stateful across
    // blocks (33-tap crossover + 1 ms centre alignment).
    let mut guard_mix = barge.is_some().then(aec::fusion::ArrayMixer::new);
    // A measured per-unit mic adjustment affects only mono PCM sent to Core.
    // Keep the spotter, raw arbitration level and echo diagnostics on their calibrated input.
    let uplink_gain = if cfg.profile.model == "LX06" && !cfg.hw_ref {
        std::env::var("HM_LX06_UPLINK_GAIN_DB")
            .ok()
            .and_then(|s| s.parse::<f32>().ok())
            .filter(|db| (0.0..=6.0).contains(db))
            .map_or(1.0, |db| 10.0_f32.powf(db / 20.0))
    } else {
        1.0
    };
    if uplink_gain > 1.0 {
        fwlog!("[capture] LX06 idle mono uplink gain={uplink_gain:.3}");
    }
    let mut xrun_backoff = XrunBackoff::default();
    loop {
        if let Err(e) = super::capture_read::read_block(&mut buf, UPLINK_FRAMES, |bytes, frames| {
            hw.capture_read(bytes, frames)
        }) {
            let (sleep_dur, log_suppressed) = xrun_backoff.on_error(&e, kws::mono_ms());
            if let Some(suppressed) = log_suppressed {
                if suppressed > 0 {
                    fwlog!("[firmware] capture read failed: {e} ({suppressed} repeated xrun logs suppressed)");
                } else {
                    fwlog!("[firmware] capture read failed: {e}");
                }
            }
            let playing = {
                let sh = shared.lock().unwrap_or_else(|e| e.into_inner());
                sh.play_live[0] || sh.play_live[1]
            };
            // No old buffer, PCM, KWS feed or healthy heartbeat survives a
            // discontinuity. Invalidate reference confidence before retry.
            nearend
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .update_echo_health(false, playing, kws::mono_ms());
            if let Some(pipe) = pipeline.as_ref() {
                pipe.invalidate_capture_alignment();
            }
            if let Some(d) = sleep_dur {
                std::thread::sleep(d);
            }
            continue;
        }
        xrun_backoff.reset();
        // M4 capture heartbeat: only a REAL block counts. A panicked thread
        // or a read that never returns leaves this stale and the pump turns
        // it into an audio fault — the old code retried forever and the box
        // stayed up deaf.
        ok_ms.store(kws::mono_ms(), std::sync::atomic::Ordering::Relaxed);
        let (on, muted, north_tx, epoch, play_any, vendor) = {
            let sh = shared.lock().unwrap_or_else(|e| e.into_inner());
            (
                sh.mic_on,
                sh.muted,
                sh.north_tx.clone(),
                sh.play_epoch,
                sh.play_live[0] || sh.play_live[1],
                sh.vendor.clone(),
            )
        };
        if epoch != probe_epoch {
            if loopback_probe {
                if probe_armed() {
                    probe = ProbeSink::open("capture.pcm");
                    probe_mics = ProbeSink::open("mics.pcm");
                } else {
                    probe = None;
                    probe_mics = None;
                }
            }
            probe_epoch = epoch;
        }
        // Hardware-level privacy (onepager §3-3): muted = no uplink at all,
        // not a software flag somewhere upstream. Capture keeps draining so
        // the stream stays healthy; the frames simply never leave.
        if muted {
            continue;
        }
        // #18: when `north_tx` is None or `mic_on` is false, keep running
        // `pipe.process(...)` so AEC and dealias stay locked and warm across
        // link/mic-session gaps, then skip the uplink send below.
        let capture_allowed = || vendor.as_ref().is_none_or(|v| v.capture_allowed());
        let uplink_active = north_tx.is_some() && on && capture_allowed();
        // Barge-in guard inputs (D/T5), filled by the AEC branch: the
        // PRE-AEC mic downmix and the cleaned DAC reference for this
        // block. Echo is what the canceller removes — correlating the
        // pre-AEC mix against the reference is what separates "the
        // speakers" from "a person".
        let mut barge_ctx: Option<(Vec<i16>, Vec<i16>, bool)> = None;
        let mut block_ref_locked = false;
        let mut block = if let Some(pipe) = pipeline.as_mut() {
            decode_s24_block(&buf, &mut decoded);
            if uplink_active {
                // Loopback self-check (task B): ch8 is the DAC loopback the AEC
                // reference is aligned against — writing it here (and the DAC
                // side in playback_thread) turns the G6 "ch8 cross-boot silent"
                // question into an observable instead of a memory.
                if let Some(p) = probe.as_mut() {
                    let ch8: Vec<i16> = decoded.iter().map(|s| s[cfg.ref_channel]).collect();
                    p.append(&ch8);
                }
                // 7-mic evaluation dump (task C): the same window as a 9ch
                // interleaved track, so per-mic level and arrival-time
                // differences can be measured off-device/in-core.
                if let Some(m) = probe_mics.as_mut() {
                    let mut bytes = Vec::with_capacity(decoded.len() * 9 * 2);
                    for s in &decoded {
                        for sample in s.iter().take(9) {
                            bytes.extend_from_slice(&sample.to_le_bytes());
                        }
                    }
                    m.append_bytes(&bytes);
                }
            }
            let (cleaned_ref, ref_lock_all) = pipe.process(&decoded, &buf, &mut fused);
            block_ref_locked = ref_lock_all;
            if !ref_lock_all {
                degraded_ref_blocks += 1;
            }
            if uplink_active {
                // D/T5 debt #1: boot-time capture health. Only observed while
                // the mic is ON and NOT muted — a muted device legitimately
                // reads all zeros, which would be a false "mic dead" verdict.
                // The sentinel is clock-free (P0 discipline): pure block counts.
                if let Some(sentinel) = boot.as_mut() {
                    if let Some(report) = sentinel.observe(&decoded, &cleaned_ref, play_any) {
                        fwlog!("{}", report.as_log_line());
                        if report.verdict.is_failure() {
                            fwlog!(
                                "[boot-health] FAILURE — see tools/ch8_repro.py for the reboot matrix"
                            );
                        }
                    }
                }
                if let Some(gmix) = guard_mix.as_mut() {
                    // Pre-AEC array fusion (same 33-tap crossover as the uplink
                    // path, but on the RAW seven mics): the echo stays intact
                    // and stays correlated with the reference, which is what
                    // the guard needs. The naive average is gone.
                    let mut pre_mic = vec![0i16; decoded.len()];
                    for f in 0..decoded.len() / aec::FRAME {
                        let chans: Vec<Vec<i16>> = (0..aec::MIC_CHANNELS)
                            .map(|c| {
                                decoded[f * aec::FRAME..(f + 1) * aec::FRAME]
                                    .iter()
                                    .map(|s| s[c])
                                    .collect()
                            })
                            .collect();
                        let refs: Vec<&[i16]> = chans.iter().map(|c| c.as_slice()).collect();
                        let out = &mut pre_mic[f * aec::FRAME..(f + 1) * aec::FRAME];
                        gmix.process(&refs, out);
                    }
                    barge_ctx = Some((pre_mic, cleaned_ref.clone(), ref_lock_all));
                }
                if let (Some(pre), Some(m)) = (pre_aec.as_mut(), measure_files.as_mut()) {
                    let chans: Vec<Vec<i16>> = (0..aec::MIC_CHANNELS)
                        .map(|c| decoded.iter().map(|s| s[c]).collect())
                        .collect();
                    let refs: Vec<&[i16]> = chans.iter().map(|c| c.as_slice()).collect();
                    pre.process(&refs, &mut pre_out);
                    m.append(&pre_out, &fused, &cleaned_ref);
                }
            }
            if stats_at.elapsed() >= std::time::Duration::from_secs(30) {
                stats_at = std::time::Instant::now();
                let st = pipe.dealias_stats();
                fwlog!(
                    "[aec] dealias locked={} ever={} relocks={} losses={} backoffs={} degraded_blocks={}",
                    st.locked, st.ever_locked, st.relocks, st.losses, st.backoffs, degraded_ref_blocks
                );
            }
            if !uplink_active {
                continue;
            }
            fused.clone()
        } else {
            if !uplink_active {
                continue;
            }
            downmix_block(
                &buf,
                CAP_CHANNELS,
                &cfg.mic_channels,
                cfg.ref_channel,
                cfg.hw_ref,
            )
        };
        let Some(tx) = north_tx else { continue };
        // v0.2.9 (wake arbitration): remember how loud this block was, over
        // the SAME samples the spotter just received. Cheap (one pass over 80
        // ms of i16) and it runs whether or not a spotter is running, so the
        // window is populated from boot rather than from the first hit.
        {
            let dbfs = block_dbfs(&block);
            let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
            sh.mic_level.push(dbfs);
        }
        // Semantic barge-in (D/T5) + single acoustic evaluator (W1):
        // update echo-path health first so both barge-in and KWS guard
        // share the same fail-closed truth.
        let (voice_live, play_any) = {
            let sh = shared.lock().unwrap_or_else(|e| e.into_inner());
            (sh.play_live[0], sh.play_live[0] || sh.play_live[1])
        };
        let now_mono = kws::mono_ms();
        let (echo_health, _) = nearend
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .update_echo_health(block_ref_locked, play_any, now_mono);
        let mut barge_fired = false;
        let mut raw_corr: Option<f32> = None;
        if let (Some(det), Some((pre_mic, ref_blk, ref_lock_all))) =
            (barge.as_ref(), barge_ctx.as_ref())
        {
            let now = now_mono;
            if voice_live {
                barge_voice_since.get_or_insert(now);
            } else {
                barge_voice_since = None;
            }
            // The guard's correlation is only meaningful against a fully
            // verified rebuild of the DAC reference. This is the BLOCK
            // state: `ref_lock_all` is false if ANY 20 ms frame in this
            // 80 ms block lost its match, even if a later frame relocked.
            // The old code read the live lock after the block and could
            // miss exactly that loss+relock (candidate B, 2026-11-12).
            let ref_locked = *ref_lock_all;
            let mut det = det.lock().unwrap();
            det.set_ref_locked(ref_locked);
            det.set_echo_health(echo_health);
            // Calibration stat line (opt-in): the full corr/energy
            // distribution while voice plays, not just the onset samples
            // the guard/fire lines carry. `voice_ms` lets the analysis
            // split the TTS-onset transient from steady-state echo.
            if cfg.barge_in_stats > 0
                && voice_live
                && now.saturating_sub(barge_stat_at) >= cfg.barge_in_stats
            {
                barge_stat_at = now;
                let corr = det.guard_now(now);
                let voice_ms = now.saturating_sub(barge_voice_since.unwrap_or(now));
                fwlog!(
                    "[barge-in] stat corr={corr:.3} energy={:.1} ref={:.1} voice_ms={voice_ms} settle={} lock={}",
                    bargein::level_db(&block),
                    bargein::level_db(ref_blk),
                    if voice_ms < cfg.barge_in_settle { 1 } else { 0 },
                    if ref_locked { 1 } else { 0 }
                );
            }
            match det.process(
                &block,
                pre_mic,
                ref_blk,
                cfg.barge_in && voice_live,
                play_any,
                now,
            ) {
                bargein::Outcome::None => {}
                bargein::Outcome::GuardSuppress { corr, energy_db } => {
                    fwlog!(
                        "[barge-in] guard held: echo-correlated window corr={corr:.3} ({energy_db:.1} dBFS)"
                    );
                }
                bargein::Outcome::Settle {
                    corr,
                    energy_db,
                    voice_ms,
                } => {
                    fwlog!(
                        "[barge-in] settle: playback just started (voice_ms={voice_ms}) corr={corr:.3} ({energy_db:.1} dBFS) — not a person"
                    );
                }
                bargein::Outcome::RefUnlocked { voice_ms } => {
                    fwlog!(
                        "[barge-in] ref degraded: candidate at voice_ms={voice_ms} suppressed — at least one 20 ms frame lost its ring match, so neither correlation nor residual is trustworthy"
                    );
                }
                bargein::Outcome::NotConverged {
                    energy_db,
                    ref_db,
                    residual_db,
                    voice_ms,
                } => {
                    fwlog!(
                        "[barge-in] not converged: candidate at voice_ms={voice_ms} energy={energy_db:.1} ref={ref_db:.1} residual={residual_db:.1} dB — the AEC has not shown >=15 dB suppression on this episode"
                    );
                }
                bargein::Outcome::Fire {
                    preroll,
                    corr,
                    energy_db,
                    ref_db,
                    residual_db,
                } => {
                    barge_fired = true;
                    raw_corr = Some(corr);
                    // D 1.1 forensics: the reference level next to the mic
                    // level separates "the echo is loud here" (residual) from
                    // "someone is louder than the echo" (a person).
                    use base64::Engine;
                    // R1d: cut the audio HERE, in the capture thread, before
                    // the fact goes north. The fact needs the core (semantic
                    // "does this count as a turn"); the STOP does not — the
                    // same detector block already proved this is a person,
                    // and waiting cost 3.078 s + 1.9 s on 2026-11-12.
                    //
                    // The gates mirror the core's own acceptance so the audio
                    // cannot be cut where the core would refuse to explain it:
                    // interface support (ref_locked + residual margin), the
                    // answer protection period, privacy mute, and one stop
                    // per episode.
                    let voice_ms = now.saturating_sub(barge_voice_since.unwrap_or(now));
                    let local = {
                        let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
                        let trusted = *ref_lock_all && residual_db > -15.0;
                        let task = sh.active[0].as_ref().map(|(t, _)| t.clone());
                        let fresh = voice_ms >= cfg.barge_in_protect;
                        let duplex_full = sh.duplex_effective == "full";
                        let allowed = local_stop_allowed(
                            sh.play_live[0],
                            duplex_full,
                            sh.muted,
                            trusted,
                            voice_ms,
                            cfg.barge_in_protect,
                            sh.local_stop.is_some(),
                        );
                        if !allowed && !fresh {
                            if let Some(t) = task.as_ref() {
                                fwlog!(
                                    "[barge-in] local stop withheld: task={t} voice_ms={voice_ms} < protect {}",
                                    cfg.barge_in_protect
                                );
                            }
                        }
                        if !duplex_full && task.is_some() {
                            // R1g: the reflex is a full-duplex ability. The
                            // fact still goes north so the core can count it;
                            // the audio does not move.
                            fwlog!(
                                "[barge-in] local stop withheld: half duplex routes interruption through the wake word (residual={residual_db:.1} dB)"
                            );
                        }
                        match (allowed, task) {
                            (true, Some(task)) => {
                                sh.local_stop = Some((task.clone(), now_ms()));
                                if let Some(tx) = sh.play_tx.as_ref() {
                                    sh.send_play(tx, PlayAction::Flush(Chan::Voice));
                                }
                                Some(task)
                            }
                            _ => None,
                        }
                    };
                    if let Some(task) = &local {
                        fwlog!(
                            "[barge-in] LOCAL STOP: flushed voice task={task} voice_ms={voice_ms} residual={residual_db:.1} dB (reflex, no core round trip)"
                        );
                    }
                    let pcm_b64 =
                        base64::engine::general_purpose::STANDARD.encode(pcm_to_bytes(&preroll));
                    fwlog!(
                        "[barge-in] near-end speech: corr={corr:.3} energy={energy_db:.1} ref={ref_db:.1} residual={residual_db:.1} dBFS lock={} preroll={} samples",
                        if ref_locked { 1 } else { 0 },
                        preroll.len()
                    );
                    if !capture_allowed() {
                        continue;
                    }
                    tx.send_ctrl(GatewayFrame::BargeIn {
                        device: cfg.device_id.clone(),
                        ts: now_ms(),
                        seq: seq.fetch_add(1, Ordering::Relaxed) + 1,
                        pcm_b64,
                        sample_rate: CAP_RATE,
                        channels: 1,
                        corr,
                        energy_db,
                        ref_db,
                        residual_db,
                        ref_locked: *ref_lock_all,
                    });
                }
            }
            if raw_corr.is_none() && play_any {
                raw_corr = Some(det.counters.last_corr);
            }
        }
        // Publish current-block health/correlation before exposing its PCM
        // to the asynchronous sidecar reader. A hit cannot race old Healthy.
        if !capture_allowed() {
            continue;
        }
        let eval_out = publish_before_kws(
            &nearend,
            &block,
            play_any,
            block_ref_locked,
            raw_corr,
            barge_fired,
            now_mono,
            now_ms(),
            || {
                if let Some(feed) = &kws_feed {
                    for slice in block.chunks(KWS_SLICE) {
                        feed.feed(slice);
                    }
                }
            },
        );
        if let Some(frame) = eval_out.echo_health_frame {
            tx.send_ctrl(frame);
        }
        if let Some(frame) = eval_out.presence_frame {
            tx.send_ctrl(frame);
        }
        // v1 mechanism (#13): re-check the latch AFTER processing, BEFORE
        // the send. The top-of-loop check (post-read) already discards a
        // block captured across a mute flip; this closes the remaining
        // milliseconds of the processing window, so silence starts no later
        // than the current block's edge.
        if !capture_allowed() || shared.lock().unwrap_or_else(|e| e.into_inner()).muted {
            continue;
        }
        use base64::Engine;
        let vad_speech = kws_feed.as_ref().and_then(|f| f.vad_speech());
        if uplink_gain > 1.0 && !play_any && vad_speech.is_none() && vendor.is_some() {
            boost_quiet_uplink(&mut block, uplink_gain);
        }
        let frame = GatewayFrame::AudioChunk {
            device: cfg.device_id.clone(),
            ts: now_ms(),
            seq: seq.fetch_add(1, Ordering::Relaxed) + 1,
            pcm_b64: base64::engine::general_purpose::STANDARD.encode(pcm_to_bytes(&block)),
            format: "s16le".into(),
            sample_rate: CAP_RATE,
            channels: if cfg.hw_ref { 2 } else { 1 },
            layout: if cfg.hw_ref {
                "mic_ref".into()
            } else {
                "mono".into()
            },
            vad_speech,
            vad_prob: None,
        };
        tx.try_send_audio(frame);
    }
}

/// Decode one S24_3LE interleaved block into `out` as per-sample [i16; 9]
/// rows. TWO scales, and the split is the point: mic channels 0..6 use the
/// established `>>4` uplink scale (G4/G5-2 calibration basis), while the
/// REFERENCE channel 8 uses `>>8` — ch8 is the S16 output shifted into 24
/// bits (dealias.go), so >>8 recovers the exact S16 the DAC received and
/// the sample-exact match can lock. A single >>4 for everything ran the
/// reference 16x hot and clamped it: no lock, immediate backoff (measured
/// on-device, 2026-11-08).
pub(super) fn decode_s24_block(bytes: &[u8], out: &mut Vec<[i16; 9]>) {
    out.clear();
    for frame in bytes.as_chunks::<{ CAP_CHANNELS * 3 }>().0 {
        let mut row = [0i16; 9];
        for (ch, s) in frame.as_chunks::<3>().0.iter().enumerate() {
            // Sign-extend from bit 23 FIRST (the validated decode_s24_3le
            // construction — `(v << 8) >> 8`). Without it every negative
            // sample misreads as +2^24-ish and, on the >>4 mic scale,
            // clamps to +32767: half the ambient samples railed, the pre
            // track measured +15885 mean DC in a quiet room (2026-11-08,
            // measure-track evidence). With the extension: mics decode on
            // the calibrated >>4 scale, and the REFERENCE channel's >>8
            // recovers the exact S16 the DAC received (ch8 = w<<8).
            let u = (s[0] as i32) | ((s[1] as i32) << 8) | ((s[2] as i32) << 16);
            let v = (u << 8) >> 8;
            row[ch] = if ch == aec::REF_CHANNEL {
                (v >> 8) as i16
            } else {
                (v >> 4).clamp(-32768, 32767) as i16
            };
        }
        out.push(row);
    }
}

/// ERLE dual-track sink (acceptance §0): pre-AEC / post-AEC / reference as
/// raw 16kHz S16 mono, bounded to 120s per track then stopped.
pub(super) struct MeasureTracks {
    pre: Option<std::fs::File>,
    post: Option<std::fs::File>,
    r: Option<std::fs::File>,
    written: u64,
    cap: u64,
}

impl MeasureTracks {
    fn open(dir: &std::path::Path, secs: u64) -> Option<MeasureTracks> {
        let d = dir.join("aec-measure");
        std::fs::create_dir_all(&d).ok()?;
        let mk = |name: &str| std::fs::File::create(d.join(name)).ok();
        fwlog!("[aec] measure tracks open under {}", d.display());
        Some(MeasureTracks {
            pre: mk("pre.pcm"),
            post: mk("post.pcm"),
            r: mk("ref.pcm"),
            written: 0,
            cap: CAP_RATE as u64 * secs.max(1),
        })
    }

    fn append(&mut self, pre: &[i16], post: &[i16], r: &[i16]) {
        use std::io::Write;
        if self.written >= self.cap {
            return; // cap reached: files stay complete for offline ERLE
        }
        let mut bytes = Vec::with_capacity(pre.len() * 2);
        for s in pre {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        let _ = self.pre.as_mut().map(|f| f.write_all(&bytes));
        bytes.clear();
        for s in post {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        let _ = self.post.as_mut().map(|f| f.write_all(&bytes));
        bytes.clear();
        for s in r {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        let _ = self.r.as_mut().map(|f| f.write_all(&bytes));
        self.written += pre.len() as u64;
        if self.written >= self.cap {
            fwlog!(
                "[aec] measure capture complete ({}s), files closed",
                self.cap / CAP_RATE as u64
            );
            self.pre = None;
            self.post = None;
            self.r = None;
        }
    }
}

fn boost_quiet_uplink(block: &mut [i16], max_gain: f32) {
    let peak = block.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
    let gain = (f32::from(i16::MAX) / f32::from(peak.max(1))).clamp(1.0, max_gain);
    for s in block {
        *s = (f32::from(*s) * gain).round() as i16;
    }
}

pub(super) fn pcm_to_bytes(samples: &[i16]) -> Vec<u8> {
    let mut v = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

#[cfg(test)]
mod publication_tests {
    #[test]
    fn quiet_uplink_boost_preserves_silence_and_full_scale() {
        let mut speech = [0, -500, 1000];
        super::boost_quiet_uplink(&mut speech, 2.0);
        assert_eq!(speech, [0, -1000, 2000]);
        let mut loud = [-24_000, 12_000, 0];
        super::boost_quiet_uplink(&mut loud, 2.0);
        assert_eq!(loud, [-32767, 16384, 0]);
        let mut extremes = [i16::MIN, i16::MAX];
        super::boost_quiet_uplink(&mut extremes, 2.0);
        assert_eq!(extremes, [i16::MIN, i16::MAX]);
    }

    use super::*;

    #[test]
    fn immediate_sidecar_hit_observes_current_degraded_reference() {
        let state = Arc::new(Mutex::new(nearend::NearEndEvaluator::new(
            "test", true, true,
        )));
        state.lock().unwrap().update_echo_health(true, true, 0);
        publish_before_kws(
            &state,
            &[100; 1280],
            true,
            false,
            Some(0.0),
            false,
            80,
            80,
            || {
                assert_eq!(
                    state.lock().unwrap().last_assessment().echo_health,
                    crate::protocol::EchoPathHealth::Degraded
                );
                assert!(nearend::kws_guard_should_suppress(
                    true, &state, None, 0.5, 80
                ));
            },
        );
    }

    #[test]
    fn epipe_backoff_retries_immediately_then_bounds_sleep_and_rate_limits_logs() {
        let epipe = std::io::Error::from_raw_os_error(libc::EPIPE);
        let estrpipe = std::io::Error::from_raw_os_error(alsa::ESTRPIPE);
        let eio = std::io::Error::from_raw_os_error(libc::EIO);

        let mut b = XrunBackoff::default();
        // First 3 xruns: 0ms sleep; only first logs within 1000ms window.
        assert_eq!(b.on_error(&epipe, 100), (None, Some(0)));
        assert_eq!(b.on_error(&estrpipe, 105), (None, None));
        assert_eq!(b.on_error(&epipe, 110), (None, None));
        // 4th+ consecutive xrun: bounded 15ms sleep; log still suppressed until +1000ms.
        assert_eq!(
            b.on_error(&epipe, 115),
            (
                Some(std::time::Duration::from_millis(EPIPE_BACKOFF_MS)),
                None
            )
        );
        // After 1000ms, log fires once with suppressed count = 3.
        assert_eq!(
            b.on_error(&epipe, 1100),
            (
                Some(std::time::Duration::from_millis(EPIPE_BACKOFF_MS)),
                Some(3)
            )
        );
        // Reset restores immediate retries.
        b.reset();
        assert_eq!(b.on_error(&epipe, 1120), (None, None));
        // Non-xrun error sleeps 200ms and resets consecutive counter.
        assert_eq!(
            b.on_error(&eio, 1125),
            (Some(std::time::Duration::from_millis(200)), Some(0))
        );
    }
}
