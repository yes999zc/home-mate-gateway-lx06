//! Direct ALSA PCM access via /dev/snd ioctls — zero C deps.
//!
//! ## Why this file looks unusual (live hardware facts, 2026-11-08)
//!
//! The dot2-d kernel is an Amazon/FireOS vendor fork whose ALSA ABI deviates
//! from mainline in THREE ways, each caught empirically (`gateway pcmprobe`,
//! evidence in docs/g5/direct-firmware-design-20261108.md §5):
//!
//! 1. **Struct size + layout**: `snd_pcm_hw_params` is a 604-byte (32-bit
//!    task) / 608-byte (64-bit task) vendor struct — not mainline's 392.
//!    Field offsets decoded from the system's own libtinyalsa.so
//!    `param_init`: masks at 0x04/0x08, 0x24/0x28, 0x44/0x48; intervals at
//!    0x104 (12 × {min,max,flags}); rmask/cmask/info at 0x200/0x204/0x208.
//! 2. **Vendor enums**: S24_3LE is mask bit **32** (format word 1, bit 0),
//!    not mainline's bit 6. We never hardcode it — the refine step hands us
//!    the kernel's own mask.
//! 3. **hw_params only accepts a REFINED buffer**: the kernel validates
//!    fields (info, masks) that only HW_REFINE legitimizes. The working flow
//!    is exactly libtinyalsa's: param_init → HW_REFINE (kernel fills its
//!    constraints) → pin the interval values → HW_PARAMS. Notably the
//!    system's own 64-bit tinycap CANNOT open the capture device (EINVAL);
//!    the 32-bit compat path is the only one that works — the firmware
//!    therefore builds for armv7.
//!
//! The "blocked open" behavior is also live evidence: opening a PCM the
//! EchoMuse server still holds BLOCKS (no EBUSY) — a starting component
//! simply waits and takes over when the old owner stops.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;

const IOC_NONE: u32 = 0;
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

/// The asm-generic encoding — and, empirically, the ONLY one this kernel's
/// compat dispatch matches (the arm32-specific DIRSHIFT=29 layout is NOT
/// what its compat table holds; caught live via `gateway pcmprobe`).
const fn ioc(dir: u32, ty: u8, nr: u8, size: u32) -> libc::c_ulong {
    ((dir << 30) | (size << 16) | ((ty as u32) << 8) | (nr as u32)) as libc::c_ulong
}

// --- vendor hw_params layout (decoded from libtinyalsa; header note 1) -----------

/// Total struct size per task width (vendor ABI).
#[cfg(target_arch = "arm")]
pub const HW_PARAMS_SIZE: usize = 604;
#[cfg(not(target_arch = "arm"))]
pub const HW_PARAMS_SIZE: usize = 608;

const OFF_FLAGS: usize = 0x000;
/// masks: [0]=access (0x04/0x08), [1]=format (0x24/0x28), [2]=subformat (0x44/0x48)
const MASK_WORDS: [(usize, usize); 3] = [(0x04, 0x08), (0x24, 0x28), (0x44, 0x48)];
const OFF_INTERVALS: usize = 0x104;
const INTERVAL_COUNT: usize = 12;
const OFF_RMASK: usize = 0x200;

// interval param indices (vendor = mainline numbering here).
const P_SAMPLE_BITS: usize = 3;
const P_CHANNELS: usize = 5;
const P_RATE: usize = 6;
const P_PERIOD_SIZE: usize = 8;
const P_PERIODS: usize = 10;
const P_BUFFER_SIZE: usize = 12;

fn req_hw_refine() -> libc::c_ulong {
    ioc(IOC_WRITE | IOC_READ, b'A', 0x10, HW_PARAMS_SIZE as u32)
}
fn req_hw_params() -> libc::c_ulong {
    ioc(IOC_WRITE | IOC_READ, b'A', 0x11, HW_PARAMS_SIZE as u32)
}
fn req_prepare() -> libc::c_ulong {
    ioc(IOC_NONE, b'A', 0x40, 0)
}
fn req_start() -> libc::c_ulong {
    ioc(IOC_NONE, b'A', 0x42, 0)
}
fn req_drop() -> libc::c_ulong {
    ioc(IOC_NONE, b'A', 0x43, 0)
}
fn req_drain() -> libc::c_ulong {
    ioc(IOC_NONE, b'A', 0x44, 0)
}
/// 2026-11-13 (M6a): SNDRV_PCM_IOCTL_DELAY — frames between the application
/// pointer and the DAC's hardware pointer. The only device-side fact that
/// separates "we wrote N ms" from "the hardware consumed N−D ms".
fn req_delay() -> libc::c_ulong {
    ioc(
        IOC_READ,
        b'A',
        0x21,
        std::mem::size_of::<libc::c_long>() as u32,
    )
}

