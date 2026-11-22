//! Playback-time KWS without a trustworthy correlation source is guarded.

use crate::support::*;
use home_mate_device_gateway::firmware::FirmwareConfig;
use home_mate_device_gateway::protocol::{
    CoreFrame, GatewayFrame, PROTOCOL_MIN_VERSION, PROTOCOL_VERSION,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The sidecar reports a real keyword while playback is live, with semantic
/// barge-in/AEC disabled. The report must stay guarded and cannot locally
/// flush; Core remains able to issue a named Stop. Also verifies raw KWS RMS
/// despite a nonzero stored microphone calibration offset.
#[tokio::test]
async fn an_unverified_playback_keyword_cannot_locally_flush() {
    use std::os::unix::fs::PermissionsExt;

    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes.clone());
    let sd = state_dir("local-exit");
    let kws_dir = std::env::temp_dir().join(format!("hm-fw-kws-exit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&kws_dir);
    std::fs::create_dir_all(kws_dir.join("model")).unwrap();
    std::fs::write(
        kws_dir.join("model/tokens.txt"),
        "n 10\nǐ 11\nh 12\nǎo 13\nt 14\níng 15\n",
    )
    .unwrap();
    let script = kws_dir.join("run.sh");
    // Ready → give the core time to deploy and start a play → consume one
    // fed block (4 × 20 ms slices = 2560 bytes) → report the retreat word.
    std::fs::write(
        &script,
        "#!/bin/sh\n\
         echo \"{\\\"event\\\":\\\"ready\\\",\\\"load_ms\\\":1,\\\"threshold\\\":${KWS_THRESHOLD:-0},\\\"score\\\":${KWS_SCORE:-0}}\"\n\
         sleep 1\n\
         dd bs=2560 count=1 of=/dev/null 2>/dev/null\n\
         echo '{\"event\":\"keyword\",\"keyword\":\"停止播放\",\"fed_ms\":80,\"lag_ms\":0}'\n\
         exec sleep 30\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: sd,
        loudness: false,
        kws: true,
        kws_dir: kws_dir.clone(),
        ..Default::default()
    };
    let mut cfg = cfg;
    cfg.profile.mic_gain_offset_db = 6.0;
    spawn_firmware(cfg, Arc::new(hw));

    let hello = core.recv().await;
    match &hello {
        GatewayFrame::Hello {
            aec_tail_ms,
            profile_version,
            ..
        } => {
            assert_eq!(*aec_tail_ms, None); // test config has AEC disabled
            assert_eq!(
                profile_version.as_deref(),
                Some(home_mate_device_gateway::protocol::PROFILE_VERSION)
            );
        }
        other => panic!("expected hello, got {other:?}"),
    }
    core.send(&CoreFrame::HelloAck {
        protocol: PROTOCOL_VERSION,
        min_protocol: PROTOCOL_MIN_VERSION,
        core_version: "test-core".into(),
        supported_versions: vec![],
        capabilities: vec![],
        generation: 1,
    })
    .await;
    // The spotter hears the same post-AEC block the uplink carries; with the
    // microphone OFF nothing is fed (privacy, not a defect).
    core.send(&CoreFrame::MicSession {
        device: "dot2-d".into(),
        action: "start".into(),
        mode: Some("continuous".into()),
    })
    .await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::MicReport { active: true, .. }))
        .await;

    // The deploy names the exit word and the map version; the device reports
    // what it actually armed.
    core.send(&CoreFrame::KwsDeploy {
        device: "dot2-d".into(),
        keywords: "n ǐ h ǎo @你好\nt íng @停止播放\n".into(),
        retreat_words: vec!["停止播放".into()],
        intents_hash: "map-1".into(),
        threshold: Some(0.30),
        score: Some(2.5),
    })
    .await;
    let receipt = core
        .until(|f| matches!(f, GatewayFrame::KwsDeployReceipt { .. }))
        .await;
    let GatewayFrame::KwsDeployReceipt {
        accepted,
        local_exit,
        intents_hash,
        threshold,
        score,
        detail,
        ..
    } = receipt
    else {
        unreachable!()
    };
    assert!(accepted, "{detail}");
    assert_eq!(local_exit, 1, "one retreat word armed: {detail}");
    assert_eq!(
        intents_hash, "map-1",
        "the meaning's version is echoed back"
    );
    // Contract v2 #4: the sensitivity rides the deploy and is echoed back.
    assert!(
        (threshold - 0.30).abs() < 1e-6,
        "threshold echo {threshold}"
    );
    assert!((score - 2.5).abs() < 1e-6, "score echo {score}");
    let tuning: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(kws_dir.join("kw.tuning.json")).expect("kw.tuning.json"),
    )
    .unwrap();
    assert!((tuning["threshold"].as_f64().unwrap() - 0.30).abs() < 1e-6);
    assert!((tuning["score"].as_f64().unwrap() - 2.5).abs() < 1e-6);
    let exit_doc: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(kws_dir.join("kw.exit.json")).expect("kw.exit.json"),
    )
    .unwrap();
    assert_eq!(exit_doc["words"], serde_json::json!(["停止播放"]));

    // The armed count is also a status fact (survives a core restart): the
    // spotter's first report after the deploy's restart must carry it.
    let report = core
        .until(|f| matches!(f, GatewayFrame::KwsReport { state, .. } if state == "running"))
        .await;
    let GatewayFrame::KwsReport {
        local_exit: reported,
        threshold: applied_t,
        score: applied_s,
        ..
    } = report
    else {
        unreachable!()
    };
    assert_eq!(reported, 1, "the device must report what it armed");
    // The launcher received the sensitivity as env and the model echoed it in
    // its ready line: this is the APPLIED fact, not what we asked for.
    assert!(
        (applied_t - 0.30).abs() < 1e-6,
        "applied threshold {applied_t}"
    );
    assert!((applied_s - 2.5).abs() < 1e-6, "applied score {applied_s}");

    // A streamed answer: live, starved (waiting for its next chunk), audible.
    core.send(&begin_frame("A1", 16_000)).await;
    core.send(&chunk_frame("A1", 0, &pcm16k_ramp(1280))).await;

    let keyword = core
        .until(|f| matches!(f, GatewayFrame::Keyword { .. }))
        .await;
    let GatewayFrame::Keyword {
        keyword,
        self_heard,
        rms_dbfs,
        ..
    } = keyword
    else {
        unreachable!()
    };
    assert_eq!(keyword, "停止播放");
    assert!(
        self_heard,
        "absent correlation source must fail closed during playback"
    );
    // Fake capture emits mic samples 3000..4279 in every 80 ms block.
    let raw: Vec<i16> = (3000..4280).collect();
    let expected = home_mate_device_gateway::firmware::block_dbfs(&raw);
    assert!(
        (rms_dbfs.unwrap() - expected).abs() < 0.01,
        "raw RMS must not gain the stored +6 dB offset"
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        probes.lock().unwrap().flushed,
        0,
        "unverified keyword must not cut the DAC"
    );

    // A Core-authorized named Stop still reaches the hardware and reports truth.
    core.send(&CoreFrame::Stop {
        task_id: "A1".into(),
        flush: true,
    })
    .await;
    let stop = core
        .until(|f| matches!(f, GatewayFrame::StopReceipt { .. }))
        .await;
    let GatewayFrame::StopReceipt {
        task_id,
        stage,
        played_ms,
        ..
    } = stop
    else {
        unreachable!()
    };
    assert_eq!(task_id, "A1");
    assert_eq!(stage, "device_confirmed");
    assert!(
        played_ms.is_some(),
        "Core's actual flush reports played progress"
    );
    assert_eq!(probes.lock().unwrap().flushed, 1);
    let _ = std::fs::remove_dir_all(&kws_dir);
}
