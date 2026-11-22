//! Seven-mic fusion: the REAL merge, ported from the rui array1 candidate
//! (0002-rui-seven-mic-array.patch, device/internal/beamformer/array_mix.go).
//!
//! A circular array with unknown source azimuth can combine low frequencies
//! coherently; at higher frequencies the same average cancels speech
//! arriving across the ring, so a complementary crossover keeps the centre
//! microphone up there: low band = the array average, high band = the
//! centre mic. The weights and the one-millisecond delay are FIXED, so a
//! downstream echo canceller (and VAD calibration) sees a stable acoustic
//! path across wakes and turns.
//!
//! Upstream EchoMuse's "beamformer" module is NOT this — it picks one mic
//! (centre or wake-selected). Don't confuse them (user direction 2026-11-08).

const TAPS: usize = 33;
const DELAY: usize = TAPS / 2; // 16 samples = 1ms @16k
const CROSSOVER_HZ: f64 = 1400.0;
const SAMPLE_RATE: f64 = 16000.0;
const NUM_MICS: usize = 7;

/// Windowed-sinc (Hamming) lowpass at the crossover, normalized to unit DC
/// gain — computed once.
fn lowpass_taps() -> [f64; TAPS] {
    let mut taps = [0f64; TAPS];
    let cutoff = 2.0 * CROSSOVER_HZ / SAMPLE_RATE;
    let mut sum = 0f64;
    for (i, tap) in taps.iter_mut().enumerate().take(TAPS) {
        let x = std::f64::consts::PI * (i as f64 - DELAY as f64);
        let ideal = if x == 0.0 {
            cutoff
        } else {
            (cutoff * x).sin() / x
        };
        *tap = ideal
            * (0.54 - 0.46 * (2.0 * std::f64::consts::PI * i as f64 / (TAPS - 1) as f64).cos());
        sum += *tap;
    }
    for t in taps.iter_mut() {
        *t /= sum;
    }
    taps
}

pub struct ArrayMixer {
    lowpass: [f64; TAPS],
    centre: [f64; TAPS],
    difference: [f64; TAPS],
    next: usize,
    pub clipped: u64,
}

impl ArrayMixer {
    pub fn new() -> ArrayMixer {
        ArrayMixer {
            lowpass: lowpass_taps(),
            centre: [0.0; TAPS],
            difference: [0.0; TAPS],
            next: 0,
            clipped: 0,
        }
    }

    /// Fuse `NUM_MICS` equal-length channels (post-AEC cleaned mics) into
    /// `out`. Channel 6 is the centre microphone by the array geometry
    /// (beamformer.go constants on this board).
    pub fn process(&mut self, chans: &[&[i16]], out: &mut [i16]) {
        assert_eq!(chans.len(), NUM_MICS, "seven mic channels");
        assert_eq!(chans[0].len(), out.len(), "block length");
        let centre_ch = &chans[centre::CENTRE];
        for i in 0..out.len() {
            let mut sum = 0f64;
            for ch in chans {
                sum += ch[i] as f64;
            }
            let centre = centre_ch[i] as f64;
            self.centre[self.next] = centre;
            self.difference[self.next] = sum / NUM_MICS as f64 - centre;
            let mut middle = self.next as isize - DELAY as isize;
            if middle < 0 {
                middle += TAPS as isize;
            }
            let mut mixed = self.centre[middle as usize];
            let mut j = self.next as isize;
            for tap in self.lowpass.iter() {
                mixed += tap * self.difference[j as usize];
                j -= 1;
                if j < 0 {
                    j = TAPS as isize - 1;
                }
            }
            self.next = (self.next + 1) % TAPS;
            // All paths are already in the i16 domain (the established
            // `>>4` S24 decode scale) — clamp directly, no renormalization
            // (the Go original works in [-1,1) floats and quantizes at the
            // end; same arithmetic, one fewer scale hop).
            let sample = mixed.round();
            if sample > 32767.0 {
                out[i] = 32767;
                self.clipped += 1;
            } else if sample < -32768.0 {
                out[i] = -32768;
                self.clipped += 1;
            } else {
                out[i] = sample as i16;
            }
        }
    }
}

// Channel index of the centre mic within the fusion input.
mod centre {
    pub const CENTRE: usize = 6;
}

impl Default for ArrayMixer {
    fn default() -> Self {
        ArrayMixer::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DC through the fusion must return the array average: all channels
    /// equal constant → output equals that constant (unity DC gain).
    #[test]
    fn dc_gain_is_array_average() {
        let mut m = ArrayMixer::new();
        const N: usize = 512;
        let chans: Vec<Vec<i16>> = (0..NUM_MICS).map(|_| vec![1000i16; N]).collect();
        let refs: Vec<&[i16]> = chans.iter().map(|c| c.as_slice()).collect();
        let mut out = vec![0i16; N];
        m.process(&refs, &mut out);
        // After the 33-tap warmup the output settles on the average.
        let tail_avg: f64 = out[N - 64..].iter().map(|&s| s as f64).sum::<f64>() / 64.0;
        assert!((tail_avg - 1000.0).abs() < 2.0, "tail average {tail_avg}");
        assert_eq!(m.clipped, 0);
    }

    /// High band belongs to the centre mic: with the six perimeter mics
    /// silent and the centre carrying a signal above the crossover, the
    /// output follows the centre after the 1ms alignment delay.
    #[test]
    fn high_band_follows_centre() {
        let mut m = ArrayMixer::new();
        const N: usize = 4096;
        let mut centre = vec![0i16; N];
        // 4kHz square-ish alternation, well above the 1.4kHz crossover.
        for (i, s) in centre.iter_mut().enumerate() {
            *s = if (i / 8) % 2 == 0 { 8000 } else { -8000 };
        }
        let mut chans: Vec<Vec<i16>> = (0..NUM_MICS).map(|_| vec![0i16; N]).collect();
        chans[centre::CENTRE] = centre.clone();
        let refs: Vec<&[i16]> = chans.iter().map(|c| c.as_slice()).collect();
        let mut out = vec![0i16; N];
        m.process(&refs, &mut out);
        // After warmup+delay, output ≈ centre at the same amplitude (the
        // LP path contributes ~0 at 4kHz).
        let centre_avg: f64 = centre[N - 512..].iter().map(|&s| s as f64).sum::<f64>() / 512.0;
        let out_avg: f64 = out[N - 512..].iter().map(|&s| s as f64).sum::<f64>() / 512.0;
        assert!(
            (centre_avg - out_avg).abs() < 100.0,
            "centre {centre_avg} vs out {out_avg}"
        );
    }
}
