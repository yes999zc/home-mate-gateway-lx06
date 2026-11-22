//! Protocol v1 link-fault lamps (#12, 2026-11-16): a dead core is the
//! OFFLINE lamp (never a semantic state), and the Wi-Fi probe must NOT claim
//! "down" it cannot prove — on any machine without a radio control socket
//! the verdict is unknown, and unknown paints nothing.

use crate::support::*;
use home_mate_device_gateway::firmware::leds;
use home_mate_device_gateway::firmware::FirmwareConfig;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A core that accepts the TCP connection and vanishes WITHOUT an ack: the
/// handshake verdict is "offline" (not a protocol case — the core never
/// spoke). The lamp must be the dim-white OFFLINE color: never the reserved
/// green, never the reboot gray of a device that has not yet been judged.
#[tokio::test]
async fn a_core_that_vanishes_before_the_ack_is_offline() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes.clone());
    let sd = state_dir("link-offline");
    // A listener that accepts and immediately drops: connect succeeds,
    // hello goes out, the socket dies before any ack.
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (s, _) = l.accept().await.unwrap();
        drop(s); // EOF to the firmware right after its hello
    });
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: sd,
        ..Default::default()
    };
    spawn_firmware(cfg, Arc::new(hw));

    // First verdict must land inside HELLO_TIMEOUT_MS + slack, well inside
    // the reconnect pacing.
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        let painted = probes.lock().unwrap().leds.clone();
        assert!(
            painted
                .iter()
                .all(|f| f[0] != leds::state_rgb("listening")[0]),
            "a dead core must never show the reserved green"
        );
        if painted.iter().any(|f| f[0] == leds::OFFLINE_RGB) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the offline lamp never appeared; frames: {:?}",
            painted.iter().map(|f| f[0]).collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // The BOOT frame is legitimately the Reboot lamp (the device really is
    // starting then). After the first verdict, though, the gray must never
    // come BACK: reconnects keep the judged verdict.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let painted = probes.lock().unwrap().leds.clone();
    let first_offline = painted.iter().position(|f| f[0] == leds::OFFLINE_RGB);
    let reboot_frames = painted
        .iter()
        .filter(|f| f[0] == leds::FAULT_REBOOT_RGB)
        .count();
    let reboot_after_offline = match first_offline {
        Some(i) => painted[i..].iter().any(|f| f[0] == leds::FAULT_REBOOT_RGB),
        None => false,
    };
    assert!(
        !reboot_after_offline,
        "reconnects must not re-enter Starting (reboot frames total {reboot_frames})"
    );
}

/// Probe honesty: this test machine has no `/system/bin/wpa_cli`, so every
/// probe answer is "no answer". The three-state rule says unknown paints
/// NOTHING — the ring must never show the wifi_down magenta here, and the
/// offline/reboot lamps must be untouched by the probe.
#[tokio::test]
async fn a_probe_that_cannot_ask_never_claims_wifi_down() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes.clone());
    let sd = state_dir("link-wifi-probe");
    let (core, port) = spawn_fake_core().await;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: sd,
        ..Default::default()
    };
    spawn_firmware(cfg, Arc::new(hw));

    let _hello = core.recv().await;
    // Long enough for several probe rounds (1 s cadence, the fail-streak
    // fallback after 3) — no magenta may appear, with or without the core.
    tokio::time::sleep(Duration::from_millis(4500)).await;
    let painted = probes.lock().unwrap().leds.clone();
    assert!(
        !painted.iter().any(|f| f[0] == leds::FAULT_WIFI_RGB),
        "an unanswerable probe must not paint wifi_down"
    );
    assert!(
        !painted
            .iter()
            .any(|f| f[0] == leds::state_rgb("listening")[0]),
        "the probe must not paint semantic states either"
    );
}

