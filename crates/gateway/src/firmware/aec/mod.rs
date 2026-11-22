//! Device-side AEC: per-mic Speex cancellation → seven-mic fusion → single
//! mono uplink (G6, user direction 2026-11-08: AEC lives ON the device; the
//! server side stays off). Three pieces:
//!
//! - `speex`: safe FFI over the vendored SpeexDSP MDF canceller
//!   (speexdsp/, EchoMuse's floating-point build flags).
//! - `dealias`: rebuilds the ch8 hardware reference without its decimation
//!   aliases by matching it against the firmware's own playback output ring
//!   (port of EchoMuse's aec/dealias.go; without this the canceller chases
//!   spectral ghosts — 1dB vs 25.7dB cancellation, their measurement).
//! - `fusion`: the real seven-mic merge (33-tap complementary crossover at
//!   1.4kHz, 1ms centre alignment — port of the rui array1 candidate; NOT
//!   upstream's pick-one-mic "beamformer").
//!
//! Topology per the user's directive: cancel each mic ch0..6 against the
//! cleaned reference FIRST, then fuse the seven CLEANED channels. The
//! uplink scale keeps the established `>>4` S24 decode calibration.

use std::sync::Arc;

pub mod dealias;
pub mod fusion;
pub mod speex;

/// Capture/uplink rate.
pub const SAMPLE_RATE: usize = 16000;

/// Rate handed to Speex for its parameter derivation
/// (`SPEEX_ECHO_SET_SAMPLING_RATE`) — NOT the capture rate.
///
/// R0 (2026-11-12): the cancellers never called this and ran Speex's 8 kHz
/// default by accident. Making it explicit and re-measuring ERLE turned the
/// accident into a measurement: same night, same stimulus/volume/placement,
/// 4 runs at 16000 → 20.6 dB median (19.9-20.7); 3 runs at 8000 → 21.3 dB
/// (21.1-21.7). Speex derives beta0 = 2*frame/rate (0.04 at 16k, 0.08 at
/// 8k), so at 16 kHz the filter adapts more slowly and converges less within
/// a 28 s utterance. The measured-better value wins; changing it requires
/// re-running the ERLE measurement (tools/erle_run.py + erle_report.py).
/// Evidence: deploy/dot2d/emos/results/r0-erle-20261112/.
pub const SPEEX_RATE: usize = 8000;

/// Fixed pipeline geometry (16kHz).
pub const MIC_CHANNELS: usize = 7; // ch0..6: six perimeter + centre at ch6
pub const CENTRE_CHANNEL: usize = 6;
pub const REF_CHANNEL: usize = 8; // playback loopback (aliased ch8)
/// AEC frame size in samples: 20ms — four frames per 80ms uplink block.
pub const FRAME: usize = 320;
/// Echo tail in samples. T8 root fix (2026-11-09): 1280 (80ms) → 2560
/// (160ms) — the 80ms reference window could not cover the room reverb
/// tail (ERLE 15.6dB, post-play self-hear). Measured with tail 2560:
/// ERLE median 22.8dB (voice) / 23.7dB (music), CPU 36.7% median
/// single-core (≤40% budget). Freezable parameter — change requires
/// re-running the ERLE measurement before VAD re-cal.
pub const TAIL: usize = 2560;

/// The full chain for one 80ms capture block: nine decoded channels in,
/// one fused mono block out. Stateful across blocks (filters + ring).
pub struct Pipeline {
    cancellers: Vec<speex::Canceller>,
    fuse: fusion::ArrayMixer,
    ref_cleaner: Arc<dealias::Dealias>,
    direct_clean_ref: bool,
    // Scratch, allocated once.
    mic_frames: Vec<Vec<i16>>, // [7][FRAME] per sub-frame, reused per sub
}

impl Pipeline {
    /// `tail` is the echo tail in samples (default [`TAIL`]; `--aec-tail`
    /// overrides for the tail→CPU measurement table — one binary, many
    /// points). Must be a positive multiple of FRAME.
    pub fn new(tail: usize) -> (Pipeline, Arc<dealias::Dealias>) {
        assert!(
            tail > 0 && tail.is_multiple_of(FRAME),
            "tail must be a positive multiple of FRAME"
        );
        let ref_cleaner = Arc::new(dealias::Dealias::new());
        (
            Pipeline {
                cancellers: (0..MIC_CHANNELS)
                    .map(|_| speex::Canceller::new(FRAME, tail, SPEEX_RATE))
                    .collect(),
                fuse: fusion::ArrayMixer::new(),
                ref_cleaner: ref_cleaner.clone(),
                direct_clean_ref: false,
                mic_frames: (0..MIC_CHANNELS).map(|_| vec![0i16; FRAME]).collect(),
            },
            ref_cleaner,
        )
    }

