//! Reference de-aliasing: rebuild the ch8 hardware loopback without its
//! decimation aliases, by matching it against the firmware's own playback
//! output ring. Faithful port of EchoMuse's device/internal/aec/dealias.go
//! (race-review hardened upstream: search runs on a copy, stop-checks under
//! the same lock, owned output slices).
//!
//! Why this exists (their measured evidence, VVV 2026-10-18): ch8 is the
//! playback looped into the 16kHz mic capture by keeping every third sample
//! of the 48kHz output with NO anti-aliasing filter, so everything the
//! output carries above 8kHz lands in-band in the reference at full level
//! while the microphones (codec-filtered) show nothing there — the
//! canceller chases spectral ghosts: 1dB cancellation on pink noise with
//! raw ch8, 25.7dB with the rebuilt reference.
//!
//! The cure is the output itself: the firmware writes every period, ch8 is
//! an exact copy of every third written sample (to 1 LSB — the premise the
//! lock state PROVES on this unit; if a board's loopback never matches, the
//! raw ch8 passes through unchanged and the pipeline reports it honestly).
//! Locked: position tracked and verified per block, reference = the output
//! ring decimated through a proper low-pass.

use crate::fwlog;
use std::sync::Mutex;

const RING: usize = 1 << 16; // 1.37s of 48kHz output; write-ahead is ~171ms
const TAPS: usize = 95; // odd, so the filter's centre is a whole sample
const HALF: usize = TAPS / 2;
/// ch8 is the S16 output shifted into 24 bits; extracted back at unity it
/// differs by at most 1 — 2 allows the LSB of slop.
const TOL: i32 = 2;
/// A fresh lock needs this many non-silent agreeing samples; following one
/// needs every sample to agree, so quiet passages neither gain nor drop it.
const MIN_MATCH: usize = 64;
const MAX_MISSES: usize = 3;
const BACKOFF: u32 = 30; // batches to skip after repeated loud misses
/// Verbose miss diagnostics (candidates/pairs/raw bytes). Flip for field
/// debugging; leave off in production logs.
const MISS_DEBUG: bool = false;

struct Ring {
    buf: Vec<i16>, // 48kHz left channel
    head: i64,     // samples ever written; index = head % RING
}

struct Inner {
    locked: bool,
    pos: i64, // output index of the next ch8 sample, when locked
    ever_locked: bool,
    misses: u32,
    skip: u32,
    fir: Vec<f64>,
    span: Vec<i16>,
}

/// The locked/loss telemetry is the on-device PROOF that "written samples
/// equal the loopback" holds on this unit (user verification gate,
/// 2026-11-08).
#[derive(Default, Clone, Copy, Debug)]
pub struct Stats {
    pub locked: bool,
    pub ever_locked: bool,
    pub relocks: u32,
    pub losses: u32,
    pub backoffs: u32,
}

/// Whether every sample of a cleaned reference block came from a verified
/// output-ring match. `Degraded` means at least one 20 ms frame lost the
/// match (or the search failed) and the block was coasted from the last
/// known position — usable by the canceller, never a basis for a near-end
/// fact (the correlation/energy of a coasted block are not trustworthy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockState {
    Locked,
    Degraded,
}

/// One block of the cleaned DAC reference plus its integrity.
pub struct CleanedRef {
    pub samples: Vec<i16>,
    pub state: LockState,
}

/// Keep the canceller fed without ever handing it the raw ch8 alias: if the
/// output ring still holds the predicted window, use it (the output IS the
/// reference; only its alignment is unverified); otherwise zeros. The FIR
/// span is always advanced so a later relock does not replay stale samples.
fn coast_or_zeros(inner: &mut Inner, ring: &Mutex<Ring>, n: usize) -> Vec<i16> {
    let ring = ring.lock().unwrap();
    let w = Window {
        buf: &ring.buf,
        head: ring.head,
    };
    if inner.ever_locked && n > 0 && w.fits(inner.pos, n) {
        copy_span(inner, &w, inner.pos, n);
        inner.pos += 3 * n as i64;
        filter(&inner.fir, &inner.span, n)
    } else {
        inner.span.clear();
        inner.span.resize(3 * (n.saturating_sub(1)) + TAPS, 0);
        vec![0i16; n]
    }
}

