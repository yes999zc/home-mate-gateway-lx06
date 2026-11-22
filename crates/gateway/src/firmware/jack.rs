//! 3.5 mm jack (AUX/headphone) routing — user request 2026-11-15: "aux out 插上
//! 音箱后本机是不是也会响". It did: `bringup::amp_up` turns the built-in amp On
//! unconditionally, so a speaker on the jack played NEXT TO the internal one.
//!
//! Detection is the codec's own switch, not a guess: `/sys/class/switch/h2w/state`
//! reads 0 (nothing), 1 (headset with mic) or 2 (headphones). The routing values
//! are RUI-local's measured ones (read-only archive 2026-11-05,
//! `internal/bindings/speaker/jackrouting.go`); nothing here was re-guessed:
//!
//!   inserted : Ext_Speaker_Amp_Switch=Off, HP Driver Gain Volume="11 11", Audio_DacMux_Setting=On
//!   removed  : Ext_Speaker_Amp_Switch=On,  HP Driver Gain Volume="6 6",   Audio_DacMux_Setting=Off
//!
//! `Headphone_Speaker_Mux` / `Ext_Headphone_Amp_Switch` are deliberately NOT
//! driven: the jack's contacts do the routing, those controls do not describe
//! where the sound goes (RUI comment, same file).
//!
//! Three rules from the plan (docs/d/aux-jack-routing-20261115.md):
//! 1. the FIRST reading is applied too — a speaker plugged in before boot has
//!    no edge, so poll-only would leave it silent on the internal amp;
//! 2. `amp_up` re-enables the internal amp unconditionally, so the apply after
//!    bring-up must come AFTER it;
//! 3. a missing switch file is "no jack on this device": never touch a control.

use super::mixer::Ctl;
use crate::fwlog;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const H2W_STATE: &str = "/sys/class/switch/h2w/state";
const SPEAKER_AMP: &str = "Ext_Speaker_Amp_Switch";
const HP_GAIN: &str = "HP Driver Gain Volume";
const DAC_MUX: &str = "Audio_DacMux_Setting";
const POLL_MS: u64 = 1_000;

/// The sound path the voicing chain must assume. The bass guard is bypassed
/// while a speaker is on the jack (the external box has its own low end; the
/// guard exists for the tiny internal driver).
static EXTERNAL: AtomicBool = AtomicBool::new(false);

pub fn external() -> bool {
    EXTERNAL.load(Ordering::Relaxed)
}

fn set_external(external: bool) {
    EXTERNAL.store(external, Ordering::Relaxed);
}

/// h2w state → is something plugged in. Unknown/garbage text = None (leave the
/// device alone rather than guess).
pub fn parse_state(raw: &str) -> Option<bool> {
    match raw.trim() {
        "0" => Some(false),
        "1" | "2" => Some(true),
        _ => None,
    }
}

pub fn read_state() -> Option<bool> {
    std::fs::read_to_string(H2W_STATE)
        .ok()
        .and_then(|s| parse_state(&s))
}

/// The three controls and their values for a state (unit-tested table).
pub fn routing(external: bool) -> [(&'static str, &'static [&'static str]); 3] {
    if external {
        [
            (SPEAKER_AMP, &["Off"]),
            (HP_GAIN, &["11", "11"]),
            (DAC_MUX, &["On"]),
        ]
    } else {
        [
            (SPEAKER_AMP, &["On"]),
            (HP_GAIN, &["6", "6"]),
            (DAC_MUX, &["Off"]),
        ]
    }
}

/// Write the routing table. Idempotent (absolute values) and loud on failure.
/// Returns how many writes failed.
pub fn apply(ctl: &Ctl, external: bool) -> usize {
    let mut failed = 0;
    for (name, values) in routing(external) {
        if let Err(e) = ctl.set(name, values) {
            failed += 1;
            fwlog!("[jack] {name} = {values:?}: {e}");
        }
    }
    if failed == 0 {
        fwlog!(
            "[jack] routing applied ({})",
            if external { "external" } else { "internal" }
        );
    }
    failed
}

/// Read, apply, remember. Used for the boot-time reading and every edge.
pub fn apply_now(ctl: &Ctl, state: Option<bool>) -> bool {
    match state {
        Some(external) => {
            set_external(external);
            apply(ctl, external);
            external
        }
        None => {
            fwlog!("[jack] no h2w switch (or unknown value): aux routing stays off this device");
            false
        }
    }
}

/// 1 s poll, edges only. The initial value must already have been applied by
/// the caller (`apply_now`) so a cable present at boot is not missed.
pub fn spawn_watcher(ctl: Arc<Mutex<Ctl>>, mut last: Option<bool>) {
    let spawned = std::thread::Builder::new()
        .name("jack".into())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_millis(POLL_MS));
            let now = read_state();
            if now.is_none() || now == last {
                continue;
            }
            let external = now.unwrap();
            fwlog!(
                "[jack] {} (state={})",
                if external { "inserted" } else { "removed" },
                if external { 2 } else { 0 }
            );
            if let Ok(ctl) = ctl.lock() {
                apply(&ctl, external);
            }
            set_external(external);
            last = now;
        });
    if let Err(e) = spawned {
        fwlog!("[jack] watcher thread did not start: {e} — aux routing is boot-time only");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h2w_zero_is_unplugged_and_1_or_2_is_plugged() {
        assert_eq!(parse_state("0\n"), Some(false));
        assert_eq!(parse_state("1"), Some(true));
        assert_eq!(parse_state("2"), Some(true));
        assert_eq!(parse_state(""), None);
        assert_eq!(parse_state("nonsense"), None);
    }

    #[test]
    fn inserted_silences_the_internal_amp_and_opens_the_dac_mux() {
        let ext = routing(true);
        assert_eq!(ext[0], (SPEAKER_AMP, &["Off"][..]));
        assert_eq!(ext[1], (HP_GAIN, &["11", "11"][..]));
        assert_eq!(ext[2], (DAC_MUX, &["On"][..]));
    }

    #[test]
    fn removed_restores_the_internal_amp() {
        let int = routing(false);
        assert_eq!(int[0], (SPEAKER_AMP, &["On"][..]));
        assert_eq!(int[1], (HP_GAIN, &["6", "6"][..]));
        assert_eq!(int[2], (DAC_MUX, &["Off"][..]));
    }
}
