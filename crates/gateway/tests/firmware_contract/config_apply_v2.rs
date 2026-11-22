//! Contract tests for Stage 2 M2.1 (`#112` WAKE-CONFIG + `#113` ACOUSTIC-APPLY
//! + `config_apply_v2` effective receipts).

use crate::support::*;
use home_mate_device_gateway::firmware::FirmwareConfig;
use home_mate_device_gateway::protocol::{
    ConfigBlock, ConfigReceiptStatus, CoreFrame, GatewayFrame, PROTOCOL_VERSION,
};
use serde_json::json;
use std::sync::{Arc, Mutex};

async fn handshake_v2(core: &FakeCore) {
    let hello = core.recv().await;
    let GatewayFrame::Hello {
        protocol,
        supported_versions,
        devices,
        ..
    } = hello
    else {
        panic!("expected hello, got {hello:?}");
    };
    assert_eq!(protocol, PROTOCOL_VERSION);
    assert_eq!(supported_versions, vec![0, 2]);
    assert!(devices[0]
        .capabilities
        .iter()
        .any(|c| c == "config_apply_v2"));
    core.send(&CoreFrame::HelloAck {
        protocol: 2,
        min_protocol: 0,
        supported_versions: vec![0, 2],
        capabilities: vec!["config_apply_v2".into()],
        core_version: "test-v2".into(),
        generation: 1,
    })
    .await;
}

#[tokio::test]
async fn config_apply_v2_state_validate_apply_and_idempotency() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes.clone());
    let sd = state_dir("cfgv2-idem");
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: sd.clone(),
        ..Default::default()
    };
    spawn_firmware(cfg, Arc::new(hw));
    handshake_v2(&core).await;

    core.send(&CoreFrame::ConfigStateGet {
        request_id: "req-state-1".into(),
        device: "dot2-d".into(),
    })
    .await;
    let state = core
        .until(|f| matches!(f, GatewayFrame::ConfigState { .. }))
        .await;
    let GatewayFrame::ConfigState {
        request_id,
        gateway_boot_id,
        state_revision,
        blocks,
        ..
    } = state
    else {
        unreachable!();
    };
    assert_eq!(request_id, "req-state-1");
    assert!(!gateway_boot_id.is_empty());
    assert_eq!(blocks.len(), 5);

    let led_vals = json!({
        "night_enabled": true,
        "night_start_minute": 1320,
        "night_end_minute": 420,
        "night_brightness_pct": 20,
        "orientation_rotation": 2,
        "orientation_reverse": false,
        "timezone": "Asia/Shanghai",
    });

    core.send(&CoreFrame::ConfigValidate {
        request_id: "req-val-1".into(),
        device: "dot2-d".into(),
        expected_gateway_boot_id: Some(gateway_boot_id.clone()),
        expected_state_revision: Some(state_revision),
        block: ConfigBlock::Led,
        schema_version: 1,
        block_hash: "hash-led-1".into(),
        values: led_vals.clone(),
    })
    .await;
    let val = core
        .until(|f| matches!(f, GatewayFrame::ConfigValidated { .. }))
        .await;
    let GatewayFrame::ConfigValidated {
        supported,
        blockers,
        capability_fingerprint,
        ..
    } = val
    else {
        unreachable!();
    };
    assert!(supported, "unexpected blockers: {blockers:?}");
    assert!(
        !sd.join("led_config.json").exists(),
        "validate must not write"
    );

    core.send(&CoreFrame::ConfigApply {
        device: "dot2-d".into(),
        core_node_id: "core-1".into(),
        data_epoch: 1,
        application_id: "app-1".into(),
        operation_id: "op-led-1".into(),
        desired_revision: 1,
        block_generation: 1,
        block: ConfigBlock::Led,
        schema_version: 1,
        block_hash: "hash-led-1".into(),
        expected_gateway_boot_id: Some(gateway_boot_id.clone()),
        expected_state_revision: Some(state_revision),
        capability_fingerprint: capability_fingerprint.clone(),
        values: led_vals.clone(),
    })
    .await;
    let r1 = core
        .until(|f| matches!(f, GatewayFrame::ConfigReceipt { .. }))
        .await;
    let GatewayFrame::ConfigReceipt {
        status,
        state_revision: rev1,
        ..
    } = &r1
    else {
        unreachable!();
    };
    assert_eq!(*status, ConfigReceiptStatus::Active);
    assert_eq!(*rev1, state_revision + 1);

    // Idempotent retry: same operation_id + same block_hash returns recorded receipt.
    core.send(&CoreFrame::ConfigApply {
        device: "dot2-d".into(),
        core_node_id: "core-1".into(),
        data_epoch: 1,
        application_id: "app-1".into(),
        operation_id: "op-led-1".into(),
        desired_revision: 1,
        block_generation: 1,
        block: ConfigBlock::Led,
        schema_version: 1,
        block_hash: "hash-led-1".into(),
        expected_gateway_boot_id: Some(gateway_boot_id),
        expected_state_revision: Some(*rev1),
        capability_fingerprint,
        values: led_vals,
    })
    .await;
    let r2 = core
        .until(|f| matches!(f, GatewayFrame::ConfigReceipt { .. }))
        .await;
    assert_eq!(r1, r2, "idempotent replay must return identical receipt");

    // Conflicting retry: same operation_id + different block_hash fails.
    core.send(&CoreFrame::ConfigApply {
        device: "dot2-d".into(),
        core_node_id: "core-1".into(),
        data_epoch: 1,
        application_id: "app-1".into(),
        operation_id: "op-led-1".into(),
        desired_revision: 2,
        block_generation: 2,
        block: ConfigBlock::Led,
        schema_version: 1,
        block_hash: "hash-led-DIFFERENT".into(),
        expected_gateway_boot_id: None,
        expected_state_revision: None,
        capability_fingerprint: None,
        values: json!({}),
    })
    .await;
    let r3 = core
        .until(|f| matches!(f, GatewayFrame::ConfigReceipt { error_code: Some(c), .. } if c == "operation_conflict"))
        .await;
    let GatewayFrame::ConfigReceipt { status, .. } = r3 else {
        unreachable!();
    };
    assert_eq!(status, ConfigReceiptStatus::Failed);
}