pub struct Dealias {
    ring: Mutex<Ring>,
    inner: Mutex<Inner>,
    stats: Mutex<Stats>,
}

fn at(ring: &Ring, i: i64) -> i16 {
    ring.buf[((i % RING as i64) + RING as i64) as usize % RING]
}

/// Ring view valid for one operation — either the live ring under its
/// mutex, or a snapshot copy (searches run on the copy so the playback
/// thread never waits on them; upstream review #866).
struct Window<'a> {
    buf: &'a [i16],
    head: i64,
}

impl<'a> Window<'a> {
    fn at(&self, i: i64) -> i16 {
        self.buf[((i % RING as i64) + RING as i64) as usize % RING]
    }
    fn fits(&self, p: i64, n: usize) -> bool {
        let n = n as i64;
        p - HALF as i64 >= (self.head - RING as i64).max(0)
            && p + 3 * (n - 1) + (HALF as i64) < self.head
    }
    /// Does `ref` (16kHz S16) match every third output sample from `p`,
    /// with at least `need` non-silent agreements?
    fn matches(&self, r: &[i16], p: i64, need: usize) -> bool {
        let n = r.len();
        if !self.fits(p, n) {
            return false;
        }
        let mut agree = 0;
        for (i, s) in r.iter().enumerate() {
            let d = *s as i32 - self.at(p + 3 * i as i64) as i32;
            if !(-TOL..=TOL).contains(&d) {
                return false;
            }
            if *s > 64 || *s < -64 {
                agree += 1;
            }
        }
        agree >= need
    }
    /// Find the output index `r` starts at, newest first. `loud=false`
    /// means there was nothing to match on (not a miss).
    fn search(&self, r: &[i16]) -> (i64, bool, bool) {
        let mut i0 = None;
        for (i, s) in r.iter().enumerate() {
            if *s > 64 || *s < -64 {
                i0 = Some(i);
                break;
            }
        }
        let Some(i0) = i0 else {
            return (0, false, false);
        };
        let r0 = r[i0] as i32;
        let mut j = self.head - 1;
        while j >= (self.head - RING as i64).max(0) {
            let d = self.at(j) as i32 - r0;
            if (-TOL..=TOL).contains(&d) {
                let p = j - 3 * i0 as i64;
                if self.matches(r, p, MIN_MATCH) {
                    return (p, true, true);
                }
            }
            j -= 1;
        }
        (0, false, true)
    }
}

