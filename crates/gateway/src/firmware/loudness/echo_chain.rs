use super::biquad::Biquad;
use super::limiter::Limiter;
use super::{CustomBand, RATE};

// --- EchoMuse-aligned voicing (2026-11-14) -------------------------------
//
// Ported from wilbowes/EchoMuse's biscuit voicing (device/pkg/board/
// voicing.go `biscuitVoicing`, device/internal/outchain/{eq,bassguard,
// stocksound}.go) so the two projects can be compared and tuned together.
// Their numbers are fitted to this board's own driver sweep (#247) and then
// picked by ear; we reuse them verbatim instead of re-deriving them.
//
// Chain: 80 Hz LR4 high-pass + 65 Hz notch → driver correction (4 peaks +
// air shelf) → bass guard (LR4 115 Hz, the stock MBCL's band 1) → three
// upper-band compressors (LR4 500/7500) → −1 dB limiter, with their +10 dB
// volume ceiling so full volume drives the compressors, not the DAC.
const EM_CORRECTION: [(f32, f32, f32); 4] = [
    (174.9, 26.13, 0.388),
    (3277.5, -10.94, 0.753),
    (576.3, -10.93, 0.735),
    (8707.8, 5.52, 2.985),
];
const EM_SHELF: (f32, f32) = (11874.4, 12.0);
const EM_HPF_HZ: f32 = 80.0;
const EM_NOTCH: (f32, f32, f32) = (65.0, -25.0, 4.0);
const EM_CEILING_DB: f32 = 10.0;
const EM_CROSSOVER: [f32; 2] = [500.0, 7500.0];
/// (ratio, threshold dB, attack ms) for the three bands above the guard.
const EM_UPPER: [(f32, f32, f32); 3] = [(3.0, -16.0, 20.0), (2.0, -10.0, 10.0), (2.0, -10.0, 10.0)];
const EM_UPPER_FLOOR_DB: f32 = -40.0;
const EM_UPPER_HOLD_MS: f32 = 30.0;
const EM_UPPER_RELEASE_MS: f32 = 100.0;
const EM_BASS: (f32, f32, f32) = (20.0, -50.0, 200.0);
const EM_BASS_FLOOR_DB: f32 = -40.0;
const EM_BASS_HOLD_MS: f32 = 20.0;
const EM_LIMIT_DB: f32 = -1.0;
const EM_LIMIT_RELEASE_MS: f32 = 150.0;
/// Release is expressed as "time to recover 10 dB", so one number means the
/// same thing at 1 dB and 12 dB of reduction (their `releaseReferenceDb`).
const EM_RELEASE_REFERENCE_DB: f32 = 10.0;

/// One compressor band of the EchoMuse chain: static law → trailing minimum
/// over the hold window → (optional) attack one-pole → instant down,
/// slew-limited up. `attack_ms == 0` is the bass guard's instant attack.
struct EchoComp {
    inv_ratio: f32,
    th_db: f32,
    floor_db: f32,
    attack: f32,
    slew: f32,
    hold: std::collections::VecDeque<(u64, f32)>,
    hold_n: u64,
    t: u64,
    smooth_db: f32,
    gain_db: f32,
    max_reduction_db: f32,
}

impl EchoComp {
    fn new(
        ratio: f32,
        th_db: f32,
        floor_db: f32,
        attack_ms: f32,
        hold_ms: f32,
        release_ms: f32,
    ) -> Self {
        let hold_n = ((hold_ms / 1000.0 * RATE).round() as u64).max(1);
        EchoComp {
            inv_ratio: 1.0 / ratio,
            th_db,
            floor_db,
            attack: if attack_ms > 0.0 {
                1.0 - (-1.0 / (attack_ms / 1000.0 * RATE)).exp()
            } else {
                1.0 // instant
            },
            slew: EM_RELEASE_REFERENCE_DB / (release_ms.max(0.1) / 1000.0) / RATE,
            hold: std::collections::VecDeque::with_capacity(hold_n.min(4096) as usize),
            hold_n,
            t: 0,
            smooth_db: 0.0,
            gain_db: 0.0,
            max_reduction_db: 0.0,
        }
    }

