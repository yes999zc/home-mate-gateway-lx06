use super::biquad::Biquad;
use super::limiter::Limiter;
use super::RATE;

// --- M6c: the official MBCL (4-band compressor + limiters) ---------------
//
// Parameters from dot2c's FireOS `/vendor/etc/audio-algorithms/MBCL.cfg`:
// 4 bands split at 115 / 500 / 7500 Hz; band 1 (sub-bass) 20:1 at −50 dB and
// its limiter at −8 dB / 200 ms; bands 2-4 2:1 at −10 dB with limiters at
// 0 dB / 1 ms; the full band at −0.1 dB / 200 ms; PreFilter bypassed.
//
// The config does NOT carry compressor attack/release times (only the
// limiter release), so 5 ms attack / 120 ms release are our documented
// defaults. The split is a 4th-order Linkwitz-Riley tree (a magnitude-flat
// sum); the vendor's own filterbank topology is not replicated — the band
// edges and dynamics are.
const MBCL_FC: [f32; 3] = [115.0, 500.0, 7500.0];
const MBCL_THRESH_DB: [f32; 4] = [-50.0, -10.0, -10.0, -10.0];
const MBCL_RATIO: [f32; 4] = [20.0, 2.0, 2.0, 2.0];
const MBCL_GAIN_MIN_DB: [f32; 4] = [-40.0; 4];
const MBCL_LIM_THRESH_DB: [f32; 4] = [-8.0, 0.0, 0.0, 0.0];
const MBCL_LIM_RELEASE_MS: [f32; 4] = [200.0, 1.0, 1.0, 1.0];
const MBCL_COMP_ATTACK_MS: f32 = 5.0;
const MBCL_COMP_RELEASE_MS: f32 = 120.0;

/// One 4th-order Linkwitz-Riley low/high half: two cascaded Butterworth
/// sections at the same corner (Q = 1/√2).
struct Lr4 {
    a: Biquad,
    b: Biquad,
}

impl Lr4 {
    fn low(f: f32) -> Lr4 {
        let q = std::f32::consts::FRAC_1_SQRT_2;
        Lr4 {
            a: Biquad::low_pass(f, q),
            b: Biquad::low_pass(f, q),
        }
    }
    fn high(f: f32) -> Lr4 {
        let q = std::f32::consts::FRAC_1_SQRT_2;
        Lr4 {
            a: Biquad::high_pass(f, q),
            b: Biquad::high_pass(f, q),
        }
    }
    #[inline]
    fn tick(&mut self, x: f32) -> f32 {
        self.b.tick(self.a.tick(x))
    }
    fn reset(&mut self) {
        self.a.reset();
        self.b.reset();
    }
}

/// The EXACT allpass of an LR4 split: `LP4(z) + HP4(z)`. It is not two
/// 2nd-order RBJ allpasses (that is a different phase), and the difference
/// is what a cascaded three-way tree needs to sum back to a magnitude-flat
/// signal. Computed at construction from the LR4 branch polynomials.
struct AllPass4 {
    b: [f32; 5],
    a: [f32; 4], // a1..a4; a0 = 1
    x: [f32; 4],
    y: [f32; 4],
}

impl AllPass4 {
    fn new(f0: f32) -> AllPass4 {
        let q = std::f32::consts::FRAC_1_SQRT_2;
        let w0 = 2.0 * std::f32::consts::PI * f0 / RATE;
        let cw = w0.cos();
        let sw = w0.sin();
        let al = sw / (2.0 * q);
        let a0 = 1.0 + al;
        let lp = [
            (1.0 - cw) / (2.0 * a0),
            (1.0 - cw) / a0,
            (1.0 - cw) / (2.0 * a0),
        ];
        let hp = [
            (1.0 + cw) / (2.0 * a0),
            -(1.0 + cw) / a0,
            (1.0 + cw) / (2.0 * a0),
        ];
        let d = [1.0, -2.0 * cw / a0, (1.0 - al) / a0];
        let conv = |p: &[f32; 3], r: &[f32; 3]| {
            [
                p[0] * r[0],
                p[0] * r[1] + p[1] * r[0],
                p[0] * r[2] + p[1] * r[1] + p[2] * r[0],
                p[1] * r[2] + p[2] * r[1],
                p[2] * r[2],
            ]
        };
        let l2 = conv(&lp, &lp);
        let h2 = conv(&hp, &hp);
        let num = [
            l2[0] + h2[0],
            l2[1] + h2[1],
            l2[2] + h2[2],
            l2[3] + h2[3],
            l2[4] + h2[4],
        ];
        let den = conv(&d, &d);
        AllPass4 {
            b: num,
            a: [den[1], den[2], den[3], den[4]],
            x: [0.0; 4],
            y: [0.0; 4],
        }
    }

