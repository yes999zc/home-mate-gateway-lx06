//! Playback and stop: the absorbed-component contract, whole-WAV plays,
//! and the receipts a stop owes (all M6a variants included).

use crate::support::*;
use home_mate_device_gateway::firmware::{ButtonEvent, FirmwareConfig, PLAY_PERIOD};
use home_mate_device_gateway::protocol::{
    AudioMeta, CoreFrame, GatewayFrame, PROTOCOL_MIN_VERSION, PROTOCOL_VERSION,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use home_mate_device_gateway::protocol::CoreFrame as CF;

// --- the contract --------------------------------------------------------------------

#[tokio::test]
async fn full_absorbed_component_contract() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, buttons) = FakeHardware::new(probes.clone());
    let sd = state_dir("full");

    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: sd.clone(),
        // The gain-law assertions below pin the exact software-gain-scaled
        // sample; the loudness stage (on by default since D stage) shapes
        // and delays that sample, so this contract runs the flat chain —
        // the loudness ladder/limiter have their own unit tests.
        loudness: false,
        ..Default::default()
    };
    // Mid-stream stop needs a handle on the gate AFTER the earlier plays:
    // those must run free (the fake DAC is instant), the stop case must not.
    let hw = Arc::new(hw);
    spawn_firmware(cfg, hw.clone());

    // Hello: honest descriptor, direct-form identity.
    let hello = core.recv().await;
    let GatewayFrame::Hello {
        protocol,
        min_protocol: _,
        gateway,
        devices,
        ..
    } = hello
    else {
        panic!("expected hello, got {hello:?}")
    };
    assert_eq!(protocol, PROTOCOL_VERSION);
    assert!(gateway.contains("dot2-direct"), "gateway id {gateway}");
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].stable_id, "dot2-d");
    assert_eq!(
        devices[0].capabilities,
        vec![
            "audio_in",
            "audio_out",
            "stop",
            "volume",
            "play_stream",
            // v1 mechanism (#13/#14, 2026-11-16): remote mute + the
            // formalized interrupt contract, then wifi (declared last —
            // a radio-less form omits it and refuses WifiConfig).
            "remote_mute",
            "local_exit",
            "interrupt_wake_word",
            "interrupt_retreat_local_exit",
            "config_apply_v2",
            "config_block_kws",
            "config_block_audio_eq",
            "config_block_led",
            "config_block_duplex",
            "config_block_acoustics",
            "wifi"
        ]
    );
    assert!(devices[0]
        .firmware
        .as_deref()
        .unwrap()
        .starts_with("hm-firmware"));

    core.send(&CoreFrame::HelloAck {
        protocol: PROTOCOL_VERSION,
        min_protocol: PROTOCOL_MIN_VERSION,

        core_version: "test-core".into(),
        supported_versions: vec![],
        capabilities: vec![],
        generation: 1,
    })
    .await;

    // Post-connect truth: online + descriptor + boot volume.
    let status = core
        .until(|f| matches!(f, GatewayFrame::DeviceStatus { online: true, .. }))
        .await;
    let GatewayFrame::DeviceStatus {
        descriptor: Some(d),
        ..
    } = status
    else {
        panic!("expected device_status with descriptor")
    };
    assert!(d.firmware.as_deref().unwrap().contains("direct"));
    let vol = core
        .until(|f| matches!(f, GatewayFrame::VolumeReceipt { .. }))
        .await;
    let GatewayFrame::VolumeReceipt {
        effective, stage, ..
    } = vol
    else {
        unreachable!()
    };
    assert!(
        (effective - 100.0 / 127.0).abs() < 1e-9,
        "boot volume {effective}"
    );
    assert_eq!(stage, "device_confirmed");

    // Mic session: continuous uplink, mono layout, deterministic downmix.
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
    assert_eq!(layout.as_deref(), Some("mono"));
    assert_eq!(channels, Some(1));

    let chunk1 = core
        .until(|f| matches!(f, GatewayFrame::AudioChunk { .. }))
        .await;
    let GatewayFrame::AudioChunk {
        pcm_b64,
        seq: s1,
        sample_rate,
        layout,
        ..
    } = chunk1
    else {
        unreachable!()
    };
    // Boot consumes seqs before the first chunk: the vol:boot receipt, then
    // the mute-truth MuteState (G6) — both from the shared counter. The
    // MicReport waiter may also consume the first chunk when it lands before
    // the report (until() discards non-matching frames) — accept 3 or 4,
    // then require strict increments.
    assert!(s1 == 3 || s1 == 4, "first observed seq {s1}");
    assert_eq!(sample_rate, 16_000);
    assert_eq!(layout, "mono");
    let pcm = base64::engine::general_purpose::STANDARD
        .decode(pcm_b64)
        .unwrap();
    assert_eq!(pcm.len(), 2560, "mono 80 ms block");
    let s0 = i16::from_le_bytes([pcm[0], pcm[1]]);
    assert_eq!(s0, 3000, "mean of ch0..6 pattern at frame 0");
    let next = core
        .until(|f| matches!(f, GatewayFrame::AudioChunk { .. }))
        .await;
    let GatewayFrame::AudioChunk { seq: s2, .. } = next else {
        unreachable!()
    };
    assert_eq!(s2, s1 + 1, "seq must increment per block");

    // Volume: software law, persisted, confirmed immediately.
    core.send(&CoreFrame::VolumeSet {
        task_id: "vt1".into(),
        device: "dot2-d".into(),
        value: 0.75,
        unit: "linear".into(),
    })
    .await;
    let vr = core
        .until(|f| matches!(f, GatewayFrame::VolumeReceipt { task_id, .. } if task_id == "vt1"))
        .await;
    let GatewayFrame::VolumeReceipt {
        effective, stage, ..
    } = vr
    else {
        unreachable!()
    };
    assert!((effective - 95.0 / 127.0).abs() < 1e-9);
    assert_eq!(stage, "device_confirmed");
    let persisted: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sd.join("state.json")).unwrap()).unwrap();
    assert_eq!(persisted["volume"], 95);

    // Volume: absorbed-mode volume applies synchronously — sequential sets
    // each get their own confirmation (no pending window to displace).
    core.send(&CoreFrame::VolumeSet {
        task_id: "vt2".into(),
        device: "dot2-d".into(),
        value: 0.5,
        unit: "linear".into(),
    })
    .await;
    let vr2 = core
        .until(|f| matches!(f, GatewayFrame::VolumeReceipt { task_id, .. } if task_id == "vt2"))
        .await;
    let GatewayFrame::VolumeReceipt {
        effective, stage, ..
    } = vr2
    else {
        unreachable!()
    };
    assert!((effective - 64.0 / 127.0).abs() < 1e-9);
    assert_eq!(stage, "device_confirmed");

    // Play: receipts accepted → sent → device_confirmed + PlayFinished; the
    // hardware wrote periods; software gain applied (level 64 ≈ −31.5 dB →
    // amp 8000 → ~214).
    core.send(&play_frame("pt1", &wav_48k(4096, 8000), None))
        .await;
    let _ = core.until(|f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "pt1" && stage == "accepted")).await;
    let _ = core.until(|f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "pt1" && stage == "sent")).await;
    let confirmed = core.until(|f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "pt1" && stage == "device_confirmed")).await;
    let GatewayFrame::PlayReceipt { detail, .. } = confirmed else {
        unreachable!()
    };
    assert!(
        detail.as_deref().unwrap().contains("bytes=8192"),
        "detail {detail:?}"
    );
    let finished = core
        .until(|f| matches!(f, GatewayFrame::PlayFinished { task_id, .. } if task_id == "pt1"))
        .await;
    let GatewayFrame::PlayFinished { played_ms, .. } = finished else {
        unreachable!()
    };
    assert_eq!(played_ms, 85, "4096 samples @48k = 85 ms (integer floor)");
    {
        let p = probes.lock().unwrap();
        assert!(
            p.written_frames >= 4096,
            "wrote {} frames",
            p.written_frames
        );
        // The continuous silence clock (G6) also writes zero periods, so
        // the gain evidence is "some written period carries the scaled
        // sample", not "the last one does".
        assert!(
            p.first_samples.iter().any(|s| (*s as i32 - 214).abs() < 6),
            "gain check: no period carries the scaled sample ({:?})",
            &p.first_samples[..p.first_samples.len().min(8)]
        );
    }

    // Stop for an unknown task still confirms cleanly.
    core.send(&CoreFrame::Stop {
        task_id: "st-unknown".into(),
        flush: true,
    })
    .await;
    let _ = core.until(|f| matches!(f, GatewayFrame::StopReceipt { task_id, stage, .. } if task_id == "st-unknown" && stage == "device_confirmed")).await;

    // Media channel play + Stop mid-stream flushes the hardware.
    //
    // The fake DAC finishes a period in microseconds, so a 1 s asset drains
    // long before the Stop's TCP round trip: on Ubuntu CI both plays had
    // already reported their finish, the Stop found an empty slot and
    // confirmed WITHOUT a flush — correct behaviour, wrong test, because
    // nothing was left to flush. Park the writes from here on so the stop
    // lands where this contract is about: mid-stream.
    let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
    hw.write_gate.lock().unwrap().replace(gate_rx);
    core.send(&play_frame("pt2", &wav_48k(48_000, 4000), Some("media")))
        .await;
    let _ = core.until(|f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "pt2" && stage == "sent")).await;
    // Let three write periods through first: the one the gate caught in
    // flight was a silence period, so this puts real audio at the DAC before
    // the stop arrives.
    for _ in 0..3 {
        write_one_period(&probes, &gate_tx).await;
    }
    core.send(&CoreFrame::Stop {
        task_id: "pt2".into(),
        flush: true,
    })
    .await;
    stop_barrier(&core).await;
    // Keep the writer moving one period at a time until the flush lands; the
    // audio thread is parked in a write and only returns for one permit at a
    // time, so the flush is reached — and counted — period by period.
    assert!(
        write_until(
            &probes,
            &gate_tx,
            |p| p.flushed >= 1,
            Duration::from_secs(5)
        )
        .await,
        "stop must flush the PCM"
    );
    let _ = core.until(|f| matches!(f, GatewayFrame::StopReceipt { task_id, stage, .. } if task_id == "pt2" && stage == "device_confirmed")).await;
    core.no_finish_for("pt2", Duration::from_millis(400)).await;
    assert!(
        probes.lock().unwrap().flushed >= 1,
        "stop must flush the PCM"
    );

    // AssistantState → palette (reserved green only via listening/hearing).
    // LED B (2026-11-15): "listening" BREATHES, so its frames are a scaled
    // green — the hue is the contract, not one exact level.
    core.send(&CoreFrame::AssistantState {
        device: "dot2-d".into(),
        state: "listening".into(),
    })
    .await;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let leds = probes.lock().unwrap().leds.clone();
        // v1 mechanism (#12): the STARTUP frame is the Reboot lamp gray and
        // the ring may legitimately hold link-lamp frames — the contract
        // under test is the listening GREEN, so wait for a green-hued frame
        // specifically instead of "any non-black paint".
        if let Some(last) = leds
            .iter()
            .rev()
            .find(|f| f[0].0 == 0 && f[0].1 > 0 && f[0].2 > 0)
        {
            let (r, g, b) = last[0];
            assert_eq!(
                r, 0,
                "listening keeps the reserved green hue (red channel off)"
            );
            assert!(g > 0 && b > 0, "listening paints green: {last:?}");
            assert!(b <= g, "green must stay green, got {last:?}");
            break;
        }
        assert!(std::time::Instant::now() < deadline, "led never painted");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // "hearing" = the same green held SOLID (a segment is being captured).
    core.send(&CoreFrame::AssistantState {
        device: "dot2-d".into(),
        state: "hearing".into(),
    })
    .await;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let leds = probes.lock().unwrap().leds.clone();
        if leds.last().map(|f| f[0]) == Some((0x00, 0xA0, 0x50)) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "hearing never painted solid green"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // idle: the close fade (green ramps down) and then a dark ring — the
    // dark frame must arrive within the fade window plus a cadence step.
    core.send(&CoreFrame::AssistantState {
        device: "dot2-d".into(),
        state: "idle".into(),
    })
    .await;
    let faded_at = std::time::Instant::now();
    let frames_before = probes.lock().unwrap().leds.len();
    let deadline = faded_at + Duration::from_secs(3);
    loop {
        let leds = probes.lock().unwrap().leds.clone();
        if let Some(last) = leds.last() {
            if leds.len() > frames_before && last[0] == (0, 0, 0) {
                let took = std::time::Instant::now() - faded_at;
                assert!(
                    took >= Duration::from_millis(200),
                    "the close fade is a ramp, not an instant cut ({took:?})"
                );
                break;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "idle never painted dark after the fade"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Buttons: action maps to the adapter's action_press; volume keys step
    // the device level and report the fact.
    buttons.send(ButtonEvent::Action).unwrap();
    let btn = core
        .until(|f| matches!(f, GatewayFrame::Button { .. }))
        .await;
    let GatewayFrame::Button { action, .. } = btn else {
        unreachable!()
    };
    assert_eq!(action, "action_press");

    buttons.send(ButtonEvent::VolumeUp).unwrap();
    let stepped = core.until(|f| matches!(f, GatewayFrame::VolumeReceipt { task_id, .. } if task_id.starts_with("vol:dev:"))).await;
    let GatewayFrame::VolumeReceipt { effective, .. } = stepped else {
        unreachable!()
    };
    assert!(
        (effective - 72.0 / 127.0).abs() < 1e-9,
        "stepped to {effective}"
    );

    // ModeSet: honest half-duplex negotiation.
    core.send(&CoreFrame::ModeSet {
        device: "dot2-d".into(),
        duplex_requested: "full".into(),
    })
    .await;
    let mode = core
        .until(|f| matches!(f, GatewayFrame::ModeReport { .. }))
        .await;
    let GatewayFrame::ModeReport { duplex, .. } = mode else {
        unreachable!()
    };
    assert_eq!(duplex.capability, "half");
    assert_eq!(duplex.effective, "half");
}

/// T4 regression: a WHOLE-WAV play must be interruptible.
///
/// Live evidence (2026-11-10): Stop arrived 4 s into a 28 s asset, the
/// firmware answered device_confirmed, and the DAC kept playing to the end
/// (`played_ms=28032`). Cause: the playback burst loop drained the entire
/// pre-queued WAV before returning to the command channel, so Flush could
/// only land after the asset finished. The write path now takes one period
/// per outer iteration, so a stop must take effect within a couple of
/// periods no matter how much is queued.
#[tokio::test]
async fn whole_wav_play_is_interruptible_within_a_few_periods() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes.clone());
    // Pace the writer: one permit = one period written, so the test can
    // count exactly how much audio escaped after the stop.
    let (gate_tx, gate_rx) = std::sync::mpsc::channel();
    hw.write_gate.lock().unwrap().replace(gate_rx);
    let sd = state_dir("whole-stop");
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

    // 5 s of 48 kHz audio queued in ONE frame (117 periods of 2048).
    let wav = wav_48k(240_000, 8_000);
    core.send(&CoreFrame::Play {
        task_id: "wq".into(),
        device: "dot2-d".into(),
        content_ref: "test:whole".into(),
        audio: AudioMeta {
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
    let _ = core
        .until(|f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "wq" && stage == "accepted"))
        .await;

    // Let three periods through, then stop.
    for _ in 0..3 {
        write_one_period(&probes, &gate_tx).await;
    }
    let before_stop = probes.lock().unwrap().written_frames;
    core.send(&CF::Stop {
        task_id: "wq".into(),
        flush: true,
    })
    .await;
    // Wait for the Stop to reach the playback thread before letting any more
    // audio through. The writer is parked, so nothing plays during this wait:
    // it keeps the measurement from counting the test's own link latency as
    // DAC periods.
    stop_barrier(&core).await;

    // The contract is about AUDIO, so count the frames the DAC actually took
    // between the Stop and the flush — not the permits the test happened to
    // send (those are buffered, and they also pay for silence-clock periods).
    assert!(
        write_until(&probes, &gate_tx, |p| p.flushed > 0, Duration::from_secs(5)).await,
        "no playback flush after Stop"
    );
    let p = probes.lock().unwrap();
    let extra_periods = (p.written_frames - before_stop) / PLAY_PERIOD as u64;
    assert!(
        extra_periods <= 2,
        "stop took {extra_periods} extra periods after {} frames were already written",
        before_stop
    );
    let _ = core
        .until(|f| matches!(f, GatewayFrame::StopReceipt { task_id, .. } if task_id == "wq"))
        .await;
}

/// R0 (2026-11-13): a stop's `device_confirmed` is a HARDWARE fact, so it
/// must leave after the flush, and it must carry how much audio the DAC
/// actually took. The old ordering enqueued the flush and confirmed at once
/// (`played_ms` absent, hardware not yet flushed); a 200 ms slow flush makes
/// that ordering visible instead of a race.
#[tokio::test]
async fn stop_receipt_follows_the_flush_and_carries_played_ms() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes.clone());
    let (gate_tx, gate_rx) = std::sync::mpsc::channel();
    hw.write_gate.lock().unwrap().replace(gate_rx);
    // The flush itself is held open by the test (see below).
    let (flush_tx, flush_rx) = std::sync::mpsc::channel();
    hw.flush_gate.lock().unwrap().replace(flush_rx);
    let sd = state_dir("stop-order");
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

    let wav = wav_48k(240_000, 8_000); // 5 s, one whole-WAV play
    core.send(&CoreFrame::Play {
        task_id: "sk".into(),
        device: "dot2-d".into(),
        content_ref: "test:stop-order".into(),
        audio: AudioMeta {
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
    let _ = core
        .until(|f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "sk" && stage == "accepted"))
        .await;

    // Let four periods through, then stop mid-play.
    for _ in 0..4 {
        write_one_period(&probes, &gate_tx).await;
    }
    core.send(&CF::Stop {
        task_id: "sk".into(),
        flush: true,
    })
    .await;
    // Let the stop reach the playback thread with the writer parked (nothing
    // plays while the test holds the permits), then keep the writer moving
    // one period at a time until the audio thread actually enters the flush
    // (it is parked in a write when the Stop arrives).
    stop_barrier(&core).await;
    assert!(
        write_until(
            &probes,
            &gate_tx,
            |p| p.flush_started > 0,
            Duration::from_secs(5)
        )
        .await,
        "the stop never reached playback_flush"
    );
    let p = probes.lock().unwrap();
    assert!(p.flush_started > 0, "the stop never reached playback_flush");
    assert_eq!(p.flushed, 0, "the flush gate did not hold");
    drop(p);

    // While the flush is still running there is no hardware fact yet, so no
    // receipt may exist. (Old behavior sent it here.)
    let early = tokio::time::timeout(
        Duration::from_millis(300),
        core.until(|f| matches!(f, GatewayFrame::StopReceipt { task_id, .. } if task_id == "sk")),
    )
    .await;
    assert!(
        early.is_err(),
        "StopReceipt left before playback_flush completed — the receipt is not a hardware fact"
    );

    // Now let the flush complete; the receipt must arrive, with the truth.
    let _ = flush_tx.send(());
    let receipt = core
        .until(|f| matches!(f, GatewayFrame::StopReceipt { task_id, .. } if task_id == "sk"))
        .await;
    assert!(
        probes.lock().unwrap().flushed > 0,
        "receipt without a completed flush"
    );
    match receipt {
        GatewayFrame::StopReceipt {
            stage, played_ms, ..
        } => {
            assert_eq!(stage, "device_confirmed");
            let ms = played_ms
                .expect("played_ms must state how much audio the DAC took before the stop");
            // 3-6 periods of 2048 frames at 48k (42.7 ms each). The permits
            // above are paced one period at a time, so this number can only
            // be what the DAC took: four permitted periods (± one the stop
            // caught in flight), not however many the test managed to send.
            assert!(
                (100..=400).contains(&ms),
                "played_ms={ms} does not look like 3-6 whole periods (128-256 ms)"
            );
        }
        other => panic!("unexpected frame: {other:?}"),
    }
}

/// Regression (device log 2026-11-12 08:19:26): the cue is cancelled and the
/// answer stream opens in the same instant. The device reported
/// `play_finished { the answer's id, played_ms = the CUE's amount }` 0 s in;
/// the core dropped the answer's record and then ignored a barge-in as
/// `no_voice_playback` while audio was still coming out.
///
/// Playback is frozen with the write gate so the cue is genuinely mid-play
/// when the stop and the new stream arrive — the exact race.
#[tokio::test]
async fn a_stopped_play_does_not_report_a_finish_for_the_next_stream() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes.clone());
    let hw = Arc::new(hw);
    let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
    hw.write_gate.lock().unwrap().replace(gate_rx);
    let sd = state_dir("stop-then-stream");
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: sd,
        loudness: false,
        ..Default::default()
    };
    spawn_firmware(cfg, hw.clone());

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

    // 1) The cue: whole WAV, queued; playback parks at the first write.
    core.send(&play_frame("cue", &wav_48k(24_000, 1200), Some("voice")))
        .await;
    let _ = core.until(|f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "cue" && stage == "sent")).await;
    tokio::time::sleep(Duration::from_millis(60)).await;

    // 2) The core cancels the cue and opens the answer stream immediately —
    //    the filler replace path. The role loop handles all of this while the
    //    playback thread is still parked inside write().
    core.send(&CF::Stop {
        task_id: "cue".into(),
        flush: true,
    })
    .await;
    core.send(&begin_frame("answer", 16_000)).await;
    let pcm = vec![900i16; 4000];
    for (i, chunk) in pcm.chunks(1000).enumerate() {
        core.send(&chunk_frame("answer", i as u64, chunk)).await;
    }
    core.send(&CF::PlayEnd {
        task_id: "answer".into(),
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    // 3) Release playback: every send unparks one write (the gate holds its
    //    mutex while parked, so the gate cannot be removed from here). The
    //    two queued flushes run first, then the stream's periods.
    for _ in 0..40 {
        let _ = gate_tx.send(());
    }

    let deadline = tokio::time::Instant::now() + Duration::from_millis(2000);
    let mut finished: Vec<(String, u64)> = Vec::new();
    loop {
        match tokio::time::timeout_at(deadline, core.recv()).await {
            Ok(GatewayFrame::PlayFinished {
                task_id, played_ms, ..
            }) => finished.push((task_id, played_ms)),
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    let answer_finishes: Vec<&(String, u64)> =
        finished.iter().filter(|(id, _)| id == "answer").collect();
    let cue_finishes: Vec<&(String, u64)> = finished.iter().filter(|(id, _)| id == "cue").collect();
    assert!(
        cue_finishes.is_empty(),
        "a STOPPED cue must not report a finish: {cue_finishes:?}"
    );
    assert_eq!(
        answer_finishes.len(),
        1,
        "exactly one finish for the stream; a flush must never be reported as one (all: {finished:?})"
    );
    let played = answer_finishes[0].1;
    assert!(
        (200..=320).contains(&played),
        "the stream's played_ms must be its own (4000 frames at 16k = 250 ms), got {played}"
    );
}

/// M6a: "we wrote N ms" and "the DAC consumed M ms" are different facts. The
/// fake reports a 48 ms backlog at drain time, so a 100 ms play must finish
/// as played_ms≈100 / consumed_ms≈52 / basis "hw_delay". When the platform
/// cannot report the backlog the finish must NOT invent a consumption number
/// — it says `written`.
#[tokio::test]
async fn a_finish_reports_consumed_against_the_hardware_backlog() {
    for (report, want_basis) in [(true, "hw_delay"), (false, "written")] {
        let probes = Arc::new(Mutex::new(Probes::default()));
        let (mut hw, _btn) = FakeHardware::new(probes.clone());
        hw.report_delay = report;
        // 48 ms at 48 kHz = 2304 frames.
        hw.delay_frames
            .store(2_304, std::sync::atomic::Ordering::Relaxed);
        let (core, port) = spawn_fake_core().await;
        let cfg = FirmwareConfig {
            core_addr: format!("127.0.0.1:{port}"),
            state_dir: state_dir(&format!("consumed-{want_basis}")),
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
        // 4800 samples at 48k = 100 ms.
        core.send(&play_frame("m6a", &wav_48k(4_800, 8_000), Some("voice")))
            .await;
        let fin = core
            .until(|f| matches!(f, GatewayFrame::PlayFinished { task_id, .. } if task_id == "m6a"))
            .await;
        let GatewayFrame::PlayFinished {
            played_ms,
            consumed_ms,
            progress_basis,
            ..
        } = fin
        else {
            unreachable!()
        };
        assert_eq!(
            progress_basis.as_deref(),
            Some(want_basis),
            "the finish must say how it knows (played_ms={played_ms})"
        );
        if report {
            let consumed = consumed_ms.expect("a reported backlog must produce consumed_ms");
            // The whole-WAV path counts its padded final period, so compare
            // against the finish's own played_ms rather than the nominal 100.
            assert_eq!(
                consumed,
                played_ms - 48,
                "consumed_ms must be played_ms minus the 48 ms backlog"
            );
        } else {
            assert_eq!(consumed_ms, None, "no backlog = no consumption claim");
        }
    }
}

/// M6a: a flush that FAILED may have left the cue audible, so the stop must
/// not be confirmed. It goes out as `unknown` with the reason, and the core's
/// stop waiter is still released (bounded behaviour), never as "silence".
#[tokio::test]
async fn a_failed_flush_is_never_confirmed() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (mut hw, _btn) = FakeHardware::new(probes.clone());
    hw.fail_flush = true;
    // The fake DAC writes instantly, so a whole asset would drain before the
    // stop arrives; park ONE write to keep the play genuinely mid-stream.
    let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
    hw.write_gate.lock().unwrap().replace(gate_rx);
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: state_dir("flush-fail"),
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

    core.send(&play_frame("ff", &wav_48k(48_000, 8_000), Some("voice")))
        .await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "ff" && stage == "sent"))
        .await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    core.send(&CF::Stop {
        task_id: "ff".into(),
        flush: true,
    })
    .await;
    // Let the stop reach the playback thread (the writer is parked, so
    // nothing plays meanwhile), then keep it moving ONE period at a time
    // until the thread actually reaches the (failing) hardware flush. A fixed
    // burst of permits does not work: they are buffered, and on Ubuntu CI the
    // writer burned through all four before the role loop had even queued the
    // Flush — it then parked with the stop still waiting and the stall
    // watchdog killed the test process (exit 75) ten seconds later.
    stop_barrier(&core).await;
    assert!(
        write_until(
            &probes,
            &gate_tx,
            |p| p.flush_attempts > 0,
            Duration::from_secs(5)
        )
        .await,
        "the stop never reached playback_flush"
    );
    let receipt = core
        .until(|f| matches!(f, GatewayFrame::StopReceipt { task_id, .. } if task_id == "ff"))
        .await;
    let GatewayFrame::StopReceipt { stage, detail, .. } = receipt else {
        unreachable!()
    };
    assert_eq!(
        stage, "unknown",
        "a failed flush must never confirm the stop"
    );
    assert!(
        detail
            .as_deref()
            .is_some_and(|d| d.starts_with("flush_failed")),
        "the reason must travel with the fact: {detail:?}"
    );
}

/// M6a: the north link IS the generation. A link drop while a play is live
/// must flush the DAC and clear the slots — otherwise the next connection
/// inherits a Role that is still playing and reports an old task's facts into
/// a new generation.
#[tokio::test]
async fn a_link_drop_flushes_the_dac_and_clears_the_slots() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (mut hw, _btn) = FakeHardware::new(probes.clone());
    // Park an actual data write, never the startup silence clock. A silence
    // write can precede Samples installation on Linux; invalidating queued
    // old PCM then correctly requires no old-stream hardware flush.
    hw.park_audio_writes = true;
    // Keep the old stream live until teardown is observed, then release
    // exactly its in-flight period to let the queued flush reach hardware.
    let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
    hw.write_gate.lock().unwrap().replace(gate_rx);
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: state_dir("link-drop"),
        loudness: false,
        ..Default::default()
    };
    let firmware = home_mate_device_gateway::firmware::Firmware::new(cfg, Arc::new(hw));
    let stats = firmware.northbound_observer();
    std::thread::spawn(move || {
        let _ = firmware.run();
    });
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

    core.send(&play_frame("drop", &wav_48k(96_000, 8_000), Some("voice")))
        .await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "drop" && stage == "sent"))
        .await;
    let data_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while probes.lock().unwrap().audio_write_started == 0 {
        assert!(
            tokio::time::Instant::now() < data_deadline,
            "old stream never entered a real DAC write"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let generation = stats.snapshot().link_generation;
    assert_eq!(
        probes.lock().unwrap().flushed,
        0,
        "nothing flushed while the link is healthy"
    );

    // Disconnect, then observe production teardown before unblocking the DAC.
    // Cleanup records the point at which the old stamped flush was queued.
    // Releasing a burst before that point races the entire two-second asset.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    *core.writer.lock().unwrap() = None;
    loop {
        let probe = stats.probe();
        if probe["last_cleanup_generation"].as_u64() == Some(generation)
            && probe["last_cleanup_us"].as_u64().is_some_and(|at| at > 0)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "dead link never retired its playback generation"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    gate_tx.send(()).unwrap();
    loop {
        if probes.lock().unwrap().flushed > 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "a dead link with a live play must flush the DAC"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// #15: base64 decoding & resampling run on a dedicated decode worker thread,
/// so `Stop` sent while a large `Play` is decoding reaches `playback_thread`'s
/// Flush quickly (< 100 ms) and watchdog/LED ticks keep advancing.
#[tokio::test]
async fn stop_during_large_play_decode_reaches_flush_quickly() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes.clone());
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: state_dir("decode-offload-stop"),
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
    let _ = core
        .until(|f| matches!(f, GatewayFrame::MuteState { .. }))
        .await;

    // 5 seconds @ 48 kHz mono WAV (~480 KB raw, ~640 KB base64).
    let large_wav = wav_48k(240_000, 12_000);
    core.send(&play_frame("big-play", &large_wav, Some("voice")))
        .await;
    let t0 = std::time::Instant::now();
    core.send(&CoreFrame::Stop {
        task_id: "big-play".into(),
        flush: true,
    })
    .await;

    let _receipt = core
        .until(|f| matches!(f, GatewayFrame::StopReceipt { task_id, stage, .. } if task_id == "big-play" && stage == "device_confirmed"))
        .await;
    let stop_elapsed = t0.elapsed();
    assert!(
        stop_elapsed < Duration::from_millis(100),
        "Stop took {:?} to reach Flush + StopReceipt (expected < 100 ms)",
        stop_elapsed
    );
    assert!(
        probes.lock().unwrap().flush_attempts >= 1,
        "Stop must reach playback_thread's hardware Flush"
    );
    // Watchdog / LED ticks keep advancing without stalling.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        !probes.lock().unwrap().leds.is_empty(),
        "LED/watchdog ticks must continue advancing"
    );
    core.no_finish_for("big-play", Duration::from_millis(200))
        .await;
}

/// #17: a >8 MiB inbound `Play` frame is drained and rejected with
/// `PlayReceipt { stage: "failed" }` without tearing down the TCP link.
#[tokio::test]
async fn oversized_play_frame_is_rejected_without_dropping_link() {
    use tokio::io::AsyncWriteExt;

    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes);
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: state_dir("oversized-play"),
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
    let _ = core
        .until(|f| matches!(f, GatewayFrame::MuteState { .. }))
        .await;

    // Craft an >8 MiB JSON line with task_id near the front.
    let prefix = r#"{"type":"play","task_id":"too-big","device":"dot2-d","audio":{"format":"wav","sample_rate":48000,"channels":1},"audio_b64":""#;
    let pad_len = home_mate_device_gateway::protocol::MAX_FRAME_BYTES + 64 * 1024;
    {
        let mut guard = core.writer.lock().unwrap();
        let wr = guard.as_mut().unwrap();
        wr.write_all(prefix.as_bytes()).await.unwrap();
        let chunk = vec![b'A'; 64 * 1024];
        let mut written = 0usize;
        while written < pad_len {
            let n = (pad_len - written).min(chunk.len());
            wr.write_all(&chunk[..n]).await.unwrap();
            written += n;
        }
        wr.write_all(b"\"}\n").await.unwrap();
        wr.flush().await.unwrap();
    }

    let failed = core
        .until(|f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "too-big" && stage == "failed"))
        .await;
    let GatewayFrame::PlayReceipt { detail, .. } = failed else {
        unreachable!()
    };
    assert_eq!(detail.as_deref(), Some("frame exceeds 8 MiB limit"));

    // Link must still be alive: send a normal VolumeSet on the same socket.
    core.send(&CoreFrame::VolumeSet {
        task_id: "vol-after".into(),
        device: "dot2-d".into(),
        value: 0.5,
        unit: "linear".into(),
    })
    .await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::VolumeReceipt { task_id, stage, .. } if task_id == "vol-after" && stage == "device_confirmed"))
        .await;
}