impl Dealias {
    pub fn new() -> Dealias {
        // Windowed sinc (Blackman), cut at 7.2kHz = 0.15 of 48kHz.
        let fc = 7200.0 / 48000.0;
        let mut fir = vec![0f64; TAPS];
        let mut sum = 0f64;
        for (i, t) in fir.iter_mut().enumerate() {
            let n = i as f64 - HALF as f64;
            let h = if n == 0.0 {
                2.0 * fc
            } else {
                (2.0 * std::f64::consts::PI * fc * n).sin() / (std::f64::consts::PI * n)
            };
            let w = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (TAPS - 1) as f64).cos()
                + 0.08 * (4.0 * std::f64::consts::PI * i as f64 / (TAPS - 1) as f64).cos();
            *t = h * w;
            sum += *t;
        }
        for t in fir.iter_mut() {
            *t /= sum;
        }
        Dealias {
            ring: Mutex::new(Ring {
                buf: vec![0i16; RING],
                head: 0,
            }),
            inner: Mutex::new(Inner {
                locked: false,
                pos: 0,
                ever_locked: false,
                misses: 0,
                skip: 0,
                fir,
                span: Vec::new(),
            }),
            stats: Mutex::new(Stats::default()),
        }
    }

    /// Playback side: append one written period (48kHz stereo S16LE); the
    /// LEFT channel is what ch8 mirrors.
    pub fn feed(&self, period: &[i16]) {
        if period.len() < 2 {
            return;
        }
        let mut ring = self.ring.lock().unwrap();
        for pair in period.as_chunks::<2>().0 {
            let idx = (ring.head.rem_euclid(RING as i64)) as usize;
            ring.buf[idx] = pair[0];
            ring.head += 1;
        }
    }

    /// Mic side: return the cleaned reference for this block. The output is
    /// ALWAYS rebuilt from the output ring (or zeros when no history is
    /// usable) — never the raw ch8 alias. Raw ch8 is an aliased decimation
    /// of the DAC stream; feeding it to the canceller poisons the filter
    /// (residual ≈ reference) and feeding it to the near-end guard makes
    /// the echo look uncorrelated. The `state` reports whether EVERY
    /// sample of this block came from a verified ring match.
    ///
    /// Interior-mutable: the playback thread holds the same Arc and feeds.
    pub fn clean(&self, r: &[i16], r_bytes: &[u8]) -> CleanedRef {
        let n = r.len();
        if r.iter().all(|&s| s == 0) {
            // Zero ch8 is not proof of DAC silence. Keep the relock cursor
            // across an unverified gap, but report THIS block degraded.
            // Only advance `pos` if the output ring actually holds matching
            // zeros at `pos` (i.e. silence written to the DAC); across dropped
            // or zeroed ch8 samples `pos` must NOT drift ahead of the ring.
            let mut inner = self.inner.lock().unwrap();
            let state = if inner.locked {
                let ring = self.ring.lock().unwrap();
                let w = Window {
                    buf: &ring.buf,
                    head: ring.head,
                };
                if w.matches(r, inner.pos, 0) {
                    inner.pos += 3 * n as i64;
                    LockState::Locked
                } else {
                    LockState::Degraded
                }
            } else {
                LockState::Degraded
            };
            return CleanedRef {
                samples: r.to_vec(),
                state,
            };
        }
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.locked {
                let ok = {
                    let ring = self.ring.lock().unwrap();
                    let w = Window {
                        buf: &ring.buf,
                        head: ring.head,
                    };
                    let pos = inner.pos;
                    if w.matches(r, pos, 0) {
                        copy_span(&mut inner, &w, pos, n);
                        true
                    } else {
                        false
                    }
                };
                if ok {
                    inner.pos += 3 * n as i64;
                    return CleanedRef {
                        samples: filter(&inner.fir, &inner.span, n),
                        state: LockState::Locked,
                    };
                }
                inner.locked = false;
                self.stats.lock().unwrap().losses += 1;
            }
            if inner.skip > 0 {
                inner.skip -= 1;
                let samples = coast_or_zeros(&mut inner, &self.ring, n);
                return CleanedRef {
                    samples,
                    state: LockState::Degraded,
                };
            }
        }
        // Search on a copy so the playback thread never waits (#866).
        let (p, ok, loud) = {
            let ring = self.ring.lock().unwrap();
            let head = ring.head;
            let snapshot = ring.buf.clone();
            drop(ring);
            let w = Window {
                buf: &snapshot,
                head,
            };
            w.search(r)
        };
        let mut inner = self.inner.lock().unwrap();
        if !ok {
            if loud {
                // Per-backoff diagnostic: anchor like the search does
                // (first loud ref sample), then for the first few ring
                // candidates report the prefix agreement length and the
                // sample diff at the first mismatch — "off by 1 LSB" vs
                // "scaled" vs "wrong channel" vs "not there" at a glance.
                if MISS_DEBUG {
                    if let Some(i0) = r.iter().position(|s| s.abs() > 64) {
                        let ring = self.ring.lock().unwrap();
                        let head = ring.head;
                        let n = r.len();
                        let r0 = r[i0] as i32;
                        let lo = (head - RING as i64).max(0);
                        let mut shown = 0;
                        let mut candidates = 0;
                        let mut best_prefix = 0usize;
                        let mut j = head - 1;
                        while j >= lo && shown < 3 {
                            if (at(&ring, j) as i32 - r0).abs() <= TOL {
                                candidates += 1;
                                let p = j - 3 * i0 as i64;
                                let mut k = 0usize;
                                while i0 + k < n {
                                    let pos = p + 3 * (i0 + k) as i64;
                                    if pos < 0 || pos >= head {
                                        break;
                                    }
                                    if (r[i0 + k] as i32 - at(&ring, pos) as i32).abs() > TOL {
                                        break;
                                    }
                                    k += 1;
                                }
                                best_prefix = best_prefix.max(k);
                                if k > 2 {
                                    let pos = p + 3 * (i0 + k) as i64;
                                    let diff = if pos >= 0 && pos < head {
                                        r[i0 + k] as i32 - at(&ring, pos) as i32
                                    } else {
                                        0
                                    };
                                    // Pairs around the anchor: ref vs ring value
                                    // by value — offset vs gain vs skew at a look.
                                    let mut pairs: Vec<String> = Vec::new();
                                    for m in 0..10 {
                                        let ri = i0 + m;
                                        let pi = p + 3 * ri as i64;
                                        if pi >= 0 && pi < head && ri < n {
                                            pairs.push(format!("{}:{}", r[ri], at(&ring, pi)));
                                        }
                                    }
                                    // Raw bytes of the first few ref samples:
                                    // settles the ch8 S24 encoding (sign position,
                                    // shift, offset) without more theory.
                                    let mut raws: Vec<String> = Vec::new();
                                    for m in 0..6 {
                                        let ri = i0 + m;
                                        if ri < n {
                                            let b = &r_bytes[ri * 3..ri * 3 + 3];
                                            raws.push(format!(
                                                "{:02x}{:02x}{:02x}",
                                                b[0], b[1], b[2]
                                            ));
                                        }
                                    }
                                    fwlog!("[aec] dealias raw ch8 bytes: {raws:?}");
                                    fwlog!(
                                    "[aec] dealias miss: cand pos {p} prefix {k} diff {diff} pairs(r:ring) {pairs:?} (head {head}, i0 {i0})"
                                );
                                    shown += 1;
                                }
                            }
                            j -= 1;
                        }
                        fwlog!(
                        "[aec] dealias miss: {candidates} anchor candidates, best prefix {best_prefix} (r0 {r0}, head {head})"
                    );
                        let rs: Vec<i32> = r.iter().skip(i0).take(12).map(|s| *s as i32).collect();
                        let mut tail: Vec<i32> = Vec::new();
                        for k in 0..12 {
                            let pos = head - 12 + k as i64;
                            tail.push(at(&ring, pos) as i32);
                        }
                        let rmax = r.iter().map(|s| s.abs()).max().unwrap_or(0);
                        let wmax = (lo..head).map(|p| at(&ring, p).abs()).max().unwrap_or(0);
                        fwlog!(
                        "[aec] dealias ref[i0..]={rs:?} ring_tail={tail:?} ref_max={rmax} ring_max={wmax}"
                    );
                    }
                } // MISS_DEBUG
                inner.misses += 1;
                if inner.misses >= MAX_MISSES as u32 {
                    inner.misses = 0;
                    inner.skip = BACKOFF;
                    self.stats.lock().unwrap().backoffs += 1;
                }
            }
            let samples = coast_or_zeros(&mut inner, &self.ring, n);
            return CleanedRef {
                samples,
                state: LockState::Degraded,
            };
        }
        inner.misses = 0;
        let mut stats = self.stats.lock().unwrap();
        if inner.ever_locked {
            stats.relocks += 1;
        } else {
            inner.ever_locked = true;
            fwlog!("[aec] de-aliased reference locked to the output");
        }
        inner.locked = true;
        let ring = self.ring.lock().unwrap();
        let w = Window {
            buf: &ring.buf,
            head: ring.head,
        };
        copy_span(&mut inner, &w, p, n);
        inner.pos = p + 3 * n as i64;
        CleanedRef {
            samples: filter(&inner.fir, &inner.span, n),
            state: LockState::Locked,
        }
    }

    /// Playback side: the DAC ring buffer was flushed (`snd_pcm_drop`), so
    /// samples buffered ahead of the current `pos` will never reach ch8.
    /// Unlock cleanly without counting a loss or triggering backoff so the
    /// next non-zero reference block resyncs immediately from `ring.head`.
    pub fn on_dac_drop(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.locked = false;
        inner.misses = 0;
        inner.skip = 0;
    }

    pub fn stats(&self) -> Stats {
        let inner = self.inner.lock().unwrap();
        let s = *self.stats.lock().unwrap();
        Stats {
            locked: inner.locked,
            ever_locked: inner.ever_locked,
            ..s
        }
    }
}