#[tokio::test]
async fn audio_eq_strict_validation_and_post_rebuild_receipt() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes);
    let sd = state_dir("cfgv2-eq");
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: sd.clone(),
        ..Default::default()
    };
    spawn_firmware(cfg, Arc::new(hw));
    handshake_v2(&core).await;

    // Out-of-range band must fail validation instead of silently clamping.
    core.send(&CoreFrame::ConfigApply {
        device: "dot2-d".into(),
        core_node_id: "core-1".into(),
        data_epoch: 1,
        application_id: "app-eq-bad".into(),
        operation_id: "op-eq-bad".into(),
        desired_revision: 1,
        block_generation: 1,
        block: ConfigBlock::AudioEq,
        schema_version: 1,
        block_hash: "hash-eq-bad".into(),
        expected_gateway_boot_id: None,
        expected_state_revision: None,
        capability_fingerprint: None,
        values: json!({
            "curve": "custom",
            "bands": [{ "freq": 5.0, "gain_db": 2.0, "q": 1.0 }]
        }),
    })
    .await;
    let bad = core
        .until(|f| matches!(f, GatewayFrame::ConfigReceipt { operation_id, .. } if operation_id == "op-eq-bad"))
        .await;
    let GatewayFrame::ConfigReceipt {
        status, error_code, ..
    } = bad
    else {
        unreachable!();
    };
    assert_eq!(status, ConfigReceiptStatus::Failed);
    assert_eq!(error_code.as_deref(), Some("validation_failed"));

    // Valid EQ must persist and emit Active receipt AFTER the 42ms period-boundary Loudness rebuild.
    core.send(&CoreFrame::ConfigApply {
        device: "dot2-d".into(),
        core_node_id: "core-1".into(),
        data_epoch: 1,
        application_id: "app-eq-ok".into(),
        operation_id: "op-eq-ok".into(),
        desired_revision: 2,
        block_generation: 2,
        block: ConfigBlock::AudioEq,
        schema_version: 1,
        block_hash: "hash-eq-ok".into(),
        expected_gateway_boot_id: None,
        expected_state_revision: None,
        capability_fingerprint: None,
        values: json!({
            "curve": "custom",
            "bands": [{ "freq": 1000.0, "gain_db": 4.5, "q": 1.4 }]
        }),
    })
    .await;
    let ok = core
        .until(|f| matches!(f, GatewayFrame::ConfigReceipt { operation_id, .. } if operation_id == "op-eq-ok"))
        .await;
    let GatewayFrame::ConfigReceipt {
        status, evidence, ..
    } = ok
    else {
        unreachable!();
    };
    assert_eq!(status, ConfigReceiptStatus::Active);
    assert_eq!(
        evidence
            .as_ref()
            .and_then(|e| e["applied_at_period_boundary"].as_bool()),
        Some(true)
    );
    assert!(sd.join("audio_eq.json").exists());
}