/// #17: `simulated_ms` in `silence_wav` is capped at `MAX_SIMULATED_MS` (60s)
/// so a huge `simulated_ms` cannot OOM the 512 MB board.
#[tokio::test]
async fn simulated_ms_is_capped() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes);
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: state_dir("sim-cap"),
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

    core.send(&CoreFrame::Play {
        task_id: "sim-huge".into(),
        device: "dot2-d".into(),
        audio: home_mate_device_gateway::protocol::AudioMeta {
            format: "wav".into(),
            sample_rate: 48_000,
            channels: 1,
        },
        audio_b64: None,
        simulated_ms: Some(1_000_000_000),
        channel: Some("voice".into()),
        text: None,
        content_ref: String::new(),
        barge_in: None,
    })
    .await;
    let sent = core
        .until(|f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "sim-huge" && stage == "sent"))
        .await;
    let GatewayFrame::PlayReceipt { detail, .. } = sent else {
        unreachable!()
    };
    assert!(
        detail.as_deref().is_some_and(|d| d.contains("~60000 ms")),
        "expected simulated_ms capped at 60000 ms, got {detail:?}"
    );
}

/// #18: Stopping one channel while the other channel is still live must NOT
/// flush the shared DAC or claim `hw_delay` DAC-drop consumption.
#[tokio::test]
async fn stop_while_other_channel_live_does_not_claim_dac_drop() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes.clone());
    hw.delay_frames
        .store(2400, std::sync::atomic::Ordering::Relaxed);
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: state_dir("stop-other-live"),
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

    // Start a music stream (without PlayEnd, so music stays live) and a voice
    // stream concurrently.
    let pcm_chunk = base64::engine::general_purpose::STANDARD.encode(vec![0u8; 4096]);
    core.send(&CoreFrame::PlayBegin {
        task_id: "bg-music".into(),
        device: "dot2-d".into(),
        audio: AudioMeta {
            format: "pcm_s16le".into(),
            sample_rate: 48_000,
            channels: 1,
        },
        channel: Some("media".into()),
        text: None,
        content_ref: String::new(),
        barge_in: None,
    })
    .await;
    core.send(&CoreFrame::PlayChunk {
        task_id: "bg-music".into(),
        seq: 0,
        audio_b64: pcm_chunk.clone(),
    })
    .await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "bg-music" && stage == "streaming"))
        .await;

    core.send(&CoreFrame::PlayBegin {
        task_id: "fg-voice".into(),
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
        task_id: "fg-voice".into(),
        seq: 0,
        audio_b64: pcm_chunk,
    })
    .await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "fg-voice" && stage == "streaming"))
        .await;
    // Give playback_thread a moment to ingest both streams before stopping voice.
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Stop voice while music is still live.
    core.send(&CoreFrame::Stop {
        task_id: "fg-voice".into(),
        flush: true,
    })
    .await;
    let receipt = core
        .until(|f| matches!(f, GatewayFrame::StopReceipt { task_id, .. } if task_id == "fg-voice"))
        .await;
    let GatewayFrame::StopReceipt {
        stage,
        consumed_ms,
        progress_basis,
        ..
    } = receipt
    else {
        unreachable!()
    };
    assert_eq!(stage, "device_confirmed");
    assert_eq!(
        progress_basis.as_deref(),
        Some("written"),
        "when music is still live, DAC was not dropped so progress_basis must not claim hw_delay"
    );
    assert_eq!(consumed_ms, None);
    assert_eq!(
        probes.lock().unwrap().flushed,
        0,
        "hardware playback_flush must not run while music is live"
    );
}