/// Fill `span` with the output samples the filter needs for n ch8
/// samples from p.
fn copy_span(inner: &mut Inner, w: &Window, p: i64, n: usize) {
    let size = 3 * (n - 1) + TAPS;
    inner.span.clear();
    inner.span.reserve(size);
    for i in 0..size {
        inner.span.push(w.at(p - HALF as i64 + i as i64));
    }
}

/// Decimate `span` through the low-pass: n output samples.
fn filter(fir: &[f64], span: &[i16], n: usize) -> Vec<i16> {
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let mut acc = 0f64;
        for (k, h) in fir.iter().enumerate() {
            acc += h * span[3 * i + k] as f64;
        }
        out.push(acc.round().clamp(-32768.0, 32767.0) as i16);
    }
    out
}

impl Default for Dealias {
    fn default() -> Self {
        Dealias::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The premise this whole module rides on, proven in-process: ch8 =
    /// every third written output sample, matched and rebuilt cleanly.
    #[test]
    fn locks_and_rebuilds_from_output() {
        let d = Dealias::new();
        // Pre-fill the ring with a pseudorandom non-silent signal (loud so
        // the search has content), stereo interleaved.
        let mut rng = 12345u64;
        let mut next = move || {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((rng >> 33) as i32 % 8000) as i16
        };
        let mut period = vec![0i16; 2 * 4800]; // 100ms stereo
        for pair in period.as_chunks_mut::<2>().0 {
            let v = next();
            pair[0] = v;
            pair[1] = v;
        }
        d.feed(&period);
        // ch8 block = every third left sample starting somewhere inside.
        let start_off = 300i64;
        let n = 160;
        let ring = d.ring.lock().unwrap();
        let mut ch8 = vec![0i16; n];
        for (i, slot) in ch8.iter_mut().enumerate().take(n) {
            *slot = at(&ring, start_off + 3 * i as i64);
        }
        drop(ring);
        let cleaned = d.clean(&ch8, &[]);
        assert_eq!(
            cleaned.state,
            LockState::Locked,
            "a verified frame is locked"
        );
        let cleaned = cleaned.samples;
        let st = d.stats();
        assert!(st.locked && st.ever_locked, "must lock: {st:?}");
        // Rebuilt reference is a low-passed version: for a broadband input
        // it cannot equal the raw aliased ch8 everywhere, but it must stay
        // correlated with the true (slowly varying) signal. Feed smooth
        // low-frequency content instead to assert closeness numerically.
        let d2 = Dealias::new();
        let mut smooth = vec![0i16; 2 * 48000]; // 1s stereo, 300Hz sine
        for (i, pair) in smooth.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            let v =
                (3000.0 * (2.0 * std::f64::consts::PI * 300.0 * i as f64 / 48000.0).sin()) as i16;
            pair[0] = v;
            pair[1] = v;
        }
        d2.feed(&smooth);
        let ring = d2.ring.lock().unwrap();
        let mut ch8b = vec![0i16; 160];
        for (i, slot) in ch8b.iter_mut().enumerate().take(160) {
            *slot = at(&ring, 4000 + 3 * i as i64);
        }
        drop(ring);
        let cleaned_b = d2.clean(&ch8b, &[]);
        assert_eq!(cleaned_b.state, LockState::Locked, "smooth tone must lock");
        assert!(d2.stats().locked, "smooth tone must lock");
        let cleaned_b = cleaned_b.samples;
        // A 300Hz sine decimated cleanly is itself: output ≈ ch8 samples
        // (no aliases to remove), to FIR tolerance.
        for (a, b) in cleaned_b.iter().zip(ch8b.iter()) {
            assert!((a - b).abs() < 48, "clean {} vs raw {}", a, b);
        }
        let _ = cleaned;
    }