/// When the core is connected (`link_fault == None`) and `wifi_down == true`,
/// `render_ring` must paint `FAULT_WIFI_RGB`. When the core link drops
/// (`Offline`), `Offline` must outrank `WifiDown`.
#[tokio::test]
async fn wifi_down_paints_magenta_when_connected_and_offline_outranks_it() {
    use home_mate_device_gateway::firmware::Firmware;
    use home_mate_device_gateway::protocol::{CoreFrame, GatewayFrame};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes.clone());
    let sd = state_dir("link-wifi-down-precedence");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: sd,
        ..Default::default()
    };
    let fw = Firmware::new(cfg, Arc::new(hw));
    let set_wifi_down = fw.test_wifi_down_setter();
    std::thread::spawn(move || {
        let _ = fw.run();
    });

    let (stream, _) = listener.accept().await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();

    // Handshake
    let _hello = lines.next_line().await.unwrap().unwrap();
    let ack = home_mate_device_gateway::protocol::encode_core(&CoreFrame::HelloAck {
        protocol: 0,
        min_protocol: 0,
        core_version: "test-core".into(),
        supported_versions: vec![],
        capabilities: vec![],
        generation: 1,
    })
    .unwrap();
    wr.write_all(ack.as_bytes()).await.unwrap();
    wr.write_all(b"\n").await.unwrap();

    // Wait for boot MuteState so we know link_fault is None (connected).
    loop {
        let line = lines.next_line().await.unwrap().unwrap();
        if let Ok(GatewayFrame::MuteState { source, .. }) = serde_json::from_str(&line) {
            if source == "device_boot" {
                break;
            }
        }
    }

    // Flip wifi_down = true while connected -> FAULT_WIFI_RGB must be painted.
    set_wifi_down(true);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let last = probes.lock().unwrap().leds.last().copied().unwrap();
    assert_eq!(
        last[0],
        leds::FAULT_WIFI_RGB,
        "wifi_down while connected must paint FAULT_WIFI_RGB, got {:?}",
        last[0]
    );

    // Drop the core connection and listener -> next dial fails -> Offline must outrank WifiDown.
    drop(wr);
    drop(lines);
    drop(listener);
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let last = probes.lock().unwrap().leds.last().copied().unwrap();
        if last[0] == leds::OFFLINE_RGB {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Offline did not override WifiDown; last={:?}",
            last[0]
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Runtime `WifiConfig` without `wifi` capability emits `ProtocolError`,
/// paints `FAULT_PROTOCOL_RGB`, drops the link, and paces at
/// `PROTOCOL_ERROR_RETRY_MS` (not 1s fast reconnect).
#[tokio::test]
async fn runtime_wifi_config_without_cap_emits_protocol_error_and_paces_slowly() {
    use home_mate_device_gateway::protocol::{CoreFrame, GatewayFrame};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes.clone());
    let sd = state_dir("link-wificonfig-refuse");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: sd,
        wifi: false,
        ..Default::default()
    };
    spawn_firmware(cfg, Arc::new(hw));

    let (stream, _) = listener.accept().await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();

    let _hello = lines.next_line().await.unwrap().unwrap();
    let ack = home_mate_device_gateway::protocol::encode_core(&CoreFrame::HelloAck {
        protocol: 0,
        min_protocol: 0,
        core_version: "test-core".into(),
        supported_versions: vec![],
        capabilities: vec![],
        generation: 1,
    })
    .unwrap();
    wr.write_all(ack.as_bytes()).await.unwrap();
    wr.write_all(b"\n").await.unwrap();

    loop {
        let line = lines.next_line().await.unwrap().unwrap();
        if let Ok(GatewayFrame::MuteState { source, .. }) = serde_json::from_str(&line) {
            if source == "device_boot" {
                break;
            }
        }
    }

    // Send WifiConfig to a build without `wifi` capability.
    let wcfg = home_mate_device_gateway::protocol::encode_core(&CoreFrame::WifiConfig {
        ssid: "ssid".into(),
        psk: "psk".into(),
    })
    .unwrap();
    wr.write_all(wcfg.as_bytes()).await.unwrap();
    wr.write_all(b"\n").await.unwrap();

    // Expect ProtocolError before EOF.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut got_err = None;
    loop {
        let rem = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!rem.is_zero(), "timed out waiting for ProtocolError");
        match tokio::time::timeout(rem, lines.next_line()).await {
            Ok(Ok(Some(line))) => {
                if let Ok(GatewayFrame::ProtocolError { reason, .. }) = serde_json::from_str(&line)
                {
                    got_err = Some(reason);
                    break;
                }
            }
            Ok(Ok(None)) => break,
            Ok(Err(e)) => panic!("read error: {e}"),
            Err(_) => panic!("timed out waiting for ProtocolError"),
        }
    }
    assert_eq!(
        got_err.as_deref(),
        Some("capability_missing:wifi"),
        "expected ProtocolError(capability_missing:wifi)"
    );

    // Wait for socket EOF.
    let eof = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
        .await
        .expect("firmware must close socket after ProtocolError")
        .unwrap();
    assert!(
        eof.is_none(),
        "expected EOF after ProtocolError, got {eof:?}"
    );

    // Lamp must be FAULT_PROTOCOL_RGB.
    let last = probes.lock().unwrap().leds.last().copied().unwrap();
    assert_eq!(
        last[0],
        leds::FAULT_PROTOCOL_RGB,
        "expected FAULT_PROTOCOL_RGB, got {:?}",
        last[0]
    );

    // Must NOT fast-reconnect at 1s (RECONNECT_MIN_MS).
    let reconnect = tokio::time::timeout(Duration::from_millis(2500), listener.accept()).await;
    assert!(
        reconnect.is_err(),
        "firmware fast-reconnected instead of pacing at PROTOCOL_ERROR_RETRY_MS"
    );
}

