//! 32-bit ARM: keep `clock_gettime` off the kernel's missing time64 syscall.
//!
//! musl 1.2+ (the libc zig bundles) gives 32-bit targets a 64-bit `time_t` and
//! implements `clock_gettime` with `SYS_clock_gettime64` (403 on arm). This
//! board's kernel is 3.18, whose AArch32 compat table has no entry 403, so
//! **every** call traps. MediaTek's compat `traps` handler answers with a full
//! register dump (~24 lines, ~1.3 KB) printed to `console=ttyMT0,921600`.
//! The 8250-family console write holds interrupts off while it drains the
//! FIFO: ~113 ms per dump at 921600 baud. Two dumps in flight (two threads
//! calling at the same time) serialise on the console lock for ~220 ms.
//! The audio SPI path has a 100 ms transfer timeout; a dump landing on an
//! in-flight transfer makes `spi_data_read` fail and the vendor driver's
//! error path NULL-derefs, killing the kernel. The hardware watchdog then
//! resets the board (`androidboot.bootreason=wdt_by_pass_pwk`).
//!
//! The public entry point is defined here, which keeps musl's
//! `clock_gettime32.o` out of the link, and routes calls through the 32-bit
//! `SYS_clock_gettime` (263) that this kernel does implement. The 32-bit
//! timespec the kernel writes is widened here. CLOCK_MONOTONIC/BOOTTIME are
//! unaffected by 2038; CLOCK_REALTIME is only 32-bit-safe until then, which
//! is the trade for a board that otherwise resets every few minutes.
//!
//! Residual: musl's *internal* timed waits (`pthread_cond_timedwait`,
//! `sem_timedwait`) call `__clock_gettime` directly, which still uses the
//! time64 syscall. The firmware does not use those on a timer, and the trap
//! counter on the device is the check that this stays true.
//!
//! Evidence and the image-level remainder: docs/d/p0-watchdog-resets-20261109.md

#[cfg(target_arch = "arm")]
use std::ffi::c_int;
#[cfg(target_arch = "arm")]
use std::ffi::c_long;

/// The 32-bit arm kernel ABI's `struct timespec` (two 32-bit words).
#[cfg(target_arch = "arm")]
#[repr(C)]
struct Timespec32 {
    tv_sec: i32,
    tv_nsec: i32,
}

/// `__NR_clock_gettime` on arm EABI. Deliberately a literal: this must be
/// the number the *kernel* implements, not whatever the libc binding says.
#[cfg(target_arch = "arm")]
const SYS_CLOCK_GETTIME: c_long = 263;

/// Public entry point (what Rust's std and every C dependency call).
#[cfg(target_arch = "arm")]
#[no_mangle]
pub unsafe extern "C" fn clock_gettime(clk: c_int, ts: *mut libc::timespec) -> c_int {
    let mut t32 = Timespec32 {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // libc::syscall returns -1 and sets errno on failure, which is exactly
    // the C contract clock_gettime requires.
    let r = libc::syscall(
        SYS_CLOCK_GETTIME,
        clk as c_long,
        &mut t32 as *mut Timespec32,
    );
    if r < 0 {
        return -1;
    }
    (*ts).tv_sec = t32.tv_sec as libc::time_t;
    (*ts).tv_nsec = t32.tv_nsec as c_long;
    0
}

/// Reference the symbol from `main` so the linker never garbage-collects it
/// (it is only reached through libc/std call sites).
#[cfg(target_arch = "arm")]
#[inline(never)]
pub fn keep() {
    std::hint::black_box(clock_gettime as usize);
}

/// Host builds (tests, virtual terminal, Mac dev) keep the libc default.
#[cfg(not(target_arch = "arm"))]
#[inline(always)]
pub fn keep() {}