#[tokio::test]
async fn led_persistence_failure_reports_failed_receipt() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes);
    let sd = state_dir("cfgv2-led-err");
    // Place a directory at led_config.json so rename() fails.
    std::fs::create_dir_all(sd.join("led_config.json")).unwrap();
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: sd,
        ..Default::default()
    };
    spawn_firmware(cfg, Arc::new(hw));
    handshake_v2(&core).await;

    core.send(&CoreFrame::ConfigApply {
        device: "dot2-d".into(),
        core_node_id: "core-1".into(),
        data_epoch: 1,
        application_id: "app-led-err".into(),
        operation_id: "op-led-err".into(),
        desired_revision: 1,
        block_generation: 1,
        block: ConfigBlock::Led,
        schema_version: 1,
        block_hash: "hash-led-err".into(),
        expected_gateway_boot_id: None,
        expected_state_revision: None,
        capability_fingerprint: None,
        values: json!({
            "night_enabled": true,
            "timezone": "UTC",
        }),
    })
    .await;
    let r = core
        .until(|f| matches!(f, GatewayFrame::ConfigReceipt { operation_id, .. } if operation_id == "op-led-err"))
        .await;
    let GatewayFrame::ConfigReceipt {
        status, error_code, ..
    } = r
    else {
        unreachable!();
    };
    assert_eq!(status, ConfigReceiptStatus::Failed);
    assert_eq!(error_code.as_deref(), Some("persistence_failed"));
}

