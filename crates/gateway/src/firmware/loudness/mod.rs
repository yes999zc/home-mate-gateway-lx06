//! Output loudness stage (D stage, 2026-11-10): the RUI Local dot2 plug
//! rebuilds the stock FireOS volume-indexed EQ + limiter that the flat
//! EchoMuse chain lost (~10-14 dB quieter than stock at full volume —
//! measured, rui-dot2-bridge dsp.py). Our firmware had no such stage, so
//! TTS speech sounded small next to music (user report 2026-11-09: "the
//! problem is not loud music, it is quiet speech").
//!
//! Parameter migration (rui-dot2-bridge `dsp.py`, corrected ladder
//! 2026-10-26, owner A/B approved): six tiers keyed on the device level's
//! percent (level*100/127, boundaries 50/60/70/80/90/100), each tier =
//! bass shelf @350 Hz (9.0 dB at EQ_50 falling to 6.0 at EQ_100) +
//! presence bell @4.2 kHz (0.8 rising to 3.5 dB) + 2nd-order roll-off
//! below 70 Hz (the driver cannot reproduce it; feeding the limiter with
//! sub-bass only pumps it) + absolute 1 kHz gain ladder (+2.0 at EQ_50 →
//! +10.0 dB at EQ_100) + trim −2 dB, then a look-ahead limiter
//! (ceiling −1 dBFS, 5 ms lookahead, 80 ms release, hard clip −0.1 dB).
//!
//! Deviation from RUI (honest, constraint-driven): RUI realises the tier
//! as a 1024-tap LINEAR-PHASE FIR on the controller (x86); this port runs
//! on the armv7 speaker next to the AEC, so the same magnitude target is
//! realised as three RBJ biquads per channel (mono here — the mix is
//! duplicated to stereo after processing). ~15 MACs/sample vs ~100 M/s
//! for the FIR; minimum-phase instead of linear-phase, inaudible on a
//! mono speech speaker. The 1 kHz gain is exact (scalar), shelf/bell
//! magnitudes match within a fraction of a dB (unit-tested).
//!
//! Placement: AFTER the channel mix and software volume gain, BEFORE the
//! dealias reference feed and the DAC write — the AEC reference must see
//! the signal that actually plays, so the canceller follows the new curve
//! automatically.
//!
//! Layout (issue #27): `biquad` (the RBJ filters), `limiter` (the look-ahead
//! limiter), `echo_chain` (the EchoMuse voicing) and `mbcl` (the official
//! 4-band chain) hold the blocks this file used to carry; the types are
//! re-exported below so `loudness::Limiter` and friends keep their old paths.
//! This file keeps the curve, the tier ladder and `Loudness` itself.

mod biquad;
mod echo_chain;
mod limiter;
mod mbcl;

pub use biquad::Biquad;
pub use echo_chain::EchoChain;
pub use limiter::Limiter;
pub use mbcl::Mbcl;

/// Volume-tier boundaries in percent (level*100/127), RUI stock ladder.
const TIER_BOUNDARIES: [f32; 6] = [50.0, 60.0, 70.0, 80.0, 90.0, 100.0];
/// Corrected-ladder targets at EQ_50 (tier 0) and EQ_100 (tier 5).
const BASS_DB: [f32; 2] = [9.0, 6.0];
const PRESENCE_DB: [f32; 2] = [0.8, 3.5];
const GAIN_DB: [f32; 2] = [2.0, 10.0];
const TRIM_DB: f32 = -2.0;

const BASS_HZ: f32 = 350.0;
const PRESENCE_HZ: f32 = 4200.0;
/// Gaussian bell width in octaves (RUI LADDER_PRESENCE_OCTAVES) → peaking Q.
#[cfg(test)]
const PRESENCE_OCTAVES: f32 = 1.05;
const ROLLOFF_HZ: f32 = 70.0;

// The look-ahead limiter's own three parameters live in `limiter`; the hard
// clip is applied there but is also pinned by this stage's tests, so it stays.
const HARD_CLIP_DB: f32 = -0.1;

pub const RATE: f32 = 48_000.0;

