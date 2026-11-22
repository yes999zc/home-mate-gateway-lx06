//! Playback stall watchdog and the fault exits it owes: the receipts, the
//! test-only stay-up mode, and the production nonzero exit.

use crate::support::*;
use home_mate_device_gateway::firmware::FirmwareConfig;
use home_mate_device_gateway::protocol::{GatewayFrame, PROTOCOL_MIN_VERSION, PROTOCOL_VERSION};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use home_mate_device_gateway::protocol::CoreFrame as CF;

/// The 2026-11-13 hang shape: the DAC write stopped making progress while the
/// process stayed up and the core waited for a `play_finished` that could
/// never come. The watchdog names the failure (`play_watchdog`, with the
/// numbers) and does NOT fake a finish.
///
/// M4 changed the tail of this contract: production exits nonzero after a
/// bounded drain so the supervisor can restart the codec (`an_unrecoverable_`
/// `audio_fault_exits_nonzero` pins the exit code in a subprocess). This test
/// keeps the exit off to assert the receipts in-process — the "link survives"
/// part is now a TEST-ONLY mode, not the shipped behavior.
#[tokio::test]
async fn a_frozen_data_write_trips_the_play_watchdog_and_the_link_survives() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (mut hw, _btn) = FakeHardware::new(probes.clone());
    // Let every idle-silence period through; only the actual stream data
    // parks. No assumption about how many idle periods precede the chunk.
    hw.park_audio_writes = true;
    let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
    hw.write_gate.lock().unwrap().replace(gate_rx);
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: state_dir("play-watchdog"),
        loudness: false,
        play_watchdog_ms: 1_200,
        exit_on_audio_fault: false,
        ..Default::default()
    };
    spawn_firmware(cfg, Arc::new(hw));
    let _hello = core.recv().await;
    core.send(&CF::HelloAck {
        protocol: PROTOCOL_VERSION,
        min_protocol: PROTOCOL_MIN_VERSION,
        core_version: "test-core".into(),
        supported_versions: vec![],
        capabilities: vec![],
        generation: 1,
    })
    .await;

    core.send(&begin_frame("wd", 16_000)).await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "wd" && stage == "accepted"))
        .await;
    // Enough for one complete 48k period, then the write parks forever.
    core.send(&chunk_frame("wd", 0, &pcm16k_ramp(8_000))).await;

    let failed = core
        .until(|f| {
            matches!(f, GatewayFrame::PlayReceipt { ref task_id, ref stage, ref detail, .. }
                if task_id == "wd" && stage == "failed"
                    && detail.as_deref().is_some_and(|d| d.starts_with("play_watchdog")
                        && d.contains("played_ms=") && d.contains("pushed_ms=")))
        })
        .await;
    assert!(matches!(failed, GatewayFrame::PlayReceipt { .. }));
    // A stall is not a finish: the core must not advance a program cursor.
    core.no_finish_for("wd", Duration::from_millis(300)).await;

    // The firmware is up and the link still answers.
    let _ = gate_tx.send(());
    core.send(&CF::VolumeSet {
        task_id: "ping".into(),
        device: "dot2-d".into(),
        value: 0.5,
        unit: "linear".into(),
    })
    .await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::VolumeReceipt { task_id, .. } if task_id == "ping"))
        .await;
}