/// struct snd_xferi — 12 B as a 32-bit task, 24 B as a 64-bit one; the
/// request-code size must match (kernel compares the whole cmd word).
#[repr(C)]
struct Xferi {
    result: i32,
    #[cfg(not(target_arch = "arm"))]
    _pad: i32,
    buf: *mut libc::c_void,
    frames: u32,
    #[cfg(not(target_arch = "arm"))]
    _pad2: i32,
}

fn req_writei() -> libc::c_ulong {
    ioc(IOC_WRITE, b'A', 0x50, std::mem::size_of::<Xferi>() as u32)
}
fn req_readi() -> libc::c_ulong {
    ioc(IOC_READ, b'A', 0x51, std::mem::size_of::<Xferi>() as u32)
}

/// libc's ioctl request type is `c_ulong` on macOS and glibc, and `c_int` on
/// musl (the armv7 firmware). Casting every non-macOS request to `c_int`
/// does not type-check against glibc, so `cargo test` never started on Linux.
#[cfg(target_os = "macos")]
fn raw_ioctl(fd: libc::c_int, req: libc::c_ulong, arg: *mut libc::c_void) -> libc::c_int {
    unsafe { libc::ioctl(fd, req, arg) }
}
#[cfg(not(target_os = "macos"))]
fn raw_ioctl(fd: libc::c_int, req: libc::c_ulong, arg: *mut libc::c_void) -> libc::c_int {
    #[cfg(target_env = "musl")]
    let request = req as libc::c_int;
    #[cfg(not(target_env = "musl"))]
    let request = req;
    unsafe { libc::ioctl(fd, request, arg) }
}

/// One ALSA PCM substream (capture or playback) held open exclusively.
pub struct Pcm {
    _file: File, // keeps the fd alive
    underruns: u64,
}