// --- M6c: the OFFICIAL FireOS curve (2026-11-13) -------------------------
//
// Measured from dot2c's FireOS 5.1.1 `/vendor/etc/audio-algorithms/EQ_50.cfg`
// (1024-tap 48 kHz FIR, magnitude taken relative to 1 kHz). The vendor file
// is NOT copied: this is an 8-section RBJ peaking fit to the measured
// response, after a 1st-order 18 Hz high-pass (the official DC rolloff).
// Fit quality vs the measurement: max |error| 4.3 dB, RMS 1.1 dB over
// 80 Hz–12 kHz; the probes in 100 Hz–8 kHz are within 1.3 dB. (f0 Hz, Q,
// gain dB).
const OFFICIAL_SECTIONS: [(f32, f32, f32); 8] = [
    (169.323, 0.517, 28.739),
    (616.381, 1.212, -9.582),
    (2900.623, 0.869, -14.765),
    (4788.247, 2.578, 4.803),
    (15696.748, 0.489, 1.855),
    (12404.077, 3.5, -9.0),
    (8640.43, 1.216, 7.097),
    (6296.49, 3.5, -12.0),
];
/// The official 1 kHz scalar per volume tier, relative to EQ_50. Tiers
/// 50/60/70 are byte-identical in the vendor files (0 dB); 80/90/100 are
/// the same curve scaled by 1.5/2.383/5.335 (+3.52/+7.54/+14.54 dB).
const OFFICIAL_TIER_DB: [f32; 6] = [0.0, 0.0, 0.0, 3.52, 7.54, 14.54];
/// EQ_50's own 1 kHz level (the base of the absolute ladder).
const OFFICIAL_BASE_DB: f32 = -0.86;
/// The fitted chain's 1 kHz residual (it sits 0.8 dB below the target at
/// 1 kHz); compensated in the scalar so the absolute 1 kHz is exact.
const OFFICIAL_FIT_1K_DB: f32 = -0.8;

/// Which loudness curve the stage applies. `Rui` = the 2026-10-26
/// reverse-engineered ladder (the previous implementation); `Official` =
/// the M6c measurement-derived curve. Selected at construction (env).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Curve {
    Rui,
    Official,
    /// A user-defined peaking chain (M6c follow-on, 2026-11-14): flat +
    /// the listed bands + the limiter, with NO tier ladder and no MBCL.
    /// The user is choosing the tone; the volume knob only sets the level.
    /// Same parameterisation the console editor will drive.
    Custom,
    /// The EchoMuse voicing for this exact board (2026-11-14): their driver
    /// correction, the stock MBCL's band 1 as a bass guard, three upper-band
    /// compressors, and their +10 dB volume ceiling. Numbers are theirs,
    /// copied verbatim so the two projects can be compared and tuned
    /// together (device/pkg/board/voicing.go `biscuitVoicing`).
    EchoMuse,
}

/// One peaking band of a user curve: centre, gain and Q. `freq` is clamped
/// to 20 Hz–Nyquist and `q` to 0.3–8 by the caller (CLI or config), because
/// out-of-range values would make the biquad unstable or inaudible.
#[derive(Clone, Copy, Debug)]
pub struct CustomBand {
    pub freq: f32,
    pub gain_db: f32,
    pub q: f32,
}

/// Map a device level (0–127) to the RUI tier index (0 = EQ_50 … 5 = EQ_100).
pub fn tier_for_level(level: u64) -> usize {
    let percent = (level.min(127) as f32) * 100.0 / 127.0;
    for (i, b) in TIER_BOUNDARIES.iter().enumerate() {
        if percent <= *b {
            return i;
        }
    }
    TIER_BOUNDARIES.len() - 1
}

/// Per-tier target parameters (interpolated like RUI `ladder_params`).
pub fn tier_params(tier: usize) -> (f32, f32, f32) {
    let f = (tier.min(5)) as f32 / 5.0;
    let lerp = |v: [f32; 2]| v[0] + (v[1] - v[0]) * f;
    (lerp(BASS_DB), lerp(PRESENCE_DB), lerp(GAIN_DB))
}

/// The full loudness stage for the mono mixed signal.
pub struct Loudness {
    enabled: bool,
    curve: Curve,
    /// Scale on the bass shelf (D 1.1 A/B: the shelf is +6…9 dB at 350 Hz,
    /// which is the band a small driver reproduces least linearly and the
    /// one that costs the most limiter headroom. 0 = shelf bypassed.)
    /// RUI curve only.
    bass_scale: f32,
    level: u64,
    hpf: Biquad,
    bass: Biquad,
    presence: Biquad,
    scalar: f32,
    /// The official curve's cascade (HP + 8 fitted peaking sections).
    official: Vec<Biquad>,
    official_scalar: f32,
    /// Curve::Custom's peaking chain (rebuilt only at construction).
    custom: Vec<Biquad>,
    /// Curve::EchoMuse's whole voicing (correction + compression + limiter).
    em: Option<EchoChain>,
    /// The user's taste EQ on top of the selected curve (not used by
    /// Curve::Custom, where the bands ARE the curve).
    user: Vec<Biquad>,
    /// The official MBCL (4-band compressor/limiters + full-band limiter).
    /// Some only with the official curve AND `HM_LOUDNESS_MBCL=on`; when it
    /// is on, it REPLACES the broadband limiter (double limiting would not
    /// be the stock chain).
    mbcl: Option<Mbcl>,
    limiter: Limiter,
}