#[tokio::test]
async fn kws_fail_closed_tuning_and_bundle_id_on_keyword() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes);
    let sd = state_dir("cfgv2-kws");
    let kws_dir = sd.join("kws");
    std::fs::create_dir_all(kws_dir.join("model")).unwrap();
    std::fs::write(
        kws_dir.join("model").join("tokens.txt"),
        "n 0\ni 1\nh 2\nao 3\n",
    )
    .unwrap();
    // Make kw.tuning.json a directory so write_tuning fails closed!
    std::fs::create_dir_all(kws_dir.join("kw.tuning.json")).unwrap();

    // Fake sidecar script that prints ready and then emits a keyword hit.
    let run_sh = kws_dir.join("run.sh");
    std::fs::write(
        &run_sh,
        "#!/bin/sh\n\
         echo '{\"event\":\"ready\",\"load_ms\":5,\"threshold\":0.25,\"score\":3.0}'\n\
         if [ -f \"$(dirname \"$0\")/kw.txt\" ]; then\n\
           echo '{\"event\":\"keyword\",\"keyword\":\"你好\",\"fed_ms\":160,\"lag_ms\":20}'\n\
         fi\n\
         exec sleep 30\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&run_sh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&run_sh, perms).unwrap();
    }

    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: sd.clone(),
        kws: true,
        kws_dir: kws_dir.clone(),
        ..Default::default()
    };
    spawn_firmware(cfg, Arc::new(hw));
    handshake_v2(&core).await;

    // 1. Failing tuning write must fail closed.
    core.send(&CoreFrame::ConfigApply {
        device: "dot2-d".into(),
        core_node_id: "core-1".into(),
        data_epoch: 1,
        application_id: "app-kws-1".into(),
        operation_id: "op-kws-fail".into(),
        desired_revision: 1,
        block_generation: 1,
        block: ConfigBlock::Kws,
        schema_version: 1,
        block_hash: "hash-kws-1".into(),
        expected_gateway_boot_id: None,
        expected_state_revision: None,
        capability_fingerprint: None,
        values: json!({
            "keywords": "n i h ao @你好",
            "retreat_words": [],
            "intents_hash": "ih-1",
            "threshold": 0.3,
            "score": 2.5
        }),
    })
    .await;
    let fail = core
        .until(|f| matches!(f, GatewayFrame::ConfigReceipt { operation_id, .. } if operation_id == "op-kws-fail"))
        .await;
    let GatewayFrame::ConfigReceipt {
        status, error_code, ..
    } = fail
    else {
        unreachable!();
    };
    assert_eq!(status, ConfigReceiptStatus::Failed);
    assert_eq!(error_code.as_deref(), Some("kws_tuning_write_failed"));

    // 2. Remove the blocking directory and apply again -> succeeds with bundle_id & kws_generation.
    std::fs::remove_dir_all(kws_dir.join("kw.tuning.json")).unwrap();
    core.send(&CoreFrame::ConfigApply {
        device: "dot2-d".into(),
        core_node_id: "core-1".into(),
        data_epoch: 1,
        application_id: "app-kws-2".into(),
        operation_id: "op-kws-ok".into(),
        desired_revision: 2,
        block_generation: 2,
        block: ConfigBlock::Kws,
        schema_version: 1,
        block_hash: "hash-kws-2".into(),
        expected_gateway_boot_id: None,
        expected_state_revision: None,
        capability_fingerprint: None,
        values: json!({
            "keywords": "n i h ao @你好",
            "retreat_words": [],
            "intents_hash": "ih-2",
            "threshold": 0.3,
            "score": 2.5
        }),
    })
    .await;
    let ok = core
        .until(|f| matches!(f, GatewayFrame::ConfigReceipt { operation_id, .. } if operation_id == "op-kws-ok"))
        .await;
    let GatewayFrame::ConfigReceipt {
        status,
        active_values,
        ..
    } = ok
    else {
        unreachable!();
    };
    assert_eq!(status, ConfigReceiptStatus::Active);
    let expected_bid = active_values
        .as_ref()
        .and_then(|v| v["bundle_id"].as_str())
        .unwrap()
        .to_string();

    // Keyword frame emitted after sidecar restart must carry bundle_id & kws_generation.
    let kw = core
        .until(|f| {
            matches!(
                f,
                GatewayFrame::Keyword {
                    bundle_id: Some(_),
                    ..
                }
            )
        })
        .await;
    let GatewayFrame::Keyword {
        bundle_id,
        kws_generation,
        ..
    } = kw
    else {
        unreachable!();
    };
    assert_eq!(bundle_id.as_deref(), Some(expected_bid.as_str()));
    assert!(kws_generation.unwrap_or(0) >= 1);
}

