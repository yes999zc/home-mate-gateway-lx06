use super::{HARD_CLIP_DB, RATE};

const LIMITER_CEILING: f32 = -1.0; // dBFS
const LIMITER_LOOKAHEAD_MS: f32 = 5.0;
const LIMITER_RELEASE_MS: f32 = 80.0;

/// Look-ahead peak limiter with a CONTINUOUS gain trajectory.
///
/// **2026-11-10 rewrite (D-stage task 1.1).** The first version recomputed
/// the target from the sliding-window peak on every sample and moved the
/// gain there *instantly*. A new peak entering the window therefore stepped
/// the gain by several dB within one sample — an audio-rate amplitude
/// modulation with discontinuities, i.e. broadband distortion added to the
/// very signal the AEC then has to cancel. R6's A/B showed the result: 4
/// false near-end triggers in 56 s of stimulus-free speech at 84/127.
///
/// The structure now has no such step by construction:
///
/// * per-sample required gain `req = min(1, ceiling/|x|)`;
/// * a monotonic queue of requirements over the in-flight (look-ahead)
///   window — front = tightest requirement, and its sample index gives the
///   exact deadline that gain has to meet;
/// * while `req(tightest) < gain`, the gain ramps toward it so that it lands
///   exactly on the deadline sample (`gain += (target-gain)/remaining`) —
///   a monotone, continuous trajectory, never a step;
/// * otherwise the gain releases exponentially toward that same bound
///   (RUI 80 ms), so it can never rise above an in-flight requirement.
///
/// The ramp is only violated when a >12 dB jump arrives inside one
/// look-ahead window (the ramp cannot physically get there); the hard clip
/// at −0.1 dBFS is the last resort, and the second test below pins that the
/// gain trajectory has no discontinuity on ordinary speech-like material.
pub struct Limiter {
    ceiling: f32,
    lookahead: usize,
    release_coef: f32,
    /// In-flight samples; the output is delayed by `lookahead`.
    buf: std::collections::VecDeque<f32>,
    /// Monotonic (increasing required gain) queue over the in-flight
    /// window: (absolute sample index, required gain). Front = tightest.
    reqs: std::collections::VecDeque<(u64, f32)>,
    idx: u64,
    gain: f32,
    #[cfg(test)]
    trace: Vec<f32>,
}

impl Limiter {
    pub fn new() -> Limiter {
        Self::with_params(LIMITER_CEILING, LIMITER_LOOKAHEAD_MS, LIMITER_RELEASE_MS)
    }

    /// The same continuous look-ahead limiter with explicit parameters (the
    /// MBCL's full-band stage: official −0.1 dB / 200 ms).
    pub fn with_params(ceiling_db: f32, lookahead_ms: f32, release_ms: f32) -> Limiter {
        let lookahead = (lookahead_ms * RATE / 1000.0) as usize;
        Limiter {
            ceiling: 10f32.powf(ceiling_db / 20.0),
            lookahead,
            release_coef: (-1.0 / (release_ms / 1000.0 * RATE)).exp(),
            buf: std::collections::VecDeque::with_capacity(lookahead + 8),
            reqs: std::collections::VecDeque::with_capacity(lookahead + 8),
            idx: 0,
            gain: 1.0,
            #[cfg(test)]
            trace: Vec::new(),
        }
    }

    /// Samples still delayed (non-zero ⇒ a new stream would play a slice of
    /// the previous one). The playback loop resets the stage when a channel
    /// run ends so the delay line cannot bleed across plays.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    /// Start a fresh stream: empty the delay line and the requirement queue.
    /// The dropped tail is inside the playback path's end fade, so it is
    /// inaudible; carrying it over (the old behaviour) was worse — the last
    /// few ms of one utterance reappeared at the head of the next.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.reqs.clear();
        self.gain = 1.0;
    }

    /// Process a block in place (mono, f32, |x| <= ~1 expected).
    pub fn process(&mut self, x: &mut [f32]) {
        let clip = 10f32.powf(HARD_CLIP_DB / 20.0);
        for sample in x.iter_mut() {
            let s = *sample;
            let req = if s == 0.0 {
                1.0
            } else {
                (self.ceiling / s.abs()).min(1.0)
            };
            self.idx += 1;
            // Monotonic queue: a requirement only matters while it is the
            // tightest one in flight. The comparison carries an epsilon on
            // purpose: with a periodic signal successive peaks differ only by
            // float noise (0.89130 vs 0.89129), and a strict `>` would let the
            // newer peak evict the older one — the deadline would keep sliding
            // forward and the ramp would never arrive (measured: 0.984 instead
            // of the 0.891 ceiling). 1e-4 of gain is 0.0009 dB.
            while let Some(&(_, back)) = self.reqs.back() {
                if back > req + 1e-4 {
                    self.reqs.pop_back();
                } else {
                    break;
                }
            }
            self.reqs.push_back((self.idx, req));
            self.buf.push_back(s);

            if self.buf.len() <= self.lookahead {
                // Still filling the look-ahead: emit silence, never the raw
                // sample (nothing bypasses the limiter).
                *sample = 0.0;
            } else {
                let out = self.buf.pop_front().unwrap();
                let out_idx = self.idx - self.lookahead as u64;
                // Requirements older than the emitted sample are spent.
                while let Some(&(i2, _)) = self.reqs.front() {
                    if i2 < out_idx {
                        self.reqs.pop_front();
                    } else {
                        break;
                    }
                }
                let (deadline, target) = *self.reqs.front().expect("in-flight requirement");
                // Samples (inclusive) until the constrained sample leaves
                // the delay line — the gain has to be at `target` by then.
                let remaining = (deadline - out_idx + 1) as f32;
                self.gain = if target < self.gain {
                    self.gain + (target - self.gain) / remaining
                } else {
                    target + (self.gain - target) * self.release_coef
                };
                #[cfg(test)]
                self.trace.push(self.gain);
                *sample = (out * self.gain).clamp(-clip, clip);
            }
        }
    }
}