    #[inline]
    fn tick(&mut self, x: f32) -> f32 {
        let y = self.b[0] * x
            + self.b[1] * self.x[0]
            + self.b[2] * self.x[1]
            + self.b[3] * self.x[2]
            + self.b[4] * self.x[3]
            - self.a[0] * self.y[0]
            - self.a[1] * self.y[1]
            - self.a[2] * self.y[2]
            - self.a[3] * self.y[3];
        self.x.rotate_right(1);
        self.x[0] = x;
        self.y.rotate_right(1);
        self.y[0] = y;
        y
    }

    fn reset(&mut self) {
        self.x = [0.0; 4];
        self.y = [0.0; 4];
    }
}

/// One band compressor: peak envelope → static curve → smoothed gain (dB).
struct BandComp {
    th_db: f32,
    inv_ratio: f32,
    gain_min_db: f32,
    att: f32,
    rel: f32,
    env: f32,
    gain_db: f32,
}

impl BandComp {
    fn new(th_db: f32, ratio: f32, gain_min_db: f32) -> BandComp {
        let att = 1.0 - (-1.0 / (MBCL_COMP_ATTACK_MS / 1000.0 * RATE)).exp();
        let rel = 1.0 - (-1.0 / (MBCL_COMP_RELEASE_MS / 1000.0 * RATE)).exp();
        BandComp {
            th_db,
            inv_ratio: 1.0 / ratio,
            gain_min_db,
            att,
            rel,
            env: 0.0,
            gain_db: 0.0,
        }
    }

    #[inline]
    fn gain(&mut self, x: f32) -> f32 {
        let a = x.abs();
        self.env = if a > self.env {
            self.env + self.att * (a - self.env)
        } else {
            self.env + self.rel * (a - self.env)
        };
        let level_db = 20.0 * self.env.max(1e-9).log10();
        let over = level_db - self.th_db;
        let target = if over > 0.0 {
            (-(over * (1.0 - self.inv_ratio))).max(self.gain_min_db)
        } else {
            0.0
        };
        self.gain_db = if target < self.gain_db {
            self.gain_db + self.att * (target - self.gain_db)
        } else {
            self.gain_db + self.rel * (target - self.gain_db)
        };
        10f32.powf(self.gain_db / 20.0)
    }

    fn reset(&mut self) {
        self.env = 0.0;
        self.gain_db = 0.0;
    }
}

/// One band limiter: instantaneous reduction to the threshold, exponential
/// release. No look-ahead (the full-band limiter is the safety net).
struct BandLimiter {
    th: f32,
    rel: f32,
    gain: f32,
}

impl BandLimiter {
    fn new(th_db: f32, release_ms: f32) -> BandLimiter {
        BandLimiter {
            th: 10f32.powf(th_db / 20.0),
            rel: 1.0 - (-1.0 / (release_ms / 1000.0 * RATE)).exp(),
            gain: 1.0,
        }
    }

    #[inline]
    fn gain(&mut self, x: f32) -> f32 {
        let a = x.abs();
        let req = if a > self.th { self.th / a } else { 1.0 };
        self.gain = if req < self.gain {
            req
        } else {
            req + (self.gain - req) * self.rel
        };
        self.gain
    }

    fn reset(&mut self) {
        self.gain = 1.0;
    }
}

