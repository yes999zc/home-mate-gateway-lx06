//! Safe wrapper over the vendored SpeexDSP echo canceller (speexdsp/).
//!
//! One `Canceller` = one microphone's MDF filter. `process` cancels the
//! reference in place on the mic frame. The C API is
//! speex_echo_state_init / speex_echo_cancellation / speex_echo_state_destroy
//! (speex/speex_echo.h); cancel-reset exists for stream restarts.

use std::ffi::c_void;
use std::os::raw::c_int;

/// Speex requests (speex/speex_echo.h).
const SPEEX_ECHO_SET_SAMPLING_RATE: c_int = 24;
const SPEEX_ECHO_GET_SAMPLING_RATE: c_int = 25;

extern "C" {
    fn speex_echo_state_init(frame_size: c_int, filter_length: c_int) -> *mut c_void;
    fn speex_echo_state_destroy(st: *mut c_void);
    fn speex_echo_cancellation(st: *mut c_void, rec: *const i16, play: *const i16, out: *mut i16);
    fn speex_echo_state_reset(st: *mut c_void);
    fn speex_echo_ctl(st: *mut c_void, request: c_int, ptr: *mut c_void) -> c_int;
}

pub struct Canceller {
    st: *mut c_void,
    frame: usize,
    scratch: Vec<i16>,
}

unsafe impl Send for Canceller {}

impl Canceller {
    /// `sample_rate` is what Speex is told to derive its adaptation
    /// parameters from (beta0 = 2*frame/rate, notch_radius, spec_average —
    /// mdf.c: SPEEX_ECHO_SET_SAMPLING_RATE). It is set BEFORE the first
    /// cancellation. See aec::SPEEX_RATE for why the value is not the
    /// capture rate.
    pub fn new(frame_size: usize, tail: usize, sample_rate: usize) -> Canceller {
        assert!(frame_size > 0 && tail >= frame_size);
        let st = unsafe { speex_echo_state_init(frame_size as c_int, tail as c_int) };
        assert!(!st.is_null(), "speex_echo_state_init failed");
        let c = Canceller {
            st,
            frame: frame_size,
            scratch: vec![0i16; frame_size],
        };
        assert!(sample_rate > 0);
        let mut rate = sample_rate as c_int;
        let rc = unsafe {
            speex_echo_ctl(
                c.st,
                SPEEX_ECHO_SET_SAMPLING_RATE,
                &mut rate as *mut c_int as *mut c_void,
            )
        };
        assert_eq!(rc, 0, "SPEEX_ECHO_SET_SAMPLING_RATE failed");
        c
    }

    /// The rate the MDF filter is being adapted for. `speex_echo_state_init`
    /// leaves Speex's 8 kHz default; the 16 kHz geometry (frame 320, tail
    /// 2560) only makes sense once this reports 16000.
    pub fn sampling_rate(&self) -> i32 {
        let mut rate: c_int = 0;
        let rc = unsafe {
            speex_echo_ctl(
                self.st,
                SPEEX_ECHO_GET_SAMPLING_RATE,
                &mut rate as *mut c_int as *mut c_void,
            )
        };
        assert_eq!(rc, 0, "SPEEX_ECHO_GET_SAMPLING_RATE failed");
        rate
    }

    /// Cancel `reference` from `mic_frame`, in place. Both must be
    /// `frame_size` long.
    pub fn process(&mut self, mic_frame: &mut [i16], reference: &[i16]) {
        assert_eq!(mic_frame.len(), self.frame, "mic frame length");
        assert_eq!(reference.len(), self.frame, "reference length");
        unsafe {
            speex_echo_cancellation(
                self.st,
                mic_frame.as_ptr(),
                reference.as_ptr(),
                self.scratch.as_mut_ptr(),
            );
            mic_frame.copy_from_slice(&self.scratch);
        }
    }

    /// Drop the adaptive state (stream restart, mute window, etc).
    pub fn reset(&mut self) {
        unsafe { speex_echo_state_reset(self.st) };
    }
}

impl Drop for Canceller {
    fn drop(&mut self) {
        unsafe { speex_echo_state_destroy(self.st) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R0 (2026-11-12): B never called SPEEX_ECHO_SET_SAMPLING_RATE, so the
    /// filter silently ran Speex's 8 kHz default. It is now an explicit,
    /// measured parameter (aec::SPEEX_RATE) — and the measurement says the
    /// default adapts 0.7 dB better on this geometry, so that is the value.
    #[test]
    fn sampling_rate_is_explicit_and_measured() {
        let c = Canceller::new(320, 2560, 8000);
        assert_eq!(
            c.sampling_rate(),
            8000,
            "the reported rate must be the value set"
        );
    }

    /// FFI smoke + real cancellation on synthetic echo: mic = attenuated
    /// delayed reference (+ near-end burst in a second phase). The MDF must
    /// suppress the echo by >20dB once adapted — proving the vendored C
    /// build works end to end (device ERLE is measured separately).
    #[test]
    fn cancels_synthetic_echo() {
        let frame = 320;
        let tail = 1280;
        let mut c = Canceller::new(frame, tail, 16000);
        let mut rng: u64 = 42;
        let mut next = move || {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (((rng >> 33) as i32 % 6000) as f64 / 6000.0 * 8000.0) as i16
        };
        // Synthetic echo path: bulk delay D + a decaying kernel (within the
        // 80ms tail — the MDF must model it entirely).
        const D: usize = 100;
        let kernel: Vec<f64> = (0..100).map(|j| 0.6 * (-(j as f64) / 80.0).exp()).collect();
        let mut hist: Vec<f64> = vec![0.0; D + kernel.len()];
        let mut energy_mic = 0f64;
        let mut energy_out = 0f64;
        for block in 0..120 {
            let mut reference = vec![0i16; frame];
            let mut mic = vec![0i16; frame];
            for i in 0..frame {
                let r = if block >= 20 { next() as f64 } else { 0.0 };
                reference[i] = r as i16;
                hist.push(r);
                if hist.len() > D + kernel.len() {
                    hist.remove(0);
                }
                let mut e = 0f64;
                for (j, k) in kernel.iter().enumerate() {
                    e += k * hist[hist.len() - 1 - D - j];
                }
                mic[i] = e.clamp(-32768.0, 32767.0) as i16;
            }
            let mut m = mic.clone();
            c.process(&mut m, &reference);
            if block >= 60 {
                for i in 0..frame {
                    energy_mic += mic[i] as f64 * mic[i] as f64;
                    energy_out += m[i] as f64 * m[i] as f64;
                }
            }
        }
        let erle = 10.0 * (energy_mic / energy_out.max(1.0)).log10();
        assert!(
            erle > 20.0,
            "synthetic ERLE {erle:.1} dB — FFI wiring suspect"
        );
    }
}
