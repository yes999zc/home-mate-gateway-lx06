//! D/T5 semantic barge-in: near-end speech detection while the device is
//! playing its own voice channel, with the two RUI Local defenses the user
//! directed us to borrow (2026-11-10):
//!
//! ① **near-end vs DAC-reference cross-correlation guard** — a harder
//!    self-trigger line than any energy threshold. Echo is, by physics, a
//!    (room-filtered) copy of what the DAC played: the PRE-AEC microphone
//!    during echo-only moments is highly correlated with the reference.
//!    Near-end speech is a different source and decorrelates the window.
//!    The detector therefore fires only when the mic window is loud AND its
//!    best-lag correlation with the reference is LOW. This same guard also
//!    protects the T4 keyword spotter: a hit that arrives while playback
//!    live and the mic is echo-dominated is a self-heard hit and is
//!    suppressed at the device ([`Detector::guard_now`]).
//!
//! ② **onset pre-roll** — the ring keeps the last ~400 ms of the post-AEC
//!    stream, so the audio from BEFORE the detection rides along in the
//!    `barge_in` frame and the first syllable of the interrupting speech
//!    survives (the core has been correctly DROPPING chunks while voice
//!    played; without the pre-roll that audio is gone for good).
//!
//! Architecture (same shape as T4): the device reports a FACT — "genuine
//! near-end speech started at ts with this pre-roll, guard value corr" —
//! and the CORE decides whether a playback stops and whether the words
//! become a turn ([voice.rs::on_barge_in] in the core repo). A false
//! positive can only ever be acted on by tested core rules. One fact is
//! reported per playback episode (latched until the voice channel goes
//! quiet), so a core that ignores the fact is not spammed.
//!
//! Time in this module is drift-free by construction: everything is
//! driven by the capture thread's 80 ms blocks, not wall clocks.

use std::collections::VecDeque;

use crate::protocol::EchoPathHealth;

/// Onset slice length (samples @16k = 20 ms) — same granularity as the KWS
/// feed: the detector can only act on samples it has.
const SLICE: usize = 320;
/// Consecutive loud slices that confirm an onset (~60 ms).
const ONSET_SLICES: usize = 3;
/// Consecutive guard passes (loud + uncorrelated blocks) required to fire.
/// One 80 ms window of correlation is noisy; two windows 80 ms apart make
/// a soft-spoken onset fire one block later but keep residual spikes from
/// slipping through a single lucky window.
const CORR_PASS_RUN: usize = 2;
/// Correlation window W (samples @16k = 80 ms, one capture block): long
/// enough for a robust correlation estimate, short enough to react inside
/// the same block the onset confirms.
const GUARD_WINDOW: usize = 1280;
/// Lag search half-range (samples @16k = ±40 ms): the acoustic
/// speaker-to-mic path is centimetres (≤50 samples) and the codec paths
/// are in the same chip; ±40 ms covers any plausible fixed skew.
const GUARD_LAG: usize = 640;
/// Pre-roll kept in the ring (ms). Speech began up to ~200 ms before the
/// fire decision (onset confirm + two guard passes); 400 ms also gives the
/// ASR the ~300 ms of leading context the T2 pre-roll experiment proved
/// helps short-phrase CER.
pub const PREROLL_MS: u64 = 400;
/// Default playback-settle window (D/T5 R1, 2026-11-09 night): after the
/// voice channel STARTS, the AEC reference relocks and the residual is
/// decorrelated for a few hundred milliseconds. The guard consequently
/// reads "loud + uncorrelated" = near-end speech at every TTS onset — the
/// exact mechanism that cut every answer off on the first T5 acceptance.
/// Facts inside the settle window are suppressed (the core has its own
/// answer-protection period on top; this one rides the DAC clock).
pub const SETTLE_MS: u64 = 600;

/// Detector tunables (CLI-overridable for on-device calibration; the
/// deployed defaults carry the measured values).
#[derive(Debug, Clone)]
pub struct Config {
    /// Absolute energy floor for any candidate (dBFS). This is NOT the
    /// near-end criterion: it only keeps digital silence and room floor
    /// out of the arithmetic. The old absolute -45 dBFS onset was the
    /// 2026-11-12 false-trigger source — at 124/127 the AEC residual sat
    /// at -37 dBFS (18 dB below the reference) and still counted as a
    /// "loud" onset.
    pub onset_db: f32,
    /// The near-end criterion: a candidate slice is near-end only when it
    /// is within this many dB OF THE DAC REFERENCE. 15 dB is the Step-3
    /// acceptance number (R1 self-hearing): the AEC must show >=15 dB
    /// suppression, and a person must raise the residual above ref-15.
    pub near_end_margin_db: f32,
    /// Below this reference level (dBFS) there is nothing worth
    /// interrupting; candidates are not evaluated. Keeps a quiet passage
    /// from treating room noise as speech.
    pub ref_floor_db: f32,
    /// Fire requires best-lag correlation BELOW this. Echo-only windows
    /// measure near 1.0; speech mixed over echo drops well below. This is
    /// a SECOND line of defense: the multi-mic pre-AEC correlation is not
    /// reliable enough to be the only gate at high volume (2026-11-12).
    pub corr_max: f32,
    /// Suppress candidate onsets for this long after the voice channel
    /// starts (playback settle). 0 disables (only for A/B measurement).
    pub settle_ms: u64,
    /// Consecutive echo-only blocks with >= `near_end_margin_db` measured
    /// suppression required before a near-end fact is allowed. This is the
    /// convergence gate: the AEC needs ~3 s to reach 15 dB from cold, and
    /// every old false trigger cut the answer at 1-1.6 s, so the filter
    /// never converged (a self-sustaining loop). 5 blocks = 400 ms.
    /// 0 disables (unit tests that exercise the guard alone).
    pub converged_blocks: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            // Digital-silence floor only; the relative criterion does the
            // discrimination. Kept as a CLI field for field diagnostics.
            onset_db: -60.0,
            near_end_margin_db: 15.0,
            ref_floor_db: -50.0,
            // On-device calibration (T5, 2026-11-09, fw 0.0.17): a NON-wake
            // near-end phrase measured corr=0.214; our own echo measured
            // 0.651/0.652. 0.50 splits the measured gap with ~0.15 margin
            // on BOTH sides; --barge-in-corr remains the field knob.
            corr_max: 0.50,
            settle_ms: SETTLE_MS,
            converged_blocks: 5,
        }
    }
}