/// The official 4-band compressor/limiter, mono.
pub struct Mbcl {
    lp115: Lr4,
    hp115: Lr4,
    lp500: Lr4,
    hp500: Lr4,
    lp7500: Lr4,
    hp7500: Lr4,
    /// Phase compensation for the cascaded tree: band 1 passes the 500 Hz
    /// and 7500 Hz allpasses, band 2 the 7500 Hz one, so the four bands sum
    /// to the allpass of the input (magnitude-flat) instead of a coloured
    /// serial sum.
    ap500: AllPass4,
    ap7500_b1: AllPass4,
    ap7500_b2: AllPass4,
    comp: [BandComp; 4],
    lim: [BandLimiter; 4],
    full: Limiter,
}

impl Default for Mbcl {
    fn default() -> Self {
        Self::new()
    }
}

impl Mbcl {
    pub fn new() -> Mbcl {
        Mbcl {
            lp115: Lr4::low(MBCL_FC[0]),
            hp115: Lr4::high(MBCL_FC[0]),
            lp500: Lr4::low(MBCL_FC[1]),
            hp500: Lr4::high(MBCL_FC[1]),
            lp7500: Lr4::low(MBCL_FC[2]),
            hp7500: Lr4::high(MBCL_FC[2]),
            ap500: AllPass4::new(MBCL_FC[1]),
            ap7500_b1: AllPass4::new(MBCL_FC[2]),
            ap7500_b2: AllPass4::new(MBCL_FC[2]),
            comp: [
                BandComp::new(MBCL_THRESH_DB[0], MBCL_RATIO[0], MBCL_GAIN_MIN_DB[0]),
                BandComp::new(MBCL_THRESH_DB[1], MBCL_RATIO[1], MBCL_GAIN_MIN_DB[1]),
                BandComp::new(MBCL_THRESH_DB[2], MBCL_RATIO[2], MBCL_GAIN_MIN_DB[2]),
                BandComp::new(MBCL_THRESH_DB[3], MBCL_RATIO[3], MBCL_GAIN_MIN_DB[3]),
            ],
            lim: [
                BandLimiter::new(MBCL_LIM_THRESH_DB[0], MBCL_LIM_RELEASE_MS[0]),
                BandLimiter::new(MBCL_LIM_THRESH_DB[1], MBCL_LIM_RELEASE_MS[1]),
                BandLimiter::new(MBCL_LIM_THRESH_DB[2], MBCL_LIM_RELEASE_MS[2]),
                BandLimiter::new(MBCL_LIM_THRESH_DB[3], MBCL_LIM_RELEASE_MS[3]),
            ],
            full: Limiter::with_params(-0.1, 5.0, 200.0),
        }
    }

    /// Split into the four bands (no dynamics) — the crossover's own test.
    #[inline]
    fn split(&mut self, x: f32) -> [f32; 4] {
        let h1 = self.hp115.tick(x);
        let h2 = self.hp500.tick(h1);
        let b1 = self.ap7500_b1.tick(self.ap500.tick(self.lp115.tick(x)));
        let b2 = self.ap7500_b2.tick(self.lp500.tick(h1));
        let b3 = self.lp7500.tick(h2);
        let b4 = self.hp7500.tick(h2);
        [b1, b2, b3, b4]
    }

    #[cfg(test)]
    fn split_only(&mut self, x: f32) -> [f32; 4] {
        self.split(x)
    }

    /// Split → per-band compressor → per-band limiter → sum.
    #[inline]
    pub fn band_sum(&mut self, x: f32) -> f32 {
        let bands = self.split(x);
        let mut y = 0.0;
        for i in 0..4 {
            let g = self.comp[i].gain(bands[i]);
            let l = self.lim[i].gain(bands[i] * g);
            y += bands[i] * g * l;
        }
        y
    }

    /// The full-band limiter over one block (official −0.1 dB / 200 ms).
    pub fn full_band(&mut self, x: &mut [f32]) {
        self.full.process(x);
    }

