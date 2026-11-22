//! T5 firmware debt #1: boot-time capture health sentinel.
//!
//! The "ch8 cross-boot race" (mic or DAC loopback silently dead after some
//! reboots / measure toggles) was never reproducible, and changing the OS
//! image before locating it would be guessing. This module turns the
//! question into an observable instead: on every boot the first ~2 s of
//! 9-channel capture are classified (are the mics alive? is ch8 alive?) and,
//! on the first playback, the captured ch8 is correlated in memory against
//! the cleaned DAC reference the AEC just used. The result is ONE structured
//! log line that `tools/ch8_repro.py` collects across N reboots.
//!
//! Deliberately CLOCK-FREE and SYSCALL-FREE: everything is driven by the
//! capture thread's 80 ms blocks. The P0 root cause (2026-11-09) was a
//! `clock_gettime64` trap on this 3.18 kernel; new code must not introduce
//! another time/stat call, so there is no `Instant::now()` here at all.
//!
//! This sentinel OBSERVES. It never resets a device, changes a route, or
//! touches the OS image — if the race turns out to be an image-level
//! bring-up ordering bug, the fix belongs to the "C legacy OS image update"
//! milestone, not here.

use std::collections::VecDeque;

/// Warm-up window: 25 capture blocks × 80 ms = 2 s of boot audio.
pub const WARMUP_BLOCKS: usize = 25;
/// Correlation window = one capture block (80 ms @16k).
pub const CORR_WINDOW: usize = 1280;
/// Half-range of the lag search (samples @16k = ±40 ms), same as the
/// barge-in guard — the codec/DAC path skew lives well inside this.
const CORR_LAG: usize = 640;
/// A captured ch8 that is a loopback of the DAC must be strongly correlated
/// with the reference we built from what we actually wrote. Below this the
/// two are not the same signal (mis-routed, stale, or the ch8 race).
pub const CORR_HEALTHY: f32 = 0.50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Mics alive, ch8 alive, first-play correlation healthy.
    Healthy,
    /// All seven mic channels were digital zero for the whole warm-up.
    MicAllZero,
    /// Mics alive but ch8 (DAC loopback) stayed at zero.
    Ch8Silent,
    /// ch8 had signal but did not track the DAC reference.
    Ch8Uncorrelated,
    /// No playback happened during the observation window (nothing to
    /// correlate). NOT a failure — the repro script keeps rebooting.
    NoPlaybackYet,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Healthy => "healthy",
            Verdict::MicAllZero => "mic_all_zero",
            Verdict::Ch8Silent => "ch8_silent",
            Verdict::Ch8Uncorrelated => "ch8_uncorrelated",
            Verdict::NoPlaybackYet => "no_playback_yet",
        }
    }
    pub fn is_failure(self) -> bool {
        matches!(
            self,
            Verdict::MicAllZero | Verdict::Ch8Silent | Verdict::Ch8Uncorrelated
        )
    }
}

/// One immutable boot report, emitted exactly once per process lifetime.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub blocks: usize,
    pub mic_nonzero: usize,
    pub ch8_nonzero: bool,
    /// Best-lag correlation captured on the first playback, if one happened
    /// before the warm-up window closed.
    pub first_play_corr: Option<f32>,
    pub verdict: Verdict,
}

impl Report {
    /// Fixed-shape line so a shell script can grep it across reboots.
    pub fn as_log_line(&self) -> String {
        format!(
            "[boot-health] blocks={} mic_nonzero={}/7 ch8_nonzero={} first_play_corr={} verdict={}",
            self.blocks,
            self.mic_nonzero,
            self.ch8_nonzero,
            self.first_play_corr
                .map(|c| format!("{c:.3}"))
                .unwrap_or_else(|| "n/a".into()),
            self.verdict.as_str(),
        )
    }
}

pub struct BootSentinel {
    blocks: usize,
    /// Blocks observed while some playback channel was live.
    play_blocks: usize,
    mic_nonzero: [bool; 7],
    /// ch8 had signal at ANY point (a fact for the log line).
    ch8_nonzero: bool,
    /// ch8 had signal WHILE PLAYING — the only condition under which a
    /// silent ch8 means anything. A quiet device's loopback is zero by
    /// design, so boot-time silence before the first play is NOT a fault.
    ch8_nonzero_during_play: bool,
    ch8_ring: VecDeque<i16>,
    ref_ring: VecDeque<i16>,
    first_play_corr: Option<f32>,
    report: Option<Report>,
}

impl Default for BootSentinel {
    fn default() -> Self {
        Self::new()
    }
}

