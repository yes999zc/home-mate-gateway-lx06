use super::RATE;

/// RBJ audio-cookbook biquad, f32, one channel.
#[derive(Clone, Copy)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    pub fn peaking(freq: f32, q: f32, gain_db: f32) -> Biquad {
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * freq / RATE;
        let cw = w0.cos();
        let sw = w0.sin();
        let alpha = sw / (2.0 * q);
        let b0 = 1.0 + alpha * a;
        let b1 = -2.0 * cw;
        let b2 = 1.0 - alpha * a;
        let a0 = 1.0 + alpha / a;
        let a1 = -2.0 * cw;
        let a2 = 1.0 - alpha / a;
        Biquad {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    pub fn low_shelf(freq: f32, gain_db: f32) -> Biquad {
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * freq / RATE;
        let cw = w0.cos();
        let sw = w0.sin();
        // RBJ cookbook, S = 1 (maximally flat shelf edge): alpha =
        // sin(w0)/2 · sqrt(2), and the shelf terms carry 2·sqrt(A)·alpha.
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * sw * std::f32::consts::FRAC_1_SQRT_2;
        let b0 = a * ((a + 1.0) - (a - 1.0) * cw + two_sqrt_a_alpha);
        let b1 = 2.0 * a * ((a - 1.0) - (a + 1.0) * cw);
        let b2 = a * ((a + 1.0) - (a - 1.0) * cw - two_sqrt_a_alpha);
        let a0 = (a + 1.0) + (a - 1.0) * cw + two_sqrt_a_alpha;
        let a1 = -2.0 * ((a - 1.0) + (a + 1.0) * cw);
        let a2 = (a + 1.0) + (a - 1.0) * cw - two_sqrt_a_alpha;
        Biquad {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    /// 1st-order high-pass in biquad form (b2 = a2 = 0): the official
    /// curve's DC rolloff is gentle (−3.9 dB at 20 Hz), not a 55–70 Hz
    /// speaker-protection high-pass.
    pub fn high_pass_first_order(freq: f32) -> Biquad {
        let k = (std::f32::consts::PI * freq / RATE).tan();
        let n = 1.0 / (1.0 + k);
        Biquad {
            b0: n,
            b1: -n,
            b2: 0.0,
            a1: (k - 1.0) / (k + 1.0),
            a2: 0.0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    pub fn low_pass(freq: f32, q: f32) -> Biquad {
        let w0 = 2.0 * std::f32::consts::PI * freq / RATE;
        let cw = w0.cos();
        let sw = w0.sin();
        let alpha = sw / (2.0 * q);
        let b0 = (1.0 - cw) / 2.0;
        let b1 = 1.0 - cw;
        let b2 = (1.0 - cw) / 2.0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cw;
        let a2 = 1.0 - alpha;
        Biquad {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    pub fn high_pass(freq: f32, q: f32) -> Biquad {
        let w0 = 2.0 * std::f32::consts::PI * freq / RATE;
        let cw = w0.cos();
        let sw = w0.sin();
        let alpha = sw / (2.0 * q);
        let b0 = (1.0 + cw) / 2.0;
        let b1 = -(1.0 + cw);
        let b2 = (1.0 + cw) / 2.0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cw;
        let a2 = 1.0 - alpha;
        Biquad {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    /// RBJ high shelf (S = 1), the shape EchoMuse's biscuit voicing uses for
    /// the air band.
    pub fn high_shelf(freq: f32, gain_db: f32) -> Biquad {
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * freq / RATE;
        let cw = w0.cos();
        let sw = w0.sin();
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * sw * std::f32::consts::FRAC_1_SQRT_2;
        let b0 = a * ((a + 1.0) + (a - 1.0) * cw + two_sqrt_a_alpha);
        let b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cw);
        let b2 = a * ((a + 1.0) + (a - 1.0) * cw - two_sqrt_a_alpha);
        let a0 = (a + 1.0) - (a - 1.0) * cw + two_sqrt_a_alpha;
        let a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cw);
        let a2 = (a + 1.0) - (a - 1.0) * cw - two_sqrt_a_alpha;
        Biquad {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    /// 2nd-order allpass (RBJ, Q = 1/√2): what an LR4 crossover's halves sum
    /// to, so a band that skips a crossover can be phase-aligned with one.
    pub fn all_pass(freq: f32, q: f32) -> Biquad {
        let w0 = 2.0 * std::f32::consts::PI * freq / RATE;
        let cw = w0.cos();
        let sw = w0.sin();
        let alpha = sw / (2.0 * q);
        let b0 = 1.0 - alpha;
        let b1 = -2.0 * cw;
        let b2 = 1.0 + alpha;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cw;
        let a2 = 1.0 - alpha;
        Biquad {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    /// Clear the filter state (stream end): a new play must not start with
    /// the previous one's tail.
    pub(super) fn reset(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.y1 = 0.0;
        self.y2 = 0.0;
    }

    #[inline]
    pub(super) fn tick(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }

    /// Magnitude response at `freq` (state-independent, for tests).
    pub fn magnitude_db_at(&self, freq: f32) -> f32 {
        let w = 2.0 * std::f32::consts::PI * freq / RATE;
        let z1 = complex_exp(-w); // z^-1
        let num = self.b0 + self.b1 * z1.0 + self.b2 * (z1.0 * z1.0 - z1.1 * z1.1);
        let num_im = self.b1 * z1.1 + self.b2 * 2.0 * z1.0 * z1.1;
        let den = 1.0 + self.a1 * z1.0 + self.a2 * (z1.0 * z1.0 - z1.1 * z1.1);
        let den_im = self.a1 * z1.1 + self.a2 * 2.0 * z1.0 * z1.1;
        20.0 * ((num * num + num_im * num_im) / (den * den + den_im * den_im))
            .sqrt()
            .log10()
    }
}

/// e^{-jw} as (re, im).
fn complex_exp(w: f32) -> (f32, f32) {
    (w.cos(), -w.sin())
}