/// The counterpart: a live stream waiting for its NEXT chunk shows no DAC
/// progress either, and that is normal synthesis latency, not a stall. The
/// watchdog must stay silent (a 600 ms bound with 1.5 s of starvation).
#[tokio::test]
async fn a_starved_stream_is_not_a_play_watchdog_stall() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes.clone());
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: state_dir("play-starve"),
        loudness: false,
        play_watchdog_ms: 600,
        ..Default::default()
    };
    spawn_firmware(cfg, Arc::new(hw));
    let _hello = core.recv().await;
    core.send(&CF::HelloAck {
        protocol: PROTOCOL_VERSION,
        min_protocol: PROTOCOL_MIN_VERSION,
        core_version: "test-core".into(),
        supported_versions: vec![],
        capabilities: vec![],
        generation: 1,
    })
    .await;

    core.send(&begin_frame("starve", 16_000)).await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "starve" && stage == "accepted"))
        .await;
    // Less than one 48k period: the thread waits for more, writing nothing.
    core.send(&chunk_frame("starve", 0, &pcm16k_ramp(100)))
        .await;

    let deadline = tokio::time::Instant::now() + Duration::from_millis(1_500);
    loop {
        match tokio::time::timeout_at(deadline, core.frames.lock().unwrap().recv()).await {
            Ok(Some(GatewayFrame::PlayReceipt { stage, detail, .. })) if stage == "failed" => {
                panic!("a starve is not a stall: {detail:?}")
            }
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => break,
        }
    }
}

/// R0 debt closed: after a data-write failure the playback thread is gone, so
/// no receipt can leave it any more. The core's Stop must still be answered —
/// the failure handler clears the slot, and the Stop then takes the
/// immediate-receipt path instead of parking a `pending_stop` forever.
#[tokio::test]
async fn a_stop_after_a_write_failure_still_gets_a_receipt() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (mut hw, _btn) = FakeHardware::new(probes.clone());
    hw.fail_audio_writes = true; // silence clock keeps working, data dies
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: state_dir("write-fail"),
        loudness: false,
        // The write-failure path is now an unrecoverable audio fault; keep
        // the process alive so the Stop receipt below is observed here (the
        // exit code itself is pinned by the subprocess test).
        exit_on_audio_fault: false,
        ..Default::default()
    };
    spawn_firmware(cfg, Arc::new(hw));
    let _hello = core.recv().await;
    core.send(&CF::HelloAck {
        protocol: PROTOCOL_VERSION,
        min_protocol: PROTOCOL_MIN_VERSION,
        core_version: "test-core".into(),
        supported_versions: vec![],
        capabilities: vec![],
        generation: 1,
    })
    .await;

    core.send(&play_frame("wf", &wav_48k(4_800, 8_000), Some("voice")))
        .await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "wf" && stage == "failed"))
        .await;

    core.send(&CF::Stop {
        task_id: "wf".into(),
        flush: true,
    })
    .await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::StopReceipt { task_id, .. } if task_id == "wf"))
        .await;
}

// --- 2026-11-13 M4: the first-write blind spot and the fault exit ------------

/// M4 regression: the loop-top live mask is one command drain stale. A play
/// whose VERY FIRST period parks in `playback_write` used to publish mask 0,
/// so the watchdog returned "nothing live" forever — the wedged play was
/// never named. The write site now records the live set it blocks.
///
/// The fake parks only writes that carry real audio, so the idle silence
/// clock keeps flowing and the first data period is exactly the one that
/// wedges (a gate on every write would park the clock before the play).
#[tokio::test]
async fn the_first_period_of_a_play_trips_the_watchdog_too() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (mut hw, _btn) = FakeHardware::new(probes.clone());
    let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
    hw.write_gate.lock().unwrap().replace(gate_rx);
    hw.park_audio_writes = true;
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: state_dir("first-write"),
        loudness: false,
        play_watchdog_ms: 800,
        exit_on_audio_fault: false,
        ..Default::default()
    };
    spawn_firmware(cfg, Arc::new(hw));
    let _hello = core.recv().await;
    core.send(&CF::HelloAck {
        protocol: PROTOCOL_VERSION,
        min_protocol: PROTOCOL_MIN_VERSION,
        core_version: "test-core".into(),
        supported_versions: vec![],
        capabilities: vec![],
        generation: 1,
    })
    .await;

    // A whole WAV in ONE frame: the samples are queued and the data write in
    // that same iteration is the first write of this play.
    core.send(&play_frame("first", &wav_48k(4_800, 8_000), Some("voice")))
        .await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "first" && stage == "sent"))
        .await;

    let failed = core
        .until(|f| {
            matches!(f, GatewayFrame::PlayReceipt { ref task_id, ref stage, ref detail, .. }
                if task_id == "first" && stage == "failed"
                    && detail.as_deref().is_some_and(|d| d.starts_with("play_watchdog")))
        })
        .await;
    assert!(matches!(failed, GatewayFrame::PlayReceipt { .. }));
    // A stall is not a finish (the core must not advance a cursor).
    core.no_finish_for("first", Duration::from_millis(200))
        .await;
    // Release the parked write so the thread is not holding the gate at drop.
    let _ = gate_tx.send(());
}

