//! Home Mate Device Gateway (G1: protocol v0 + virtual dot2-d terminal).
//!
//! Responsibilities (see docs/HANDOFF.md): vendor protocols, connections,
//! raw media layouts, hardware parameter mapping, play/stop semantics, and
//! device-state translation — exposed to the Home Mate core through a
//! versioned protocol. Intent, tools, and task lifecycle stay in the core.

pub mod client;
pub mod clock32;
pub mod firmware;
pub mod fwlog;
pub mod protocol;
pub mod terminal;

/// Firmware logging entry point. Defined at the crate root so every firmware
/// module (audio threads included) can call it without importing anything;
/// see `fwlog` for why the audio path must never do a blocking `println!`.
#[macro_export]
macro_rules! fwlog {
    ($($arg:tt)*) => { $crate::fwlog::line(format_args!($($arg)*)) };
}

/// Protocol version this gateway build speaks. Bumped on any incompatible
/// change; independent of the software version.
pub use protocol::PROTOCOL_VERSION;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_version_supports_zero_and_two() {
        assert_eq!(PROTOCOL_VERSION, 2);
        assert_eq!(protocol::PROTOCOL_MIN_VERSION, 0);
        assert_eq!(protocol::SUPPORTED_PROTOCOLS, &[0, 2]);
    }
}

// --- D-stage leak accounting (2026-11-10) -----------------------------------
// Wraps the system allocator and keeps live-byte counters. The firmware's RSS
// grows ~0.33 MB per second of played audio; static reading of the hot paths
// found no retention, so the question to answer is "does the program still
// hold the bytes, or does the allocator keep freed memory?" — these counters
// answer it directly. Costs two relaxed atomics per allocation, which is
// nothing next to the ioctl it surrounds; it is left in the binary because it
// is the only allocator telemetry this platform can produce (no heap profiler
// on a 3.18 Android kernel).
struct CountingAlloc;

static ALLOC_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static FREE_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn alloc_live() -> (u64, u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    let a = ALLOC_BYTES.load(Relaxed);
    let f = FREE_BYTES.load(Relaxed);
    (a, f, a - f)
}

unsafe impl std::alloc::GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        ALLOC_BYTES.fetch_add(layout.size() as u64, std::sync::atomic::Ordering::Relaxed);
        std::alloc::System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        FREE_BYTES.fetch_add(layout.size() as u64, std::sync::atomic::Ordering::Relaxed);
        std::alloc::System.dealloc(ptr, layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        if new_size > layout.size() {
            ALLOC_BYTES.fetch_add(
                (new_size - layout.size()) as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
        } else {
            FREE_BYTES.fetch_add(
                (layout.size() - new_size) as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        std::alloc::System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;
