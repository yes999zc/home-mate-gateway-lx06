//! Actual capture worker → PCM wire, with partial and discontinuous Hardware.

use crate::support::*;
use base64::Engine as _;
use home_mate_device_gateway::firmware::FirmwareConfig;
use home_mate_device_gateway::protocol::{
    CoreFrame, GatewayFrame, PROTOCOL_MIN_VERSION, PROTOCOL_VERSION,
};
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn capture_assembles_partial_reads_and_discards_discontinuous_prefixes() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _buttons) = FakeHardware::new(probes);
    let plan = hw.capture_plan.clone();
    let (core, port) = spawn_fake_core().await;
    spawn_firmware(
        FirmwareConfig {
            core_addr: format!("127.0.0.1:{port}"),
            state_dir: state_dir("partial-capture"),
            loudness: false,
            ..Default::default()
        },
        Arc::new(hw),
    );
    let _hello = core.recv().await;
    core.send(&CoreFrame::HelloAck {
        protocol: PROTOCOL_VERSION,
        min_protocol: PROTOCOL_MIN_VERSION,
        core_version: "test".into(),
        supported_versions: vec![],
        capabilities: vec![],
        generation: 1,
    })
    .await;
    core.send(&CoreFrame::MicSession {
        device: "dot2-d".into(),
        action: "start".into(),
        mode: Some("continuous".into()),
    })
    .await;
    core.until(|f| matches!(f, GatewayFrame::MicReport { active: true, .. }))
        .await;

    plan.lock()
        .unwrap()
        .extend([(Ok(128), 1000), (Ok(1152), 2000)]);
    let frame = core
        .until(|f| {
            let GatewayFrame::AudioChunk { pcm_b64, .. } = f else {
                return false;
            };
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(pcm_b64)
                .unwrap();
            let samples: Vec<i16> = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
            samples.len() == 1280
                && samples[..128].iter().all(|&s| s == 1000)
                && samples[128..].iter().all(|&s| s == 2000)
        })
        .await;
    assert!(matches!(frame, GatewayFrame::AudioChunk { .. }));

    // The first prefix is lost on overrun. Neither it nor the previous
    // complete block's suffix may appear in the next emitted block.
    plan.lock()
        .unwrap()
        .extend([(Ok(128), 1234), (Err(libc::EPIPE), 0), (Ok(1280), 4000)]);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(4);
    loop {
        let frame = tokio::time::timeout_at(deadline, core.recv())
            .await
            .unwrap();
        if let GatewayFrame::AudioChunk { pcm_b64, .. } = frame {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(pcm_b64)
                .unwrap();
            let samples: Vec<i16> = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
            assert!(
                !samples.contains(&1234),
                "a partial failed block must not be published"
            );
            if samples.iter().all(|&s| s == 4000) {
                break;
            }
        }
    }
}

#[tokio::test]
async fn capture_recovers_from_transient_epipe_without_sleep_stall() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _buttons) = FakeHardware::new(probes);
    let plan = hw.capture_plan.clone();
    let (core, port) = spawn_fake_core().await;
    spawn_firmware(
        FirmwareConfig {
            core_addr: format!("127.0.0.1:{port}"),
            state_dir: state_dir("epipe-no-sleep"),
            loudness: false,
            ..Default::default()
        },
        Arc::new(hw),
    );
    let _hello = core.recv().await;
    core.send(&CoreFrame::HelloAck {
        protocol: PROTOCOL_VERSION,
        min_protocol: PROTOCOL_MIN_VERSION,
        core_version: "test".into(),
        supported_versions: vec![],
        capabilities: vec![],
        generation: 1,
    })
    .await;
    core.send(&CoreFrame::MicSession {
        device: "dot2-d".into(),
        action: "start".into(),
        mode: Some("continuous".into()),
    })
    .await;
    core.until(|f| matches!(f, GatewayFrame::MicReport { active: true, .. }))
        .await;

    // Anchor on a scripted 4000 block so the capture thread is actively
    // draining the plan, then measure 3 consecutive EPIPEs -> 5000 block.
    // With the old 200 ms sleep per EPIPE this took >= 600 ms.
    plan.lock().unwrap().extend([
        (Ok(1280), 4000),
        (Err(libc::EPIPE), 0),
        (Err(libc::EPIPE), 0),
        (Err(libc::EPIPE), 0),
        (Ok(1280), 5000),
    ]);
    let is_constant_sample = |f: &GatewayFrame, target: i16| -> bool {
        let GatewayFrame::AudioChunk { pcm_b64, .. } = f else {
            return false;
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(pcm_b64)
            .unwrap();
        bytes
            .as_chunks::<2>()
            .0
            .iter()
            .all(|b| i16::from_le_bytes([b[0], b[1]]) == target)
    };
    core.until(|f| is_constant_sample(f, 4000)).await;
    let started = std::time::Instant::now();
    core.until(|f| is_constant_sample(f, 5000)).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(350),
        "expected immediate retry after EPIPE, but recovery took {elapsed:?}"
    );
}

#[tokio::test]
async fn persistent_epipe_applies_bounded_backoff_before_recovery() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _buttons) = FakeHardware::new(probes);
    let plan = hw.capture_plan.clone();
    let (core, port) = spawn_fake_core().await;
    spawn_firmware(
        FirmwareConfig {
            core_addr: format!("127.0.0.1:{port}"),
            state_dir: state_dir("epipe-bounded-backoff"),
            loudness: false,
            ..Default::default()
        },
        Arc::new(hw),
    );
    let _hello = core.recv().await;
    core.send(&CoreFrame::HelloAck {
        protocol: PROTOCOL_VERSION,
        min_protocol: PROTOCOL_MIN_VERSION,
        core_version: "test".into(),
        supported_versions: vec![],
        capabilities: vec![],
        generation: 1,
    })
    .await;
    core.send(&CoreFrame::MicSession {
        device: "dot2-d".into(),
        action: "start".into(),
        mode: Some("continuous".into()),
    })
    .await;
    core.until(|f| matches!(f, GatewayFrame::MicReport { active: true, .. }))
        .await;

    // 13 consecutive EPIPEs: first 3 are immediate (0ms), next 10 sleep 15ms each (>= 150ms),
    // preventing 100% CPU spin on persistent driver failure while still recovering promptly.
    plan.lock().unwrap().push_back((Ok(1280), 4100));
    for _ in 0..13 {
        plan.lock().unwrap().push_back((Err(libc::EPIPE), 0));
    }
    plan.lock().unwrap().push_back((Ok(1280), 5100));

    let is_constant_sample = |f: &GatewayFrame, target: i16| -> bool {
        let GatewayFrame::AudioChunk { pcm_b64, .. } = f else {
            return false;
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(pcm_b64)
            .unwrap();
        bytes
            .as_chunks::<2>()
            .0
            .iter()
            .all(|b| i16::from_le_bytes([b[0], b[1]]) == target)
    };
    core.until(|f| is_constant_sample(f, 4100)).await;
    let started = std::time::Instant::now();
    core.until(|f| is_constant_sample(f, 5100)).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed >= std::time::Duration::from_millis(120),
        "expected bounded backoff on persistent EPIPE, but took only {elapsed:?}"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(600),
        "expected bounded backoff (< 600ms) rather than 200ms*13, took {elapsed:?}"
    );
}