/// Resolve the loudness chain: explicit value (CLI) > env seam > default.
/// Split out so the boot log can print the SAME decision the audio thread
/// made instead of re-deriving it.
pub fn resolve_chain(curve: Option<Curve>, mbcl: Option<bool>) -> (Curve, bool) {
    let curve = curve.unwrap_or_else(
        || match std::env::var("HM_LOUDNESS_CURVE").ok().as_deref() {
            Some("official") => Curve::Official,
            _ => Curve::Rui,
        },
    );
    let mbcl = mbcl.unwrap_or_else(|| {
        matches!(
            std::env::var("HM_LOUDNESS_MBCL").ok().as_deref(),
            Some("on") | Some("1")
        )
    });
    // The MBCL only exists with the official curve (stock chain).
    (curve, mbcl && curve == Curve::Official)
}

impl Loudness {
    /// Production constructor: the curve comes from `HM_LOUDNESS_CURVE`
    /// (`official` | `rui`), default RUI. The env seam keeps the A/B a
    /// supervisor-line change instead of a rebuild, and keeps the default
    /// behaviour byte-identical to the shipped RUI stage.
    pub fn new(enabled: bool) -> Loudness {
        let (curve, mbcl) = resolve_chain(None, None);
        Self::new_with_chain(enabled, curve, mbcl)
    }

    /// Config-aware constructor: an explicit CLI value (`--loudness-curve`,
    /// `--loudness-mbcl`) wins over the env seam, which wins over the RUI
    /// default. Both exist because the A/B must be a supervisor-line change
    /// (`$DIR/barge-args` / the script's env) and never a rebuild.
    pub fn from_config(
        enabled: bool,
        curve: Option<Curve>,
        mbcl: Option<bool>,
        bands: &[CustomBand],
    ) -> Loudness {
        let (curve, mbcl) = resolve_chain(curve, mbcl);
        let mut l = Self::new_with_chain(enabled, curve, mbcl);
        if curve == Curve::EchoMuse {
            l.em = Some(EchoChain::new(bands));
        } else {
            l.user = bands
                .iter()
                .map(|b| {
                    Biquad::peaking(
                        b.freq.clamp(20.0, RATE / 2.0 - 100.0),
                        b.q.clamp(0.3, 8.0),
                        b.gain_db,
                    )
                })
                .collect();
        }
        if curve == Curve::Custom {
            l.custom = bands
                .iter()
                .map(|b| {
                    Biquad::peaking(
                        b.freq.clamp(20.0, RATE / 2.0 - 100.0),
                        b.q.clamp(0.3, 8.0),
                        b.gain_db,
                    )
                })
                .collect();
        }
        l
    }

    pub fn new_with_curve(enabled: bool, curve: Curve) -> Loudness {
        Self::new_with_chain(enabled, curve, false)
    }

    /// Explicit chain: curve + whether the MBCL replaces the broadband
    /// limiter (the MBCL only exists with the official curve).
    pub fn new_with_chain(enabled: bool, curve: Curve, mbcl: bool) -> Loudness {
        let mbcl = if mbcl && curve == Curve::Official {
            Some(Mbcl::new())
        } else {
            None
        };
        let official = if curve == Curve::Official {
            let mut chain = Vec::with_capacity(OFFICIAL_SECTIONS.len() + 1);
            chain.push(Biquad::high_pass_first_order(18.0));
            chain.extend(
                OFFICIAL_SECTIONS
                    .iter()
                    .map(|&(f0, q, gain)| Biquad::peaking(f0, q, gain)),
            );
            chain
        } else {
            Vec::new()
        };
        let mut l = Loudness {
            enabled,
            curve,
            bass_scale: 1.0,
            level: u64::MAX, // force a rebuild on first process
            hpf: Biquad::high_pass(ROLLOFF_HZ, std::f32::consts::FRAC_1_SQRT_2),
            bass: Biquad::low_shelf(BASS_HZ, 0.0),
            presence: Biquad::peaking(PRESENCE_HZ, presence_q(), 0.0),
            scalar: 1.0,
            official,
            official_scalar: 1.0,
            custom: Vec::new(),
            em: None,
            user: Vec::new(),
            mbcl,
            limiter: Limiter::new(),
        };
        l.set_level(100);
        l
    }