impl Config {
    pub fn preroll_samples(&self) -> usize {
        PREROLL_MS as usize * 16 // 16000 / 1000
    }
}

/// Below this a dBFS number is not a measurement (the detector's own
/// `f32::MIN` sentinel lives far below; real audio stays above -120).
const MIN_VALID_DB: f32 = -200.0;

/// What one 80 ms capture block produced.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Nothing (quiet, still confirming, latched, or voice not live).
    None,
    /// A candidate onset was suppressed by the guard (echo-correlated).
    /// Telemetry: log it, do not act.
    GuardSuppress { corr: f32, energy_db: f32 },
    /// A candidate onset landed inside the playback settle window (R1):
    /// the microphone is hearing the answer's own onset transient while
    /// the AEC relocks. Logged once per episode, never acted on.
    Settle {
        corr: f32,
        energy_db: f32,
        voice_ms: u64,
    },
    /// The DAC reference was not usable (dealias unlocked): the
    /// correlation cannot distinguish echo from speech, so no fact may be
    /// reported. Logged once per episode.
    RefUnlocked { voice_ms: u64 },
    /// The AEC has not yet demonstrated >=15 dB suppression on this
    /// episode: the residual cannot be told apart from a person. Logged
    /// once per episode.
    NotConverged {
        energy_db: f32,
        ref_db: f32,
        residual_db: f32,
        voice_ms: u64,
    },
    /// Genuine near-end speech: report north with the pre-roll.
    Fire {
        preroll: Vec<i16>,
        corr: f32,
        energy_db: f32,
        ref_db: f32,
        residual_db: f32,
    },
}

/// Process-lifetime counters (boot self-check + console evidence).
#[derive(Debug, Default, Clone, Copy)]
pub struct Counters {
    /// Candidate onsets that reached the loud threshold.
    pub onsets: u64,
    /// Candidate episodes the guard suppressed (echo).
    pub guarded: u64,
    /// Facts reported north.
    pub fired: u64,
    /// Guard evaluations on keyword hits that suppressed a self-heard hit.
    pub kws_guarded: u64,
    /// Candidate onsets suppressed inside the playback settle window (R1).
    pub settled: u64,
    /// Candidate onsets suppressed because the reference was unlocked.
    pub ref_unlocked: u64,
    /// Candidate onsets suppressed because the AEC had not converged.
    pub not_converged: u64,
    /// The last computed guard value and when (mono ms, 0 = never).
    pub last_corr: f32,
    pub last_corr_ms: u64,
}

pub struct Detector {
    pub cfg: Config,
    fused_ring: VecDeque<i16>,
    mic_ring: VecDeque<i16>,
    ref_ring: VecDeque<i16>,
    onset_run: usize,
    corr_pass: usize,
    /// A candidate episode is open (for once-per-episode counters).
    held: bool,
    /// One fact per playback episode; reset when the voice channel stops.
    fired_episode: bool,
    /// Previous voice-live state and the mono-ms stamp of its rising edge
    /// (D/T5 R1 playback settle).
    voice_live_prev: bool,
    live_since_ms: u64,
    /// The settle suppression was already logged for this episode (log
    /// once — a settle window must not flood the log at 12.5 lines/s).
    settle_logged: bool,
    /// Whether EVERY 20 ms frame of the current 80 ms block came from a
    /// verified reference match. While false, neither the correlation nor
    /// the residual is trustworthy.
    ref_locked: bool,
    echo_health: EchoPathHealth,
    unlock_logged: bool,
    /// Consecutive echo-only blocks with >= `near_end_margin_db` measured
    /// suppression. The convergence gate (see Config::converged_blocks).
    converged: u32,
    not_converged_logged: bool,
    pub counters: Counters,
}

impl Detector {
    pub fn new(cfg: Config) -> Detector {
        let preroll = cfg.preroll_samples();
        Detector {
            cfg,
            fused_ring: VecDeque::with_capacity(preroll + 1280),
            mic_ring: VecDeque::with_capacity(GUARD_WINDOW + 2 * GUARD_LAG),
            ref_ring: VecDeque::with_capacity(GUARD_WINDOW + 2 * GUARD_LAG),
            onset_run: 0,
            corr_pass: 0,
            held: false,
            fired_episode: false,
            voice_live_prev: false,
            live_since_ms: 0,
            settle_logged: false,
            ref_locked: true,
            echo_health: EchoPathHealth::Healthy,
            unlock_logged: false,
            converged: 0,
            not_converged_logged: false,
            counters: Counters::default(),
        }
    }

    pub fn set_onset_db(&mut self, onset_db: f32) {
        self.cfg.onset_db = onset_db;
    }

    pub fn onset_db(&self) -> f32 {
        self.cfg.onset_db
    }