    /// Enable direct clean reference mode for boards (e.g. LX06) whose
    /// hardware digital loopback is already anti-alias-decimated from 48 kHz
    /// synchronously with the microphone channels.
    pub fn set_direct_clean_ref(&mut self, direct: bool) {
        self.direct_clean_ref = direct;
    }

    pub fn dealias_stats(&self) -> dealias::Stats {
        if self.direct_clean_ref {
            dealias::Stats {
                locked: true,
                ever_locked: true,
                relocks: 0,
                losses: 0,
                backoffs: 0,
            }
        } else {
            self.ref_cleaner.stats()
        }
    }

    /// An incomplete/overrun capture block broke the microphone timeline.
    /// Keep DAC history, but require a new verified reference match.
    pub fn invalidate_capture_alignment(&self) {
        self.ref_cleaner.on_dac_drop();
    }

    /// The adaptation rate each canceller reports (one per mic channel).
    /// Diagnostics + the R0 regression test: every one must be 16000.
    pub fn sampling_rates(&self) -> Vec<i32> {
        self.cancellers.iter().map(|c| c.sampling_rate()).collect()
    }

    /// `block`: 9-channel interleaved samples for one capture read
    /// (UPLINK_FRAMES × 9, already decoded to i16 via the `>>4` scale).
    /// `out` receives the fused mono block (UPLINK_FRAMES samples).
    /// Returns the cleaned reference actually used (for diagnostics and
    /// the measure tracks) and whether EVERY 20 ms frame of the block came
    /// from a verified ring match. A block with one degraded frame must not
    /// be used for near-end decisions, even if the last frame relocked
    /// (2026-11-12: `stats().locked` sampled after the block hid exactly
    /// that loss+relock and the detector fired on a poisoned reference).
    pub fn process(&mut self, block: &[[i16; 9]], raw: &[u8], out: &mut [i16]) -> (Vec<i16>, bool) {
        let n = block.len();
        debug_assert_eq!(out.len(), n);
        let mut ref_all = Vec::with_capacity(n);
        let mut lock_all = true;
        for f in 0..n / FRAME {
            // Clean reference sub-block (never the raw aliased ch8; the
            // cleaner coasts from the output ring when its match is lost).
            let raw_ref: Vec<i16> = block[f * FRAME..(f + 1) * FRAME]
                .iter()
                .map(|s| s[REF_CHANNEL])
                .collect();
            let cleaned = if self.direct_clean_ref {
                raw_ref
            } else {
                // Raw S24 bytes of channel 8 (bytes 24..27 of each 27-byte
                // frame), for the dealias encoder probe.
                let ref_bytes: Vec<u8> = raw[f * FRAME * 27..(f + 1) * FRAME * 27]
                    .as_chunks::<27>()
                    .0
                    .iter()
                    .flat_map(|fr| fr[24..27].iter().copied())
                    .collect();
                let cleaned_ref = self.ref_cleaner.clean(&raw_ref, &ref_bytes);
                if cleaned_ref.state != dealias::LockState::Locked {
                    lock_all = false;
                }
                cleaned_ref.samples
            };
            ref_all.extend_from_slice(&cleaned);
            // Per-mic cancellation on this sub-frame.
            for m in 0..MIC_CHANNELS {
                for (i, s) in block[f * FRAME..(f + 1) * FRAME].iter().enumerate() {
                    self.mic_frames[m][i] = s[m];
                }
                self.cancellers[m].process(&mut self.mic_frames[m], &cleaned);
            }
            // Fuse the seven CLEANED channels.
            let chans: Vec<&[i16]> = self.mic_frames.iter().map(|v| v.as_slice()).collect();
            let out_slice = &mut out[f * FRAME..(f + 1) * FRAME];
            self.fuse.process(&chans, out_slice);
        }
        (ref_all, lock_all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R0 (2026-11-12): the rate is set on the whole pipeline, not just one
    /// wrapper (the loop in `Pipeline::new` is what the device runs), and the
    /// value is the measured one.
    #[test]
    fn every_canceller_gets_the_measured_rate() {
        let (pipeline, _dealias) = Pipeline::new(TAIL);
        assert_eq!(
            pipeline.sampling_rates(),
            vec![SPEEX_RATE as i32; MIC_CHANNELS]
        );
    }
}