impl BootSentinel {
    pub fn new() -> BootSentinel {
        BootSentinel {
            blocks: 0,
            play_blocks: 0,
            mic_nonzero: [false; 7],
            ch8_nonzero: false,
            ch8_nonzero_during_play: false,
            ch8_ring: VecDeque::with_capacity(CORR_WINDOW + 2 * CORR_LAG),
            ref_ring: VecDeque::with_capacity(CORR_WINDOW + 2 * CORR_LAG),
            first_play_corr: None,
            report: None,
        }
    }

    /// Feed one 80 ms block of decoded 9-channel capture plus the cleaned
    /// DAC reference for the same block. `play_any` says whether the
    /// playback thread had ANY channel live (voice or media).
    ///
    /// Returns the report once, when either (a) the warm-up window closed
    /// with a conclusive verdict, or (b) a first playback produced a
    /// correlation. Reporting early on a conclusive failure matters: the
    /// repro script needs the evidence even if the device resets soon after.
    pub fn observe(
        &mut self,
        decoded: &[[i16; 9]],
        cleaned_ref: &[i16],
        play_any: bool,
    ) -> Option<Report> {
        if self.report.is_some() {
            return None;
        }
        self.blocks += 1;
        for s in decoded {
            for (k, flag) in self.mic_nonzero.iter_mut().enumerate() {
                if s[k] != 0 {
                    *flag = true;
                }
            }
            if s[8] != 0 {
                self.ch8_nonzero = true;
            }
        }

        // Accumulate the correlation pair only while something is playing:
        // correlating silence against silence is meaningless (and would
        // produce a fake 0.0 "uncorrelated" verdict on a healthy device).
        if play_any {
            self.play_blocks += 1;
            for s in decoded {
                if s[8] != 0 {
                    self.ch8_nonzero_during_play = true;
                }
                self.ch8_ring.push_back(s[8]);
            }
            self.ref_ring.extend(cleaned_ref.iter().copied());
            while self.ch8_ring.len() > CORR_WINDOW {
                self.ch8_ring.pop_front();
            }
            while self.ref_ring.len() > CORR_WINDOW + 2 * CORR_LAG {
                self.ref_ring.pop_front();
            }
            // A silent ch8 produces a mathematically valid 0.0 correlation,
            // but the honest verdict is "silent", not "uncorrelated" —
            // correlating two windows where one is all zeros says nothing
            // about routing. Only measure once ch8 actually carried signal.
            if self.first_play_corr.is_none()
                && self.ch8_nonzero_during_play
                && self.ch8_ring.len() >= CORR_WINDOW
                && self.ref_ring.len() >= CORR_WINDOW + 2 * CORR_LAG
            {
                let corr = crate::firmware::bargein::best_lag_corr(
                    &self.ch8_ring,
                    &self.ref_ring,
                    CORR_WINDOW,
                );
                self.first_play_corr = Some(corr);
            }
        }

        let mic_count = self.mic_nonzero.iter().filter(|&&b| b).count();
        // Only two conditions are conclusive EARLY: the mics are dead at
        // any time, or ch8 stayed dead across several PLAYING blocks. A
        // quiet boot with no playback is normal — extend the window so a
        // later first play can still be correlated, then report honestly.
        let conclusive_failure = self.blocks >= 3
            && (mic_count == 0 || (self.play_blocks >= 3 && !self.ch8_nonzero_during_play));
        let window_closed = self.blocks >= WARMUP_BLOCKS;
        let correlated = self.first_play_corr.is_some();
        if !(conclusive_failure
            || (window_closed && correlated)
            || self.blocks >= WARMUP_BLOCKS * 3)
        {
            return None;
        }
        let verdict = if mic_count == 0 {
            Verdict::MicAllZero
        } else if self.play_blocks >= 3 && !self.ch8_nonzero_during_play {
            Verdict::Ch8Silent
        } else if let Some(c) = self.first_play_corr {
            if c >= CORR_HEALTHY {
                Verdict::Healthy
            } else {
                Verdict::Ch8Uncorrelated
            }
        } else {
            Verdict::NoPlaybackYet
        };
        let report = Report {
            blocks: self.blocks,
            mic_nonzero: mic_count,
            ch8_nonzero: self.ch8_nonzero,
            first_play_corr: self.first_play_corr,
            verdict,
        };
        self.report = Some(report.clone());
        Some(report)
    }

