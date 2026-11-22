//! v0.3 streamed playback (D/T3): begin/chunk/end, equivalence with a whole
//! WAV, the failure modes a stream can hit, and the end fade.

use crate::support::*;
use home_mate_device_gateway::firmware::FirmwareConfig;
use home_mate_device_gateway::protocol::{
    CoreFrame, GatewayFrame, PROTOCOL_MIN_VERSION, PROTOCOL_VERSION,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use home_mate_device_gateway::protocol::CoreFrame as CF;

/// v0.3 §5/§7-①: streaming the same source in odd chunks reproduces the
/// whole-WAV play sample for sample (flat loudness chain, unity volume).
#[tokio::test]
async fn streamed_play_matches_whole_play_sample_for_sample() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes.clone());
    let sd = state_dir("stream-eq");
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: sd,
        loudness: false,
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
    // Unity gain so output samples equal the resampled source.
    core.send(&CF::VolumeSet {
        task_id: "v0".into(),
        device: "dot2-d".into(),
        value: 1.0,
        unit: "linear".into(),
    })
    .await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::VolumeReceipt { task_id, .. } if task_id == "v0"))
        .await;

    let pcm = pcm16k_ramp(8000); // 0.5s at 16k -> 24000 at 48k

    // Whole-WAV reference run.
    core.send(&play_frame("whole", &pcm_to_wav_16k(&pcm), None))
        .await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::PlayFinished { task_id, .. } if task_id == "whole"))
        .await;
    let whole: Vec<i16> = {
        let mut p = probes.lock().unwrap();
        std::mem::take(&mut p.all_samples)
    };

    // Streamed run: odd-size chunks exercise the cross-chunk resampler.
    core.send(&begin_frame("str", 16_000)).await;
    let _ = core.until(|f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "str" && stage == "accepted")).await;
    for (seq, chunk) in pcm.chunks(321).enumerate() {
        core.send(&chunk_frame("str", seq as u64, chunk)).await;
    }
    let streaming = core.until(|f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "str" && stage == "streaming")).await;
    assert!(
        matches!(streaming, GatewayFrame::PlayReceipt { .. }),
        "streaming receipt"
    );
    core.send(&CF::PlayEnd {
        task_id: "str".into(),
    })
    .await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::PlayFinished { task_id, .. } if task_id == "str"))
        .await;
    let streamed_raw: Vec<i16> = {
        let mut p = probes.lock().unwrap();
        std::mem::take(&mut p.all_samples)
    };

    let a = trim_zero_edges(&whole);
    let b = trim_zero_edges(&streamed_raw);
    assert_eq!(
        a.len(),
        b.len(),
        "length differs: whole {} vs streamed {}",
        a.len(),
        b.len()
    );
    let diffs: Vec<usize> = (0..a.len()).filter(|&i| a[i] != b[i]).collect();
    assert!(
        diffs.is_empty(),
        "sample mismatch at {} of {}: first {:?} vs {:?}",
        diffs.len(),
        a.len(),
        a.get(diffs[0]).copied(),
        b.get(diffs[0]).copied()
    );
}

/// v0.3 §3/§7-②: stop mid-stream flushes, receipts honestly, and later
/// stale chunks are ignored (the receipt chain already closed the task).
#[tokio::test]
async fn mid_stream_stop_flushes_and_receipts() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes.clone());
    let sd = state_dir("stream-stop");
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: sd,
        loudness: false,
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

    let pcm = pcm16k_ramp(1600);
    core.send(&begin_frame("s1", 16_000)).await;
    let _ = core.until(|f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "s1" && stage == "accepted")).await;
    core.send(&chunk_frame("s1", 0, &pcm)).await;
    core.send(&CF::Stop {
        task_id: "s1".into(),
        flush: true,
    })
    .await;
    let stop_rc = core
        .until(|f| matches!(f, GatewayFrame::StopReceipt { task_id, .. } if task_id == "s1"))
        .await;
    assert!(
        matches!(stop_rc, GatewayFrame::StopReceipt { stage, .. } if stage == "device_confirmed")
    );
    // A stop is not a finish: nothing may claim s1 ended on its own.
    core.no_finish_for("s1", Duration::from_millis(400)).await;

    // Stale chunk after stop: ignored, link alive.
    core.send(&chunk_frame("s1", 1, &pcm)).await;
    core.send(&CF::VolumeSet {
        task_id: "alive".into(),
        device: "dot2-d".into(),
        value: 0.5,
        unit: "linear".into(),
    })
    .await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::VolumeReceipt { task_id, .. } if task_id == "alive"))
        .await;
}

