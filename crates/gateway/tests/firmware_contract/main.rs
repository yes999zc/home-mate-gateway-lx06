//! G5-2 contract tests: the REAL direct-occupation audio component (hardware
//! behind a deterministic fake) against a fake core speaking the v0 LDJSON
//! protocol — the absorbed-controller form: hello/descriptor truth, mic
//! uplink layout, play→device_confirmed→play_finished receipts, stop, the
//! software volume law + persistence + P3-3 displacement, AssistantState →
//! LED palette, and button mapping.
//!
//!Split into theme modules (issue #28): this file is the entry point, the
//! fakes live in `support`, and every test keeps its name, its assertions and
//! its body — only its module path changed.
#![allow(clippy::await_holding_lock)]

mod acoustic_guard;
mod bounded_northbound;
mod bringup;
mod capture;
mod config_apply_v2;
mod handshake;
mod link_faults;
mod mute;
mod playback;
mod retreat_word;
mod streaming;
mod support;
mod watchdog;
