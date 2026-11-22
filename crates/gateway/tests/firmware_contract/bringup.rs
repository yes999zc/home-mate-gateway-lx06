//! Bring-up and local controls: hello/clock, the uplink layout, the Mute
//! gate and the long-press report.

use crate::support::*;
use home_mate_device_gateway::firmware::{ButtonEvent, FirmwareConfig};
use home_mate_device_gateway::protocol::{
    CoreFrame, GatewayFrame, PROTOCOL_MIN_VERSION, PROTOCOL_VERSION,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;

#[tokio::test]
async fn time_set_projects_clock_and_keeps_link_alive() {
    let (core, port) = spawn_fake_core().await;
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _buttons) = FakeHardware::new(probes.clone());
    let sd = state_dir("timeset");
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: sd,
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
        .until(|f| matches!(f, GatewayFrame::MuteState { .. }))
        .await;

    // The projection lands on the hardware seam verbatim (G6 replaced-
    // capability #2: emOS has no time service, the core owns the clock).
    core.send(&CoreFrame::TimeSet {
        ts_ms: 1_799_000_000_000,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(probes.lock().unwrap().time_sets, vec![1_799_000_000_000]);

    // Fire-and-forget: no reply frame, and the link survives — a VolumeSet
    // right after still gets its device_confirmed receipt.
    core.send(&CoreFrame::VolumeSet {
        task_id: "t-time-1".into(),
        device: "dot2-d".into(),
        value: 0.5,
        unit: "linear".into(),
    })
    .await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::VolumeReceipt { task_id, stage, .. } if task_id == "t-time-1" && stage == "device_confirmed"))
        .await;
}

#[tokio::test]
async fn hw_ref_uplink_is_two_channel() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _buttons) = FakeHardware::new(probes);
    let sd = state_dir("hwref");

    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: sd,
        hw_ref: true,
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
    core.send(&CoreFrame::MicSession {
        device: "dot2-d".into(),
        action: "start".into(),
        mode: Some("continuous".into()),
    })
    .await;
    let report = core
        .until(|f| matches!(f, GatewayFrame::MicReport { active: true, .. }))
        .await;
    let GatewayFrame::MicReport {
        layout, channels, ..
    } = report
    else {
        unreachable!()
    };
    assert_eq!(layout.as_deref(), Some("mic_ref"));
    assert_eq!(channels, Some(2));

    let chunk = core
        .until(|f| matches!(f, GatewayFrame::AudioChunk { .. }))
        .await;
    let GatewayFrame::AudioChunk {
        pcm_b64, layout, ..
    } = chunk
    else {
        unreachable!()
    };
    assert_eq!(layout, "mic_ref");
    let pcm = base64::engine::general_purpose::STANDARD
        .decode(pcm_b64)
        .unwrap();
    assert_eq!(pcm.len(), 5120, "2ch [mic|ref] 80 ms block");
    let mic = i16::from_le_bytes([pcm[0], pcm[1]]);
    let r = i16::from_le_bytes([pcm[2], pcm[3]]);
    assert_eq!(mic, 3000);
    assert_eq!(r, 20000);
}

// --- G6: device-owned hardware mic mute (onepager §3-3) -----------------------------