/// v0.3 §1/§7-③: a seq gap is a poisoned stream — the link drops.
#[tokio::test]
async fn stream_seq_gap_disconnects() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes.clone());
    let sd = state_dir("stream-seq");
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: sd,
        loudness: false,
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

    let pcm = pcm16k_ramp(320);
    core.send(&begin_frame("p", 16_000)).await;
    let _ = core.until(|f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "p" && stage == "accepted")).await;
    core.send(&chunk_frame("p", 0, &pcm)).await;
    // Poison: skip seq 1.
    core.send(&chunk_frame("p", 2, &pcm)).await;

    // The frames channel must close (reader EOF) within the window.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let got = tokio::time::timeout_at(deadline, core.frames.lock().unwrap().recv())
            .await
            .expect("link did not drop after seq violation");
        if got.is_none() {
            break; // all senders gone: disconnected
        }
    }
}

/// v0.3 §4/§7-④: exceeding the ahead-of-playback cap fails THAT task with
/// `stream_overflow` and keeps the link alive.
#[tokio::test]
async fn stream_overflow_fails_task_not_link() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes.clone());
    // Freeze playback progress: writes park until released.
    let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
    hw.write_gate.lock().unwrap().replace(gate_rx);
    let sd = state_dir("stream-ovf");
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: sd,
        loudness: false,
        stream_ahead: 4_800, // 100ms: tiny cap for the test
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

    let pcm = pcm16k_ramp(8000); // 0.5s@16k = 24k@48k >> 100ms cap
    core.send(&begin_frame("ov", 16_000)).await;
    let _ = core.until(|f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "ov" && stage == "accepted")).await;
    for (seq, chunk) in pcm.chunks(1600).enumerate() {
        core.send(&chunk_frame("ov", seq as u64, chunk)).await;
    }
    // The failure must name its numbers (R1d): a bare "stream_overflow"
    // cannot tell "synthesis outran playback" from "playback stopped".
    let failed = core
        .until(|f| {
            matches!(f, GatewayFrame::PlayReceipt { task_id, ref stage, ref detail, .. }
        if task_id == "ov" && stage == "failed"
            && detail.as_deref().is_some_and(|d| d.starts_with("stream_overflow pushed_ms=")
                && d.contains("ahead_ms=") && d.contains("cap_ms=")))
        })
        .await;
    assert!(matches!(failed, GatewayFrame::PlayReceipt { .. }));

    // Link survives: release the gate and ping with a volume round-trip.
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

/// v0.3 §7-⑤: chunks for a task that never began are ignored (no crash,
/// no output) — the begin gate is the core's capability check.
#[tokio::test]
async fn chunk_for_unknown_stream_ignored() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes.clone());
    let sd = state_dir("stream-unk");
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: sd,
        loudness: false,
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

    let pcm = pcm16k_ramp(320);
    core.send(&chunk_frame("ghost", 0, &pcm)).await;
    core.send(&CF::PlayEnd {
        task_id: "ghost".into(),
    })
    .await;
    core.send(&CF::VolumeSet {
        task_id: "alive".into(),
        device: "dot2-d".into(),
        value: 0.5,
        unit: "linear".into(),
    })
    .await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::VolumeReceipt { task_id, .. } if task_id == "alive"))
        .await;
    // And nothing was played under that name.
    let p = probes.lock().unwrap();
    assert!(
        !p.first_samples.iter().any(|&s| s != 0),
        "unexpected non-silence output"
    );
}