    pub fn reset(&mut self) {
        self.lp115.reset();
        self.hp115.reset();
        self.lp500.reset();
        self.hp500.reset();
        self.lp7500.reset();
        self.hp7500.reset();
        self.ap500.reset();
        self.ap7500_b1.reset();
        self.ap7500_b2.reset();
        for c in &mut self.comp {
            c.reset();
        }
        for l in &mut self.lim {
            l.reset();
        }
        self.full.reset();
    }

    pub fn pending(&self) -> usize {
        self.full.pending()
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{rms_db, sine_blocks};
    use super::*;

    /// The LR4 split reconstructs the signal: the sum of the four bands is
    /// magnitude-flat (allpass), so a steady sine keeps its level.
    #[test]
    fn mbcl_crossover_sums_flat() {
        for freq in [60.0, 200.0, 1000.0, 4000.0, 10000.0] {
            let mut m = Mbcl::new();
            let input = sine_blocks(freq, 1.0);
            let mut sum = Vec::with_capacity(input.len());
            for &v in &input {
                let b = m.split_only(v);
                sum.push(b.iter().sum::<f32>());
            }
            let skip = 4800;
            let before = rms_db(&input[skip..]);
            let after = rms_db(&sum[skip..]);
            assert!(
                (after - before).abs() < 0.5,
                "{freq} Hz: split sum {after:.2} dB vs input {before:.2} dB"
            );
        }
    }

    /// Band 1's static curve: 20:1 above −50 dB. A −20 dBFS tone must land
    /// ~28.5 dB down (30 dB over × (1 − 1/20)).
    #[test]
    fn mbcl_band1_static_curve() {
        let mut c = BandComp::new(-50.0, 20.0, -40.0);
        let mut gain_db = 0.0;
        for i in 0..48000 {
            let x = (2.0 * std::f32::consts::PI * 60.0 * i as f32 / RATE).sin() * 0.1;
            let g = c.gain(x);
            gain_db = 20.0 * g.log10();
        }
        assert!(
            (gain_db - (-28.5)).abs() < 1.5,
            "band 1 steady gain {gain_db:.2} dB, expected ~-28.5"
        );
        // The clamp holds at the configured minimum.
        let mut c = BandComp::new(-50.0, 20.0, -10.0);
        for i in 0..48000 {
            let x = (2.0 * std::f32::consts::PI * 60.0 * i as f32 / RATE).sin() * 0.5;
            let g = c.gain(x);
            gain_db = 20.0 * g.log10();
        }
        assert!(gain_db >= -10.01, "gain {gain_db:.2} dB below gain_min");
    }

    /// A band limiter holds its threshold on a steady tone (instant attack,
    /// exponential release).
    #[test]
    fn mbcl_band_limiter_holds_its_threshold() {
        let mut l = BandLimiter::new(-8.0, 200.0);
        let th = 10f32.powf(-8.0 / 20.0);
        let mut peak = 0f32;
        for i in 0..48000 {
            let x = (2.0 * std::f32::consts::PI * 60.0 * i as f32 / RATE).sin() * 0.9;
            let g = l.gain(x);
            peak = peak.max((x * g).abs());
        }
        assert!(
            peak <= th * 1.02,
            "band limiter peak {peak} vs threshold {th}"
        );
    }

    /// The MBCL's full-band stage is the official −0.1 dB limiter (checked
    /// directly: the band compressors normally keep the sum well below it).
    #[test]
    fn mbcl_full_band_limiter_ceiling() {
        let look = (5.0 * RATE / 1000.0) as usize;
        let mut lim = Limiter::with_params(-0.1, 5.0, 200.0);
        let n = RATE as usize;
        let mut x: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / RATE).sin() * 2.0)
            .collect();
        x.extend(std::iter::repeat_n(0.0f32, look));
        lim.process(&mut x);
        let peak = x[..n].iter().fold(0f32, |m, v| m.max(v.abs()));
        let ceil = 10f32.powf(-0.1 / 20.0);
        assert!(peak <= ceil * 1.0023, "full-band peak {peak} vs {ceil}");
    }
}