impl Pcm {
    /// Open a PCM device by card/device number (`c`/`p` suffix via `capture`).
    /// NOTE: if another process holds the substream this BLOCKS until it
    /// releases (vendor driver behavior — see the module header).
    pub fn open(card: u32, device: u32, capture: bool) -> io::Result<Pcm> {
        let path = format!(
            "/dev/snd/pcmC{card}D{device}{}",
            if capture { "c" } else { "p" }
        );
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| io::Error::new(e.kind(), format!("open {path}: {e}")))?;
        Ok(Pcm {
            _file: file,
            underruns: 0,
        })
    }

    /// The libtinyalsa flow (module header note 3): init all-open → refine
    /// (kernel narrows masks, fills info — the ONLY source of legal values)
    /// → pin interval values → hw_params → prepare → start. The requested
    /// format is advisory: the refine result is authoritative and this
    /// device offers exactly one, so masks are left as refined.
    pub fn configure(
        &mut self,
        capture: bool,
        _format: u32, // advisory only (vendor enum differs; refine decides)
        channels: u32,
        rate: u32,
        period: u32,
        buffer: u32,
    ) -> io::Result<()> {
        let mut b = vec![0u8; HW_PARAMS_SIZE];
        b[OFF_FLAGS] = 0x04; // as libtinyalsa
        for (w0, w1) in MASK_WORDS {
            b[w0..w0 + 4].copy_from_slice(&u32::MAX.to_le_bytes());
            b[w1..w1 + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        }
        for i in 0..INTERVAL_COUNT {
            let o = OFF_INTERVALS + i * 12;
            b[o + 4..o + 8].copy_from_slice(&u32::MAX.to_le_bytes()); // max open
        }
        b[OFF_RMASK..OFF_RMASK + 4].copy_from_slice(&u32::MAX.to_le_bytes());

        self.ioctl_buf(req_hw_refine(), &mut b)
            .map_err(|e| io::Error::new(e.kind(), format!("hw_refine: {e}")))?;

        // Pin ambiguous masks. Playback offers several formats (numbered as
        // mainline on this device) and two access modes; capture offers
        // exactly one (vendor-shifted S24_3LE=bit32) and keeps its refined
        // mask. Rule: pin the desired mainline bit when the kernel offers
        // it; otherwise keep the refined mask if it is single-bit, else pin
        // the lowest offered bit.
        let read_mask = |b: &[u8], (w0, w1): (usize, usize)| -> u64 {
            let lo = u32::from_le_bytes(b[w0..w0 + 4].try_into().unwrap()) as u64;
            let hi = u32::from_le_bytes(b[w1..w1 + 4].try_into().unwrap()) as u64;
            lo | (hi << 32)
        };
        let pin_mask = |b: &mut [u8], (w0, w1): (usize, usize), bits: u64| {
            b[w0..w0 + 4].copy_from_slice(&((bits & 0xFFFFFFFF) as u32).to_le_bytes());
            b[w1..w1 + 4].copy_from_slice(&((bits >> 32) as u32).to_le_bytes());
        };
        for (slot, desired) in [(0usize, 1u64 << 3), (1, 1u64 << _format), (2, 1u64 << 0)] {
            let offered = read_mask(&b, MASK_WORDS[slot]);
            let pinned = if offered & desired != 0 {
                desired
            } else if offered.count_ones() == 1 {
                offered
            } else {
                offered & (!(offered - 1)) // lowest set bit
            };
            pin_mask(&mut b, MASK_WORDS[slot], pinned);
        }

        // Pin our values on the refined buffer (min=max, integer flag).
        // Sample bits follow the requested mainline format index (2=S16_LE, 10=S32_LE).
        let sample_bits: u32 = if _format == 2 {
            16
        } else if _format == 10 {
            32
        } else {
            24
        };
        let frame_bits = sample_bits * channels;
        let periods = (buffer / period).max(1);
        for (param, v) in [
            (P_SAMPLE_BITS, sample_bits),
            (P_SAMPLE_BITS + 1, frame_bits),
            (P_CHANNELS, channels),
            (P_RATE, rate),
            (P_PERIOD_SIZE, period),
            (P_PERIODS, periods),
            (P_BUFFER_SIZE, buffer),
        ] {
            let o = OFF_INTERVALS + (param - 3) * 12;
            b[o..o + 4].copy_from_slice(&v.to_le_bytes());
            b[o + 4..o + 8].copy_from_slice(&v.to_le_bytes());
            b[o + 8] |= 0x04; // integer
        }
        if _format == 10 {
            b[OFF_RMASK..OFF_RMASK + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        }

        self.ioctl_buf(req_hw_params(), &mut b).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("hw_params(ch={channels},rate={rate},per={period},buf={buffer}): {e}"),
            )
        })?;
        // Capture: PREPARE + START (the stream is consumed by reads).
        // Playback: PREPARE only — START on an empty buffer is an instant
        // underrun (EPIPE); the first writei triggers the start instead.
        if capture {
            self.prepare_and_start()
        } else {
            self.ioctl_val(req_prepare(), 0u32)
                .map_err(|e| io::Error::new(e.kind(), format!("prepare: {e}")))
        }
    }

    /// PREPARE + START (capture; underrun recovery uses this too).
    pub fn prepare_and_start(&mut self) -> io::Result<()> {
        self.ioctl_val(req_prepare(), 0u32)
            .map_err(|e| io::Error::new(e.kind(), format!("prepare: {e}")))?;
        self.ioctl_val(req_start(), 0u32)
            .map_err(|e| io::Error::new(e.kind(), format!("start: {e}")))?;
        Ok(())
    }

    pub fn prepare(&mut self) -> io::Result<()> {
        // PREPARE from SETUP (fresh configure) or XRUN (recovery) — both
        // legal without hw_free (tinyalsa semantics; calling hw_free here
        // would drop a just-set configuration back to OPEN → EBADFD).
        self.ioctl_val(req_prepare(), 0u32)
            .map_err(|e| io::Error::new(e.kind(), format!("prepare: {e}")))?;
        self.ioctl_val(req_start(), 0u32)
            .map_err(|e| io::Error::new(e.kind(), format!("start: {e}")))?;
        Ok(())
    }

    /// One blocking capture transfer. `x.result` is the actual frame count;
    /// `x.frames` remains the request. The capture assembler handles shorts.
    /// Overrun recovery returns an error so a partial block is discarded.
    pub fn readi(&mut self, buf: &mut [u8], frames: u32) -> io::Result<u32> {
        if frames == 0 || buf.is_empty() || !buf.len().is_multiple_of(frames as usize) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid readi layout",
            ));
        }
        let mut x = Xferi {
            result: 0,
            #[cfg(not(target_arch = "arm"))]
            _pad: 0,
            buf: buf.as_mut_ptr() as *mut libc::c_void,
            frames,
            #[cfg(not(target_arch = "arm"))]
            _pad2: 0,
        };
        let r = raw_ioctl(
            self.fd(),
            req_readi(),
            &mut x as *mut Xferi as *mut libc::c_void,
        );
        let result = if r == 0 {
            capture_result(x.result, frames)
        } else {
            Err(io::Error::last_os_error())
        };
        if let Err(err) = &result {
            match err.raw_os_error() {
                Some(libc::EPIPE) => {
                    self.underruns += 1;
                    self.prepare_and_start()?;
                }
                Some(ESTRPIPE) => self.prepare_and_start()?,
                _ => {}
            }
        }
        result
    }

    /// Blocking interleaved write of exactly `frames` frames.
    ///
    /// The kernel reports what it took in `x.result` (frames written, or a
    /// negative errno), NOT in the ioctl return value — so the loop is driven
    /// by `x.result` and continues from the offset it reports. R0
    /// (2026-11-13): the previous loop ignored `x.result`, returned the
    /// requested frame count as if everything had been written, and on an
    /// underrun retried the whole period from its start — replaying samples
    /// the DAC already had (the same defect family as the start/end pops,
    /// docs/analysis-pop-20261113).
    pub fn writei(&mut self, buf: &[u8], frames: u32) -> io::Result<u32> {
        if frames == 0 {
            return Ok(0);
        }
        // Bytes per frame for this layout (interleaved s16 stereo = 4).
        let stride = buf.len() / frames as usize;
        if stride == 0 || stride * frames as usize != buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("writei buffer {}B is not {frames} whole frames", buf.len()),
            ));
        }
        let base = buf.as_ptr() as usize;
        let fd = self.fd();
        let underruns = &mut self.underruns;
        let prep = req_prepare() as libc::c_ulong;
        let prepare = || -> io::Result<()> {
            let r = raw_ioctl(fd, prep, &mut 0u32 as *mut u32 as *mut libc::c_void);
            if r == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        };
        pump(
            frames,
            |offset, count| {
                let mut x = Xferi {
                    result: 0,
                    #[cfg(not(target_arch = "arm"))]
                    _pad: 0,
                    buf: (base + offset as usize * stride) as *mut libc::c_void,
                    frames: count,
                    #[cfg(not(target_arch = "arm"))]
                    _pad2: 0,
                };
                let r = raw_ioctl(fd, req_writei(), &mut x as *mut Xferi as *mut libc::c_void);
                if r == 0 {
                    Ok(x.result)
                } else {
                    // Same recovery path as a negative x.result: an ioctl
                    // that reports EPIPE and a result of -EPIPE are the same
                    // fact (nothing was written).
                    let err = io::Error::last_os_error();
                    Ok(-err.raw_os_error().unwrap_or(libc::EIO))
                }
            },
            |errno| {
                if errno == libc::EPIPE {
                    *underruns += 1;
                }
                prepare()
            },
        )
    }

    /// Immediate stop + silence (speaker_flush semantics).
    ///
    /// DROP is the fact that matters: once it succeeds the DAC has nothing
    /// queued. PREPARE+START only re-arms the clock for the next period
    /// write, and this vendor driver answers START with EPIPE for an empty
    /// buffer (measured on device 2026-11-13: every stop produced
    /// `start: Broken pipe`). That is an underrun, not a failure to stop —
    /// counting it keeps `pcm_underruns` honest, and a real fault (EBADFD,
    /// EIO) still propagates so the receipt can say "unknown".
    pub fn flush(&mut self) -> io::Result<()> {
        self.ioctl_val(req_drop(), 0u32)?;
        if let Err(e) = self.ioctl_val(req_prepare(), 0u32) {
            return Err(io::Error::new(e.kind(), format!("prepare: {e}")));
        }
        match self.ioctl_val(req_start(), 0u32) {
            Ok(()) => Ok(()),
            Err(e)
                if e.raw_os_error() == Some(libc::EPIPE) || e.raw_os_error() == Some(ESTRPIPE) =>
            {
                self.underruns += 1;
                Ok(())
            }
            Err(e) => Err(io::Error::new(e.kind(), format!("start: {e}"))),
        }
    }

    /// Graceful stop (drain what is buffered).
    pub fn drain(&mut self) -> io::Result<()> {
        self.ioctl_val(req_drain(), 0u32)
    }

    /// Frames still queued ahead of the DAC's hardware pointer (M6a).
    ///
    /// This is a REQUEST, not a promise: the vendor kernel may reject it (a
    /// compat-dispatch quirk like the one that forced the armv7 build), so
    /// every caller treats `Err` as "not observable" and says so instead of
    /// pretending a write count was a consumption count.
    pub fn delay_frames(&self) -> io::Result<u64> {
        let mut delay: libc::c_long = 0;
        let r = raw_ioctl(
            self.fd(),
            req_delay(),
            &mut delay as *mut libc::c_long as *mut libc::c_void,
        );
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(delay.max(0) as u64)
    }

    pub fn underruns(&self) -> u64 {
        self.underruns
    }

    fn fd(&self) -> libc::c_int {
        self._file.as_raw_fd()
    }

    fn ioctl_val(&mut self, req: libc::c_ulong, arg: impl Sized) -> io::Result<()> {
        let r = raw_ioctl(self.fd(), req, &arg as *const _ as *mut libc::c_void);
        if r == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn ioctl_buf(&mut self, req: libc::c_ulong, buf: &mut [u8]) -> io::Result<()> {
        let r = raw_ioctl(self.fd(), req, buf.as_mut_ptr() as *mut libc::c_void);
        if r == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

/// Linux-only errno (no libc constant on some hosts; generic value 86 —
/// same as tinyalsa's recovery check, exercised only on the device).
pub(super) const ESTRPIPE: i32 = 86;

/// What one WRITEI attempt accomplished, decoded from the kernel's
/// `x.result`: frames the device took, or a negative errno.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Xfer {
    Wrote(u32),
    /// `x.result < 0`: nothing was written. EPIPE = underrun, ESTRPIPE =
    /// suspended stream; both are recoverable with PREPARE.
    Rewind(i32),
}

pub(crate) fn decode_xfer(result: i32) -> Xfer {
    if result >= 0 {
        Xfer::Wrote(result as u32)
    } else {
        Xfer::Rewind(-result)
    }
}

/// Decode the signed ALSA transfer result without trusting the request count.
fn capture_result(result: i32, requested: u32) -> io::Result<u32> {
    if result < 0 {
        return Err(io::Error::from_raw_os_error(
            result.checked_neg().unwrap_or(libc::EIO),
        ));
    }
    let count = result as u32;
    if count > requested {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "readi returned excess frames",
        ));
    }
    Ok(count)
}