    pub fn report(&self) -> Option<&Report> {
        self.report.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One real-shaped capture block: 1280 frames of 9 channels.
    fn block(mic: i16, ch8: &[i16]) -> Vec<[i16; 9]> {
        (0..1280)
            .map(|i| {
                let mut s = [mic; 9];
                s[8] = ch8[i];
                s
            })
            .collect()
    }

    /// Deterministic white-ish noise (LCG) — different seeds are genuinely
    /// uncorrelated, unlike shifted ramps/sines whose autocorrelation can
    /// accidentally exceed the health threshold at some lag.
    fn noise(n: usize, seed: u64) -> Vec<i16> {
        let mut x = seed
            .wrapping_mul(2862933555777941757)
            .wrapping_add(3037000493);
        (0..n)
            .map(|_| {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((x >> 33) as i32 % 20000 - 10000) as i16
            })
            .collect()
    }

    #[test]
    fn all_zero_capture_reports_failure_early() {
        let mut s = BootSentinel::new();
        let zero = vec![[0i16; 9]; 1280];
        let r = (0..3)
            .find_map(|_| s.observe(&zero, &vec![0i16; 1280], true))
            .expect("must report a conclusive failure without waiting 2 s");
        assert_eq!(r.verdict, Verdict::MicAllZero);
        assert!(r.verdict.is_failure());
        assert_eq!(r.mic_nonzero, 0);
        assert!(r.as_log_line().contains("verdict=mic_all_zero"));
    }

    #[test]
    fn silent_ch8_only_during_playback_is_the_race_signature() {
        let mut s = BootSentinel::new();
        // Quiet boot (no playback): ch8 silence is normal, must NOT report
        // a failure — this is the exact false positive the race hunt cannot
        // afford on a healthy idle device.
        for _ in 0..WARMUP_BLOCKS {
            assert!(s
                .observe(&block(300, &vec![0i16; 1280]), &vec![0i16; 1280], false)
                .is_none());
        }
        let mut out = None;
        for _ in 0..4 {
            out = s.observe(&block(300, &vec![0i16; 1280]), &noise(1280, 7), true);
            if out.is_some() {
                break;
            }
        }
        let r = out.expect("ch8 silent across playing blocks is conclusive");
        assert_eq!(r.verdict, Verdict::Ch8Silent);
        assert_eq!(r.mic_nonzero, 7);
        assert!(!r.ch8_nonzero);
    }

    #[test]
    fn loopback_tracking_the_dac_reference_is_healthy() {
        let mut s = BootSentinel::new();
        let mut out = None;
        for i in 0..WARMUP_BLOCKS + 2 {
            // The reference is what the firmware ACTUALLY wrote; the ch8
            // loopback is the same signal. Two blocks fill the ref ring
            // (window + 2*lag), so the correlation becomes measurable then.
            let sig = noise(1280, 100 + i as u64);
            out = s.observe(&block(300, &sig), &sig, true);
            if out.is_some() {
                break;
            }
        }
        let r = out.expect("a first-play correlation must produce a report");
        let c = r.first_play_corr.expect("corr must be measured");
        assert!(c >= CORR_HEALTHY, "loopback must track the DAC ref: {c}");
        assert_eq!(r.verdict, Verdict::Healthy);
        assert!(!r.verdict.is_failure());
        assert!(r.as_log_line().contains("verdict=healthy"));
    }

    #[test]
    fn uncorrelated_ch8_is_flagged_not_silently_accepted() {
        let mut s = BootSentinel::new();
        let mut out = None;
        for i in 0..WARMUP_BLOCKS + 2 {
            let sig = noise(1280, 100 + i as u64);
            let unrelated = noise(1280, 900_000 + i as u64);
            out = s.observe(&block(300, &unrelated), &sig, true);
            if out.is_some() {
                break;
            }
        }
        let r = out.expect("must report");
        assert_eq!(r.verdict, Verdict::Ch8Uncorrelated);
        assert!(r.verdict.is_failure());
        assert!(r.first_play_corr.unwrap() < CORR_HEALTHY);
    }

    #[test]
    fn no_playback_is_honest_not_a_failure() {
        let mut s = BootSentinel::new();
        let mut out = None;
        for _ in 0..WARMUP_BLOCKS * 3 + 1 {
            out = s.observe(&block(300, &vec![400i16; 1280]), &vec![0i16; 1280], false);
            if out.is_some() {
                break;
            }
        }
        let r = out.expect("the extended window must close");
        assert_eq!(r.verdict, Verdict::NoPlaybackYet);
        assert!(!r.verdict.is_failure());
        assert!(r.first_play_corr.is_none());
        assert!(r.as_log_line().contains("first_play_corr=n/a"));
    }

    #[test]
    fn reports_exactly_once() {
        let mut s = BootSentinel::new();
        let zero = vec![[0i16; 9]; 1280];
        for _ in 0..3 {
            let _ = s.observe(&zero, &vec![0i16; 1280], true);
        }
        assert!(s.report().is_some());
        assert!(s.observe(&zero, &vec![0i16; 1280], true).is_none());
    }
}