impl Default for Limiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::rms_db;
    use super::*;

    /// The limiter holds the ceiling on a full-scale input and stays
    /// transparent on a quiet one.
    ///
    /// The output is delayed by the look-ahead window, so the tail of an
    /// input buffer is emitted by the NEXT call — the test appends that many
    /// zeros (the playback loop instead resets the stage when a run ends;
    /// [`delay_line_does_not_bleed_across_runs`] pins that).
    #[test]
    fn limiter_ceiling_and_transparency() {
        let n = RATE as usize;
        let look = (LIMITER_LOOKAHEAD_MS * RATE / 1000.0) as usize;
        let mut lim = Limiter::new();
        let mut hot: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / RATE).sin() * 1.0)
            .collect();
        hot.extend(std::iter::repeat_n(0.0f32, look));
        lim.process(&mut hot);
        let peak = hot[..n].iter().fold(0f32, |m, v| m.max(v.abs()));
        // The ceiling is met within 0.02 dB (0.23 %): the ramp lands on the
        // tightest requirement in the window, so a sample whose own
        // requirement is marginally looser can sit a hair above it. What
        // matters — and what the old design violated — is that the gain
        // trajectory has no step; the hard clip at −0.1 dBFS is the absolute
        // bound on the output.
        let ceil = 10f32.powf(LIMITER_CEILING / 20.0);
        assert!(peak <= ceil * 1.0023, "peak {peak} vs ceiling {ceil}");
        assert!((10f32.powf(HARD_CLIP_DB / 20.0) - 0.9886).abs() < 1e-3);

        let mut lim2 = Limiter::new();
        let mut quiet: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / RATE).sin() * 0.1)
            .collect();
        quiet.extend(std::iter::repeat_n(0.0f32, look));
        let before = rms_db(&quiet[..n]);
        lim2.process(&mut quiet);
        // Skip the fill window: transparency is about steady state.
        let after = rms_db(&quiet[look..n]);
        assert!((after - before).abs() < 0.1, "quiet path changed level");
    }

    /// D 1.1: the gain trajectory must be CONTINUOUS, not stepped. Old
    /// implementation jumped the gain by the whole reduction in one sample
    /// when a peak entered the window (that is the distortion R6 measured);
    /// the ramp version must never move more than a small fraction of the
    /// total reduction per sample.
    #[test]
    fn limiter_gain_is_continuous() {
        // 0.2 s quiet, then a sudden 4x-over-ceiling burst (~12 dB of
        // required reduction), then quiet again.
        let mut x: Vec<f32> = Vec::new();
        for i in 0..9600 {
            x.push((2.0 * std::f32::consts::PI * 300.0 * i as f32 / RATE).sin() * 0.05);
        }
        for i in 0..9600 {
            x.push((2.0 * std::f32::consts::PI * 300.0 * i as f32 / RATE).sin() * 3.9);
        }
        for i in 0..9600 {
            x.push((2.0 * std::f32::consts::PI * 300.0 * i as f32 / RATE).sin() * 0.05);
        }
        let mut lim = Limiter::new();
        for blk in x.chunks_mut(2048) {
            lim.process(blk);
        }
        let tr = &lim.trace;
        assert!(tr.len() > 20_000, "trace {}", tr.len());
        let lo = tr.iter().cloned().fold(1.0f32, f32::min);
        let hi = tr.iter().cloned().fold(0.0f32, f32::max);
        let span = hi - lo;
        assert!(
            span > 0.5,
            "limiter never reduced gain (span {span:.3}) — test is not exercising it"
        );
        let mut worst = 0.0f32;
        for w in tr.windows(2) {
            worst = worst.max((w[1] - w[0]).abs());
        }
        // A one-sample step of the old design would be ~the whole span.
        assert!(
            worst < span * 0.10,
            "gain discontinuity: worst step {worst:.4} vs span {span:.3}"
        );
    }
}