/// Drive a blocking WRITEI to completion from the kernel's own frame count.
/// `attempt(offset, count)` reports `x.result` for the slice starting
/// `offset` frames in; `recover(errno)` runs the underrun/restart path.
/// A short write continues at the reported offset — replaying the period
/// from its start would duplicate samples the device already took.
/// Returns the frames written (== `frames` on success).
pub(crate) fn pump<A, R>(frames: u32, mut attempt: A, mut recover: R) -> io::Result<u32>
where
    A: FnMut(u32, u32) -> io::Result<i32>,
    R: FnMut(i32) -> io::Result<()>,
{
    /// Recovery attempts in a row before the write is declared broken. A
    /// stalled kernel (0 frames, no error) or a stream that keeps underrunning
    /// must not spin this thread forever.
    const MAX_STALLS: u32 = 3;
    let mut done = 0u32;
    let mut stalls = 0u32;
    while done < frames {
        let count = frames - done;
        match decode_xfer(attempt(done, count)?) {
            Xfer::Wrote(0) => {
                stalls += 1;
                if stalls > MAX_STALLS {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        format!(
                            "writei made no progress ({done}/{frames} frames, {stalls} attempts)"
                        ),
                    ));
                }
            }
            Xfer::Wrote(n) => {
                stalls = 0;
                done += n.min(count); // a bogus count cannot run past the buffer
            }
            Xfer::Rewind(errno) if errno == libc::EPIPE || errno == ESTRPIPE => {
                stalls += 1;
                if stalls > MAX_STALLS {
                    return Err(io::Error::from_raw_os_error(errno));
                }
                recover(errno)?;
            }
            Xfer::Rewind(errno) => return Err(io::Error::from_raw_os_error(errno)),
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_uses_signed_result_and_rejects_invalid_counts() {
        assert_eq!(capture_result(320, 1280).unwrap(), 320);
        assert_eq!(capture_result(0, 1280).unwrap(), 0);
        assert_eq!(
            capture_result(-libc::EPIPE, 1280)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EPIPE)
        );
        assert_eq!(
            capture_result(-libc::EIO, 1280).unwrap_err().raw_os_error(),
            Some(libc::EIO)
        );
        assert_eq!(
            capture_result(1281, 1280).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(capture_result(i32::MIN, 1280).is_err());
    }

    /// The R0 defect, modelled from the old loop: it returned `x.frames`
    /// (the REQUEST) whenever the ioctl succeeded, so a short write claimed
    /// the whole period and silently dropped what the DAC never took; and the
    /// retry after an underrun rebuilt the transfer from the period start
    /// because nothing tracked an offset.
    fn legacy_claim(result: i32, requested: u32) -> u32 {
        if result >= 0 {
            requested // lies about the short write
        } else {
            0
        }
    }

    #[test]
    fn short_write_resumes_at_the_reported_frame_not_the_period_start() {
        // 512 of 2048 frames accepted, then the rest.
        let script = [(512, 2048), (1536, 1536)];
        let mut seen = Vec::new();
        let mut i = 0;
        let n = pump(
            2048,
            |offset, count| {
                seen.push((offset, count));
                let (result, _) = script[i.min(script.len() - 1)];
                i += 1;
                Ok(result)
            },
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(n, 2048);
        assert_eq!(seen, vec![(0, 2048), (512, 1536)]);
        assert_eq!(
            legacy_claim(script[0].0, 2048),
            2048,
            "the old loop claimed the whole period after 512 frames were taken \
             (1536 frames = 32 ms lost, then a mid-period discontinuity)"
        );
    }

    #[test]
    fn underrun_restarts_at_the_same_offset_and_is_counted() {
        let mut seen = Vec::new();
        let mut recovered = Vec::new();
        let mut i = 0;
        let script = [-libc::EPIPE, 2048];
        let n = pump(
            2048,
            |offset, count| {
                seen.push((offset, count));
                let r = script[i.min(script.len() - 1)];
                i += 1;
                Ok(r)
            },
            |errno| {
                recovered.push(errno);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(n, 2048);
        // Nothing was written before the underrun, so offset 0 again — and no
        // replayed tail after a partial write:
        assert_eq!(seen, vec![(0, 2048), (0, 2048)]);
        assert_eq!(recovered, vec![libc::EPIPE]);
    }

    #[test]
    fn short_write_then_underrun_keeps_the_reported_prefix() {
        let mut seen = Vec::new();
        let mut i = 0;
        let script = [512, -libc::EPIPE, 1536];
        let n = pump(
            2048,
            |offset, count| {
                seen.push((offset, count));
                let r = script[i.min(script.len() - 1)];
                i += 1;
                Ok(r)
            },
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(n, 2048);
        assert_eq!(seen, vec![(0, 2048), (512, 1536), (512, 1536)]);
    }

    #[test]
    fn a_stalled_kernel_fails_instead_of_spinning() {
        let err = pump(2048, |_, _| Ok(0), |_| Ok(())).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn unknown_errno_is_an_error() {
        let err = pump(2048, |_, _| Ok(-libc::EIO), |_| Ok(())).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EIO));
    }

    #[test]
    fn zero_frames_is_a_no_op() {
        assert_eq!(
            pump(0, |_, _| panic!("no attempt expected"), |_| Ok(())).unwrap(),
            0
        );
    }
}
