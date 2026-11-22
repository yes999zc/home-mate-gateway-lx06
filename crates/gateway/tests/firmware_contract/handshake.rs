//! Protocol v1 handshake contract (#11, 2026-11-16): a version interval with
//! no intersection is refused with the protocol-error lamp AND a
//! `ProtocolError` reason frame, inside the disconnect budget; an OLD-shaped
//! hello (no new fields at all — a release-67-era core) still negotiates at
//! version 0, which is exactly the mixed activation window release A must
//! survive.

use crate::support::*;
use home_mate_device_gateway::firmware::leds;
use home_mate_device_gateway::firmware::FirmwareConfig;
use home_mate_device_gateway::protocol::{
    CoreFrame, GatewayFrame, PROTOCOL_MIN_VERSION, PROTOCOL_VERSION,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The core speaks [7,7]: no intersection with this build's [0,0]. The
/// firmware must paint the PROTOCOL lamp, report WHY north, and drop the
/// link — never limp along on a half-understood peer.
#[tokio::test]
async fn a_disjoint_core_interval_refuses_with_lamp_and_reason() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes.clone());
    let sd = state_dir("handshake-refuse");
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: sd,
        ..Default::default()
    };
    spawn_firmware(cfg, Arc::new(hw));

    let _hello = core.recv().await;
    core.send(&CoreFrame::HelloAck {
        protocol: 7,
        min_protocol: 7,
        core_version: "future-core".into(),
        supported_versions: vec![],
        capabilities: vec![],
        generation: 1,
    })
    .await;

    // The refusal names the reason (this frame rides the handshake socket —
    // the writer task does not exist on a refused link).
    let f = core.recv().await;
    let GatewayFrame::ProtocolError {
        protocol,
        min_protocol,
        gateway_version,
        reason,
    } = f
    else {
        panic!("expected protocol_error, got {f:?}");
    };
    assert_eq!(protocol, PROTOCOL_VERSION);
    assert_eq!(min_protocol, PROTOCOL_MIN_VERSION);
    assert!(!gateway_version.is_empty(), "the firmware names its build");
    assert!(
        reason.contains("protocol_no_intersection"),
        "the reason names the verdict: {reason}"
    );

    // The lamp is the PROTOCOL red-orange — and it STAYS (sticky across the
    // 30 s retry), never a generic offline white.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let painted = probes.lock().unwrap().leds.clone();
        assert!(
            painted.iter().any(|f| f[0] == leds::FAULT_PROTOCOL_RGB),
            "the protocol-error lamp must be painted; frames so far: {:?}",
            painted.iter().map(|f| f[0]).collect::<Vec<_>>()
        );
        if std::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Byte-for-byte an old core's ack: two fields, no min_protocol, no
/// core_version. serde defaults must land this at operating version 0 and a
/// fully working link (the activation order runs THIS firmware against the
/// OLD core for seconds — a rejection here would roll every release back).
#[tokio::test]
async fn an_old_shaped_core_ack_still_negotiates() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes.clone());
    let sd = state_dir("handshake-old");
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: sd,
        ..Default::default()
    };
    spawn_firmware(cfg, Arc::new(hw));

    let _hello = core.recv().await;
    core.send_raw(r#"{"type":"hello_ack","protocol":0,"generation":4}"#)
        .await;

    // The link is LIVE: boot mute truth flows, meaning the post-hello block
    // ran with a negotiated link.
    let boot = core
        .until(|f| matches!(f, GatewayFrame::MuteState { source, .. } if source == "device_boot"))
        .await;
    let GatewayFrame::MuteState { muted, task_id, .. } = boot else {
        unreachable!()
    };
    assert!(!muted);
    assert_eq!(task_id, None, "a boot report is a fact, not a receipt");
    // And the lamp left the Reboot gray: a successful handshake hands the
    // ring back to the semantic layer (dark idle here).
    tokio::time::sleep(Duration::from_millis(150)).await;
    let last = probes.lock().unwrap().leds.last().copied().unwrap();
    assert_eq!(
        last[0],
        (0, 0, 0),
        "a good handshake clears the reboot lamp: {:?}",
        last[0]
    );
}

/// Exercises production Hello construction rather than a manually built frame.
#[tokio::test]
async fn hello_reports_configured_tail_and_disabled_aec_honestly() {
    for (enabled, tail, expected) in [
        (true, 2560, Some(160)),
        (true, 10240, Some(640)),
        (false, 10240, None),
    ] {
        let probes = Arc::new(Mutex::new(Probes::default()));
        let (hw, _buttons) = FakeHardware::new(probes);
        let (core, port) = spawn_fake_core().await;
        spawn_firmware(
            FirmwareConfig {
                core_addr: format!("127.0.0.1:{port}"),
                state_dir: state_dir(&format!("effective-tail-{enabled}-{tail}")),
                aec: enabled,
                aec_tail: tail,
                loudness: false,
                boot_health: false,
                ..Default::default()
            },
            Arc::new(hw),
        );
        let hello = core.recv().await;
        assert!(
            matches!(hello, GatewayFrame::Hello { aec_tail_ms, .. } if aec_tail_ms == expected),
            "{hello:?}"
        );
    }
}