/// M4: an unrecoverable audio fault must END the process with a nonzero code
/// — the old contract left a deaf/mute box that still answered on the wire.
///
/// The child re-runs THIS test binary filtered to `audio_fault_child_case`;
/// the firmware's bounded exit kills it and the parent asserts the code plus
/// the `failed` receipt the child recorded BEFORE dying (libtest's own output
/// capture dies with the process, so the child appends each north frame to a
/// file — the file survives). The restart itself is the supervisor's job and
/// is proven on the device; a fresh instance playing again is asserted below.
#[tokio::test]
async fn an_unrecoverable_audio_fault_exits_nonzero() {
    for case in ["write-fail", "stall"] {
        let log =
            std::env::temp_dir().join(format!("hm-fw-fault-{}-{case}.log", std::process::id()));
        let _ = std::fs::remove_file(&log);
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "watchdog::audio_fault_child_case",
                "--test-threads=1",
            ])
            .env(CHILD_CASE_ENV, case)
            .env(CHILD_LOG_ENV, &log)
            .output()
            .expect("spawn the child test process");
        let seen = std::fs::read_to_string(&log).unwrap_or_default();
        assert_eq!(
            out.status.code(),
            Some(75),
            "case {case}: the firmware must exit 75 (EX_TEMPFAIL) for the supervisor \
             to restart it; status={:?} frames-seen={seen} stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            seen.contains("stage: \"failed\""),
            "case {case}: the owed `failed` receipt must reach the core BEFORE the exit: {seen}"
        );
        let _ = std::fs::remove_file(&log);
    }
}

const CHILD_CASE_ENV: &str = "HM_FW_FAULT_CHILD";
const CHILD_LOG_ENV: &str = "HM_FW_FAULT_CHILD_LOG";

