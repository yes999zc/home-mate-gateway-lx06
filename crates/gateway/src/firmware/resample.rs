//! Downlink audio shaping: WAV payload extraction, mono downmix, and
//! resampling to the device's 48 kHz playback rate.
//!
//! Moved here from `emuse/resample.rs` when the EchoMuse WSS adapter was
//! removed. `firmware/mod.rs` already called these functions; the
//! Catmull-Rom math is unchanged.
//!
//! Interpolation is 4-point Catmull-Rom cubic — good suppression of the
//! source-rate images for an integer-ratio upsample (16k→48k, 24k→48k)
//! without pulling a DSP dependency. Loudness EQ / limiting stays G7.

/// Parse a RIFF/WAVE (PCM s16le) buffer into interleaved samples + format.
pub fn parse_wav(wav: &[u8]) -> Option<(Vec<i16>, u32, u16)> {
    if wav.len() < 12 || &wav[0..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        return None;
    }
    let mut pos = 12;
    let mut fmt: Option<(u32, u16, u16)> = None; // (rate, channels, bits)
    let mut data: Option<&[u8]> = None;
    while pos + 8 <= wav.len() {
        let id = &wav[pos..pos + 4];
        let size =
            u32::from_le_bytes([wav[pos + 4], wav[pos + 5], wav[pos + 6], wav[pos + 7]]) as usize;
        let body = pos + 8;
        if body + size > wav.len() {
            return None;
        }
        match id {
            b"fmt " if size >= 16 => {
                let audio_format = u16::from_le_bytes([wav[body], wav[body + 1]]);
                let channels = u16::from_le_bytes([wav[body + 2], wav[body + 3]]);
                let rate = u32::from_le_bytes([
                    wav[body + 4],
                    wav[body + 5],
                    wav[body + 6],
                    wav[body + 7],
                ]);
                let bits = u16::from_le_bytes([wav[body + 14], wav[body + 15]]);
                if audio_format != 1 || bits != 16 {
                    return None; // PCM s16 only
                }
                fmt = Some((rate, channels, bits));
            }
            b"data" => data = Some(&wav[body..body + size]),
            _ => {}
        }
        pos = body + size + (size & 1);
    }
    let (rate, channels, _) = fmt?;
    let data = data?;
    // review P3-1: 0 channels would panic chunks_exact(0) in to_mono and
    // 0 rate slips through resample un-resampled — reject both.
    if rate == 0 || channels == 0 {
        return None;
    }
    if data.len() % 2 != 0 {
        return None;
    }
    let samples = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect();
    Some((samples, rate, channels))
}

/// Downmix interleaved samples to mono (average). Mono input passes through.
pub fn to_mono(samples: &[i16], channels: u16) -> Vec<i16> {
    if channels <= 1 {
        return samples.to_vec();
    }
    samples
        .chunks_exact(channels as usize)
        .map(|ch| {
            let sum: i32 = ch.iter().map(|&s| s as i32).sum();
            (sum / channels as i32) as i16
        })
        .collect()
}

/// Catmull-Rom cubic resampler. `from == to` returns a copy.
pub fn resample(input: &[i16], from: u32, to: u32) -> Vec<i16> {
    if input.is_empty() {
        return Vec::new();
    }
    if from == to || from == 0 || to == 0 {
        return input.to_vec();
    }
    let n_in = input.len();
    let n_out = (n_in as u64 * to as u64).div_ceil(from as u64);
    let ratio = from as f64 / to as f64;
    let mut out = Vec::with_capacity(n_out as usize);
    for i in 0..n_out as usize {
        let t = i as f64 * ratio;
        let i0 = t.floor() as isize;
        let f = t - i0 as f64;
        let at = |k: isize| -> f64 {
            let idx = k.clamp(0, n_in as isize - 1) as usize;
            input[idx] as f64
        };
        let p0 = at(i0 - 1);
        let p1 = at(i0);
        let p2 = at(i0 + 1);
        let p3 = at(i0 + 2);
        let v = 0.5
            * ((2.0 * p1)
                + (-p0 + p2) * f
                + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * f * f
                + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * f * f * f);
        out.push(v.round().clamp(-32768.0, 32767.0) as i16);
    }
    out
}