/// The Mute key toggles a hardware-level privacy gate: ADC mute controls are
/// driven, uplink audio_chunk STOPS entirely (not a software flag), the ring
/// shows muted, the core hears MuteState, and the state persists.
#[tokio::test]
async fn mute_key_gates_uplink_and_drives_hardware() {
    let (core, port) = spawn_fake_core().await;
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, buttons) = FakeHardware::new(probes.clone());
    let sd = state_dir("mute");
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
    // Boot truth: mute state goes north even when false.
    let boot = core
        .until(|f| matches!(f, GatewayFrame::MuteState { muted: false, source, .. } if source == "device_boot"))
        .await;
    drop(boot);

    core.send(&CoreFrame::MicSession {
        device: "dot2-d".into(),
        action: "start".into(),
        mode: Some("continuous".into()),
    })
    .await;
    let _report = core
        .until(|f| matches!(f, GatewayFrame::MicReport { active: true, .. }))
        .await;
    let _chunk = core
        .until(|f| matches!(f, GatewayFrame::AudioChunk { .. }))
        .await;

    // Mute ON.
    buttons.send(ButtonEvent::MuteToggle).unwrap();
    let ms = core
        .until(|f| matches!(f, GatewayFrame::MuteState { muted: true, source, .. } if source == "button"))
        .await;
    drop(ms);
    assert_eq!(
        probes.lock().unwrap().adc_mutes,
        vec![true],
        "hardware ADC mute driven"
    );
    let leds_now = probes.lock().unwrap().leds.last().copied().unwrap();
    assert_eq!(leds_now[0], (0x78, 0, 0), "ring shows muted red");

    // The gate: no audio_chunk for a window that would carry several blocks
    // at the fake's fill-rate (capture is instantaneous → any leak spams).
    tokio::time::sleep(Duration::from_millis(150)).await;
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
    assert_eq!(drained, 0, "no uplink blocks while muted");
    let persisted: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sd.join("state.json")).unwrap()).unwrap();
    assert_eq!(persisted["muted"], true, "muted persists");

    // Mute OFF: uplink resumes, hardware unmutes, MuteState reflects it.
    buttons.send(ButtonEvent::MuteToggle).unwrap();
    core.until(|f| matches!(f, GatewayFrame::MuteState { muted: false, .. }))
        .await;
    assert_eq!(probes.lock().unwrap().adc_mutes, vec![true, false]);
    core.until(|f| matches!(f, GatewayFrame::AudioChunk { .. }))
        .await;
    let persisted: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sd.join("state.json")).unwrap()).unwrap();
    assert_eq!(persisted["muted"], false);
}

