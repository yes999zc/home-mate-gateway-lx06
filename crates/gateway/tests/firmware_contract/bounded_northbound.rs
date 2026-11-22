//! Actual Firmware.run + normal Core wire commands, not direct queue injection.
use crate::support::*;
use home_mate_device_gateway::{
    firmware::{Firmware, FirmwareConfig},
    protocol::{CoreFrame, GatewayFrame},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

async fn command_burst_fault(requested_len: usize, expected: &str) {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = socket.listen(4).unwrap();
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (mut hw, _buttons) = FakeHardware::new(probes.clone());
    hw.pace_playback = true;
    hw.bounded_probes = true;
    if requested_len >= 100 {
        hw.capture_pace_ms = 1;
    }
    let firmware = Firmware::new(
        FirmwareConfig {
            core_addr: listener.local_addr().unwrap().to_string(),
            state_dir: state_dir(expected),
            wifi: false,
            aec: false,
            kws: false,
            loudness: false,
            ..Default::default()
        },
        Arc::new(hw),
    );
    let stats = firmware.northbound_observer();
    std::thread::spawn(move || {
        let _ = firmware.run();
    });
    let (stream, _) = listener.accept().await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    assert!(matches!(
        serde_json::from_str::<GatewayFrame>(&lines.next_line().await.unwrap().unwrap()).unwrap(),
        GatewayFrame::Hello { .. }
    ));
    wr.write_all(b"{\"type\":\"hello_ack\",\"protocol\":0,\"min_protocol\":0,\"core_version\":\"bounded-fixture\",\"generation\":1}\n").await.unwrap();
    loop {
        let frame: GatewayFrame =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        if matches!(frame, GatewayFrame::MuteState { .. }) {
            break;
        }
    }
    let frame = CoreFrame::ModeSet {
        device: "dot2-d".into(),
        duplex_requested: "x".repeat(requested_len),
    };
    let mut encoded = serde_json::to_vec(&frame).unwrap();
    encoded.push(b'\n');
    let mut burst = Vec::new();
    for _ in 0..if requested_len < 100 { 1024 } else { 16 } {
        burst.extend_from_slice(&encoded);
    }
    if requested_len >= 100 {
        // A finite burst can fit entirely in Linux's autotuned TCP send
        // buffer. Establish an actual blocked production write before
        // testing byte admission; never infer pressure from buffer settings.
        send(
            &mut wr,
            &CoreFrame::MicSession {
                device: "dot2-d".into(),
                action: "start".into(),
                mode: None,
            },
        )
        .await;
        loop {
            let frame: GatewayFrame =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            if matches!(frame, GatewayFrame::AudioChunk { .. }) {
                break;
            }
        }
        let _ = wait_socket_backpressure(&stats, false).await;
    }
    let send = tokio::spawn(async move {
        let _ = wr.write_all(&burst).await;
        wr
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let sample = stats.snapshot();
        assert!(
            sample.critical_high_water_count <= 64,
            "actual producer retained {} critical envelopes",
            sample.critical_high_water_count
        );
        assert!(
            sample.critical_high_water_bytes <= 262144,
            "actual producer retained {} encoded critical bytes",
            sample.critical_high_water_bytes
        );
        if let Some(fault) = sample.last_fault {
            assert_eq!(fault.code, expected);
            assert_eq!(
                sample.writer_timeouts, 0,
                "critical exhaustion must independently fail before writer timeout"
            );
            assert!(sample.critical_high_water_count <= 64);
            assert!(sample.critical_high_water_bytes <= 262144);
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "normal command burst did not saturate critical admission"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    send.abort();
    let _ = send.await;
    drop(lines);
    let (fresh, _) = tokio::time::timeout(Duration::from_secs(4), listener.accept())
        .await
        .expect("critical fault failed to reconnect")
        .unwrap();
    let mut fresh = BufReader::new(fresh).lines();
    assert!(matches!(
        serde_json::from_str::<GatewayFrame>(&fresh.next_line().await.unwrap().unwrap()).unwrap(),
        GatewayFrame::Hello { .. }
    ));
    assert!(
        probes.lock().unwrap().written_frames > 0,
        "north saturation stopped fake DAC clock"
    );
}
#[tokio::test]
async fn normal_core_command_burst_exhausts_critical_count_and_reconnects() {
    command_burst_fault(1, "critical_count_full").await;
}
#[tokio::test]
async fn normal_core_command_burst_exhausts_critical_bytes_and_reconnects() {
    command_burst_fault(90_000, "critical_bytes_full").await;
}

type PeerReader = tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>;
async fn handshake(stream: tokio::net::TcpStream) -> (PeerReader, tokio::net::tcp::OwnedWriteHalf) {
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    assert!(matches!(
        serde_json::from_str::<GatewayFrame>(&lines.next_line().await.unwrap().unwrap()).unwrap(),
        GatewayFrame::Hello { .. }
    ));
    wr.write_all(b"{\"type\":\"hello_ack\",\"protocol\":0,\"min_protocol\":0,\"core_version\":\"bounded-fixture\",\"generation\":1}\n").await.unwrap();
    loop {
        let frame: GatewayFrame =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        if matches!(frame, GatewayFrame::MuteState { .. }) {
            break;
        }
    }
    (lines, wr)
}
async fn send(wr: &mut tokio::net::tcp::OwnedWriteHalf, frame: &CoreFrame) {
    let mut bytes = serde_json::to_vec(frame).unwrap();
    bytes.push(b'\n');
    wr.write_all(&bytes).await.unwrap();
}
async fn until_sent(lines: &mut PeerReader) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let frame: GatewayFrame =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            if matches!(frame,GatewayFrame::PlayReceipt{stage,..} if stage=="sent") {
                break;
            }
        }
    })
    .await
    .expect("whole file never reached actual decoder");
}
async fn wait_audio(probes: &Arc<Mutex<Probes>>, count: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while probes.lock().unwrap().audio_write_started < count {
        assert!(
            tokio::time::Instant::now() < deadline,
            "fake DAC did not enter expected data write"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

#[tokio::test]
async fn actual_socket_same_task_reconnect_does_not_attribute_old_dac_completion_to_fresh_play() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (mut hw, _buttons) = FakeHardware::new(probes.clone());
    hw.bounded_probes = true;
    hw.park_audio_writes = true;
    hw.pace_playback = true;
    let (gate, gate_rx) = std::sync::mpsc::channel();
    *hw.write_gate.lock().unwrap() = Some(gate_rx);
    // Dropping this sender on any failure releases the parked old DAC call.
    spawn_firmware(
        FirmwareConfig {
            core_addr: listener.local_addr().unwrap().to_string(),
            state_dir: state_dir("same-task-reconnect"),
            wifi: false,
            aec: false,
            kws: false,
            loudness: false,
            play_watchdog_ms: 0,
            ..Default::default()
        },
        Arc::new(hw),
    );
    let (stream, _) = listener.accept().await.unwrap();
    let (mut old_lines, mut old_wr) = handshake(stream).await;
    send(
        &mut old_wr,
        &play_frame("same", &wav_48k(2048, 8000), Some("voice")),
    )
    .await;
    until_sent(&mut old_lines).await;
    wait_audio(&probes, 1).await;
    drop(old_lines);
    drop(old_wr);
    let (fresh, _) = tokio::time::timeout(Duration::from_secs(4), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let (mut lines, mut wr) = handshake(fresh).await;
    send(
        &mut wr,
        &play_frame("same", &wav_48k(4096, 8000), Some("voice")),
    )
    .await;
    until_sent(&mut lines).await;
    gate.send(()).unwrap();
    wait_audio(&probes, 2).await;
    // Fresh PCM is installed but still parked; only the OLD one-period DAC
    // write can have completed. Its terminal fact must not name fresh task same.
    let quiet = tokio::time::timeout(Duration::from_millis(150), async {
        loop {
            let line = lines.next_line().await.unwrap().unwrap();
            let frame: GatewayFrame = serde_json::from_str(&line).unwrap();
            let terminal = match frame {
                GatewayFrame::PlayFinished { .. } => true,
                GatewayFrame::PlayReceipt { stage, .. } => stage == "device_confirmed",
                _ => false,
            };
            assert!(!terminal, "old DAC completion finished fresh same-ID play");
        }
    })
    .await;
    assert!(quiet.is_err());
    gate.send(()).unwrap();
    wait_audio(&probes, 3).await;
    gate.send(()).unwrap();
    let finished = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let frame: GatewayFrame =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            if let GatewayFrame::PlayFinished {
                task_id, played_ms, ..
            } = frame
            {
                assert_eq!(task_id, "same");
                break played_ms;
            }
        }
    })
    .await
    .expect("fresh play did not finish");
    assert_eq!(
        finished,
        4096 * 1000 / 48000,
        "fresh two-period completion must carry only fresh PCM"
    );
    assert!(probes.lock().unwrap().all_samples.is_empty());
}