/// Mid-session `HelloReject` paints `FAULT_PROTOCOL_RGB`, disconnects, and
/// paces at `PROTOCOL_ERROR_RETRY_MS` (not 1s fast reconnect).
#[tokio::test]
async fn mid_session_hello_reject_paints_protocol_lamp_and_disconnects() {
    use home_mate_device_gateway::protocol::{CoreFrame, GatewayFrame};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes.clone());
    let sd = state_dir("link-mid-helloreject");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: sd,
        ..Default::default()
    };
    spawn_firmware(cfg, Arc::new(hw));

    let (stream, _) = listener.accept().await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();

    let _hello = lines.next_line().await.unwrap().unwrap();
    let ack = home_mate_device_gateway::protocol::encode_core(&CoreFrame::HelloAck {
        protocol: 0,
        min_protocol: 0,
        core_version: "test-core".into(),
        supported_versions: vec![],
        capabilities: vec![],
        generation: 1,
    })
    .unwrap();
    wr.write_all(ack.as_bytes()).await.unwrap();
    wr.write_all(b"\n").await.unwrap();

    loop {
        let line = lines.next_line().await.unwrap().unwrap();
        if let Ok(GatewayFrame::MuteState { source, .. }) = serde_json::from_str(&line) {
            if source == "device_boot" {
                break;
            }
        }
    }

    // Send mid-session HelloReject.
    let rej = home_mate_device_gateway::protocol::encode_core(&CoreFrame::HelloReject {
        protocol: 0,
        min_protocol: 0,
        core_version: "test-core".into(),
        reason: "revoked mid-flight".into(),
    })
    .unwrap();
    wr.write_all(rej.as_bytes()).await.unwrap();
    wr.write_all(b"\n").await.unwrap();

    // Firmware must close the socket.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let rem = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !rem.is_zero(),
            "firmware did not close socket on HelloReject"
        );
        match tokio::time::timeout(rem, lines.next_line()).await {
            Ok(Ok(Some(_))) => continue,
            Ok(Ok(None)) => break,
            Ok(Err(e)) => panic!("read error: {e}"),
            Err(_) => panic!("firmware did not close socket on HelloReject"),
        }
    }

    let last = probes.lock().unwrap().leds.last().copied().unwrap();
    assert_eq!(
        last[0],
        leds::FAULT_PROTOCOL_RGB,
        "expected FAULT_PROTOCOL_RGB, got {:?}",
        last[0]
    );

    // Must NOT fast-reconnect at 1s.
    let reconnect = tokio::time::timeout(Duration::from_millis(2500), listener.accept()).await;
    assert!(
        reconnect.is_err(),
        "firmware fast-reconnected instead of pacing at PROTOCOL_ERROR_RETRY_MS"
    );
}

/// #16: stalling core reads while capturing fills the bounded audio channel
/// (`UPLINK_AUDIO_CAP`) and increments `dropped_audio_frames`, while control
/// frames (`MuteState`) emitted during the stall still arrive when reads resume.
#[tokio::test]
async fn uplink_backpressure_drops_audio_but_preserves_control_frames() {
    use home_mate_device_gateway::firmware::Firmware;
    use home_mate_device_gateway::protocol::{
        decode_gateway, encode_core, CoreFrame, GatewayFrame, PROTOCOL_MIN_VERSION,
        PROTOCOL_VERSION,
    };
    use std::sync::atomic::Ordering;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let sock = tokio::net::TcpSocket::new_v4().unwrap();
    sock.set_recv_buffer_size(4096).unwrap();
    sock.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = sock.listen(4).unwrap();
    let port = listener.local_addr().unwrap().port();

    let probes = Arc::new(Mutex::new(Probes::default()));
    let (mut hw, _btn) = FakeHardware::new(probes);
    hw.capture_pace_ms = 2;
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: state_dir("uplink-drop"),
        loudness: false,
        ..Default::default()
    };
    let fw = Firmware::new(cfg, Arc::new(hw));
    let dropped = fw.test_dropped_audio_counter();
    std::thread::spawn(move || {
        let _ = fw.run();
    });

    let (stream, _) = listener.accept().await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();

    // Handshake.
    let hello_line = lines.next_line().await.unwrap().unwrap();
    assert!(matches!(
        decode_gateway(&hello_line).unwrap(),
        GatewayFrame::Hello { .. }
    ));
    let ack = encode_core(&CoreFrame::HelloAck {
        protocol: PROTOCOL_VERSION,
        min_protocol: PROTOCOL_MIN_VERSION,
        core_version: "test-core".into(),
        supported_versions: vec![],
        capabilities: vec![],
        generation: 1,
    })
    .unwrap();
    wr.write_all(ack.as_bytes()).await.unwrap();
    wr.write_all(b"\n").await.unwrap();

    // Start mic session so capture emits 80 ms AudioChunks.
    let start_mic = encode_core(&CoreFrame::MicSession {
        device: "dot2-d".into(),
        action: "start".into(),
        mode: None,
    })
    .unwrap();
    wr.write_all(start_mic.as_bytes()).await.unwrap();
    wr.write_all(b"\n").await.unwrap();

    // Read until at least one AudioChunk arrives.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        assert!(tokio::time::Instant::now() < deadline, "no AudioChunk");
        let line = lines.next_line().await.unwrap().unwrap();
        if matches!(decode_gateway(&line), Ok(GatewayFrame::AudioChunk { .. })) {
            break;
        }
    }

    // Stall core reads until the bounded audio queue overflows and drops.
    let drop_deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if dropped.load(Ordering::Relaxed) > 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < drop_deadline,
            "expected dropped_audio_frames > 0 during read stall"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Emit a control command while stalled; MuteState must NOT be dropped.
    let mute_cmd = encode_core(&CoreFrame::MuteSet {
        task_id: "m-stall".into(),
        device: "dot2-d".into(),
        muted: true,
    })
    .unwrap();
    wr.write_all(mute_cmd.as_bytes()).await.unwrap();
    wr.write_all(b"\n").await.unwrap();

    // Resume reads and verify MuteState arrives.
    let mute_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let rem = mute_deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!rem.is_zero(), "MuteState never arrived after read stall");
        let line = tokio::time::timeout(rem, lines.next_line())
            .await
            .expect("read timeout")
            .unwrap()
            .unwrap();
        if let Ok(GatewayFrame::MuteState {
            task_id: Some(tid),
            muted: true,
            ..
        }) = decode_gateway(&line)
        {
            if tid == "m-stall" {
                break;
            }
        }
    }
}