    /// Silence passes through and a lock survives it; a never-matching
    /// loud reference falls back to raw ch8 with backoff.
    #[test]
    fn silence_passthrough_and_unlocked_is_never_the_raw_alias() {
        let d = Dealias::new();
        let silent = vec![0i16; 160];
        let out = d.clean(&silent, &[]);
        assert_eq!(out.samples, silent);
        assert_eq!(out.state, LockState::Degraded, "no lock yet: honest state");
        // Loud but not derived from the (empty) ring: the output must NOT
        // be the raw ch8 (feeding it to the canceller poisons the filter;
        // this was the 2026-11-12 self-hearing root cause). With no ring
        // history the only honest reference is silence.
        let loud = vec![5000i16; 160];
        for _ in 0..5 {
            let out = d.clean(&loud, &[]);
            assert_eq!(
                out.samples,
                vec![0i16; 160],
                "unlocked reference must not be raw ch8"
            );
            assert_eq!(out.state, LockState::Degraded);
        }
        assert!(d.stats().backoffs >= 1);
        assert!(!d.stats().ever_locked);
    }

    /// A lock lost inside a block is visible to the caller: one bad frame
    /// makes that frame Degraded even though the next frame may relock.
    /// The old code sampled `stats().locked` after the block, so a loss +
    /// relock in the same 80 ms looked `locked=true` and the detector fired
    /// on a block that had been fed a bad reference (candidate B, 2026-11-12).
    #[test]
    fn a_lost_lock_inside_the_block_is_degraded_not_hidden() {
        let d = Dealias::new();
        // Ring with a clean ramp; lock on the true ch8 samples.
        let mut period = Vec::new();
        for i in 0..3000i64 {
            let s = ((i % 400) - 200) as i16 * 40;
            period.push(s);
            period.push(s);
        }
        d.feed(&period);
        let ring = d.ring.lock().unwrap();
        let start = ring.head - 960;
        let mut ch8 = vec![0i16; 320];
        for (i, slot) in ch8.iter_mut().enumerate().take(320) {
            *slot = at(&ring, start + 3 * i as i64);
        }
        drop(ring);
        assert_eq!(d.clean(&ch8, &[]).state, LockState::Locked);
        // Corrupt one sample of the next frame: the match fails, and the
        // block must report Degraded (coast), never pretend it was clean.
        let mut broken = ch8.clone();
        broken[100] = broken[100].wrapping_add(4000);
        let out = d.clean(&broken, &[]);
        assert_eq!(out.state, LockState::Degraded);
        assert_ne!(
            out.samples, broken,
            "degraded block must not pass the alias through"
        );
    }

