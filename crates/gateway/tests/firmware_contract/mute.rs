//! Protocol v1 remote mute contract (#13, 2026-11-16): the `MuteSet` frame
//! writes THE latch (the same one the physical key owns), answers with a
//! `MuteState` fact that echoes the task id, gates the uplink, persists, and
//! is NOT fooled by an `AssistantState` — while the physical key stays able
//! to re-toggle after a remote set.

use crate::support::*;
use home_mate_device_gateway::firmware::{ButtonEvent, FirmwareConfig};
use home_mate_device_gateway::protocol::{
    CoreFrame, GatewayFrame, PROTOCOL_MIN_VERSION, PROTOCOL_VERSION,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

async fn spawn_live_mic() -> (
    Arc<Mutex<Probes>>,
    std::sync::mpsc::Sender<ButtonEvent>,
    Arc<FakeCore>,
    std::path::PathBuf,
) {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, btn) = FakeHardware::new(probes.clone());
    let sd = state_dir("remote-mute");
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: sd.clone(),
        ..Default::default()
    };
    spawn_firmware(cfg, Arc::new(hw));
    let _hello = core.recv().await;
    core.send(&CoreFrame::HelloAck {
        protocol: PROTOCOL_VERSION,
        min_protocol: PROTOCOL_MIN_VERSION,
        core_version: "test-core".into(),
        supported_versions: vec![],
        capabilities: vec![],
        generation: 1,
    })
    .await;
    let _boot = core
        .until(|f| matches!(f, GatewayFrame::MuteState { source, .. } if source == "device_boot"))
        .await;
    core.send(&CoreFrame::MicSession {
        device: "dot2-d".into(),
        action: "start".into(),
        mode: Some("continuous".into()),
    })
    .await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::MicReport { active: true, .. }))
        .await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::AudioChunk { .. }))
        .await;
    (probes, btn, core, sd)
}

/// The whole #13 contract in one line-by-line walkthrough: set, gate,
/// persist, echo, key interleave, and the idle-state negative.
#[tokio::test]
async fn a_remote_mute_set_owns_the_same_latch_as_the_key() {
    let (probes, btn, core, sd) = spawn_live_mic().await;

    // 1. The frame sets the latch: hardware gate, echo receipt, persistence.
    core.send(&CoreFrame::MuteSet {
        task_id: "M-1".into(),
        device: "dot2-d".into(),
        muted: true,
    })
    .await;
    let receipt = core
        .until(|f| matches!(f, GatewayFrame::MuteState { muted: true, .. }))
        .await;
    let GatewayFrame::MuteState {
        source, task_id, ..
    } = receipt
    else {
        unreachable!()
    };
    assert_eq!(source, "core_set");
    assert_eq!(
        task_id.as_deref(),
        Some("M-1"),
        "the receipt echoes the command"
    );
    assert_eq!(
        probes.lock().unwrap().adc_mutes,
        vec![true],
        "hardware gate driven"
    );
    let persisted: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sd.join("state.json")).unwrap()).unwrap();
    assert_eq!(
        persisted["muted"], true,
        "the latch persists like a key press"
    );

    // 2. Uplink gated: no AudioChunk for a window that would carry several.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let drained = {
        let mut rx = core.frames.lock().unwrap();
        let mut seen = 0;
        while let Ok(f) = rx.try_recv() {
            if matches!(f, GatewayFrame::AudioChunk { .. }) {
                seen += 1;
            }
        }
        seen
    };
    assert_eq!(drained, 0, "no uplink while muted (privacy is local)");

    // 3. An AssistantState NEVER writes the latch (the old sovereignty rule,
    // narrowed — not even the core's own state machine may unmute).
    core.send(&CoreFrame::AssistantState {
        device: "dot2-d".into(),
        state: "idle".into(),
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    {
        let mut rx = core.frames.lock().unwrap();
        while let Ok(f) = rx.try_recv() {
            assert!(
                !matches!(f, GatewayFrame::MuteState { muted: false, .. }),
                "an idle state must not unmute: {f:?}"
            );
        }
    }
    assert_eq!(
        probes.lock().unwrap().adc_mutes,
        vec![true],
        "still hardware-muted after an idle state"
    );

    // 4. The physical key still works ON TOP of a remote set: one toggle
    // unmutes (same latch, same hardware gate).
    btn.send(ButtonEvent::MuteToggle).unwrap();
    let receipt = core
        .until(|f| matches!(f, GatewayFrame::MuteState { muted: false, .. }))
        .await;
    let GatewayFrame::MuteState {
        source, task_id, ..
    } = receipt
    else {
        unreachable!()
    };
    assert_eq!(source, "button");
    assert_eq!(task_id, None, "a key press is a fact, not a receipt");
    assert_eq!(
        probes.lock().unwrap().adc_mutes,
        vec![true, false],
        "hardware gate driven back open"
    );
    // ...and the uplink flows again.
    let _ = core
        .until(|f| matches!(f, GatewayFrame::AudioChunk { .. }))
        .await;
}

/// Idempotency: a MuteSet for a value the latch ALREADY holds still answers
/// (the core resolves its command against the receipt), but must not drive
/// the hardware twice.
#[tokio::test]
async fn an_idempotent_mute_set_still_answers_but_skips_the_hardware() {
    let (probes, _btn, core, _sd) = spawn_live_mic().await;

    core.send(&CoreFrame::MuteSet {
        task_id: "M-1".into(),
        device: "dot2-d".into(),
        muted: true,
    })
    .await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::MuteState { muted: true, .. }))
        .await;
    assert_eq!(probes.lock().unwrap().adc_mutes, vec![true]);

    core.send(&CoreFrame::MuteSet {
        task_id: "M-2".into(),
        device: "dot2-d".into(),
        muted: true,
    })
    .await;
    let receipt = core
        .until(|f| {
            matches!(f, GatewayFrame::MuteState { muted: true, task_id: Some(t), .. } if t == "M-2")
        })
        .await;
    let GatewayFrame::MuteState { task_id, .. } = receipt else {
        unreachable!()
    };
    assert_eq!(task_id.as_deref(), Some("M-2"));
    assert_eq!(
        probes.lock().unwrap().adc_mutes,
        vec![true],
        "no duplicate ADC write for an unchanged latch"
    );
}