    /// Feed one capture block. `fused` is the post-AEC mono the uplink
    /// carries, `pre_mic` the pre-AEC mic downmix (guard input — the AEC
    /// has NOT yet removed the echo there, which is exactly why the
    /// correlation is informative), `ref_blk` the cleaned DAC reference
    /// aligned to this block. `voice_live`/`play_any` come from the
    /// playback thread's channel flags.
    pub fn process(
        &mut self,
        fused: &[i16],
        pre_mic: &[i16],
        ref_blk: &[i16],
        voice_live: bool,
        play_any: bool,
        now_ms: u64,
    ) -> Outcome {
        // Guard rings track playback of ANY channel: the keyword-spotter
        // guard must stay fresh during music too (self-trigger risk is
        // highest there — media sits ~15 dB hotter than voice).
        if play_any {
            push_capped(&mut self.mic_ring, pre_mic, GUARD_WINDOW + 2 * GUARD_LAG);
            push_capped(&mut self.ref_ring, ref_blk, GUARD_WINDOW + 2 * GUARD_LAG);
        }
        if !voice_live {
            self.onset_run = 0;
            self.corr_pass = 0;
            self.held = false;
            self.fired_episode = false;
            self.voice_live_prev = false;
            self.live_since_ms = 0;
            self.settle_logged = false;
            // A new playback episode starts with a clean convergence count:
            // the AEC must demonstrate >=15 dB suppression again on THIS
            // episode before a near-end fact is allowed (warm filters open
            // the gate in ~400 ms; a cold boot needs ~3 s).
            self.converged = 0;
            self.not_converged_logged = false;
            return Outcome::None;
        }
        // R1 playback settle: the DAC just started. Until the AEC has
        // relocked, "loud + uncorrelated" is the answer's own onset, not a
        // person — suppress and wait.
        if !self.voice_live_prev {
            self.voice_live_prev = true;
            self.live_since_ms = now_ms;
            self.settle_logged = false;
            self.onset_run = 0;
            self.corr_pass = 0;
            self.held = false;
        }
        let voice_ms = now_ms.saturating_sub(self.live_since_ms);
        let settling = voice_ms < self.cfg.settle_ms;
        push_capped(&mut self.fused_ring, fused, self.cfg.preroll_samples());
        if self.fired_episode {
            return Outcome::None; // one fact per episode; the core decides
        }
        let ref_db = level_db(ref_blk);
        // Onset tracking on 20 ms slices of the POST-AEC stream (what the
        // user's speech will actually be transcribed from). The criterion
        // is RELATIVE to the DAC reference: a slice counts only when it is
        // within `near_end_margin_db` of what the speakers are playing.
        // The absolute `onset_db` is just a silence floor. This is the R1
        // Step-3 fix: at 124/127 the echo residual is -37 dBFS (18 dB
        // below the reference) and the old absolute -45 onset called it a
        // person; the relative criterion calls it echo.
        let playing = ref_db >= self.cfg.ref_floor_db;
        let near_end_floor = ref_db - self.cfg.near_end_margin_db;
        let mut energy_db = f32::MIN;
        let mut was_loud = false;
        for slice in fused.as_chunks::<SLICE>().0 {
            let db = level_db(slice);
            if playing && db >= self.cfg.onset_db && db >= near_end_floor {
                self.onset_run += 1;
                was_loud = true;
                energy_db = energy_db.max(db);
            } else {
                self.onset_run = 0;
            }
        }
        if self.onset_run == 0 {
            // Echo-only (or quiet) block: this is the only place the
            // convergence tracker is fed. Use the BLOCK's own level — the
            // `energy_db` above is the maximum over CANDIDATE onsets and is
            // still `f32::MIN` when this block had none (the normal case for
            // an echo-only block, 2026-11-13). Feeding the sentinel made the
            // tracker read `ref - MIN` as enormous suppression and count a
            // convergence the filter had not demonstrated.
            self.record_echo_block(ref_db, level_db(fused));
            self.corr_pass = 0;
            self.held = false;
            return Outcome::None;
        }
        if !was_loud {
            return Outcome::None; // run carried over; this block was quiet
        }
        if self.onset_run >= ONSET_SLICES {
            if !self.held {
                self.held = true;
                self.counters.onsets += 1;
            }
            let corr = self.guard_corr(now_ms);
            if settling {
                // The answer is still inside its settle window: never a
                // fact, and the correlation of this moment is meaningless
                // (the reference is relocking). Restart the confirmation
                // after the window instead of carrying a half-pass over.
                self.corr_pass = 0;
                self.record_echo_block(ref_db, energy_db);
                if !self.settle_logged {
                    self.settle_logged = true;
                    self.counters.settled += 1;
                    return Outcome::Settle {
                        corr,
                        energy_db,
                        voice_ms,
                    };
                }
                return Outcome::None;
            }
            if !self.ref_locked || self.echo_health != EchoPathHealth::Healthy {
                // The reference lost its match INSIDE this 80 ms block
                // (or EchoPathHealth is Degraded/Unknown): neither the
                // correlation nor the residual is trustworthy. No fact.
                self.corr_pass = 0;
                if !self.unlock_logged {
                    self.unlock_logged = true;
                    self.counters.ref_unlocked += 1;
                    return Outcome::RefUnlocked { voice_ms };
                }
                return Outcome::None;
            }
            // Convergence gate: the AEC must have shown >=15 dB suppression
            // on recent echo-only blocks. Without it the residual of a cold
            // filter is indistinguishable from a person, and every false
            // trigger cuts the answer before the filter can converge.
            if self.converged < self.cfg.converged_blocks {
                self.corr_pass = 0;
                // Reported as near-end EXCESS over the reference
                // (energy - ref), the same quantity the core's acceptance
                // gate speaks. It used to be suppression (ref - energy), so
                // a person LOUDER than the playback looked like "residual
                // too low" and was refused — the 2026-11-12 field report
                // "shouting louder interrupts worse" (R1d).
                let residual_db = energy_db - ref_db;
                if !self.not_converged_logged {
                    self.not_converged_logged = true;
                    self.counters.not_converged += 1;
                    return Outcome::NotConverged {
                        energy_db,
                        ref_db,
                        residual_db,
                        voice_ms,
                    };
                }
                return Outcome::None;
            }
            if corr >= self.cfg.corr_max {
                // Echo-correlated window: the speakers are what the
                // microphone hears. Hold the episode open — soft speech
                // grows above the echo within a block or two and fires
                // then; a transient residual spike does not. This block is
                // echo, not a person: let the convergence tracker decay.
                self.corr_pass = 0;
                self.record_echo_block(ref_db, energy_db);
                return Outcome::GuardSuppress { corr, energy_db };
            }
            self.corr_pass += 1;
            if self.corr_pass >= CORR_PASS_RUN {
                self.fired_episode = true;
                self.counters.fired += 1;
                let preroll: Vec<i16> = self.fused_ring.iter().copied().collect();
                return Outcome::Fire {
                    preroll,
                    corr,
                    energy_db,
                    ref_db,
                    residual_db: energy_db - ref_db,
                };
            }
        }
        Outcome::None
    }