    #[inline]
    fn gain(&mut self, x: f32) -> f32 {
        let a = x.abs();
        let mut target = 0.0;
        if a > 1e-9 {
            let level_db = 20.0 * a.log10();
            if level_db > self.th_db {
                target = (-(level_db - self.th_db) * (1.0 - self.inv_ratio)).max(self.floor_db);
            }
        }
        target = self.push_hold(target);
        self.smooth_db = self.attack * target + (1.0 - self.attack) * self.smooth_db;
        self.gain_db = self.smooth_db.min(self.gain_db + self.slew);
        let r = -self.gain_db;
        if r > self.max_reduction_db {
            self.max_reduction_db = r;
        }
        10f32.powf(self.gain_db / 20.0)
    }

    /// Trailing minimum over the hold window (monotonic deque, O(1)
    /// amortised) — the reduction cannot recover between a waveform's peaks.
    #[inline]
    fn push_hold(&mut self, v: f32) -> f32 {
        while let Some(&(_, back)) = self.hold.back() {
            if back >= v {
                self.hold.pop_back();
            } else {
                break;
            }
        }
        self.hold.push_back((self.t, v));
        while let Some(&(t, _)) = self.hold.front() {
            if t + self.hold_n <= self.t {
                self.hold.pop_front();
            } else {
                break;
            }
        }
        self.t += 1;
        self.hold.front().map(|&(_, v)| v).unwrap_or(0.0).min(0.0)
    }

    fn reset(&mut self) {
        self.hold.clear();
        self.t = 0;
        self.smooth_db = 0.0;
        self.gain_db = 0.0;
    }
}

/// The whole EchoMuse voicing for one mono stream.
pub struct EchoChain {
    // Correction: two cascaded Butterworth sections are the LR4 high-pass.
    hpf: [Biquad; 2],
    notch: Biquad,
    corr: Vec<Biquad>,
    // Split tree: the bass guard's LR4 at 115 Hz, then LR4 at 500/7500.
    lp115: [Biquad; 2],
    hp115: [Biquad; 2],
    lp500: [Biquad; 2],
    hp500: [Biquad; 2],
    lp7500: [Biquad; 2],
    hp7500: [Biquad; 2],
    // Phase alignment for the bands that skip a crossover.
    ap500_low: Biquad,
    ap7500_low: Biquad,
    ap7500_b2: Biquad,
    bass: EchoComp,
    upper: [EchoComp; 3],
    /// The user's own bands, on top of the voicing (EchoMuse's model: a
    /// fixed correction the user cannot reach, and a taste EQ they can).
    user: Vec<Biquad>,
    scalar: f32,
    limiter: Limiter,
    /// True while the 3.5 mm jack is the sound path: the internal driver's
    /// voicing is bypassed (see `step`).
    ext: bool,
    /// 1.0 = full level; a plug/unplug ramps it to 0, swaps the path and
    /// ramps back (40 ms total) so the switch never clicks.
    fade: f32,
    fade_step: f32,
}

