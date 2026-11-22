//! Codec bring-up: the COMPLETE mixer sequence, direct ioctls, no external
//! binary and no startup-script dependency (user direction 2026-11-08: emOS
//! has no Android HAL, DAPM leaves mic and speaker powered down, and the old
//! script carried only part of the sequence).
//!
//! Sources, cross-checked item by item against upstream EchoMuse
//! (emos-v0.10 tree, 2026-11-08):
//!   - device/internal/bindings/codec/routes.go — the ten DAPM routes (8
//!     differential-input capture routes + 2 DAC→output-mixer playback
//!     routes) that must close BEFORE the PCMs open: "DAPM decides what to
//!     power at stream open".
//!   - start_server.sh.echomuse-original — I2S1 / HP DAC / MFP Gpio / ADC
//!     gains (the part the old hm script did carry).
//!   - device/internal/bindings/speaker/pcm_speaker.go — the click-free amp
//!     order: volume 0 → clock silence into the DAC → amp On → settle →
//!     volume 127 (dacUnity; volume law stays in software).
//!   - device/internal/server/mute.go — hardware mic mute = all four ADC
//!     chips' Left+Right Mute (the C5 lesson: muting only chip A leaves the
//!     others physically hot).
//!
//! Control names verified present on THIS kernel against the archived full
//! tinymix snapshot (deploy/dot2d/direct/tinymix-full-20261108.txt).

use super::mixer::Ctl;
use crate::fwlog;
use std::io;

/// Every DAPM route + static codec setting that must hold BEFORE the first
/// PCM open. Idempotent: absolute values, safe to re-run.
const PRE_PCM: &[(&str, &[&str])] = &[
    // --- capture routes: mic array reaches the codec on the DIFFERENTIAL
    //     inputs; nothing routes DIF1 into any ADC → all four stay powered
    //     down, capture returns the I2S bus's own noise floor with a healthy
    //     clock (routes.go, measured register evidence in its header).
    ("ADC_A Right Ip Select ADC_A DIF1_R switch", &["1"]),
    ("ADC_A Left Ip Select ADC_A DIF1_L switch", &["1"]),
    ("ADC_B Right Ip Select ADC_B DIF1_R switch", &["1"]),
    ("ADC_B Left Ip Select ADC_B DIF1_L switch", &["1"]),
    ("ADC_C Right Ip Select ADC_C DIF1_R switch", &["1"]),
    ("ADC_C Left Ip Select ADC_C DIF1_L switch", &["1"]),
    ("ADC_D Right Ip Select ADC_D DIF1_R switch", &["1"]),
    ("ADC_D Left Ip Select ADC_D DIF1_L switch", &["1"]),
    // --- playback routes: DAC into the output mixer, or the DAC powers down
    //     while the firmware streams "successfully" into silence.
    ("HPR Output Mixer R_DAC Switch", &["1"]),
    ("HPL Output Mixer L_DAC Switch", &["1"]),
    // --- static codec settings (the old script's part).
    ("Audio_I2S1_Setting", &["On"]),
    ("HP DAC Playback Switch", &["1", "1"]),
    ("MFP Gpio Mute", &["On"]),
    // --- capture gains: equalised across all four ADCs (G4/G5-2 calibration
    //     basis: VAD −42/−47 measured behind exactly these gains).
    ("ADC_A Digital Volume Control", &["88", "88"]),
    ("ADC_B Digital Volume Control", &["88", "88"]),
    ("ADC_C Digital Volume Control", &["88", "88"]),
    ("ADC_D Digital Volume Control", &["88", "88"]),
    ("ADC_A MICPGA Volume Ctrl", &["40", "40"]),
    ("ADC_B MICPGA Volume Ctrl", &["40", "40"]),
    ("ADC_C MICPGA Volume Ctrl", &["40", "40"]),
    ("ADC_D MICPGA Volume Ctrl", &["40", "40"]),
];

const DAC_UNITY: &str = "127"; // PCM Playback Volume 0dB index (pcm_speaker.go)
const PLAYBACK_VOLUME: &str = "PCM Playback Volume";
const SPEAKER_AMP: &str = "Ext_Speaker_Amp_Switch";

/// The eight hardware mic-mute controls (mute.go): all four ADC chips, both
/// channels — a silent miss on any one is a hot microphone.
const ADC_MUTE: &[&str] = &[
    "ADC_A Left Mute",
    "ADC_A Right Mute",
    "ADC_B Left Mute",
    "ADC_B Right Mute",
    "ADC_C Left Mute",
    "ADC_C Right Mute",
    "ADC_D Left Mute",
    "ADC_D Right Mute",
];

/// Apply routes + static settings. Loud per-item failures, EchoMuse-style
/// summary; returns (failed, total) so the caller can surface it.
pub fn pre_pcm(ctl: &Ctl) -> (usize, usize) {
    let mut failed = 0usize;
    for (name, values) in PRE_PCM {
        match ctl.set(name, values) {
            Ok(()) => {}
            Err(e) => {
                failed += 1;
                fwlog!("[bringup] {name} = {values:?}: {e}");
            }
        }
    }
    if failed > 0 {
        fwlog!(
            "[bringup] {failed} of {} pre-PCM writes failed — audio may be silent",
            PRE_PCM.len()
        );
    } else {
        fwlog!("[bringup] {} pre-PCM settings applied", PRE_PCM.len());
    }
    (failed, PRE_PCM.len())
}

/// Click-free amp bring-up AFTER the playback PCM is open and configured.
/// `clock_silence` must push ~2 periods of zeros into the stream (the DAC
/// must be clocking silence before the amp sees it — pcm_speaker.go
/// 2026-07-10, the start-click fix).
pub fn amp_up(ctl: &Ctl, clock_silence: impl FnOnce()) {
    let _ = ctl.set(PLAYBACK_VOLUME, &["0", "0"]); // mute before touching amp
    clock_silence();
    std::thread::sleep(std::time::Duration::from_millis(100)); // silence reaches the DAC
    match ctl.set(SPEAKER_AMP, &["On"]) {
        Ok(()) => {}
        Err(e) => fwlog!("[bringup] {SPEAKER_AMP} On: {e}"),
    }
    std::thread::sleep(std::time::Duration::from_millis(50)); // amp settle
    let _ = ctl.set(PLAYBACK_VOLUME, &[DAC_UNITY, DAC_UNITY]); // volume law is software
    fwlog!("[bringup] amp up (volume at DAC unity, gain in software)");
}

/// Teardown pair of amp_up (the SIGTERM trap in the old script had this).
pub fn amp_down(ctl: &Ctl) {
    let _ = ctl.set(PLAYBACK_VOLUME, &["0", "0"]);
    let _ = ctl.set(SPEAKER_AMP, &["Off"]);
}

/// Hardware mic mute across all four ADC chips. Reports every failure — a
/// silent miss here is a hot microphone (mute.go).
pub fn adc_mute(ctl: &Ctl, on: bool) -> io::Result<()> {
    let val = if on { "On" } else { "Off" };
    let mut failed = 0usize;
    for name in ADC_MUTE {
        if ctl.set(name, &[val]).is_err() {
            failed += 1;
        }
    }
    if failed > 0 {
        fwlog!(
            "[bringup] {failed} of {} ADC mute controls failed to set {val}",
            ADC_MUTE.len()
        );
        return Err(io::Error::other(format!("{failed} ADC mute writes failed")));
    }
    Ok(())
}
