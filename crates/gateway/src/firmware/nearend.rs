//! Single acoustic near-end & echo-path health evaluator (W1).
//!
//! Unifies noise-floor estimation, SNR, clipping detection, `EchoPathHealth`,
//! and fail-closed `guard_corr` gating so `capture.rs`, `bargein.rs`, and the
//! `role.rs` KWS guard consume a single acoustic truth.

use std::sync::{Arc, Mutex};

pub use crate::protocol::EchoPathHealth;
use crate::protocol::{GatewayFrame, PROFILE_VERSION};

use super::bargein;

const DEFAULT_NOISE_FLOOR_DBFS: f32 = -65.0;
const MIN_DBFS: f32 = -120.0;
const NOISE_FLOOR_EMA_ALPHA: f32 = 0.05;
const IDLE_PRESENCE_SNR_DB: f32 = 10.0;
const IDLE_PRESENCE_MIN_DBFS: f32 = -55.0;
const IDLE_CALIBRATE_CEILING_DBFS: f32 = -40.0;
// Require 240 ms of non-speech evidence; an arbitrary first block cannot
// establish the room floor. Speech uses the conservative prior meanwhile.
const QUIET_ACQUISITION_BLOCKS: u32 = 3;
const SUSTAINED_DEGRADED_INTERVAL_MS: u64 = 1000;
const BLOCK_MS: u64 = 80;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NearEndAssessment {
    pub present: bool,
    pub confidence: f32,
    pub rms_dbfs: f32,
    pub noise_floor_dbfs: f32,
    pub snr_db: f32,
    pub clipped: bool,
    pub echo_health: EchoPathHealth,
    pub noise_floor_initialized: bool,
    pub reason: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvalOutput {
    pub assessment: NearEndAssessment,
    pub echo_health_frame: Option<GatewayFrame>,
    pub presence_frame: Option<GatewayFrame>,
}

pub struct NearEndEvaluator {
    device_id: String,
    aec_enabled: bool,
    loopback_only: bool,
    noise_floor_dbfs: f32,
    noise_floor_initialized: bool,
    quiet_acquisition_blocks: u32,
    quiet_acquisition_min_dbfs: f32,
    echo_health: EchoPathHealth,
    prev_emitted_health: Option<EchoPathHealth>,
    degraded_since_ms: Option<u64>,
    last_degraded_emit_ms: u64,
    last_corr: f32,
    last_corr_ms: u64,
    prev_present: bool,
    speech_since_ms: Option<u64>,
    last_presence_emit_ms: Option<u64>,
    last_assessment: NearEndAssessment,
}

impl NearEndEvaluator {
    pub fn new(device_id: impl Into<String>, aec_enabled: bool, loopback_only: bool) -> Self {
        let initial_health = EchoPathHealth::Unknown;
        Self {
            device_id: device_id.into(),
            aec_enabled,
            loopback_only,
            noise_floor_dbfs: DEFAULT_NOISE_FLOOR_DBFS,
            noise_floor_initialized: false,
            quiet_acquisition_blocks: 0,
            quiet_acquisition_min_dbfs: 0.0,
            echo_health: initial_health,
            prev_emitted_health: None,
            degraded_since_ms: None,
            last_degraded_emit_ms: 0,
            last_corr: 0.0,
            last_corr_ms: 0,
            prev_present: false,
            speech_since_ms: None,
            last_presence_emit_ms: None,
            last_assessment: NearEndAssessment {
                present: false,
                confidence: 0.0,
                rms_dbfs: MIN_DBFS,
                noise_floor_dbfs: DEFAULT_NOISE_FLOOR_DBFS,
                snr_db: 0.0,
                clipped: false,
                echo_health: initial_health,
                noise_floor_initialized: false,
                reason: "init",
            },
        }
    }

    pub fn echo_health(&self) -> EchoPathHealth {
        self.echo_health
    }

    pub fn noise_floor_dbfs(&self) -> f32 {
        self.noise_floor_dbfs
    }

    pub fn last_assessment(&self) -> NearEndAssessment {
        self.last_assessment
    }

    /// Update echo-path health from AEC/dealias lock state and playback state.
    pub fn update_echo_health(
        &mut self,
        ref_locked: bool,
        playing: bool,
        now_mono_ms: u64,
    ) -> (EchoPathHealth, &'static str) {
        let (health, reason) = if !self.aec_enabled {
            (EchoPathHealth::Unknown, "aec_disabled")
        } else if playing {
            if ref_locked {
                (EchoPathHealth::Healthy, "ref_locked")
            } else {
                (EchoPathHealth::Degraded, "ref_unlocked")
            }
        } else if ref_locked {
            (EchoPathHealth::Healthy, "idle_locked")
        } else {
            (EchoPathHealth::Unknown, "idle_unlocked")
        };
        self.echo_health = health;
        if health == EchoPathHealth::Degraded {
            self.degraded_since_ms.get_or_insert(now_mono_ms);
        } else {
            self.degraded_since_ms = None;
            self.last_degraded_emit_ms = 0;
        }
        (health, reason)
    }

    /// Unified guard correlation: fail-closed (`1.0`) when playback is active
    /// and echo-path health is `Degraded` or `Unknown`.
    pub fn guard_corr(&self, playing: bool, raw_corr: f32) -> f32 {
        if !playing {
            return 0.0;
        }
        if self.echo_health != EchoPathHealth::Healthy {
            return 1.0;
        }
        raw_corr.clamp(0.0, 1.0)
    }

    /// Evaluate one 80 ms post-AEC block and produce assessment + optional
    /// `EchoHealth` and `NearEndPresence` telemetry frames.
    #[allow(clippy::too_many_arguments)]
    pub fn evaluate(
        &mut self,
        fused: &[i16],
        playing: bool,
        ref_locked: bool,
        raw_corr: Option<f32>,
        barge_fired: bool,
        now_mono_ms: u64,
        now_wall_ms: u64,
    ) -> EvalOutput {
        let (echo_health, health_reason) =
            self.update_echo_health(ref_locked, playing, now_mono_ms);
        let rms_dbfs = block_rms_dbfs(fused);
        let clipped = block_clipped(fused);

        let potential_speech = rms_dbfs - self.noise_floor_dbfs >= IDLE_PRESENCE_SNR_DB
            && rms_dbfs >= IDLE_PRESENCE_MIN_DBFS;
        let quiet =
            !playing && !clipped && !potential_speech && rms_dbfs < IDLE_CALIBRATE_CEILING_DBFS;
        if quiet {
            if self.noise_floor_initialized {
                self.noise_floor_dbfs = self.noise_floor_dbfs * (1.0 - NOISE_FLOOR_EMA_ALPHA)
                    + rms_dbfs * NOISE_FLOOR_EMA_ALPHA;
            } else {
                self.quiet_acquisition_blocks += 1;
                self.quiet_acquisition_min_dbfs = self.quiet_acquisition_min_dbfs.min(rms_dbfs);
                if self.quiet_acquisition_blocks >= QUIET_ACQUISITION_BLOCKS {
                    self.noise_floor_dbfs = self.quiet_acquisition_min_dbfs;
                    self.noise_floor_initialized = true;
                }
            }
        } else {
            self.quiet_acquisition_blocks = 0;
            self.quiet_acquisition_min_dbfs = 0.0;
        }

        let snr_db = rms_dbfs - self.noise_floor_dbfs;
        let (present, confidence, reason) = if playing {
            if echo_health != EchoPathHealth::Healthy {
                (false, 0.0, health_reason)
            } else if barge_fired {
                let corr = raw_corr.unwrap_or(0.0).clamp(0.0, 1.0);
                (true, (1.0 - corr).clamp(0.0, 1.0), "bargein_fire")
            } else {
                (false, 0.0, "playback_echo_guarded")
            }
        } else if snr_db >= IDLE_PRESENCE_SNR_DB && rms_dbfs >= IDLE_PRESENCE_MIN_DBFS {
            let conf = ((snr_db - 6.0) / 14.0).clamp(0.0, 1.0);
            if self.noise_floor_initialized {
                (true, conf, "idle_snr")
            } else {
                (true, conf.min(0.5), "idle_snr_uncalibrated")
            }
        } else if !self.noise_floor_initialized {
            (false, 0.0, "calibrating_quiet")
        } else {
            (false, 0.0, "quiet")
        };

        self.last_corr = self.guard_corr(playing, raw_corr.unwrap_or(0.0));
        self.last_corr_ms = now_mono_ms;

        let degraded_ms = self
            .degraded_since_ms
            .map(|s| now_mono_ms.saturating_sub(s))
            .unwrap_or(0);
        let transitioned = self.prev_emitted_health != Some(echo_health);
        let sustained = echo_health == EchoPathHealth::Degraded
            && degraded_ms.saturating_sub(self.last_degraded_emit_ms)
                >= SUSTAINED_DEGRADED_INTERVAL_MS;
        let echo_health_frame = if transitioned || sustained {
            self.prev_emitted_health = Some(echo_health);
            if echo_health == EchoPathHealth::Degraded {
                self.last_degraded_emit_ms = degraded_ms;
            }
            Some(GatewayFrame::EchoHealth {
                device: self.device_id.clone(),
                health: echo_health,
                degraded_ms,
                reason: health_reason.to_string(),
            })
        } else {
            None
        };

        let just_ended = self.prev_present && !present;
        if present {
            self.speech_since_ms.get_or_insert(now_mono_ms);
        }
        let duration_ms = match self.speech_since_ms {
            Some(start) => now_mono_ms.saturating_sub(start).saturating_add(BLOCK_MS),
            None => 0,
        };
        let rate_ok = self
            .last_presence_emit_ms
            .is_none_or(|last| now_mono_ms.saturating_sub(last) >= BLOCK_MS);
        let presence_frame = if self.loopback_only
            && self.noise_floor_initialized
            && (present || just_ended)
            && rate_ok
        {
            self.last_presence_emit_ms = Some(now_mono_ms);
            Some(GatewayFrame::NearEndPresence {
                device: self.device_id.clone(),
                ts_ms: now_wall_ms,
                duration_ms,
                rms_dbfs,
                noise_floor_dbfs: self.noise_floor_dbfs,
                snr_db,
                clipped,
                echo_health,
                profile_version: PROFILE_VERSION.to_string(),
            })
        } else {
            None
        };
        if !present {
            self.speech_since_ms = None;
        }
        self.prev_present = present && self.noise_floor_initialized;

        let assessment = NearEndAssessment {
            present,
            confidence,
            rms_dbfs,
            noise_floor_dbfs: self.noise_floor_dbfs,
            snr_db,
            clipped,
            echo_health,
            noise_floor_initialized: self.noise_floor_initialized,
            reason,
        };
        self.last_assessment = assessment;
        EvalOutput {
            assessment,
            echo_health_frame,
            presence_frame,
        }
    }
}

/// Shared playback-time KWS guard decision consuming `NearEndEvaluator`.
pub fn kws_guard_should_suppress(
    playing: bool,
    nearend: &Arc<Mutex<NearEndEvaluator>>,
    detector: Option<&Arc<Mutex<bargein::Detector>>>,
    guard_threshold: f32,
    now_mono_ms: u64,
) -> bool {
    if !playing {
        return false;
    }
    let Some(det) = detector else {
        // Disabling semantic interruption must not disable echo integrity.
        // With no correlation source, playback-time KWS is unverified.
        return true;
    };
    let raw_corr = det
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .guard_now(now_mono_ms);
    let corr = nearend
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .guard_corr(playing, raw_corr);
    corr >= guard_threshold
}

pub fn block_rms_dbfs(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return MIN_DBFS;
    }
    let db = bargein::level_db(samples);
    if db.is_finite() {
        db.clamp(MIN_DBFS, 0.0)
    } else {
        MIN_DBFS
    }
}