impl EchoChain {
    pub fn new(user: &[CustomBand]) -> EchoChain {
        let butter = |fc: f32| {
            (
                [
                    Biquad::low_pass(fc, std::f32::consts::FRAC_1_SQRT_2),
                    Biquad::low_pass(fc, std::f32::consts::FRAC_1_SQRT_2),
                ],
                [
                    Biquad::high_pass(fc, std::f32::consts::FRAC_1_SQRT_2),
                    Biquad::high_pass(fc, std::f32::consts::FRAC_1_SQRT_2),
                ],
            )
        };
        let (lo115, hi115) = butter(115.0);
        let (lo500, hi500) = butter(EM_CROSSOVER[0]);
        let (lo7500, hi7500) = butter(EM_CROSSOVER[1]);
        let mut corr: Vec<Biquad> = EM_CORRECTION
            .iter()
            .map(|&(f, g, q)| Biquad::peaking(f, q, g))
            .collect();
        corr.push(Biquad::high_shelf(EM_SHELF.0, EM_SHELF.1));
        EchoChain {
            hpf: [
                Biquad::high_pass(EM_HPF_HZ, std::f32::consts::FRAC_1_SQRT_2),
                Biquad::high_pass(EM_HPF_HZ, std::f32::consts::FRAC_1_SQRT_2),
            ],
            notch: Biquad::peaking(EM_NOTCH.0, EM_NOTCH.2, EM_NOTCH.1),
            corr,
            lp115: lo115,
            hp115: hi115,
            lp500: lo500,
            hp500: hi500,
            lp7500: lo7500,
            hp7500: hi7500,
            ap500_low: Biquad::all_pass(EM_CROSSOVER[0], std::f32::consts::FRAC_1_SQRT_2),
            ap7500_low: Biquad::all_pass(EM_CROSSOVER[1], std::f32::consts::FRAC_1_SQRT_2),
            ap7500_b2: Biquad::all_pass(EM_CROSSOVER[1], std::f32::consts::FRAC_1_SQRT_2),
            bass: EchoComp::new(
                EM_BASS.0,
                EM_BASS.1,
                EM_BASS_FLOOR_DB,
                0.0,
                EM_BASS_HOLD_MS,
                EM_BASS.2,
            ),
            upper: [
                EchoComp::new(
                    EM_UPPER[0].0,
                    EM_UPPER[0].1,
                    EM_UPPER_FLOOR_DB,
                    EM_UPPER[0].2,
                    EM_UPPER_HOLD_MS,
                    EM_UPPER_RELEASE_MS,
                ),
                EchoComp::new(
                    EM_UPPER[1].0,
                    EM_UPPER[1].1,
                    EM_UPPER_FLOOR_DB,
                    EM_UPPER[1].2,
                    EM_UPPER_HOLD_MS,
                    EM_UPPER_RELEASE_MS,
                ),
                EchoComp::new(
                    EM_UPPER[2].0,
                    EM_UPPER[2].1,
                    EM_UPPER_FLOOR_DB,
                    EM_UPPER[2].2,
                    EM_UPPER_HOLD_MS,
                    EM_UPPER_RELEASE_MS,
                ),
            ],
            user: user
                .iter()
                .map(|b| {
                    Biquad::peaking(
                        b.freq.clamp(20.0, RATE / 2.0 - 100.0),
                        b.q.clamp(0.3, 8.0),
                        b.gain_db,
                    )
                })
                .collect(),
            scalar: 10f32.powf(EM_CEILING_DB / 20.0),
            limiter: Limiter::with_params(EM_LIMIT_DB, 5.0, EM_LIMIT_RELEASE_MS),
            ext: false,
            fade: 1.0,
            fade_step: 1.0 / (0.020 * RATE),
        }
    }