    /// Feed the convergence tracker with an echo-only block: `residual_db` is
    /// the measured suppression (reference minus post-AEC residual). A
    /// block that is not at least `near_end_margin_db` below the reference
    /// is not evidence of convergence; it decays the count instead.
    fn record_echo_block(&mut self, ref_db: f32, energy_db: f32) {
        // M6b: `energy_db` starts at `f32::MIN` and stays there when the
        // block had no onset slice. It IS finite, so the old check let it
        // through and `ref - MIN ≈ +3.4e38` counted as enormous suppression,
        // feeding the convergence gate for every quiet block. A real level
        // never approaches this floor; anything below it is "no measurement".
        if ref_db < self.cfg.ref_floor_db
            || !energy_db.is_finite()
            || !ref_db.is_finite()
            || energy_db < MIN_VALID_DB
            || ref_db < MIN_VALID_DB
        {
            return;
        }
        if ref_db - energy_db >= self.cfg.near_end_margin_db {
            self.converged = self.converged.saturating_add(1);
        } else {
            self.converged = self.converged.saturating_sub(1);
        }
    }

    /// The AEC reference cleaner's lock state for this block. Unlocked =
    /// the reference is the raw (alias-encoded) ch8; the guard must not
    /// act on a correlation it cannot trust (on-device 2026-11-09: the
    /// self-echo fires lined up with dealias relocks).
    pub fn set_ref_locked(&mut self, locked: bool) {
        self.ref_locked = locked;
        self.echo_health = if locked {
            EchoPathHealth::Healthy
        } else {
            EchoPathHealth::Degraded
        };
        if locked {
            self.unlock_logged = false;
        }
    }

    /// Update echo-path health from the single `NearEndEvaluator` truth.
    /// `Degraded` or `Unknown` fails closed during active playback.
    pub fn set_echo_health(&mut self, health: EchoPathHealth) {
        self.echo_health = health;
        if health == EchoPathHealth::Healthy && self.ref_locked {
            self.unlock_logged = false;
        }
    }

    /// Fresh guard value for an external consumer (the keyword-spotter
    /// guard): best-lag correlation of the current mic/reference windows.
    /// Callers check "playback live" first — with nothing playing there is
    /// nothing to guard against and the rings go stale by design.
    pub fn guard_now(&mut self, now_ms: u64) -> f32 {
        self.guard_corr(now_ms)
    }

    fn guard_corr(&mut self, now_ms: u64) -> f32 {
        if !self.ref_locked || self.echo_health != EchoPathHealth::Healthy {
            // Unlocked/degraded/unknown echo path is unreliable: suppress
            // playback-time KWS conservatively instead of risking self-wake.
            self.counters.last_corr = 1.0;
            self.counters.last_corr_ms = now_ms;
            return 1.0;
        }
        let corr = best_lag_corr(&self.mic_ring, &self.ref_ring, GUARD_WINDOW);
        self.counters.last_corr = corr;
        self.counters.last_corr_ms = now_ms;
        corr
    }
}

/// RMS level of a block/slice, dBFS (full scale = 32768). Public: the
/// calibration stat line and the boot sentinel measure the same way.
pub fn level_db(slice: &[i16]) -> f32 {
    let sum: f64 = slice.iter().map(|&s| (s as f64) * (s as f64)).sum();
    let rms = (sum / slice.len() as f64).sqrt();
    if rms <= 0.0 {
        f32::NEG_INFINITY
    } else {
        (20.0 * (rms / 32768.0).log10()) as f32
    }
}

fn push_capped(ring: &mut VecDeque<i16>, samples: &[i16], cap: usize) {
    if samples.len() >= cap {
        let skip = samples.len() - cap;
        ring.clear();
        ring.extend(samples[skip..].iter().copied());
        return;
    }
    for &s in samples {
        ring.push_back(s);
    }
    while ring.len() > cap {
        ring.pop_front();
    }
}

