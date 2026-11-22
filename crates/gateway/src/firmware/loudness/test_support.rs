//! Shared measurements for the loudness unit tests.
//!
//! Split out of the old single `tests` module so every sub-module's tests can
//! use the same helpers instead of carrying a copy (a copy would drift).

use super::RATE;

/// Level in dB of a block's mean power (`None`-safe, −120 dB floor).
pub(super) fn rms_db(x: &[f32]) -> f32 {
    let e: f32 = x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32;
    10.0 * e.max(1e-12).log10()
}

/// `secs` seconds of a `freq` sine at 0.25 amplitude.
pub(super) fn sine_blocks(freq: f32, secs: f32) -> Vec<f32> {
    let n = (secs * RATE) as usize;
    (0..n)
        .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / RATE).sin() * 0.25)
        .collect()
}