#[tokio::test]
async fn acoustics_block_validates_applies_and_caps_volume() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes);
    let sd = state_dir("cfgv2-acoustics");
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: sd.clone(),
        ..Default::default()
    };
    spawn_firmware(cfg, Arc::new(hw));
    handshake_v2(&core).await;

    // Invalid acoustics fails validation.
    core.send(&CoreFrame::ConfigApply {
        device: "dot2-d".into(),
        core_node_id: "core-1".into(),
        data_epoch: 1,
        application_id: "app-ac-bad".into(),
        operation_id: "op-ac-bad".into(),
        desired_revision: 1,
        block_generation: 1,
        block: ConfigBlock::Acoustics,
        schema_version: 1,
        block_hash: "hash-ac-bad".into(),
        expected_gateway_boot_id: None,
        expected_state_revision: None,
        capability_fingerprint: None,
        values: json!({
            "volume_cap": 1.5,
            "mix_gain": 1.0,
            "onset_threshold_dbfs": -40.0
        }),
    })
    .await;
    let bad = core
        .until(|f| matches!(f, GatewayFrame::ConfigReceipt { operation_id, .. } if operation_id == "op-ac-bad"))
        .await;
    let GatewayFrame::ConfigReceipt {
        status, error_code, ..
    } = bad
    else {
        unreachable!();
    };
    assert_eq!(status, ConfigReceiptStatus::Failed);
    assert_eq!(error_code.as_deref(), Some("validation_failed"));

    // Valid acoustics sets volume_cap = 0.50.
    core.send(&CoreFrame::ConfigApply {
        device: "dot2-d".into(),
        core_node_id: "core-1".into(),
        data_epoch: 1,
        application_id: "app-ac-ok".into(),
        operation_id: "op-ac-ok".into(),
        desired_revision: 2,
        block_generation: 2,
        block: ConfigBlock::Acoustics,
        schema_version: 1,
        block_hash: "hash-ac-ok".into(),
        expected_gateway_boot_id: None,
        expected_state_revision: None,
        capability_fingerprint: None,
        values: json!({
            "volume_cap": 0.50,
            "mix_gain": 0.85,
            "onset_threshold_dbfs": -45.0,
            "mic_gain_offset_db": -1.0
        }),
    })
    .await;
    let ok = core
        .until(|f| matches!(f, GatewayFrame::ConfigReceipt { operation_id, .. } if operation_id == "op-ac-ok"))
        .await;
    let GatewayFrame::ConfigReceipt { status, .. } = ok else {
        unreachable!();
    };
    assert_eq!(status, ConfigReceiptStatus::Active);
    assert!(sd.join("acoustics_config.json").exists());

    // VolumeSet 1.0 is now capped at 0.50.
    core.send(&CoreFrame::VolumeSet {
        task_id: "vol-cap-check".into(),
        device: "dot2-d".into(),
        value: 1.0,
        unit: "linear".into(),
    })
    .await;
    let vr = core
        .until(|f| matches!(f, GatewayFrame::VolumeReceipt { task_id, .. } if task_id == "vol-cap-check"))
        .await;
    let GatewayFrame::VolumeReceipt { effective, .. } = vr else {
        unreachable!();
    };
    assert!(
        (effective - 0.5039).abs() < 0.02,
        "expected ~0.50, got {effective}"
    );
}