/// v0.3.1 edge fades (user report 2026-11-10: a power-cut pop after every
/// answer and the follow tone): the first period of a stream ramps in from
/// zero and the LAST ramps out — a non-zero sample stepping to the silent
/// wire is the tick RUI's dot2 plug documented and faded the same way.
#[tokio::test]
async fn stream_tail_fades_to_zero_no_click() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes.clone());
    let sd = state_dir("fade-tail");
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: sd,
        loudness: false,
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
    core.send(&CF::VolumeSet {
        task_id: "v".into(),
        device: "dot2-d".into(),
        value: 1.0,
        unit: "linear".into(),
    })
    .await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::VolumeReceipt { task_id, .. } if task_id == "v"))
        .await;

    // Square wave ending LOUD on a non-zero sample: without the fade the
    // wire steps straight to silence (the reported pop).
    let wav = wav_48k(3000, 12000); // 3000 samples = odd count → ends on -amp
    core.send(&play_frame("fade", &wav, None)).await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::PlayFinished { task_id, .. } if task_id == "fade"))
        .await;
    let all: Vec<i16> = {
        let mut p = probes.lock().unwrap();
        std::mem::take(&mut p.all_samples)
    };
    let data = trim_zero_edges(&all);
    assert!(data.len() > 2500, "played samples: {}", data.len());
    // First ramp: the very first emitted sample starts from zero.
    assert_eq!(
        all.first(),
        Some(&0),
        "fade-in must start the stream at zero"
    );
    // Tail ramp: the last sample IS zero and the magnitude decreases
    // monotonically over the final ramp (a step-to-silence would keep the
    // square amplitude until the very end).
    // After trimming, the last NONZERO sample is deep inside the ramp —
    // near zero, never near full scale.
    let last_mag = data.last().map(|s| s.abs()).unwrap_or(0);
    assert!(
        last_mag < 120,
        "tail must fade out (last nonzero {last_mag})"
    );
    let n = data.len();
    let mags: Vec<i32> = data[n - 200..].iter().map(|s| (*s as i32).abs()).collect();
    let stepped_up = mags.windows(2).any(|w| w[1] > w[0] + 60);
    assert!(
        !stepped_up,
        "tail must ramp down, got rising magnitudes: {mags:?}"
    );
    assert!(*mags.first().unwrap() > 500, "ramp starts loud: {mags:?}");
}

/// B-M3 §3.3 finding (2026-11-12): the end fade does NOT depend on Eos timing
/// in the live path. The 16 kHz stream goes through StreamResampler, which
/// DEFERS its tail until PlayEnd (`finish()`), so the final period is always
/// written with `eos` set and the fade runs. A 48 kHz stream (no resampler)
/// ending exactly on a period boundary can be written unfaded, but the core
/// only sends 16 kHz TTS pieces, so that shape is not reachable today — the
/// pop the user hears at an utterance end is not this. Recorded instead of
/// shipped as a fix: `stream_tail_fades_to_zero_no_click` already pins the
/// 16 kHz behavior.
/// B-M3 §3.1: a live stream that stalls mid-period must not stop the clock.
/// The old loop skipped the write entirely (neither branch applied) and a
/// gap longer than the ALSA buffer emptied the DAC (XRUN at a chunk edge).
/// Zeros keep it alive; the buffered frames must survive and play later.
#[tokio::test]
async fn a_stalled_stream_keeps_the_clock_and_keeps_its_data() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes.clone());
    let sd = state_dir("stall-clock");
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: sd,
        loudness: false,
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
    core.send(&CF::VolumeSet {
        task_id: "v0".into(),
        device: "dot2-d".into(),
        value: 1.0,
        unit: "linear".into(),
    })
    .await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::VolumeReceipt { task_id, .. } if task_id == "v0"))
        .await;

    let amp: i16 = 9_000;
    let total = 6_000usize; // 18000 at 48k
    core.send(&begin_frame("stall", 16_000)).await;
    let _ = core.until(|f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "stall" && stage == "accepted")).await;
    // 4000 frames -> 12000 at 48k = 5 periods + 1760 left over: a stall with
    // data pending but less than one period.
    core.send(&chunk_frame("stall", 0, &vec![amp; 4000])).await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    let written_before = probes.lock().unwrap().written_frames;

    // STALL: nothing arrives for longer than the half-buffer budget.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let written_during_stall = probes.lock().unwrap().written_frames;
    assert!(
        written_during_stall > written_before,
        "the clock must keep running through a stall ({written_before} -> {written_during_stall})"
    );

    // The rest arrives: the buffered 1760 frames must still be there.
    core.send(&chunk_frame("stall", 1, &vec![amp; total - 4000]))
        .await;
    core.send(&CF::PlayEnd {
        task_id: "stall".into(),
    })
    .await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::PlayFinished { task_id, .. } if task_id == "stall"))
        .await;
    tokio::time::sleep(Duration::from_millis(60)).await;

    let out: Vec<i16> = probes.lock().unwrap().all_samples.clone();
    let loud = out.iter().filter(|&&s| s.abs() > amp / 2).count();
    // 6000 frames at 16k = 18000 at 48k; the fades trim ~5 ms at the head and
    // ~10 ms at the tail, so allow a small deficit but never a dropped period.
    assert!(
        loud >= 18_000 - 1_200,
        "the buffered data must not be dropped by the stall (got {loud} of ~18000)"
    );
    assert!(
        loud <= 18_000 + 2_048,
        "the stall must not duplicate data (got {loud})"
    );
}