/// Child half of `an_unrecoverable_audio_fault_exits_nonzero`. With no env
/// var it is a no-op so the parent's normal run passes.
#[tokio::test]
async fn audio_fault_child_case() {
    let Ok(case) = std::env::var(CHILD_CASE_ENV) else {
        return;
    };
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (mut hw, _btn) = FakeHardware::new(probes.clone());
    match case.as_str() {
        "write-fail" => hw.fail_audio_writes = true,
        // A parked data write: no ioctl error, just no progress.
        "stall" => {
            let (_gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
            hw.write_gate.lock().unwrap().replace(gate_rx);
            hw.park_audio_writes = true;
            // Keep the sender alive for the life of the child (a dropped
            // sender would make recv() return and un-park the write).
            std::mem::forget(_gate_tx);
        }
        other => panic!("unknown child case {other}"),
    }
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: state_dir(&format!("fault-child-{case}")),
        loudness: false,
        play_watchdog_ms: 800,
        // Production default: on. This is the path that exits.
        ..Default::default()
    };
    spawn_firmware(cfg, Arc::new(hw));
    let _hello = core.recv().await;
    core.send(&CF::HelloAck {
        protocol: PROTOCOL_VERSION,
        min_protocol: PROTOCOL_MIN_VERSION,
        core_version: "test-core".into(),
        supported_versions: vec![],
        capabilities: vec![],
        generation: 1,
    })
    .await;
    core.send(&play_frame("fault", &wav_48k(4_800, 8_000), Some("voice")))
        .await;

    // Record every north frame to the parent's file (libtest's capture buffer
    // dies with the exit; a raw file write does not).
    let mut log = {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(std::env::var(CHILD_LOG_ENV).expect("child log path"))
            .expect("open child log");
        f.write_all(b"child started\n").unwrap();
        f
    };
    loop {
        let f = core.recv().await;
        {
            use std::io::Write;
            let _ = writeln!(log, "{f:?}");
        }
        if let GatewayFrame::PlayReceipt { stage, .. } = &f {
            if stage == "failed" {
                // The firmware exits AUDIO_FAULT_DRAIN_MS after latching the
                // fault. Park here and let it do so: touching the socket now
                // would make this TEST panic (link died) and the harness
                // would exit 101 first, masking the code under test.
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        }
    }
}

/// M4 recovery boundary: the remedy for a dead DAC path is a fresh start, so
/// a NEW firmware instance (what the supervisor launches) must play a whole
/// asset to `play_finished` with no leftover state. The literal supervisor
/// restart is the on-device acceptance item.
#[tokio::test]
async fn a_fresh_instance_plays_after_a_fault() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes.clone());
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: state_dir("after-fault"),
        loudness: false,
        ..Default::default()
    };
    spawn_firmware(cfg, Arc::new(hw));
    let _hello = core.recv().await;
    core.send(&CF::HelloAck {
        protocol: PROTOCOL_VERSION,
        min_protocol: PROTOCOL_MIN_VERSION,
        core_version: "test-core".into(),
        supported_versions: vec![],
        capabilities: vec![],
        generation: 1,
    })
    .await;
    core.send(&play_frame("again", &wav_48k(4_800, 8_000), Some("voice")))
        .await;
    let mut saw_sent = false;
    let mut saw_finished = false;
    while !(saw_sent && saw_finished) {
        let f = core
            .until(|f| {
                matches!(
                    f,
                    GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "again" && stage == "sent"
                ) || matches!(f, GatewayFrame::PlayFinished { task_id, .. } if task_id == "again")
            })
            .await;
        match f {
            GatewayFrame::PlayReceipt { .. } => saw_sent = true,
            GatewayFrame::PlayFinished { .. } => saw_finished = true,
            _ => {}
        }
    }
}

const PANIC_CHILD_ENV: &str = "HM_FW_PLAYBACK_PANIC_CHILD";

/// #19: if the playback thread panics (even when idle, `want_write == false`),
/// `PlaybackExitGuard` sets `AudioFault` and the firmware exits 75 so the
/// supervisor restarts the process.
#[tokio::test]
async fn playback_thread_panic_exits_nonzero() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "watchdog::playback_panic_child_case",
            "--test-threads=1",
        ])
        .env(PANIC_CHILD_ENV, "1")
        .output()
        .expect("spawn child test process");
    assert_eq!(
        out.status.code(),
        Some(75),
        "playback thread panic must exit 75; status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn playback_panic_child_case() {
    if std::env::var(PANIC_CHILD_ENV).is_err() {
        return;
    }
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (mut hw, _btn) = FakeHardware::new(probes);
    hw.panic_playback = true;
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: state_dir("playback-panic-child"),
        loudness: false,
        ..Default::default()
    };
    spawn_firmware(cfg, Arc::new(hw));
    let _hello = core.recv().await;
    core.send(&CF::HelloAck {
        protocol: PROTOCOL_VERSION,
        min_protocol: PROTOCOL_MIN_VERSION,
        core_version: "test-core".into(),
        supported_versions: vec![],
        capabilities: vec![],
        generation: 1,
    })
    .await;
    // Wait until the process exits 75.
    tokio::time::sleep(Duration::from_secs(10)).await;
}

// --- 2026-11-13 M6a: consumed vs written, honest flush, link teardown --------