    /// #18: An all-zero ch8 block while locked (e.g. DAC gap / dropped buffer)
    /// must NOT advance `pos` past the ring. When aligned non-zero samples
    /// resume, `clean` stays locked with zero `pos` drift, zero losses, and
    /// no full-range search.
    #[test]
    fn all_zero_gap_does_not_drift_pos_and_resyncs_promptly() {
        let d = Dealias::new();
        let mut rng = 98765u64;
        let mut next = move || {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((rng >> 33) as i32 % 8000) as i16
        };
        let mut period = vec![0i16; 2 * 4800]; // 100 ms stereo
        for pair in period.as_chunks_mut::<2>().0 {
            let v = next();
            pair[0] = v;
            pair[1] = v;
        }
        d.feed(&period);

        let n = 320; // 20 ms at 16 kHz
        let start_off = 300i64;
        let (ch8_a, ch8_b) = {
            let ring = d.ring.lock().unwrap();
            let a: Vec<i16> = (0..n)
                .map(|i| at(&ring, start_off + 3 * i as i64))
                .collect();
            let b: Vec<i16> = (n..2 * n)
                .map(|i| at(&ring, start_off + 3 * i as i64))
                .collect();
            (a, b)
        };

        // 1. Lock on the first block.
        let out_a = d.clean(&ch8_a, &[]);
        assert_eq!(out_a.state, LockState::Locked);
        let pos_after_a = d.inner.lock().unwrap().pos;

        // 2. Feed several all-zero reference blocks while the ring still holds
        // non-zero audio at `pos` (simulating dropped/zeroed ch8 samples).
        let zeros = vec![0i16; n];
        for _ in 0..5 {
            let out_z = d.clean(&zeros, &[]);
            assert_eq!(out_z.samples, zeros);
            assert_eq!(out_z.state, LockState::Degraded);
        }
        assert_eq!(
            d.inner.lock().unwrap().pos,
            pos_after_a,
            "pos must not drift across all-zero blocks when ring has non-zero audio"
        );

        // 3. Feed the aligned continuation block: stays locked immediately
        // without full-range search, losses, or relocks.
        let out_b = d.clean(&ch8_b, &[]);
        assert_eq!(out_b.state, LockState::Locked);
        let st = d.stats();
        assert!(st.locked);
        assert_eq!(st.losses, 0, "no spurious lock loss across zero gap");
        assert_eq!(st.relocks, 0, "no full-range search needed");
        assert_eq!(st.backoffs, 0);

        // 4. After an explicit DAC drop (Flush), feeding a new non-zero block
        // relocks on the very first block without counting a loss.
        d.on_dac_drop();
        let out_z2 = d.clean(&zeros, &[]);
        assert_eq!(out_z2.state, LockState::Degraded);
        let out_relock = d.clean(&ch8_b, &[]);
        assert_eq!(out_relock.state, LockState::Locked);
        let st2 = d.stats();
        assert!(st2.locked);
        assert_eq!(st2.losses, 0);
        assert_eq!(st2.relocks, 1);
    }

    #[test]
    fn verified_dac_silence_is_locked_while_zeroed_nonzero_history_is_not() {
        let d = Dealias::new();
        let mut rng = 91u64;
        let mut period = vec![0i16; 2 * 4800];
        for pair in period[..2 * 1260].as_chunks_mut::<2>().0 {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            let sample = ((rng >> 33) as i32 % 8000) as i16;
            pair.copy_from_slice(&[sample, sample]);
        }
        d.feed(&period);
        let first: Vec<i16> = (0..320).map(|i| period[2 * (300 + 3 * i)]).collect();
        assert_eq!(d.clean(&first, &[]).state, LockState::Locked);
        assert_eq!(d.inner.lock().unwrap().pos, 1260);
        let silence = d.clean(&[0; 320], &[]);
        assert_eq!(silence.state, LockState::Locked);
        assert_eq!(d.inner.lock().unwrap().pos, 2220);
    }
}