/// Max normalized cross-correlation of the NEWEST `window` samples of
/// `mic` against `reference` over lags −LAG..=+LAG, where
/// LAG = (reference.len() − window) / 2. `reference` holds window + 2·LAG
/// samples, newest last; lag 0 aligns the two newest samples. Pure: no
/// device, no clocks — the guard contract is unit-testable.
///
/// Normalization is per-lag windowed (prefix-sum energy), so the value is
/// 0..=1 for any content and directly comparable across moments: echo is
/// ≈1, echo+speech drops with the speech-to-echo ratio.
pub fn best_lag_corr(mic_ring: &VecDeque<i16>, ref_ring: &VecDeque<i16>, window: usize) -> f32 {
    let lag = match ref_ring.len().checked_sub(window) {
        Some(extra) => extra / 2,
        None => return 0.0, // not enough reference yet (first blocks)
    };
    if mic_ring.len() < window || ref_ring.len() < window {
        return 0.0;
    }
    // Both windows oldest-first, NEWEST-aligned at lag 0: mic_tail[i] is
    // the mic sample `window-1-i` before the newest, refc likewise for the
    // reference. (Building one reversed and one not silently decorrelates
    // everything — caught by the random-noise test, not the sine tests.)
    let take_ref = window + 2 * lag;
    let mic_tail: Vec<i16> = (0..window)
        .map(|i| mic_ring[mic_ring.len() - window + i])
        .collect();
    let refc: Vec<i16> = (0..take_ref)
        .map(|i| ref_ring[ref_ring.len() - take_ref + i])
        .collect();
    let e_mic: f64 = mic_tail.iter().map(|&s| (s as f64) * (s as f64)).sum();
    if e_mic <= 0.0 {
        return 0.0;
    }
    // Prefix sums of reference energy for O(1) windowed normalization.
    let mut prefix = vec![0f64; refc.len() + 1];
    for (i, &s) in refc.iter().enumerate() {
        prefix[i + 1] = prefix[i] + (s as f64) * (s as f64);
    }
    let mut best = 0f64;
    for l in 0..=(2 * lag) {
        let e_ref = prefix[l + window] - prefix[l];
        if e_ref <= 0.0 {
            continue;
        }
        let mut dot = 0f64;
        for i in 0..window {
            dot += mic_tail[i] as f64 * refc[l + i] as f64;
        }
        let c = dot / (e_mic * e_ref).sqrt();
        if c > best {
            best = c;
        }
    }
    best.clamp(0.0, 1.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic LCG (same construction as the dealias tests).
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> f32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) as i32 % 20000) as f32 / 20000.0 - 0.5
        }
    }

    fn sine(freq: f32, ms: usize, amp: f32, phase: f32) -> Vec<i16> {
        (0..ms * 16)
            .map(|i| {
                (amp * (2.0 * std::f64::consts::PI as f32 * freq * i as f32 / 16000.0 + phase)
                    .sin()) as i16
            })
            .collect()
    }

    /// Mono-ms stamp for block `n`, starting PAST the playback settle
    /// window: the tests that exercise firing are "a person interrupts a
    /// playing answer", which by definition cannot be its onset transient.
    fn t(n: usize) -> u64 {
        700 + 80 * n as u64
    }

    /// Guard/onset tests are about the correlation rule alone; the settle
    /// window has its own test below.
    fn cfg_no_settle() -> Config {
        // Guard/onset tests: convergence has its own test below.
        Config {
            settle_ms: 0,
            converged_blocks: 0,
            ..Config::default()
        }
    }

    /// M6b (S3): `energy_db = f32::MIN` is this detector's "no onset slice"
    /// sentinel. It is FINITE, so the old `!is_finite()` guard let it through
    /// and `ref_db - f32::MIN ≈ +3.4e38` counted as enormous suppression —
    /// every quiet block fed the convergence gate as if the AEC had cancelled
    /// perfectly (the gate exists to refuse exactly the opposite).
    #[test]
    fn the_no_onset_sentinel_never_counts_as_convergence() {
        let mut d = Detector::new(Config::default());
        d.converged = 0;
        d.record_echo_block(-20.0, f32::MIN);
        assert_eq!(d.converged, 0, "an invalid energy must not feed the gate");
        // A real echo-only block still counts…
        d.record_echo_block(-20.0, -50.0);
        assert_eq!(d.converged, 1);
        // …and a block at/above the reference decays it.
        d.record_echo_block(-50.0, -20.0);
        assert_eq!(d.converged, 0);
    }

    #[test]
    fn identical_signals_correlate_at_one() {
        let mut d = Detector::new(Config::default());
        let sig = sine(500.0, 80, 8000.0, 0.0);
        for i in 0..3 {
            d.process(&sig, &sig.clone(), &sig.clone(), true, true, t(i));
        }
        let c = d.guard_now(0);
        assert!(c > 0.95, "identical echo must correlate: {c}");
    }

    #[test]
    fn uncorrelated_speech_drops_the_guard() {
        let mut d = Detector::new(Config::default());
        let echo = sine(300.0, 80, 8000.0, 0.0);
        let speech = sine(1500.0, 80, 8000.0, 0.7); // different source
        for i in 0..3 {
            d.process(&speech, &speech.clone(), &echo.clone(), true, true, t(i));
        }
        let c = d.guard_now(0);
        assert!(c < 0.3, "independent source must decorrelate: {c}");
    }

    #[test]
    fn echo_over_speech_mix_stays_high() {
        // Near-end speech at only −6 dB relative to the echo: the mic is
        // still echo-dominated, the guard must hold.
        let mut d = Detector::new(Config::default());
        let echo = sine(300.0, 80, 8000.0, 0.0);
        let speech = sine(1500.0, 80, 4000.0, 1.1);
        let mixed: Vec<i16> = echo.iter().zip(&speech).map(|(a, b)| a + b).collect();
        for i in 0..3 {
            d.process(&speech, &mixed, &echo, true, true, t(i));
        }
        let c = d.guard_now(0);
        assert!(c > 0.65, "echo-dominated mix must stay correlated: {c}");
    }

    #[test]
    fn fires_once_per_episode_with_preroll() {
        let mut d = Detector::new(cfg_no_settle());
        let echo = sine(300.0, 80, 3000.0, 0.0); // quiet-ish reference
        let speech = sine(1500.0, 80, 12000.0, 0.7); // loud near-end
        let mut fired = 0;
        for block in 0..6 {
            match d.process(&speech, &speech.clone(), &echo, true, true, t(block)) {
                Outcome::Fire {
                    preroll,
                    corr,
                    energy_db,
                    ..
                } => {
                    fired += 1;
                    // Block 0: onset confirms, first guard pass (the ref ring
                    // only reaches full lag range one block in). Block 1:
                    // second pass → fire.
                    assert_eq!(block, 1, "must fire on the 2nd loud block (2 passes)");
                    assert!(corr < 0.65, "fired on uncorrelated audio: {corr}");
                    assert!(energy_db > -45.0);
                    // Pre-roll: 2 blocks pushed at fire time = 2560 samples,
                    // capped at preroll (6400). All speech, no dropouts.
                    assert_eq!(preroll.len(), 2560);
                    assert!(preroll.len() <= 6400);
                    assert!(preroll.iter().any(|&s| s.abs() > 1000));
                }
                Outcome::GuardSuppress { .. } => panic!("speech is not echo"),
                Outcome::None
                | Outcome::Settle { .. }
                | Outcome::RefUnlocked { .. }
                | Outcome::NotConverged { .. } => {}
            }
        }
        assert_eq!(fired, 1, "one fact per episode");
        assert_eq!(d.counters.fired, 1);
        // Episode reset when voice stops.
        let out = d.process(&speech, &speech.clone(), &echo, false, false, t(6));
        assert_eq!(out, Outcome::None);
        assert!(!d.fired_episode);
    }

    #[test]
    fn guard_suppresses_self_heard_loud_blocks() {
        // AEC momentarily fails: the post-AEC stream carries the echo
        // loudly. The energy onset confirms, but the guard must suppress
        // (mic ≈ reference) — this is the exact T4 self-trigger scenario
        // the user asked to guard harder than thresholds.
        let mut d = Detector::new(cfg_no_settle());
        let echo = sine(300.0, 80, 12000.0, 0.0);
        let mut suppressions = 0;
        for i in 0..8 {
            if let Outcome::GuardSuppress { corr, .. } =
                d.process(&echo, &echo.clone(), &echo.clone(), true, true, t(i))
            {
                suppressions += 1;
                assert!(corr > 0.9);
            }
        }
        assert!(suppressions >= 1, "self-heard echo must never fire");
        assert_eq!(d.counters.fired, 0);
        assert_eq!(d.counters.onsets, 1, "one candidate episode");
    }

    #[test]
    fn quiet_residual_never_confirms_onset() {
        let mut d = Detector::new(Config::default());
        let residual = sine(300.0, 80, 150.0, 0.0); // ~−47 dBFS, below onset
        let r = sine(300.0, 80, 12000.0, 0.0);
        for i in 0..6 {
            assert_eq!(
                d.process(&residual, &residual.clone(), &r, true, true, t(i)),
                Outcome::None
            );
        }
        assert_eq!(d.counters.onsets, 0);
    }

    #[test]
    fn soft_onset_fires_when_speech_grows_over_echo() {
        // First block: speech barely above onset, echo dominates the mic →
        // guard holds. Speech grows → correlation drops → fire (the held
        // episode must not latch shut).
        let mut d = Detector::new(cfg_no_settle());
        let echo = sine(300.0, 80, 8000.0, 0.0);
        let soft = sine(1500.0, 80, 6000.0, 0.7);
        let loud = sine(1500.0, 80, 16000.0, 0.7);
        let mixed_soft: Vec<i16> = echo.iter().zip(&soft).map(|(a, b)| a + b).collect();
        let mixed_loud: Vec<i16> = echo.iter().zip(&loud).map(|(a, b)| a + b).collect();
        let mut saw_hold = false;
        let mut fired = false;
        for (i, (fused, mic)) in [
            (&soft[..], &mixed_soft[..]),
            (&soft[..], &mixed_soft[..]),
            (&loud[..], &mixed_loud[..]),
            (&loud[..], &mixed_loud[..]),
            (&loud[..], &mixed_loud[..]),
        ]
        .into_iter()
        .enumerate()
        {
            match d.process(fused, mic, &echo, true, true, t(i)) {
                Outcome::GuardSuppress { .. } => saw_hold = true,
                Outcome::Fire { .. } => {
                    fired = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_hold, "soft onset must be held by the guard first");
        assert!(fired, "growing speech must eventually fire");
    }

    #[test]
    fn settle_suppresses_the_fresh_answer_onset_then_fires() {
        // The R1 mechanism: the voice channel starts, the AEC relocks, and
        // for the first settle window "loud + uncorrelated" is the answer's
        // own onset. It must be reported as Settle (telemetry), never Fire;
        // a person speaking after the window fires normally.
        // Convergence is a separate gate (test below); this test is about
        // the settle window.
        let mut d = Detector::new(Config {
            converged_blocks: 0,
            ..Config::default()
        });
        let echo = sine(300.0, 80, 3000.0, 0.0);
        let speech = sine(1500.0, 80, 12000.0, 0.7);
        let mut settles = 0;
        let mut fires = 0;
        for i in 0..12 {
            let now = 80 * i as u64; // voice starts at t=0
            match d.process(&speech, &speech.clone(), &echo, true, true, now) {
                Outcome::Settle { voice_ms, .. } => {
                    settles += 1;
                    assert!(voice_ms < d.cfg.settle_ms, "settle only inside the window");
                }
                Outcome::Fire { .. } => {
                    fires += 1;
                    assert!(
                        now >= d.cfg.settle_ms,
                        "fired inside the settle window at {now}ms"
                    );
                    assert_eq!(i, 9, "settle ends at 600 ms; 640 is pass 1, 720 fires");
                }
                Outcome::GuardSuppress { .. } => {}
                Outcome::None | Outcome::RefUnlocked { .. } | Outcome::NotConverged { .. } => {}
            }
        }
        assert_eq!(settles, 1, "one settle log per episode");
        assert_eq!(fires, 1, "exactly one fact per episode");
        assert_eq!(d.counters.settled, 1);
        assert_eq!(d.counters.fired, 1);
    }

    /// R1 Step 3: the near-end criterion is RELATIVE to the reference.
    /// At 124/127 the measured echo residual is about -37 dBFS (18 dB below
    /// the -19 dBFS reference) yet still above the old absolute -45 dBFS
    /// onset — that is exactly the false trigger the user heard. This test
    /// feeds a loud residual 18 dB below the reference and asserts silence.
    #[test]
    fn a_loud_residual_far_below_the_reference_is_echo_not_a_person() {
        let mut d = Detector::new(cfg_no_settle());
        let echo = sine(300.0, 80, 12000.0, 0.0); // ref ≈ -11.7 dBFS RMS
        let residual = sine(1500.0, 80, 1500.0, 0.7); // ≈ -29.8 dBFS, 18 dB down
        assert!(
            level_db(&residual) > -45.0,
            "the old absolute onset would have counted this"
        );
        for i in 0..6 {
            let out = d.process(&residual, &residual, &echo, true, true, t(i));
            assert_eq!(out, Outcome::None, "18 dB below the reference is echo");
        }
        assert_eq!(
            d.counters.onsets, 0,
            "relative criterion must reject the residual"
        );
        assert_eq!(d.counters.fired, 0);
    }

    /// R1 self-hearing: the AEC needs ~3 s to reach 15 dB from cold, and a
    /// false trigger cuts the answer at 1-1.6 s, so the filter never
    /// converges. The convergence gate keeps the first seconds honest and
    /// opens once echo-only blocks demonstrate the Step-3 suppression.
    #[test]
    fn cold_start_never_fires_until_the_aec_has_converged() {
        let cfg = Config {
            settle_ms: 0,
            converged_blocks: 5,
            ..Config::default()
        };
        let mut d = Detector::new(cfg);
        let echo = sine(300.0, 80, 12000.0, 0.0);
        // Cold filter: residual only ~6 dB below the reference, looks like
        // a person to any relative criterion.
        let cold_residual = sine(300.0, 80, 6000.0, 0.0);
        let speech = sine(1500.0, 80, 12000.0, 0.7);
        let mut clock = 700u64;
        let mut tick = move || {
            clock += 80;
            clock
        };
        for _ in 0..3 {
            match d.process(&cold_residual, &cold_residual, &echo, true, true, tick()) {
                Outcome::NotConverged { .. } => {}
                Outcome::None => {}
                other => panic!("cold filter must not fire: {other:?}"),
            }
        }
        assert_eq!(d.counters.fired, 0);
        assert_eq!(d.counters.not_converged, 1, "logged once per episode");
        // Warm-up: echo-only blocks with >=15 dB suppression.
        let quiet = sine(300.0, 80, 240.0, 0.0); // 34 dB below the reference
        for _ in 0..5 {
            assert_eq!(
                d.process(&quiet, &echo, &echo, true, true, tick()),
                Outcome::None
            );
        }
        assert!(
            d.converged >= 5,
            "echo-only blocks must open the gate: {}",
            d.converged
        );
        // A person now raises the residual; the gate is open and the
        // uncorrelated window fires after two guard passes.
        let mut fired = false;
        for _ in 0..3 {
            match d.process(&speech, &speech, &echo, true, true, tick()) {
                Outcome::Fire {
                    residual_db,
                    ref_db,
                    energy_db,
                    ..
                } => {
                    fired = true;
                    assert!(
                        residual_db > -15.0,
                        "a near-end fact sits within 15 dB of the reference, got {residual_db}"
                    );
                    // Sign convention (R1d): residual is energy - ref, so a
                    // person louder than the playback gives a POSITIVE value.
                    assert!((energy_db - ref_db - residual_db).abs() < 1e-3);
                    break;
                }
                Outcome::None | Outcome::GuardSuppress { .. } => {}
                other => panic!("unexpected after convergence: {other:?}"),
            }
        }
        assert!(fired, "a person after convergence must fire");
    }

    /// R1d (2026-11-12 field report "shouting louder interrupts worse"):
    /// a person 25 dB ABOVE the playback is a person. The reported residual
    /// is energy - ref, so this fact carries a large POSITIVE number; when
    /// this field carried suppression (ref - energy) the same voice looked
    /// like "residual too low" and both the device and the core refused it.
    #[test]
    fn a_person_louder_than_the_playback_is_still_a_person() {
        let cfg = Config {
            settle_ms: 0,
            converged_blocks: 2,
            ..Config::default()
        };
        let mut d = Detector::new(cfg);
        let echo = sine(300.0, 80, 1000.0, 0.0); // ~-30 dBFS reference
        let speech = sine(1500.0, 80, 14000.0, 0.7); // ~25 dB louder
        let tick = [80u64, 160, 240, 320, 400, 480];
        // Warm-up: echo-only blocks open the convergence gate.
        let quiet = sine(300.0, 80, 100.0, 0.0);
        for t in tick {
            let _ = d.process(&quiet, &quiet, &echo, true, true, t);
        }
        assert!(d.converged >= 2, "echo must open the gate: {}", d.converged);
        let mut fired = None;
        for t in [560u64, 640, 720] {
            if let Outcome::Fire {
                energy_db,
                ref_db,
                residual_db,
                ..
            } = d.process(&speech, &speech, &echo, true, true, t)
            {
                fired = Some((energy_db, ref_db, residual_db));
                break;
            }
        }
        let (energy_db, ref_db, residual_db) = fired.expect("a loud person must fire");
        assert!(
            residual_db > 15.0,
            "energy-render sign: the person is far ABOVE the reference (energy={energy_db:.1} ref={ref_db:.1} residual={residual_db:.1})"
        );
    }

    /// Candidate B (2026-11-12): a block that lost its reference match —
    /// even if a later frame relocked — may never produce a fact.
    #[test]
    fn a_degraded_reference_block_never_fires() {
        let mut d = Detector::new(cfg_no_settle());
        let echo = sine(300.0, 80, 3000.0, 0.0);
        let speech = sine(1500.0, 80, 12000.0, 0.7);
        d.set_ref_locked(false);
        let mut saw_unlocked = false;
        for i in 0..4 {
            match d.process(&speech, &speech, &echo, true, true, t(i)) {
                Outcome::RefUnlocked { .. } => saw_unlocked = true,
                Outcome::Fire { .. } => panic!("degraded reference must never fire"),
                _ => {}
            }
        }
        assert!(saw_unlocked, "the refusal must be visible in telemetry");
        assert_eq!(d.counters.ref_unlocked, 1);
        assert_eq!(d.counters.fired, 0);
    }

    #[test]
    fn the_relative_margin_is_the_step3_criterion() {
        let cfg = Config::default();
        assert_eq!(
            cfg.near_end_margin_db, 15.0,
            "R1 Step 3: residual >=15 dB below ref"
        );
        assert_eq!(
            cfg.converged_blocks, 5,
            "400 ms of demonstrated suppression"
        );
    }

    #[test]
    fn best_lag_corr_finds_shifted_copy() {
        // mic = ref delayed by 25 samples: the peak must appear within the
        // search range and stay near 1.
        let mut rng = Rng(99);
        let sig: Vec<i16> = (0..GUARD_WINDOW + 2 * GUARD_LAG)
            .map(|_| (rng.next() * 12000.0) as i16)
            .collect();
        let shift = 25;
        let ref_tail: VecDeque<i16> = sig.iter().copied().collect();
        let mic_tail: VecDeque<i16> = sig[shift..].iter().copied().collect();
        let c = best_lag_corr(&mic_tail, &ref_tail, GUARD_WINDOW);
        assert!(c > 0.9, "shifted copy must be found: {c}");
    }

    #[test]
    fn best_lag_corr_short_rings_return_zero() {
        let mic: VecDeque<i16> = vec![1i16; 10].into();
        let r: VecDeque<i16> = vec![1i16; 10].into();
        assert_eq!(best_lag_corr(&mic, &r, GUARD_WINDOW), 0.0);
    }

    #[test]
    fn silence_rings_return_zero() {
        let mic: VecDeque<i16> = vec![0i16; GUARD_WINDOW].into();
        let r: VecDeque<i16> = vec![0i16; GUARD_WINDOW + 2 * GUARD_LAG].into();
        assert_eq!(best_lag_corr(&mic, &r, GUARD_WINDOW), 0.0);
    }

    #[test]
    fn guard_now_suppresses_when_ref_unlocked() {
        let mut d = Detector::new(cfg_no_settle());
        let mic = sine(1500.0, GUARD_WINDOW, 10000.0, 0.0);
        let r = sine(300.0, GUARD_WINDOW + 2 * GUARD_LAG, 10000.0, 0.7);
        push_capped(&mut d.mic_ring, &mic, GUARD_WINDOW);
        push_capped(&mut d.ref_ring, &r, GUARD_WINDOW + 2 * GUARD_LAG);
        d.set_ref_locked(true);
        assert!(
            d.guard_now(t(1)) < 0.5,
            "uncorrelated speech with locked ref must pass guard"
        );
        d.set_ref_locked(false);
        assert_eq!(
            d.guard_now(t(2)),
            1.0,
            "unlocked reference must conservatively suppress playback-time KWS"
        );
    }

    #[test]
    fn echo_health_degraded_suppresses_bargein_fire_and_guard_now() {
        let mut d = Detector::new(cfg_no_settle());
        let speech = sine(1500.0, 80, 12000.0, 0.0);
        let r = sine(300.0, 80, 10000.0, 0.7);
        d.set_ref_locked(true);
        d.set_echo_health(EchoPathHealth::Degraded);
        for i in 0..4 {
            let out = d.process(&speech, &speech, &r, true, true, t(i));
            assert!(
                !matches!(out, Outcome::Fire { .. }),
                "Degraded echo health must suppress bargein fire: {out:?}"
            );
        }
        assert_eq!(d.guard_now(t(5)), 1.0);
        d.set_echo_health(EchoPathHealth::Unknown);
        assert_eq!(d.guard_now(t(6)), 1.0);
    }
}