/// Long-press is reported north with hold semantics and NO local action.
#[tokio::test]
async fn long_press_reports_hold_north() {
    let (core, port) = spawn_fake_core().await;
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, buttons) = FakeHardware::new(probes.clone());
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: state_dir("hold"),
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
    // Sync point: wait for the boot truth sequence to finish before pressing
    // (a press that races hello is dropped by design — north_tx is None).
    core.until(|f| matches!(f, GatewayFrame::MuteState { source, .. } if source == "device_boot"))
        .await;

    buttons.send(ButtonEvent::Hold(113, 1600)).unwrap();
    let b = core
        .until(|f| matches!(f, GatewayFrame::Button { .. }))
        .await;
    match b {
        GatewayFrame::Button { action, .. } => {
            assert_eq!(action, "113_hold:1600ms");
        }
        _ => unreachable!(),
    }
    // No mute side effects from a hold.
    assert!(probes.lock().unwrap().adc_mutes.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mute_hold_enters_wifi_provisioning_stops_playback_and_never_resets() {
    let (core, port) = spawn_fake_core().await;
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, buttons) = FakeHardware::new(probes.clone());
    let dir = state_dir("mute-hold-wifi");
    std::fs::create_dir_all(&dir).unwrap();
    let state_file = dir.join("state.json");
    std::fs::write(&state_file, r#"{"volume":88,"muted":false}"#).unwrap();

    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: dir.clone(),
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
    core.until(|f| matches!(f, GatewayFrame::MuteState { source, .. } if source == "device_boot"))
        .await;

    // Start a playback so we can verify hold-mute stops it.
    let wav = wav_48k(240_000, 8_000);
    core.send(&CoreFrame::Play {
        task_id: "wifi-hold-play".into(),
        device: "dot2-d".into(),
        content_ref: "test:wifi-hold".into(),
        audio: home_mate_device_gateway::protocol::AudioMeta {
            format: "wav".into(),
            sample_rate: 48_000,
            channels: 1,
        },
        audio_b64: Some({
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(&wav)
        }),
        channel: Some("voice".into()),
        text: None,
        simulated_ms: None,
        barge_in: None,
    })
    .await;
    core.until(
        |f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "wifi-hold-play" && stage == "accepted"),
    )
    .await;

    buttons.send(ButtonEvent::Hold(113, 2000)).unwrap();

    // Expect both the hold fact and the mute_hold_wifi_setup notice.
    let hold_fact = core
        .until(|f| matches!(f, GatewayFrame::Button { action, .. } if action == "113_hold:2000ms"))
        .await;
    assert!(matches!(hold_fact, GatewayFrame::Button { .. }));

    let setup_notice = core
        .until(
            |f| matches!(f, GatewayFrame::Button { action, .. } if action == "mute_hold_wifi_setup"),
        )
        .await;
    assert!(matches!(setup_notice, GatewayFrame::Button { .. }));

    // Playback was flushed, mute latch was NOT toggled, and state.json was NOT wiped (no factory reset).
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
    while probes.lock().unwrap().flushed == 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(probes.lock().unwrap().flushed > 0);
    assert!(probes.lock().unwrap().adc_mutes.is_empty());
    assert!(
        state_file.exists(),
        "state.json must never be factory-reset"
    );

    // (1) Mute hold lights the dedicated Wi-Fi provisioning LED pattern.
    let last_led = probes.lock().unwrap().leds.last().copied().unwrap();
    assert_eq!(
        last_led,
        [home_mate_device_gateway::firmware::leds::WIFI_PROVISIONING_RGB; 12],
        "mute hold must light WIFI_PROVISIONING_RGB"
    );

    // (2) While in provisioning mode, incoming Play and PlayBegin are rejected
    // with PlayRejected { reason: "wifi_provisioning" }.
    core.send(&CoreFrame::Play {
        task_id: "play-during-prov".into(),
        device: "dot2-d".into(),
        content_ref: "test:reject-play".into(),
        audio: home_mate_device_gateway::protocol::AudioMeta {
            format: "wav".into(),
            sample_rate: 48_000,
            channels: 1,
        },
        audio_b64: None,
        channel: Some("voice".into()),
        text: None,
        simulated_ms: Some(200),
        barge_in: None,
    })
    .await;
    let rej_play = core
        .until(
            |f| matches!(f, GatewayFrame::PlayRejected { task_id, reason, .. } if task_id == "play-during-prov" && reason == "wifi_provisioning"),
        )
        .await;
    assert!(matches!(rej_play, GatewayFrame::PlayRejected { .. }));

    core.send(&CoreFrame::PlayBegin {
        task_id: "stream-during-prov".into(),
        device: "dot2-d".into(),
        content_ref: "test:reject-stream".into(),
        audio: home_mate_device_gateway::protocol::AudioMeta {
            format: "pcm_s16le".into(),
            sample_rate: 16_000,
            channels: 1,
        },
        channel: Some("voice".into()),
        text: None,
        barge_in: None,
    })
    .await;
    let rej_stream = core
        .until(
            |f| matches!(f, GatewayFrame::PlayRejected { task_id, reason, .. } if task_id == "stream-during-prov" && reason == "wifi_provisioning"),
        )
        .await;
    assert!(matches!(rej_stream, GatewayFrame::PlayRejected { .. }));

    // (3) Applying WifiConfig (which rolls back on failure in test env) clears
    // provisioning mode, emits WifiState, and resumes normal Play.
    core.send(&CoreFrame::WifiConfig {
        ssid: "TestNet".into(),
        psk: "password1234".into(),
    })
    .await;
    let wifi_state = core
        .until(|f| matches!(f, GatewayFrame::WifiState { .. }))
        .await;
    assert!(matches!(wifi_state, GatewayFrame::WifiState { .. }));

    // Normal Play now succeeds again.
    core.send(&CoreFrame::Play {
        task_id: "play-after-prov".into(),
        device: "dot2-d".into(),
        content_ref: "test:after-prov".into(),
        audio: home_mate_device_gateway::protocol::AudioMeta {
            format: "wav".into(),
            sample_rate: 48_000,
            channels: 1,
        },
        audio_b64: Some({
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(&wav)
        }),
        channel: Some("voice".into()),
        text: None,
        simulated_ms: None,
        barge_in: None,
    })
    .await;
    let accepted = core
        .until(
            |f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "play-after-prov" && stage == "accepted"),
        )
        .await;
    assert!(matches!(accepted, GatewayFrame::PlayReceipt { .. }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn led_config_frame_updates_live_ring_and_persists_across_reload() {
    use home_mate_device_gateway::firmware::leds;
    let (core, port) = spawn_fake_core().await;
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, buttons) = FakeHardware::new(probes.clone());
    let dir = state_dir("led-config-persist");

    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: dir.clone(),
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
    core.until(|f| matches!(f, GatewayFrame::MuteState { source, .. } if source == "device_boot"))
        .await;

    // Set semantic state to "speaking" (0x00, 0x6E, 0x96).
    core.send(&CoreFrame::AssistantState {
        device: "dot2-d".into(),
        state: "speaking".into(),
    })
    .await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    let full_speaking = leds::state_rgb("speaking")[0];
    assert_eq!(
        probes.lock().unwrap().leds.last().unwrap()[0],
        full_speaking
    );

    // Send CoreFrame::LedConfig with an all-day night window (start == end -> always active)
    // at 25% brightness, and rotation = 3, reverse = true.
    core.send(&CoreFrame::LedConfig {
        device: "dot2-d".into(),
        night_enabled: true,
        night_start_minute: 0,
        night_end_minute: 0,
        night_brightness_pct: 25,
        orientation_rotation: 3,
        orientation_reverse: true,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(80)).await;

    let dimmed_speaking = leds::scaled(&leds::state_rgb("speaking"), 0.25)[0];
    assert_eq!(
        probes.lock().unwrap().leds.last().unwrap()[0],
        dimmed_speaking,
        "live LED ring must immediately dim to 25%"
    );

    // Trigger a volume arc (64 -> 6 segments) to verify physical orientation (rot=3, rev=true)
    // and night-mode dimming (25%) are applied together on the live ring.
    buttons.send(ButtonEvent::VolumeUp).unwrap();
    let _vol = core
        .until(|f| matches!(f, GatewayFrame::VolumeReceipt { .. }))
        .await;
    let expected_arc = leds::ring_frame_with_prefs(
        false,
        None,
        Some(108),
        None,
        "speaking",
        0,
        leds::NightMode {
            enabled: true,
            start_minute: 0,
            end_minute: 0,
            brightness_pct: 25,
        },
        0,
        leds::LedOrientation {
            rotation: 3,
            reverse: true,
        },
    );
    assert_eq!(
        *probes.lock().unwrap().leds.last().unwrap(),
        expected_arc,
        "volume arc must reflect both 25% night dimming and rot=3/rev=true orientation"
    );

    // Verify persistence on disk (<state_dir>/led_config.json) and reload into a new Firmware.
    let saved = leds::load_led_config(&dir).expect("led_config.json must be persisted");
    assert!(saved.night_mode.enabled);
    assert_eq!(saved.night_mode.brightness_pct, 25);
    assert_eq!(saved.led_orientation.rotation, 3);
    assert!(saved.led_orientation.reverse);

    // Spawn a second firmware instance on the same state_dir (with default FirmwareConfig)
    // and verify it loads the persisted LED config on startup.
    let (core2, port2) = spawn_fake_core().await;
    let probes2 = Arc::new(Mutex::new(Probes::default()));
    let (hw2, _buttons2) = FakeHardware::new(probes2.clone());
    let cfg2 = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port2}"),
        state_dir: dir,
        ..Default::default()
    };
    spawn_firmware(cfg2, Arc::new(hw2));
    let _hello2 = core2.recv().await;
    core2
        .send(&CoreFrame::HelloAck {
            protocol: PROTOCOL_VERSION,
            min_protocol: PROTOCOL_MIN_VERSION,
            core_version: "test-core".into(),
            supported_versions: vec![],
            capabilities: vec![],
            generation: 2,
        })
        .await;
    core2
        .until(|f| matches!(f, GatewayFrame::MuteState { source, .. } if source == "device_boot"))
        .await;
    core2
        .send(&CoreFrame::AssistantState {
            device: "dot2-d".into(),
            state: "speaking".into(),
        })
        .await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        probes2.lock().unwrap().leds.last().unwrap()[0],
        dimmed_speaking,
        "reloaded firmware must apply persisted 25% night-mode dimming from led_config.json"
    );
}