/// The full downlink path: WAV → mono → 48 kHz s16le bytes, chunked into
/// device frames of at most `frame_bytes` (4096 on the wire). The final
/// partial frame is zero-padded — the device keeps its own pacing by whole
/// blocks (forensics §2.2; assumption to verify live: padding ≤ 42 ms of
/// trailing silence is inaudible).
pub fn wav_to_device_frames(wav: &[u8], frame_bytes: usize) -> Option<(Vec<Vec<u8>>, u64)> {
    let (samples, rate, channels) = parse_wav(wav)?;
    let mono = to_mono(&samples, channels);
    let mut out = if rate == 48000 {
        mono
    } else {
        resample(&mono, rate, 48000)
    };
    let ms = (out.len() as u64 * 1000) / 48000;
    // Tail fade (RUI dot2-plug migration): a stream ending on a non-zero
    // sample steps to the padded zeros / silence wire — heard as a click
    // or power-cut pop after the last word (user report 2026-11-10).
    // Fading the REAL tail here (before the frame padding) covers every
    // consumer of this path.
    // Same 10 ms / 480-sample tail ramp the firmware playback loop
    // applies to streamed tails — the equivalence contract test compares
    // whole-play and streamed output sample for sample, so the two fades
    // must be IDENTICAL in length and shape.
    let fade_n = (480usize).min(out.len());
    let n = out.len();
    for i in 0..fade_n {
        // i counts back from the end: i=0 is the LAST sample → zero.
        let g = if fade_n == 1 {
            0.0
        } else {
            i as f32 / (fade_n - 1) as f32
        };
        out[n - 1 - i] = (out[n - 1 - i] as f32 * g) as i16;
    }
    let mut bytes = Vec::with_capacity(out.len() * 2);
    for s in out {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    let mut frames = Vec::new();
    for chunk in bytes.chunks(frame_bytes) {
        let mut f = chunk.to_vec();
        while f.len() < frame_bytes {
            f.push(0);
        }
        frames.push(f);
    }
    Some((frames, ms))
}

/// Whole-WAV play, FLAT: decode → mono 48k → one `Vec<i16>`.
///
/// D-stage OOM fix (2026-11-10): the old `wav_to_device_frames` turned the
/// same audio into `bytes` (5.4 MB for a 28 s asset) plus one `Vec<u8>` per
/// 4096-byte frame (~690 separate heap blocks), and the caller then built a
/// `Vec<i16>` per period. That is ~24 MB of churn per play, and the on-device
/// measurement showed the musl heap answering it by growing: +7.9 MB of RSS
/// per 28 s play, +0.1 MB for a 275 ms one, flat when no play happens at all
/// (three bisect arms, `docs/d/d-stage-close-20261110.md` §1.6). One flat
/// buffer keeps the same samples and the same tail fade with a single
/// allocation, and the playback thread slices it per period.
///
/// Byte-for-byte identical output to `wav_to_device_frames` (same fade, same
/// zero padding of the last period) — pinned by the equivalence contract
/// test.
pub fn wav_to_pcm16(wav: &[u8]) -> Option<(Vec<i16>, u64)> {
    let (samples, rate, channels) = parse_wav(wav)?;
    let mono = to_mono(&samples, channels);
    let mut out = if rate == 48000 {
        mono
    } else {
        resample(&mono, rate, 48000)
    };
    let ms = (out.len() as u64 * 1000) / 48000;
    let fade_n = (480usize).min(out.len());
    let n = out.len();
    for i in 0..fade_n {
        let g = if fade_n == 1 {
            0.0
        } else {
            i as f32 / (fade_n - 1) as f32
        };
        out[n - 1 - i] = (out[n - 1 - i] as f32 * g) as i16;
    }
    // Pad to a whole ALSA period, exactly like the per-frame path did. The
    // playback loop's last-period fade must land on the same samples as
    // before, otherwise whole-play and streamed-play stop being sample-equal
    // (the contract test compares them byte for byte).
    const PERIOD: usize = 2048;
    while out.len() % PERIOD != 0 {
        out.push(0);
    }
    Some((out, ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, ms: usize, rate: u32) -> Vec<i16> {
        let n = rate as usize * ms / 1000;
        (0..n)
            .map(|i| {
                ((freq * i as f64 * 2.0 * std::f64::consts::PI / rate as f64).sin() * 8000.0) as i16
            })
            .collect()
    }

    fn wav_bytes(samples: &[i16], rate: u32, channels: u16) -> Vec<u8> {
        let mut w = Vec::new();
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&((36 + samples.len() * 2) as u32).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&channels.to_le_bytes());
        w.extend_from_slice(&rate.to_le_bytes());
        w.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
        w.extend_from_slice(&(channels * 2).to_le_bytes());
        w.extend_from_slice(&16u16.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&((samples.len() * 2) as u32).to_le_bytes());
        for s in samples {
            w.extend_from_slice(&s.to_le_bytes());
        }
        w
    }

    #[test]
    fn parse_wav_round_trip() {
        let s = sine(1000.0, 50, 16000);
        let wav = wav_bytes(&s, 16000, 1);
        let (out, rate, ch) = parse_wav(&wav).unwrap();
        assert_eq!((rate, ch), (16000, 1));
        assert_eq!(out, s);
    }

    #[test]
    fn parse_wav_rejects_garbage() {
        assert!(parse_wav(b"not a wav").is_none());
        assert!(parse_wav(&[]).is_none());
    }

    #[test]
    fn parse_wav_rejects_zero_rate_or_channels() {
        let s = sine(1000.0, 10, 16000);
        let zero_rate = wav_bytes(&s, 0, 1);
        assert!(parse_wav(&zero_rate).is_none(), "rate=0 must be rejected");
        let zero_ch = wav_bytes(&s, 16000, 0);
        assert!(parse_wav(&zero_ch).is_none(), "channels=0 must be rejected");
    }

    #[test]
    fn to_mono_averages_channels() {
        let stereo = vec![100, 200, -100, -200];
        assert_eq!(to_mono(&stereo, 2), vec![150, -150]);
        assert_eq!(to_mono(&stereo[..2], 1), vec![100, 200]);
    }

    #[test]
    fn resample_identity_and_length() {
        let s = sine(1000.0, 100, 16000);
        assert_eq!(resample(&s, 16000, 16000), s);
        let up = resample(&s, 16000, 48000);
        assert_eq!(up.len(), s.len() * 3);
        let down = resample(&s, 48000, 16000);
        assert_eq!(down.len(), s.len().div_ceil(3));
    }

    #[test]
    fn cubic_upsample_beats_linear_on_pure_tone() {
        // 1 kHz at 16 kHz = 16 samples/period; cubic should track the
        // analytic signal closely (and better than linear interpolation).
        let s = sine(1000.0, 100, 16000);
        let up = resample(&s, 16000, 48000);
        let mut err_cubic = 0.0f64;
        let mut err_linear = 0.0f64;
        let n = s.len();
        for (i, &v) in up.iter().enumerate() {
            let t = i as f64 / 3.0;
            let exact = (1000.0 * t * 2.0 * std::f64::consts::PI / 16000.0).sin() * 8000.0;
            err_cubic += (v as f64 - exact).powi(2);
            let i0 = (t.floor() as usize).min(n - 1);
            let i1 = (i0 + 1).min(n - 1);
            let lin = s[i0] as f64 + (s[i1] as f64 - s[i0] as f64) * (t - i0 as f64);
            err_linear += (lin - exact).powi(2);
        }
        let rms_cubic = (err_cubic / up.len() as f64).sqrt();
        let rms_linear = (err_linear / up.len() as f64).sqrt();
        assert!(rms_cubic < 40.0, "cubic RMS error too high: {rms_cubic}");
        assert!(
            rms_cubic < rms_linear,
            "cubic ({rms_cubic}) must beat linear ({rms_linear})"
        );
    }

    #[test]
    fn wav_to_device_frames_chunking() {
        let s = sine(1000.0, 100, 16000); // 1600 samples → 4800 at 48k → 9600 bytes
        let wav = wav_bytes(&s, 16000, 1);
        let (frames, ms) = wav_to_device_frames(&wav, 4096).unwrap();
        assert_eq!(frames.len(), 9600_usize.div_ceil(4096)); // 3 frames (padded last)
        assert!(frames.iter().all(|f| f.len() == 4096), "whole frames only");
        assert_eq!(ms, 100);
    }
}

/// Streaming face of [`resample`]: feed chunks, receive the outputs whose
/// full interpolation window is already available, carry the rest. The
/// contract (v0.3 §5) pins exact equivalence: the concatenation of all
/// `push` outputs plus `finish` equals `resample(whole_input)` sample for
/// sample, for ANY chunk split.
pub struct StreamResampler {
    from: u32,
    to: u32,
    /// Unprocessed input tail, starting at global index `abs_start`.
    carry: Vec<i16>,
    /// Global index of carry[0] in the total input stream.
    abs_start: u64,
    /// Next global output index to emit.
    next_out: u64,
    /// Total input samples ever pushed (set final on finish).
    total_in: u64,
    finished: bool,
}

impl StreamResampler {
    pub fn new(from: u32, to: u32) -> StreamResampler {
        StreamResampler {
            from,
            to,
            carry: Vec::new(),
            abs_start: 0,
            next_out: 0,
            total_in: 0,
            finished: false,
        }
    }

    fn interp(&self, k: isize, last: isize) -> f64 {
        let idx = k.clamp(0, last) as usize;
        self.carry[(idx as u64 - self.abs_start) as usize] as f64
    }

    /// Feed one chunk; returns whatever outputs became fully determined
    /// (their Catmull window no longer depends on future input).
    pub fn push(&mut self, chunk: &[i16]) -> Vec<i16> {
        assert!(!self.finished, "push after finish");
        self.carry.extend_from_slice(chunk);
        self.total_in += chunk.len() as u64;
        self.emit(false)
    }

    /// End of stream: flush the deferred tail with end-clamped windows.
    pub fn finish(&mut self) -> Vec<i16> {
        self.finished = true;
        self.emit(true)
    }

    fn emit(&mut self, finishing: bool) -> Vec<i16> {
        if self.from == self.to {
            // Passthrough: identity is split-invariant by construction.
            let out = self.carry.clone();
            self.abs_start = self.total_in;
            self.carry.clear();
            return out;
        }
        let last: isize = self.total_in as isize - 1; // global last index
        let mut out = Vec::new();
        loop {
            // Output next_out interpolates at t = next_out * from/to.
            let t = self.next_out as f64 * self.from as f64 / self.to as f64;
            let i0 = t.floor() as isize;
            // The window is i0-1 ..= i0+2 (indices clamped to [0, last] by
            // the whole-buffer reference). Before finish we may only emit
            // when the highest NEEDED unclamped index is within the pushed
            // input — end-clamping belongs to finish alone.
            let needed = if i0 + 2 > last { last } else { i0 + 2 };
            if needed > last && !finishing {
                break; // would clamp at the end — wait for more input
            }
            if !finishing && (i0 + 2) > last {
                break;
            }
            // All window indices must sit inside the carry (drop consumed
            // front only when safe: keep from the lowest index any future
            // output can still need, i.e. next_out's i0-1).
            let f = t - i0 as f64;
            let p0 = self.interp(i0 - 1, last);
            let p1 = self.interp(i0, last);
            let p2 = self.interp(i0 + 1, last);
            let p3 = self.interp(i0 + 2, last);
            let v = 0.5
                * ((2.0 * p1)
                    + (-p0 + p2) * f
                    + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * f * f
                    + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * f * f * f);
            out.push(v.round().clamp(-32768.0, 32767.0) as i16);
            self.next_out += 1;
            // Total output count for the finished stream must match the
            // whole-buffer reference: n_out = ceil(total_in * to / from).
            if self.finished {
                let n_total = (self.total_in * self.to as u64).div_ceil(self.from as u64);
                if self.next_out >= n_total {
                    break;
                }
            }
            // Trim the carry front: future outputs need at most i0-1 back.
            let keep_from =
                (self.next_out as f64 * self.from as f64 / self.to as f64).floor() as i64 - 1;
            let drop_until = keep_from.max(self.abs_start as i64);
            let n_drop = (drop_until - self.abs_start as i64).max(0) as usize;
            if n_drop > 0 && n_drop <= self.carry.len() {
                self.carry.drain(..n_drop);
                self.abs_start += n_drop as u64;
            }
        }
        out
    }
}

#[cfg(test)]
mod stream_tests {
    use super::*;

    /// Exact equivalence for arbitrary splits: concatenate push outputs +
    /// finish == the one-shot resample, byte for byte.
    #[test]
    fn stream_matches_whole_for_any_split() {
        let mut rng: u64 = 20261110;
        let mut next = move || {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (((rng >> 33) as i32 % 20000) as f32 / 20000.0 * 12000.0 - 6000.0) as i16
        };
        for (from, to) in [
            (16000u32, 48000u32),
            (24000, 48000),
            (22050, 48000),
            (44100, 48000),
        ] {
            for split in [1usize, 7, 321, 1024] {
                let input: Vec<i16> = (0..5000).map(|_| next()).collect();
                let whole = resample(&input, from, to);
                let mut sr = StreamResampler::new(from, to);
                let mut streamed = Vec::new();
                for chunk in input.chunks(split) {
                    streamed.extend(sr.push(chunk));
                }
                streamed.extend(sr.finish());
                assert_eq!(
                    streamed, whole,
                    "from={from} to={to} split={split}: streamed differs from whole"
                );
            }
        }
    }

    /// Identity rate passes through untouched across arbitrary splits.
    #[test]
    fn identity_passthrough() {
        let input: Vec<i16> = (0..999).map(|i| (i * 37 % 3000 - 1500) as i16).collect();
        let mut sr = StreamResampler::new(48000, 48000);
        let mut out = Vec::new();
        for c in input.chunks(13) {
            out.extend(sr.push(c));
        }
        out.extend(sr.finish());
        assert_eq!(out, input);
    }
}