/// #18: With AEC enabled and mic_on=false, capture_thread keeps running
/// pipe.process() across playback and Stop; turning mic_on=true afterwards
/// delivers AudioChunks cleanly with the hardware flush completed.
#[tokio::test]
async fn mic_off_to_on_and_stop_during_playback_keeps_aec_warm() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (mut hw, _btn) = FakeHardware::new(probes.clone());
    hw.loopback_ch8 = Some(Arc::new(Mutex::new(std::collections::VecDeque::new())));
    hw.capture_pace_ms = 20;
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: state_dir("aec-warm"),
        loudness: false,
        aec: true,
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

    // Mic starts OFF: stream audio (no PlayEnd) and stop it mid-playback.
    let pcm_chunk = base64::engine::general_purpose::STANDARD.encode(vec![0x10u8; 4096]);
    core.send(&CoreFrame::PlayBegin {
        task_id: "warm-1".into(),
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
        task_id: "warm-1".into(),
        seq: 0,
        audio_b64: pcm_chunk,
    })
    .await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "warm-1" && stage == "streaming"))
        .await;
    core.send(&CoreFrame::Stop {
        task_id: "warm-1".into(),
        flush: true,
    })
    .await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::StopReceipt { task_id, stage, .. } if task_id == "warm-1" && stage == "device_confirmed"))
        .await;
    assert!(probes.lock().unwrap().flushed >= 1);

    // Open mic session (off -> on) and start a second play; AudioChunks must flow.
    let wav = base64::engine::general_purpose::STANDARD.encode(wav_48k(48_000, 3000));
    core.send(&CoreFrame::MicSession {
        device: "dot2-d".into(),
        action: "start".into(),
        mode: None,
    })
    .await;
    core.send(&CoreFrame::Play {
        task_id: "warm-2".into(),
        device: "dot2-d".into(),
        audio: AudioMeta {
            format: "wav".into(),
            sample_rate: 48_000,
            channels: 1,
        },
        audio_b64: Some(wav),
        simulated_ms: None,
        channel: Some("voice".into()),
        text: None,
        content_ref: String::new(),
        barge_in: None,
    })
    .await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::AudioChunk { .. }))
        .await;
}