#[tokio::test]
async fn acoustics_mix_gain_scales_dac_pcm_and_failed_apply_preserves_prior() {
    use base64::Engine as _;

    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes.clone());
    let sd = state_dir("cfgv2-acoustics-mix-gain");
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: sd.clone(),
        ..Default::default()
    };
    spawn_firmware(cfg, Arc::new(hw));
    handshake_v2(&core).await;

    // Set volume to 0.80 so gain is well above zero and below clipping.
    core.send(&CoreFrame::VolumeSet {
        task_id: "vol-init".into(),
        device: "dot2-d".into(),
        value: 0.80,
        unit: "linear".into(),
    })
    .await;
    let _ = core
        .until(
            |f| matches!(f, GatewayFrame::VolumeReceipt { task_id, .. } if task_id == "vol-init"),
        )
        .await;

    let mut tone_bytes = Vec::with_capacity(4800 * 2);
    for _ in 0..4800 {
        tone_bytes.extend_from_slice(&8000i16.to_le_bytes());
    }
    let tone_b64 = base64::engine::general_purpose::STANDARD.encode(&tone_bytes);

    async fn play_and_peak(
        task_id: String,
        core: &FakeCore,
        probes: &Arc<Mutex<Probes>>,
        tone_b64: String,
    ) -> u16 {
        use home_mate_device_gateway::protocol::AudioMeta;
        probes.lock().unwrap().all_samples.clear();
        core.send(&CoreFrame::PlayBegin {
            task_id: task_id.clone(),
            device: "dot2-d".into(),
            audio: AudioMeta {
                format: "pcm_s16le".into(),
                sample_rate: 48_000,
                channels: 1,
            },
            channel: Some("voice".into()),
            text: None,
            content_ref: String::new(),
            barge_in: None,
        })
        .await;
        core.send(&CoreFrame::PlayChunk {
            task_id: task_id.clone(),
            seq: 0,
            audio_b64: tone_b64,
        })
        .await;
        core.send(&CoreFrame::PlayEnd {
            task_id: task_id.clone(),
        })
        .await;
        let _ = core
            .until(
                |f| matches!(f, GatewayFrame::PlayFinished { task_id: id, .. } if id == &task_id),
            )
            .await;
        probes
            .lock()
            .unwrap()
            .all_samples
            .iter()
            .map(|s| s.unsigned_abs())
            .max()
            .unwrap_or(0)
    }

    // 1. Apply mix_gain = 1.0, onset_threshold_dbfs = -45.0.
    core.send(&CoreFrame::ConfigApply {
        device: "dot2-d".into(),
        core_node_id: "core-1".into(),
        data_epoch: 1,
        application_id: "app-mg-1".into(),
        operation_id: "op-mg-1".into(),
        desired_revision: 1,
        block_generation: 1,
        block: ConfigBlock::Acoustics,
        schema_version: 1,
        block_hash: "hash-mg-1".into(),
        expected_gateway_boot_id: None,
        expected_state_revision: None,
        capability_fingerprint: None,
        values: json!({
            "volume_cap": 0.90,
            "mix_gain": 1.0,
            "onset_threshold_dbfs": -45.0,
            "mic_gain_offset_db": 0.0
        }),
    })
    .await;
    let r1 = core
        .until(|f| matches!(f, GatewayFrame::ConfigReceipt { operation_id, .. } if operation_id == "op-mg-1"))
        .await;
    assert!(matches!(
        r1,
        GatewayFrame::ConfigReceipt {
            status: ConfigReceiptStatus::Active,
            ..
        }
    ));
    let peak_1_0 = play_and_peak("tone-1".into(), &core, &probes, tone_b64.clone()).await;
    assert!(
        peak_1_0 > 500,
        "expected audible DAC samples at mix_gain=1.0, got {peak_1_0}"
    );

    // 2. Apply mix_gain = 0.25, onset_threshold_dbfs = -30.0 and verify DAC amplitude scales ~4x down.
    core.send(&CoreFrame::ConfigApply {
        device: "dot2-d".into(),
        core_node_id: "core-1".into(),
        data_epoch: 1,
        application_id: "app-mg-2".into(),
        operation_id: "op-mg-2".into(),
        desired_revision: 2,
        block_generation: 2,
        block: ConfigBlock::Acoustics,
        schema_version: 1,
        block_hash: "hash-mg-2".into(),
        expected_gateway_boot_id: None,
        expected_state_revision: None,
        capability_fingerprint: None,
        values: json!({
            "volume_cap": 0.90,
            "mix_gain": 0.25,
            "onset_threshold_dbfs": -30.0,
            "mic_gain_offset_db": 1.5
        }),
    })
    .await;
    let r2 = core
        .until(|f| matches!(f, GatewayFrame::ConfigReceipt { operation_id, .. } if operation_id == "op-mg-2"))
        .await;
    assert!(matches!(
        r2,
        GatewayFrame::ConfigReceipt {
            status: ConfigReceiptStatus::Active,
            ..
        }
    ));
    let peak_0_25 = play_and_peak("tone-2".into(), &core, &probes, tone_b64.clone()).await;
    assert!(
        peak_0_25 > 0 && peak_1_0 >= peak_0_25 * 3,
        "mix_gain=0.25 must attenuate DAC PCM vs mix_gain=1.0 (peak_1_0={peak_1_0}, peak_0_25={peak_0_25})"
    );

    // 3. Invalid mix_gain / onset_threshold_dbfs must fail and preserve prior active acoustics on disk and in DAC.
    core.send(&CoreFrame::ConfigApply {
        device: "dot2-d".into(),
        core_node_id: "core-1".into(),
        data_epoch: 1,
        application_id: "app-mg-bad".into(),
        operation_id: "op-mg-bad".into(),
        desired_revision: 3,
        block_generation: 3,
        block: ConfigBlock::Acoustics,
        schema_version: 1,
        block_hash: "hash-mg-bad".into(),
        expected_gateway_boot_id: None,
        expected_state_revision: None,
        capability_fingerprint: None,
        values: json!({
            "volume_cap": 0.90,
            "mix_gain": 3.5,
            "onset_threshold_dbfs": 10.0,
            "mic_gain_offset_db": 0.0
        }),
    })
    .await;
    let r_bad = core
        .until(|f| matches!(f, GatewayFrame::ConfigReceipt { operation_id, .. } if operation_id == "op-mg-bad"))
        .await;
    assert!(matches!(
        r_bad,
        GatewayFrame::ConfigReceipt {
            status: ConfigReceiptStatus::Failed,
            error_code: Some(ref e),
            ..
        } if e == "validation_failed"
    ));
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sd.join("acoustics_config.json")).unwrap())
            .unwrap();
    assert!((saved["mix_gain"].as_f64().unwrap() - 0.25).abs() < 1e-3);
    assert!((saved["onset_threshold_dbfs"].as_f64().unwrap() - (-30.0)).abs() < 1e-3);

    // 4. Rollback/restore mix_gain = 1.0 and onset_threshold_dbfs = -42.0 restores DAC amplitude.
    core.send(&CoreFrame::ConfigApply {
        device: "dot2-d".into(),
        core_node_id: "core-1".into(),
        data_epoch: 1,
        application_id: "app-mg-restore".into(),
        operation_id: "op-mg-restore".into(),
        desired_revision: 4,
        block_generation: 4,
        block: ConfigBlock::Acoustics,
        schema_version: 1,
        block_hash: "hash-mg-restore".into(),
        expected_gateway_boot_id: None,
        expected_state_revision: None,
        capability_fingerprint: None,
        values: json!({
            "volume_cap": 0.90,
            "mix_gain": 1.0,
            "onset_threshold_dbfs": -42.0,
            "mic_gain_offset_db": 0.0
        }),
    })
    .await;
    let r4 = core
        .until(|f| matches!(f, GatewayFrame::ConfigReceipt { operation_id, .. } if operation_id == "op-mg-restore"))
        .await;
    assert!(matches!(
        r4,
        GatewayFrame::ConfigReceipt {
            status: ConfigReceiptStatus::Active,
            ..
        }
    ));
    let peak_restored = play_and_peak("tone-3".into(), &core, &probes, tone_b64).await;
    assert!(
        peak_restored >= peak_0_25 * 3,
        "restored mix_gain=1.0 must restore DAC amplitude (restored={peak_restored}, peak_0_25={peak_0_25})"
    );
}