#[tokio::test]
async fn actual_socket_stalled_writer_has_fixed_deadline_and_bounded_cleanup() {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = socket.listen(4).unwrap();
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (mut hw, _buttons) = FakeHardware::new(probes.clone());
    hw.bounded_probes = true;
    hw.capture_pace_ms = 1;
    hw.pace_playback = true;
    let firmware = Firmware::new(
        FirmwareConfig {
            core_addr: listener.local_addr().unwrap().to_string(),
            state_dir: state_dir("socket-deadline"),
            wifi: false,
            aec: false,
            kws: false,
            loudness: false,
            ..Default::default()
        },
        Arc::new(hw),
    );
    let stats = firmware.northbound_observer();
    std::thread::spawn(move || {
        let _ = firmware.run();
    });
    let (stream, _) = listener.accept().await.unwrap();
    let (mut lines, mut wr) = handshake(stream).await;
    send(
        &mut wr,
        &CoreFrame::MicSession {
            device: "dot2-d".into(),
            action: "start".into(),
            mode: None,
        },
    )
    .await;
    loop {
        let frame: GatewayFrame =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        if matches!(frame, GatewayFrame::AudioChunk { .. }) {
            break;
        }
    }
    // Stop reading. Do not alter the Gateway's send buffer or scheduler.
    let (generation, start, decision) = wait_socket_backpressure(&stats, true).await;
    assert_eq!(
        decision - start,
        1_000_000,
        "production frame deadline changed"
    );
    loop {
        let probe = stats.probe();
        if probe["status"]["writer_timeouts"].as_u64().unwrap() > 0 {
            let fault = probe["last_fault_us"].as_u64().unwrap();
            assert!(
                fault >= decision && fault <= decision + 100_000,
                "actual socket writer missed fixed deadline"
            );
            if probe["last_cleanup_generation"].as_u64().unwrap() == generation {
                let cleanup = probe["last_cleanup_us"].as_u64().unwrap();
                assert!(
                    cleanup >= fault && cleanup - fault <= 1_000_000,
                    "fatal cleanup exceeded1s"
                );
                break;
            }
        } else {
            assert!(
                stats.snapshot().sampled_uptime_ms * 1000 <= start + 1_100_000,
                "actual socket frame deadline missing"
            );
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(probes.lock().unwrap().written_frames > 0);
    assert!(probes.lock().unwrap().all_samples.is_empty());
    drop(lines);
    drop(wr);
    let (fresh, _) = tokio::time::timeout(Duration::from_secs(4), listener.accept())
        .await
        .expect("deadline failed to reconnect")
        .unwrap();
    let mut fresh = BufReader::new(fresh).lines();
    assert!(matches!(
        serde_json::from_str::<GatewayFrame>(&fresh.next_line().await.unwrap().unwrap()).unwrap(),
        GatewayFrame::Hello { .. }
    ));
}

/// Actual production write ownership is the barrier. Receiver buffer sizes
/// only accelerate pressure; they are not evidence that the writer blocked.
async fn wait_socket_backpressure(
    stats: &home_mate_device_gateway::firmware::northbound::NorthboundStats,
    allow_terminal: bool,
) -> (u64, u64, u64) {
    let pressure_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let probe = stats.probe();
        let now = probe["status"]["sampled_uptime_ms"].as_u64().unwrap() * 1000;
        let start = probe["write_started_us"].as_u64().unwrap();
        if probe["status"]["writer_timeouts"].as_u64().unwrap() > 0 {
            assert!(
                allow_terminal,
                "writer timed out before byte-exhaustion stimulus entered"
            );
            return (
                probe["last_fault_generation"].as_u64().unwrap(),
                probe["last_fault_write_started_us"].as_u64().unwrap(),
                probe["last_fault_write_deadline_us"].as_u64().unwrap(),
            );
        }
        if probe["inflight_bytes"].as_u64().unwrap() > 0 && now.saturating_sub(start) > 200_000 {
            return (
                probe["status"]["link_generation"].as_u64().unwrap(),
                start,
                probe["write_deadline_us"].as_u64().unwrap(),
            );
        }
        assert!(
            tokio::time::Instant::now() < pressure_deadline,
            "peer never produced real socket backpressure"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}