    #[inline]
    fn step(&mut self, x: f32) -> f32 {
        // Path switch (plug/unplug), 40 ms total: ramp to silence, swap, ramp
        // back. FIELD 2026-11-15 20:4x (dot2-e): the internal driver's
        // correction (+26 dB @175 Hz, −11 dB @576 Hz and @3.3 kHz) fed to a
        // real external speaker is exactly "发闷" — the presence band is
        // scooped out. Flat + limiter is the honest signal for the jack.
        let target = crate::firmware::jack::external();
        if target != self.ext {
            self.fade -= self.fade_step;
            if self.fade <= 0.0 {
                self.fade = 0.0;
                self.ext = target;
            }
        } else if self.fade < 1.0 {
            self.fade = (self.fade + self.fade_step).min(1.0);
        }

        // Correction first: high-pass, notch, driver correction, air shelf.
        let mut s = self.hpf[1].tick(x);
        s = self.hpf[0].tick(s);
        s = self.notch.tick(s);
        for b in self.corr.iter_mut() {
            s = b.tick(s);
        }
        // Bass guard + three upper bands, phase-aligned.
        let low = {
            let a = self.lp115[0].tick(s);
            self.lp115[1].tick(a)
        };
        let high = {
            let a = self.hp115[0].tick(s);
            self.hp115[1].tick(a)
        };
        let b2 = {
            let a = self.lp500[0].tick(high);
            let a = self.lp500[1].tick(a);
            self.ap7500_b2.tick(a)
        };
        let rest = {
            let a = self.hp500[0].tick(high);
            self.hp500[1].tick(a)
        };
        let b3 = {
            let a = self.lp7500[0].tick(rest);
            self.lp7500[1].tick(a)
        };
        let b4 = {
            let a = self.hp7500[0].tick(rest);
            self.hp7500[1].tick(a)
        };
        let low = {
            let a = self.ap500_low.tick(low);
            self.ap7500_low.tick(a)
        };
        // Every compressor is ticked even while the jack is occupied: a few
        // multiplications keep their holds/ceilings current, so unplugging
        // never exposes a stale gain (the same reason the guard was ticked
        // before this path existed).
        let g1 = self.bass.gain(low);
        let g2 = self.upper[0].gain(b2);
        let g3 = self.upper[1].gain(b3);
        let g4 = self.upper[2].gain(b4);
        let voiced = (low * g1 + b2 * g2 + b3 * g3 + b4 * g4) * self.scalar;
        let out = if self.ext {
            // External: the raw signal (limiter in process() is the only
            // processing). The user's taste EQ rides the internal voicing it
            // was chosen against, so it stays on that path; it is ticked here
            // on a discarded copy to keep its state fresh.
            let mut keep_fresh = x;
            for b in self.user.iter_mut() {
                keep_fresh = b.tick(keep_fresh);
            }
            x
        } else {
            let mut o = voiced;
            for b in self.user.iter_mut() {
                o = b.tick(o);
            }
            o
        };
        out * self.fade
    }

    pub(super) fn process(&mut self, x: &mut [f32]) {
        for v in x.iter_mut() {
            *v = self.step(*v);
        }
        self.limiter.process(x);
    }

    pub(super) fn reset(&mut self) {
        for f in [
            &mut self.hpf,
            &mut self.lp115,
            &mut self.hp115,
            &mut self.lp500,
            &mut self.hp500,
            &mut self.lp7500,
            &mut self.hp7500,
        ] {
            f[0].reset();
            f[1].reset();
        }
        self.notch.reset();
        for b in self.corr.iter_mut() {
            b.reset();
        }
        for a in [
            &mut self.ap500_low,
            &mut self.ap7500_low,
            &mut self.ap7500_b2,
        ] {
            a.reset();
        }
        for b in self.user.iter_mut() {
            b.reset();
        }
        self.bass.reset();
        for u in self.upper.iter_mut() {
            u.reset();
        }
        self.limiter.reset();
    }

    pub(super) fn pending(&self) -> usize {
        self.limiter.pending()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Their bass guard is the stock MBCL band 1: 20:1 from −50 dBFS. An
    /// input at −20 dBFS must settle near −28.5 dB of reduction.
    #[test]
    fn the_echo_muse_bass_guard_follows_the_stock_band_one_law() {
        let mut c = EchoComp::new(
            EM_BASS.0,
            EM_BASS.1,
            EM_BASS_FLOOR_DB,
            0.0,
            EM_BASS_HOLD_MS,
            EM_BASS.2,
        );
        let amp = 10f32.powf(-20.0 / 20.0);
        let mut g = 1.0;
        for _ in 0..RATE as usize {
            g = c.gain(amp);
        }
        let db = 20.0 * g.log10();
        assert!(
            (db + 28.5).abs() < 3.0,
            "guard gain {db} dB, expected about -28.5"
        );
    }
}