    pub fn curve(&self) -> Curve {
        self.curve
    }

    pub fn mbcl_enabled(&self) -> bool {
        self.mbcl.is_some()
    }

    /// Volume level changed → rebuild the tier filters if the tier moved.
    pub fn set_level(&mut self, level: u64) {
        if !self.enabled {
            return;
        }
        if tier_for_level(level) == tier_for_level(self.level) && self.level != u64::MAX {
            self.level = level;
            return;
        }
        self.level = level;
        if self.curve == Curve::Custom {
            // A user curve is absolute: the level must not re-shape it.
            self.scalar = 1.0;
            return;
        }
        if self.curve == Curve::EchoMuse {
            // Their volume law is the plain 0.5 dB/level one (ours) and the
            // ceiling is a fixed +10 dB into the chain, so the top of the
            // range drives the compressors instead of the DAC.
            self.scalar = 1.0;
            return;
        }
        let tier = tier_for_level(level);
        if self.curve == Curve::Official {
            // Official absolute 1 kHz = base + tier; the fitted chain sits
            // OFFICIAL_FIT_1K_DB below its own 1 kHz target, so the scalar
            // carries the difference.
            self.official_scalar =
                10f32.powf((OFFICIAL_BASE_DB + OFFICIAL_TIER_DB[tier] - OFFICIAL_FIT_1K_DB) / 20.0);
            return;
        }
        let (bass_db, presence_db, gain_db) = tier_params(tier);
        self.hpf = Biquad::high_pass(ROLLOFF_HZ, std::f32::consts::FRAC_1_SQRT_2);
        self.bass = Biquad::low_shelf(BASS_HZ, bass_db * self.bass_scale);
        self.presence = Biquad::peaking(PRESENCE_HZ, presence_q(), presence_db);
        self.scalar = 10f32.powf((gain_db + TRIM_DB) / 20.0);
    }

    /// A/B knob for the bass shelf (D 1.1): rebuilds the filters at the
    /// current level so the change takes effect on the next period.
    pub fn set_bass_scale(&mut self, scale: f32) {
        self.bass_scale = scale.clamp(0.0, 1.0);
        let level = self.level;
        self.level = u64::MAX;
        self.set_level(level);
    }

    /// A playback run ended: drop the look-ahead delay line so the next
    /// utterance does not start with the tail of this one.
    pub fn reset_stream(&mut self) {
        if let Some(em) = self.em.as_mut() {
            em.reset();
        }
        self.limiter.reset();
        if let Some(m) = self.mbcl.as_mut() {
            m.reset();
        }
    }

    /// Whether the delay line still holds samples of a finished run.
    pub fn pending(&self) -> usize {
        if let Some(em) = self.em.as_ref() {
            return em.pending();
        }
        match self.mbcl.as_ref() {
            Some(m) => m.pending(),
            None => self.limiter.pending(),
        }
    }

    /// Process one mixed mono block in place (f32, nominal ±1).
    pub fn process(&mut self, x: &mut [f32]) {
        if !self.enabled {
            return;
        }
        if let Some(em) = self.em.as_mut() {
            em.process(x);
            return;
        }
        if self.curve == Curve::Custom {
            for v in x.iter_mut() {
                let mut s = *v;
                for b in self.custom.iter_mut() {
                    s = b.tick(s);
                }
                *v = s;
            }
            self.limiter.process(x);
            return;
        }
        if self.curve == Curve::Official {
            match self.mbcl.as_mut() {
                Some(m) => {
                    for v in x.iter_mut() {
                        let mut s = *v;
                        for b in self.official.iter_mut() {
                            s = b.tick(s);
                        }
                        let mut out = m.band_sum(s * self.official_scalar);
                        // The user's taste EQ rides on every chain, including
                        // this one; it used to be skipped here because the
                        // MBCL branch returned early (audit 2026-11-14).
                        for b in self.user.iter_mut() {
                            out = b.tick(out);
                        }
                        *v = out;
                    }
                    m.full_band(x);
                    return;
                }
                None => {
                    for v in x.iter_mut() {
                        let mut s = *v;
                        for b in self.official.iter_mut() {
                            s = b.tick(s);
                        }
                        *v = s * self.official_scalar;
                    }
                }
            }
        } else {
            for v in x.iter_mut() {
                let mut s = self.hpf.tick(*v);
                s = self.bass.tick(s);
                s = self.presence.tick(s);
                *v = s * self.scalar;
            }
        }
        if !self.user.is_empty() {
            for v in x.iter_mut() {
                let mut s = *v;
                for b in self.user.iter_mut() {
                    s = b.tick(s);
                }
                *v = s;
            }
        }
        self.limiter.process(x);
    }
}