/// #20: if the north writer fails (e.g. peer reset / write error), the
/// firmware tears down the full connection, paints `HealthLamp::Offline`, and
/// reconnects with a fresh `Hello`.
#[tokio::test]
async fn writer_failure_tears_down_connection_and_reconnects() {
    use home_mate_device_gateway::protocol::{
        decode_gateway, encode_core, CoreFrame, GatewayFrame, PROTOCOL_MIN_VERSION,
        PROTOCOL_VERSION,
    };
    use std::os::fd::AsRawFd;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes.clone());
    let cfg = FirmwareConfig {
        core_addr: format!("127.0.0.1:{port}"),
        state_dir: state_dir("writer-fail"),
        loudness: false,
        ..Default::default()
    };
    spawn_firmware(cfg, Arc::new(hw));

    // Session 1: complete handshake, then force TCP RST via SO_LINGER(0).
    let (stream1, _) = listener.accept().await.unwrap();
    let (rd1, mut wr1) = stream1.into_split();
    let mut lines1 = BufReader::new(rd1).lines();
    let hello1 = lines1.next_line().await.unwrap().unwrap();
    assert!(matches!(
        decode_gateway(&hello1).unwrap(),
        GatewayFrame::Hello { .. }
    ));
    let ack = encode_core(&CoreFrame::HelloAck {
        protocol: PROTOCOL_VERSION,
        min_protocol: PROTOCOL_MIN_VERSION,
        core_version: "test-core".into(),
        supported_versions: vec![],
        capabilities: vec![],
        generation: 1,
    })
    .unwrap();
    wr1.write_all(ack.as_bytes()).await.unwrap();
    wr1.write_all(b"\n").await.unwrap();
    let status1 = lines1.next_line().await.unwrap().unwrap();
    assert!(matches!(
        decode_gateway(&status1).unwrap(),
        GatewayFrame::DeviceStatus { online: true, .. }
    ));

    let stream1 = wr1.reunite(lines1.into_inner().into_inner()).unwrap();
    let std_stream = stream1.into_std().unwrap();
    let linger = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    unsafe {
        libc::setsockopt(
            std_stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            &linger as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::linger>() as libc::socklen_t,
        );
    }
    drop(std_stream);

    // Offline lamp must be painted, and firmware must reconnect with a fresh Hello.
    let (stream2, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("firmware did not reconnect after writer failure")
        .unwrap();
    let mut lines2 = BufReader::new(stream2).lines();
    let hello2 = lines2.next_line().await.unwrap().unwrap();
    assert!(
        matches!(decode_gateway(&hello2).unwrap(), GatewayFrame::Hello { .. }),
        "expected fresh Hello on reconnect"
    );
    assert!(
        probes
            .lock()
            .unwrap()
            .leds
            .iter()
            .any(|f| f[0] == leds::OFFLINE_RGB),
        "expected HealthLamp::Offline after writer failure"
    );
}