pub fn block_clipped(samples: &[i16]) -> bool {
    samples
        .iter()
        .any(|&s| s.unsigned_abs() >= (i16::MAX - 1) as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(amp: i16, len: usize) -> Vec<i16> {
        (0..len)
            .map(|i| {
                let p = 2.0 * std::f32::consts::PI * 440.0 * i as f32 / 16_000.0;
                (amp as f32 * p.sin()) as i16
            })
            .collect()
    }

    #[test]
    fn noise_floor_snr_and_clipping_update_only_when_quiet_and_idle() {
        let mut ev = NearEndEvaluator::new("dot2-d", true, true);
        let quiet = tone(15, 1280); // ~ -70 dBFS
        let initial = ev.evaluate(&quiet, false, true, None, false, 0, 920);
        assert!(!initial.assessment.noise_floor_initialized);
        ev.evaluate(&quiet, false, true, None, false, 40, 960);
        let out1 = ev.evaluate(&quiet, false, true, None, false, 80, 1000);
        assert!(out1.assessment.noise_floor_initialized);
        assert!(!out1.assessment.present);
        assert!(!out1.assessment.clipped);
        let floor_after_quiet = ev.noise_floor_dbfs();
        assert!(
            (floor_after_quiet - out1.assessment.rms_dbfs).abs() < 0.5,
            "initial floor should calibrate to quiet room: {floor_after_quiet}"
        );

        // Loud near-end speech while idle: present=true, snr_db > 0, noise floor must NOT rise.
        let speech = tone(4000, 1280);
        let out2 = ev.evaluate(&speech, false, true, None, false, 160, 1080);
        assert!(out2.assessment.present);
        assert!(out2.assessment.snr_db > 20.0);
        assert_eq!(ev.noise_floor_dbfs(), floor_after_quiet);
        assert!(out2.presence_frame.is_some());

        // Loud signal during playback: playing=true, noise floor must NOT rise.
        let out3 = ev.evaluate(&speech, true, true, Some(0.8), false, 240, 1160);
        assert!(!out3.assessment.present);
        assert_eq!(ev.noise_floor_dbfs(), floor_after_quiet);
        // Speech just ended -> presence_frame emitted once for end-of-speech.
        assert!(out3.presence_frame.is_some());

        // Clipping detection at peak >= i16::MAX - 1.
        let mut clipped_blk = vec![0i16; 1280];
        clipped_blk[10] = i16::MAX - 1;
        let out4 = ev.evaluate(&clipped_blk, false, true, None, false, 320, 1240);
        assert!(out4.assessment.clipped);
    }

    #[test]
    fn degraded_echo_health_fails_closed_for_bargein_and_kws_guard() {
        let nearend = Arc::new(Mutex::new(NearEndEvaluator::new("dot2-d", true, true)));
        let det = Arc::new(Mutex::new(bargein::Detector::new(
            bargein::Config::default(),
        )));

        // Degraded during active playback -> guard_corr returns 1.0 -> KWS suppressed.
        {
            let mut ev = nearend.lock().unwrap();
            let (health, _) = ev.update_echo_health(false, true, 1000);
            assert_eq!(health, EchoPathHealth::Degraded);
            assert_eq!(ev.guard_corr(true, 0.1), 1.0);
        }
        assert!(kws_guard_should_suppress(
            true,
            &nearend,
            Some(&det),
            0.50,
            1000
        ));

        // Healthy during active playback with low corr -> KWS NOT suppressed.
        {
            let mut ev = nearend.lock().unwrap();
            let (health, _) = ev.update_echo_health(true, true, 1080);
            assert_eq!(health, EchoPathHealth::Healthy);
            assert!((ev.guard_corr(true, 0.15) - 0.15).abs() < 1e-6);
        }
        assert!(!kws_guard_should_suppress(
            true,
            &nearend,
            Some(&det),
            0.50,
            1080
        ));

        // Unknown during active playback (e.g. aec disabled) -> fail-closed 1.0.
        let nearend_unk = Arc::new(Mutex::new(NearEndEvaluator::new("dot2-d", false, true)));
        {
            let mut ev = nearend_unk.lock().unwrap();
            let (health, _) = ev.update_echo_health(false, true, 1000);
            assert_eq!(health, EchoPathHealth::Unknown);
            assert_eq!(ev.guard_corr(true, 0.1), 1.0);
        }
        assert!(kws_guard_should_suppress(
            true,
            &nearend_unk,
            Some(&det),
            0.50,
            1000
        ));
    }

    #[test]
    fn startup_speech_does_not_become_the_noise_floor() {
        let mut ev = NearEndEvaluator::new("test", true, true);
        let speech = tone(260, 1280); // approximately -45 dBFS
        for block in 0..50 {
            let out = ev.evaluate(&speech, false, true, None, false, block * 80, block * 80);
            assert!(out.assessment.present);
            assert!(!out.assessment.noise_floor_initialized);
            assert_eq!(out.assessment.reason, "idle_snr_uncalibrated");
            assert!(
                out.presence_frame.is_none(),
                "unacquired floor grants no passive qualification"
            );
            assert_eq!(out.assessment.noise_floor_dbfs, DEFAULT_NOISE_FLOOR_DBFS);
        }
        let quiet = tone(15, 1280);
        for block in 50..53 {
            ev.evaluate(&quiet, false, true, None, false, block * 80, block * 80);
        }
        assert!(ev.last_assessment().noise_floor_initialized);
        assert!(ev.noise_floor_dbfs() < -65.0);
        assert!(
            ev.evaluate(&speech, false, true, None, false, 4240, 4240)
                .assessment
                .present
        );
    }

    #[test]
    fn quiet_noise_steps_adapt_but_playback_or_speech_do_not() {
        let mut ev = NearEndEvaluator::new("test", true, true);
        let quiet = tone(15, 1280);
        for i in 0..3 {
            ev.evaluate(&quiet, false, true, None, false, i * 80, i * 80);
        }
        let higher_noise = tone(30, 1280); // below speech minimum
        for i in 3..103 {
            let out = ev.evaluate(&higher_noise, false, true, None, false, i * 80, i * 80);
            assert!(!out.assessment.present);
        }
        assert!((ev.noise_floor_dbfs() - block_rms_dbfs(&higher_noise)).abs() < 0.1);
        let floor = ev.noise_floor_dbfs();
        ev.evaluate(&tone(260, 1280), false, true, None, false, 8240, 8240);
        ev.evaluate(&quiet, true, true, Some(0.8), false, 8320, 8320);
        assert_eq!(ev.noise_floor_dbfs(), floor);
    }

    #[test]
    fn healthy_independent_kws_survives_with_semantic_barge_disabled() {
        let nearend = Arc::new(Mutex::new(NearEndEvaluator::new("test", true, true)));
        let detector = Arc::new(Mutex::new(bargein::Detector::new(
            bargein::Config::default(),
        )));
        let mic = tone(3000, 1280);
        let reference: Vec<i16> = (0..1280)
            .map(|i| (3000.0 * (i as f32 * 731.0 * std::f32::consts::TAU / 16000.0).sin()) as i16)
            .collect();
        nearend.lock().unwrap().update_echo_health(true, true, 80);
        {
            let mut det = detector.lock().unwrap();
            det.set_ref_locked(true);
            det.set_echo_health(EchoPathHealth::Healthy);
            // The production worker passes false for voice_live when the
            // semantic feature is off, while still feeding play_any=true.
            assert!(matches!(
                det.process(&mic, &mic, &reference, false, true, 80),
                bargein::Outcome::None
            ));
            assert_eq!(det.counters.fired, 0);
        }
        assert!(!kws_guard_should_suppress(
            true,
            &nearend,
            Some(&detector),
            0.5,
            80
        ));
        nearend.lock().unwrap().update_echo_health(false, true, 160);
        assert!(kws_guard_should_suppress(
            true,
            &nearend,
            Some(&detector),
            0.5,
            160
        ));
        let no_aec = Arc::new(Mutex::new(NearEndEvaluator::new("test", false, true)));
        assert!(!kws_guard_should_suppress(false, &no_aec, None, 0.5, 240));
    }

    #[test]
    fn missing_optional_barge_detector_never_disables_playback_kws_guard() {
        for aec_enabled in [false, true] {
            let nearend = Arc::new(Mutex::new(NearEndEvaluator::new("test", aec_enabled, true)));
            for locked in [false, true] {
                nearend
                    .lock()
                    .unwrap()
                    .update_echo_health(locked, true, 1000);
                assert!(kws_guard_should_suppress(true, &nearend, None, 0.5, 1000));
                assert!(!kws_guard_should_suppress(false, &nearend, None, 0.5, 1000));
            }
        }
    }

    #[test]
    fn presence_is_loopback_only_and_rate_limited() {
        let mut ev = NearEndEvaluator::new("dot2-d", true, false);
        let speech = tone(4000, 1280);
        let out = ev.evaluate(&speech, false, true, None, false, 80, 1000);
        assert!(out.assessment.present);
        assert!(
            out.presence_frame.is_none(),
            "non-loopback must not emit NearEndPresence"
        );
    }
}