/// Gaussian bell of ±`PRESENCE_OCTAVES` → equivalent peaking Q
/// (shape match at the −3 dB-ish band edges; unit test pins the centre).
fn presence_q() -> f32 {
    // A symmetric-in-log-frequency bell of total width w octaves maps to
    // RBJ Q ≈ sqrt(2)/(w) for the same half-energy width; 1.05 oct → ~1.35.
    1.35
}

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod tests {
    use super::test_support::{rms_db, sine_blocks};
    use super::*;

    /// Tier mapping matches the RUI percent ladder.
    #[test]
    fn tier_mapping() {
        // level 100/127 = 78.7 % → EQ_80 (tier 3); 50/127 = 39 % → EQ_50.
        assert_eq!(tier_for_level(100), 3);
        assert_eq!(tier_for_level(127), 5);
        assert_eq!(tier_for_level(64), 1); // 50.4 %
        assert_eq!(tier_for_level(63), 0); // 49.6 %
        assert_eq!(tier_for_level(0), 0);
    }

    /// The RUI corrected-ladder closed form (dsp.py `ladder_tone_db`) —
    /// the reference our biquad chain approximates.
    fn rui_target_db(f: f32, tier: usize) -> f32 {
        let (bass, pres, gain) = tier_params(tier);
        let ff = f.max(1.0);
        let bass_term = bass / (1.0 + (ff / BASS_HZ).powi(2));
        let pres_term =
            pres * (-0.5 * ((ff / PRESENCE_HZ).log2() / PRESENCE_OCTAVES).powi(2)).exp();
        let ratio = (ff / ROLLOFF_HZ).powi(2);
        let hp = 10.0 * (ratio * ratio / (1.0 + ratio * ratio)).log10();
        bass_term + pres_term + hp + gain + TRIM_DB
    }

    /// The 1 kHz ladder is exact and monotonic (RUI pins |H(1 kHz)|): the
    /// measured delta equals the SCALAR ladder plus the (near-flat at
    /// 1 kHz) filter contributions, and the scalar itself is exact.
    #[test]
    fn one_khz_ladder_exact() {
        for tier in 0..=5 {
            let (bass, pres, gain) = tier_params(tier);
            let mut l = Loudness::new_with_curve(true, Curve::Rui);
            l.set_level(tier_level(tier));
            let chain_1k = l.hpf.magnitude_db_at(1000.0)
                + l.bass.magnitude_db_at(1000.0)
                + l.presence.magnitude_db_at(1000.0);
            let mut x = sine_blocks(1000.0, 1.0);
            let before = rms_db(&x[x.len() / 2..]);
            l.process(&mut x);
            let after = rms_db(&x[x.len() / 2..]);
            let d = after - before;
            let expect = gain + TRIM_DB + chain_1k;
            assert!(
                (d - expect).abs() < 0.5,
                "tier {tier}: 1 kHz delta {d:.2} dB, chain says {expect:.2} (scalar {} bass {bass} pres {pres})",
                gain + TRIM_DB
            );
            // The ladder itself (what RUI pins): the scalar is exact.
            let scalar_db = 20.0 * l.scalar.log10();
            assert!((scalar_db - (gain + TRIM_DB)).abs() < 0.01);
        }
    }

    fn tier_level(tier: usize) -> u64 {
        // A level squarely inside each tier band (percent = level·100/127).
        match tier {
            0 => 50,  // 39 %
            1 => 70,  // 55 %
            2 => 85,  // 67 %
            3 => 100, // 79 %
            4 => 110, // 87 % (115 = 90.6 % would spill into tier 5)
            _ => 127, // 100 %
        }
    }

    /// The realised tone shape tracks the RUI closed-form target at the
    /// design frequencies (biquad approximation of the gaussian bell —
    /// ±1.5 dB honest tolerance; the FIR-exact RUI shape is documented,
    /// the deviation is the armv7 port's price).
    #[test]
    fn tone_shape_matches_target() {
        let mut l = Loudness::new_with_curve(true, Curve::Rui);
        l.set_level(127); // EQ_100 tier
        for freq in [150.0, 1000.0, PRESENCE_HZ, 30.0] {
            let mut x = sine_blocks(freq, 1.0);
            // Keep the level low so the limiter never engages.
            for v in x.iter_mut() {
                *v *= 0.05;
            }
            let before = rms_db(&x[x.len() / 2..]);
            l.process(&mut x);
            let after = rms_db(&x[x.len() / 2..]);
            // Both sides carry the same 0.05 factor, so the RUI target
            // (which already includes gain+trim) compares as-is.
            let target = rui_target_db(freq, 5);
            let delta = after - before;
            assert!(
                (delta - target).abs() < 1.5,
                "{freq} Hz: delta {delta:.2} dB vs RUI target {target:.2} dB"
            );
        }
    }

    /// A finished run must not carry its delay line into the next one.
    #[test]
    fn delay_line_does_not_bleed_across_runs() {
        let mut l = Loudness::new_with_curve(true, Curve::Rui);
        l.set_level(84);
        let mut first = sine_blocks(1000.0, 0.25);
        l.process(&mut first);
        assert!(l.pending() > 0, "look-ahead line should hold a tail");
        l.reset_stream();
        assert_eq!(l.pending(), 0);
        // A fresh quiet block now passes at the stage gain with unit limiter
        // gain — no first-block zeros from a stale tail, no old samples.
        let mut second = sine_blocks(1000.0, 0.05);
        for v in second.iter_mut() {
            *v *= 0.05;
        }
        let before = rms_db(&second);
        l.process(&mut second);
        let after = rms_db(&second[second.len() / 2..]);
        assert!(
            (after - before).abs() < 12.0,
            "fresh block not processed sanely: {before:.1} -> {after:.1} dB"
        );
    }

    /// M6c: the official curve's MEASURED targets, relative to 1 kHz, from
    /// dot2c's `/vendor/etc/audio-algorithms/EQ_50.cfg` (FireOS 5.1.1).
    const OFFICIAL_TARGET_REL: [(f32, f32); 9] = [
        (100.0, 20.1),
        (200.0, 25.6),
        (350.0, 12.3),
        (500.0, 2.7),
        (1000.0, 0.0),
        (2000.0, -7.9),
        (4200.0, -5.3),
        (8000.0, 3.1),
        (12000.0, -4.9),
    ];

    fn official_chain_db(l: &Loudness, f: f32) -> f32 {
        l.official.iter().map(|b| b.magnitude_db_at(f)).sum()
    }

    /// The fitted cascade tracks the measurement in the voice band, and the
    /// absolute 1 kHz level is EQ_50's own level (−0.86 dB).
    #[test]
    fn official_curve_matches_the_measured_response() {
        let mut l = Loudness::new_with_curve(true, Curve::Official);
        l.set_level(50); // tier 0 = EQ_50
        assert_eq!(l.curve(), Curve::Official);
        let chain_1k = official_chain_db(&l, 1000.0);
        for (f, target) in OFFICIAL_TARGET_REL {
            let rel = official_chain_db(&l, f) - chain_1k;
            let tol = if (100.0..=8000.0).contains(&f) {
                2.0
            } else {
                4.0
            };
            assert!(
                (rel - target).abs() <= tol,
                "{f} Hz: fitted {rel:.2} dB vs measured {target:.2} dB (tol {tol})"
            );
        }
        let abs_1k = 20.0 * l.official_scalar.log10() + chain_1k;
        assert!(
            (abs_1k - OFFICIAL_BASE_DB).abs() < 0.05,
            "tier 0 absolute 1 kHz {abs_1k:.2} dB vs measured {OFFICIAL_BASE_DB}"
        );
    }

    /// The tier ladder is the measured scalar: 0/0/0/+3.52/+7.54/+14.54 dB.
    #[test]
    fn official_tier_gains_are_the_measured_ladder() {
        let mut l = Loudness::new_with_curve(true, Curve::Official);
        for (tier, level) in [
            (0usize, 50u64),
            (1, 70),
            (2, 85),
            (3, 100),
            (4, 110),
            (5, 127),
        ] {
            l.set_level(level);
            let db = 20.0 * l.official_scalar.log10() - (OFFICIAL_BASE_DB - OFFICIAL_FIT_1K_DB);
            assert!(
                (db - OFFICIAL_TIER_DB[tier]).abs() < 0.02,
                "tier {tier} (level {level}): {db:.2} dB vs measured {} dB",
                OFFICIAL_TIER_DB[tier]
            );
        }
    }

    /// The official curve can boost 1 kHz by +14.5 dB and 200 Hz by ~+40 dB
    /// relative to the input: the existing limiter must keep the output
    /// bounded and finite (the MBCL is not implemented yet).
    #[test]
    fn the_official_curve_stays_bounded_through_the_limiter() {
        let mut l = Loudness::new_with_curve(true, Curve::Official);
        l.set_level(127); // EQ_100, the loudest tier
        let mut x = sine_blocks(200.0, 1.0); // 0.25 amplitude
        l.process(&mut x);
        assert!(
            x.iter().all(|v| v.is_finite()),
            "NaN/inf in the official path"
        );
        let clip = 10f32.powf(HARD_CLIP_DB / 20.0);
        let peak = x.iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!(
            peak <= clip + 1e-3,
            "peak {peak} above the hard clip {clip}"
        );
    }

    /// The whole official chain (EQ + MBCL + full-band limiter) stays finite
    /// and bounded on a hot low-frequency input, and the MBCL replaces the
    /// broadband limiter only when asked.
    #[test]
    fn the_official_mbcl_chain_is_bounded() {
        let mut l = Loudness::new_with_chain(true, Curve::Official, true);
        assert!(l.mbcl_enabled());
        l.set_level(127);
        let mut x = sine_blocks(200.0, 1.0);
        l.process(&mut x);
        assert!(x.iter().all(|v| v.is_finite()), "NaN/inf in the MBCL path");
        let clip = 10f32.powf(HARD_CLIP_DB / 20.0);
        let peak = x.iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!(
            peak <= clip + 1e-3,
            "peak {peak} above the hard clip {clip}"
        );

        // RUI + requested MBCL: the MBCL belongs to the official chain only.
        let rui = Loudness::new_with_chain(true, Curve::Rui, true);
        assert!(!rui.mbcl_enabled());
    }

    /// The M6c A/B switches the chain from the supervisor line. The flag
    /// path and the explicit constructor must build the SAME chain, or the
    /// boot log would name a chain the audio thread is not running.
    #[test]
    fn the_cli_chain_flags_match_the_explicit_constructor() {
        let mut a = Loudness::from_config(true, Some(Curve::Official), Some(true), &[]);
        let mut b = Loudness::new_with_chain(true, Curve::Official, true);
        assert!(a.mbcl_enabled() && b.mbcl_enabled());
        a.set_level(124);
        b.set_level(124);
        let mut xa = sine_blocks(200.0, 0.5);
        let mut xb = xa.clone();
        a.process(&mut xa);
        b.process(&mut xb);
        let diff = xa
            .iter()
            .zip(&xb)
            .map(|(p, q)| (p - q).abs())
            .fold(0f32, f32::max);
        assert!(
            diff < 1e-6,
            "flag path differs from the explicit chain: {diff}"
        );

        // Requesting the MBCL on the RUI curve keeps the RUI chain: the
        // MBCL exists only as part of the stock (official) chain.
        let rui = Loudness::from_config(true, Some(Curve::Rui), Some(true), &[]);
        assert!(!rui.mbcl_enabled());
    }

    /// M6c follow-on: a user curve is just the listed peaking bands — the
    /// tone the console editor will let people set. A +6 dB band at 1 kHz
    /// must raise a 1 kHz tone by about 6 dB at every volume level (no
    /// tier ladder), and the output must stay bounded.
    #[test]
    fn a_custom_curve_is_the_listed_bands_at_any_level() {
        let bands = [CustomBand {
            freq: 1000.0,
            gain_db: 6.0,
            q: 1.0,
        }];
        let mut l = Loudness::from_config(true, Some(Curve::Custom), Some(false), &bands);
        for level in [30u64, 127] {
            l.set_level(level);
            // 0.5 s: the limiter's 5 ms look-ahead delays the output, so the
            // measurement skips the head (a 0.1 s block would leave nothing).
            let mut x = sine_blocks(1000.0, 0.5);
            let before = rms_db(&x);
            l.process(&mut x);
            let after = rms_db(&x[1600..]);
            assert!(
                (after - before - 6.0).abs() < 1.0,
                "level {level}: a +6 dB band gave {:.1} dB",
                after - before
            );
        }

        // A hot low-frequency input through a big bass band stays bounded.
        let hot = [CustomBand {
            freq: 120.0,
            gain_db: 24.0,
            q: 0.7,
        }];
        let mut l = Loudness::from_config(true, Some(Curve::Custom), Some(false), &hot);
        l.set_level(127);
        let mut x = sine_blocks(120.0, 1.0);
        l.process(&mut x);
        let peak = x.iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!(
            peak <= 10f32.powf(HARD_CLIP_DB / 20.0) + 1e-3,
            "peak {peak}"
        );
    }

    /// The EchoMuse voicing keeps their correction and their +10 dB ceiling.
    /// At −60 dBFS nothing compresses (all thresholds are above it), so the
    /// chain is linear and the shape can be measured.
    #[test]
    fn the_echo_muse_voicing_has_their_correction_and_ceiling() {
        let mut l = Loudness::from_config(true, Some(Curve::EchoMuse), Some(false), &[]);
        l.set_level(127);
        let measure = |l: &mut Loudness, f: f32| {
            let amp = 10f32.powf(-60.0 / 20.0);
            let n = (0.5 * RATE) as usize;
            let mut x: Vec<f32> = (0..n)
                .map(|i| (2.0 * std::f32::consts::PI * f * i as f32 / RATE).sin() * amp)
                .collect();
            l.reset_stream();
            l.process(&mut x);
            let out_db = rms_db(&x[2400..]);
            let in_db = 20.0 * (amp / std::f32::consts::SQRT_2).log10();
            out_db - in_db
        };
        let at_1k = measure(&mut l, 1000.0);
        assert!(
            (at_1k - 10.0).abs() < 2.5,
            "their +10 dB ceiling at 1 kHz gave {at_1k} dB"
        );
        let at_175 = measure(&mut l, 175.0);
        assert!(
            at_175 - at_1k > 14.0,
            "the 175 Hz driver correction is missing: {} dB re 1 kHz",
            at_175 - at_1k
        );
    }

    /// A hot low-frequency input through the whole voicing stays bounded.
    #[test]
    fn the_echo_muse_chain_is_bounded() {
        let mut l = Loudness::from_config(true, Some(Curve::EchoMuse), Some(false), &[]);
        l.set_level(127);
        let mut x = sine_blocks(120.0, 2.0);
        l.process(&mut x);
        assert!(
            x.iter().all(|v| v.is_finite()),
            "NaN/inf in the EchoMuse chain"
        );
        let peak = x.iter().fold(0f32, |m, v| m.max(v.abs()));
        let clip = 10f32.powf(HARD_CLIP_DB / 20.0);
        assert!(
            peak <= clip + 1e-3,
            "peak {peak} above the hard clip {clip}"
        );
    }

    /// The user's taste EQ sits on top of the fixed voicing: +6 dB at 3 kHz
    /// on the EchoMuse chain must show up as +6 dB at 3 kHz at a quiet level
    /// (no compressor engaged), and the bass cut must not alter the treble.
    #[test]
    fn the_user_eq_is_measured_on_top_of_the_voicing() {
        let flat = Loudness::from_config(true, Some(Curve::EchoMuse), Some(false), &[]);
        let shaped = Loudness::from_config(
            true,
            Some(Curve::EchoMuse),
            Some(false),
            &[CustomBand {
                freq: 3000.0,
                gain_db: 6.0,
                q: 1.0,
            }],
        );
        let measure = |mut l: Loudness, f: f32| {
            let amp = 10f32.powf(-60.0 / 20.0);
            let n = (0.5 * RATE) as usize;
            let mut x: Vec<f32> = (0..n)
                .map(|i| (2.0 * std::f32::consts::PI * f * i as f32 / RATE).sin() * amp)
                .collect();
            l.reset_stream();
            l.process(&mut x);
            rms_db(&x[2400..])
        };
        let a = measure(flat, 3000.0);
        let b = measure(shaped, 3000.0);
        assert!(
            (b - a - 6.0).abs() < 1.0,
            "+6 dB band measured {:.1} dB",
            b - a
        );
    }

    /// DC is blocked (the design pins the DC bin to zero).
    #[test]
    fn dc_blocked() {
        let mut l = Loudness::new_with_curve(true, Curve::Rui);
        l.set_level(100);
        let mut x = vec![0.5f32; RATE as usize];
        l.process(&mut x);
        let tail = rms_db(&x[x.len() - 4800..]);
        assert!(tail < -30.0, "DC tail {tail} dB — high-pass not working");
    }
}
