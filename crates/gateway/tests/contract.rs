//! G1 contract tests: a minimal in-test "core" (TCP listener speaking
//! protocol v0) exercises the gateway side end-to-end over a real socket —
//! handshake, capabilities, play/stop semantics, honest receipts, mode
//! negotiation and reconnect generations. These tests are independent of the
//! real core (repos share the documented contract, not code).

use home_mate_device_gateway::client::{self, GatewayConfig};
use home_mate_device_gateway::protocol::*;
use home_mate_device_gateway::terminal::VirtualDot2d;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};

/// One accepted fake-core connection: read gateway frames, let the test
/// reply with core frames.
struct FakeCore {
    rx: mpsc::Receiver<GatewayFrame>,
    write: tokio::net::tcp::OwnedWriteHalf,
}

impl FakeCore {
    async fn send(&mut self, f: &CoreFrame) {
        let line = encode_core(f).unwrap();
        self.write.write_all(line.as_bytes()).await.unwrap();
        self.write.write_all(b"\n").await.unwrap();
    }

    async fn recv(&mut self) -> GatewayFrame {
        self.rx
            .recv()
            .await
            .expect("gateway frame expected (connection may have died)")
    }

    /// Collect until a predicate matches; returns (matching, all_seen).
    async fn until(
        &mut self,
        pred: impl Fn(&GatewayFrame) -> bool,
        budget: Duration,
    ) -> (GatewayFrame, Vec<GatewayFrame>) {
        let deadline = tokio::time::Instant::now() + budget;
        let mut seen = Vec::new();
        loop {
            let f = tokio::time::timeout_at(deadline, self.recv())
                .await
                .expect("timeout waiting for expected frame");
            let hit = pred(&f);
            seen.push(f);
            if hit {
                let n = seen.len();
                return (seen[n - 1].clone(), seen);
            }
        }
    }
}

async fn spawn_fake_core() -> (u16, mpsc::Receiver<FakeCore>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (conns_tx, conns_rx) = mpsc::channel(4);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (rd, wr) = stream.into_split();
            let (tx, rx) = mpsc::channel::<GatewayFrame>(64);
            let reader_tx = tx.clone();
            conns_tx.send(FakeCore { rx, write: wr }).await.ok();
            let mut lines = BufReader::new(rd).lines();
            tokio::spawn(async move {
                while let Ok(Some(line)) = lines.next_line().await {
                    match decode_gateway(&line) {
                        Ok(f) => {
                            if reader_tx.send(f).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "bad frame from gateway"),
                    }
                }
            });
        }
    });
    (port, conns_rx)
}

/// Start the gateway client pointed at the fake core; returns the device
/// handle and an event channel. The returned shutdown sender MUST be held
/// for the test's lifetime — dropping it stops the connection actor.
async fn start_gateway(
    port: u16,
) -> (
    Arc<Mutex<VirtualDot2d>>,
    mpsc::Sender<GatewayFrame>,
    mpsc::Sender<()>,
) {
    let cfg = GatewayConfig {
        core_addr: format!("127.0.0.1:{port}"),
        out_dir: Some(std::env::temp_dir().join(format!("gw-test-{port}"))),
        afplay: false,
        min_backoff: Duration::from_millis(50),
        max_backoff: Duration::from_millis(200),
        ..Default::default()
    };
    let device = Arc::new(Mutex::new(VirtualDot2d::new("dot2-d")));
    let (tx, rx) = mpsc::channel(16);
    let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
    let d = device.clone();
    tokio::spawn(async move { client::run(cfg, d, rx, shutdown_rx).await });
    (device, tx, shutdown_tx)
}

async fn handshake(core: &mut FakeCore, expect_generation: u64) {
    match core.recv().await {
        GatewayFrame::Hello {
            protocol, devices, ..
        } => {
            assert_eq!(protocol, PROTOCOL_VERSION);
            let d = &devices[0];
            assert_eq!(d.stable_id, "dot2-d");
            assert!(d.capabilities.contains(&"audio_out".to_string()));
            assert!(d.capabilities.contains(&"wake_event".to_string()));
            assert!(
                !d.capabilities.contains(&"audio_in".to_string()),
                "virtual terminal must not fake a mic"
            );
        }
        f => panic!("expected hello, got {f:?}"),
    }
    core.send(&CoreFrame::HelloAck {
        protocol: PROTOCOL_VERSION,
        min_protocol: PROTOCOL_MIN_VERSION,
        core_version: "test-core".into(),
        supported_versions: vec![],
        capabilities: vec![],
        generation: expect_generation,
    })
    .await;
}

/// Minimal valid RIFF/WAVE (s16 mono 16 kHz).
fn wav_bytes(data_len: usize) -> Vec<u8> {
    let mut w = vec![0u8; 44 + data_len];
    w[0..4].copy_from_slice(b"RIFF");
    w[4..8].copy_from_slice(&(36 + data_len as u32).to_le_bytes());
    w[8..12].copy_from_slice(b"WAVE");
    w[12..16].copy_from_slice(b"fmt ");
    w[16..20].copy_from_slice(&16u32.to_le_bytes());
    w[20..22].copy_from_slice(&1u16.to_le_bytes());
    w[22..24].copy_from_slice(&1u16.to_le_bytes());
    w[24..28].copy_from_slice(&16000u32.to_le_bytes());
    w[28..32].copy_from_slice(&32000u32.to_le_bytes());
    w[32..34].copy_from_slice(&2u16.to_le_bytes());
    w[34..36].copy_from_slice(&16u16.to_le_bytes());
    w[36..40].copy_from_slice(b"data");
    w[40..44].copy_from_slice(&(data_len as u32).to_le_bytes());
    w
}

fn tiny_wav_play(task: &str, channel: &str) -> CoreFrame {
    // 16000 bytes of data ≈ 500 ms at 16 kHz mono s16.
    let wav = wav_bytes(16000);
    CoreFrame::Play {
        task_id: task.into(),
        device: "dot2-d".into(),
        content_ref: format!("tts:{task}"),
        audio: AudioMeta {
            format: "wav".into(),
            sample_rate: 16000,
            channels: 1,
        },
        audio_b64: Some(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            wav,
        )),
        channel: Some(channel.into()),
        text: Some("测试".into()),
        simulated_ms: None,
        barge_in: Some("stop_current".into()),
    }
}

#[tokio::test]
async fn full_voice_flow_receipts_and_finish() {
    let (port, mut conns) = spawn_fake_core().await;
    let (dev, ev, _stop) = start_gateway(port).await;
    let mut core = conns.recv().await.unwrap();
    handshake(&mut core, 1).await;

    // wake + utterance arrive with monotonic seq (stamped by the device).
    ev.send(dev.lock().await.wake()).await.unwrap();
    match core.recv().await {
        GatewayFrame::Wake { device, seq, .. } => {
            assert_eq!(device, "dot2-d");
            assert_eq!(seq, 1);
        }
        f => panic!("{f:?}"),
    }
    ev.send(dev.lock().await.utterance("讲个睡前故事"))
        .await
        .unwrap();
    match core.recv().await {
        GatewayFrame::Utterance {
            seq, source, text, ..
        } => {
            assert_eq!(seq, 2);
            assert_eq!(source, "text");
            assert_eq!(text, "讲个睡前故事");
        }
        f => panic!("{f:?}"),
    }

    // Core plays the answer; expect accepted → device_confirmed → finished.
    core.send(&tiny_wav_play("T1", "voice")).await;
    let (confirmed, _) = core
        .until(
            |f| matches!(f, GatewayFrame::PlayReceipt { stage, task_id, .. } if stage == "device_confirmed" && task_id == "T1"),
            Duration::from_secs(2),
        )
        .await;
    assert!(matches!(confirmed, GatewayFrame::PlayReceipt { .. }));
    let (fin, _) = core
        .until(
            |f| matches!(f, GatewayFrame::PlayFinished { task_id, .. } if task_id == "T1"),
            Duration::from_secs(2),
        )
        .await;
    match fin {
        GatewayFrame::PlayFinished { played_ms, .. } => assert_eq!(played_ms, 500),
        f => panic!("{f:?}"),
    }
}

#[tokio::test]
async fn stop_mid_play_confirms_and_kills_finish() {
    let (port, mut conns) = spawn_fake_core().await;
    let (_dev, _ev, _stop) = start_gateway(port).await;
    let mut core = conns.recv().await.unwrap();
    handshake(&mut core, 1).await;

    core.send(&tiny_wav_play("T2", "voice")).await;
    core.until(
        |f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "T2" && stage == "device_confirmed"),
        Duration::from_secs(2),
    )
    .await;

    core.send(&CoreFrame::Stop {
        task_id: "T2".into(),
        flush: true,
    })
    .await;
    let (receipt, seen) = core
        .until(|f| matches!(f, GatewayFrame::StopReceipt { task_id, stage, .. } if task_id == "T2" && stage == "device_confirmed"), Duration::from_secs(2))
        .await;
    assert!(matches!(receipt, GatewayFrame::StopReceipt { .. }));

    // No play_finished may follow the stop within the playback window.
    tokio::time::sleep(Duration::from_millis(700)).await;
    for f in seen {
        assert!(!matches!(&f, GatewayFrame::PlayFinished { task_id, .. } if task_id == "T2"));
    }
}

#[tokio::test]
async fn volume_and_mode_negotiate_honestly() {
    let (port, mut conns) = spawn_fake_core().await;
    let (_dev, _ev, _stop) = start_gateway(port).await;
    let mut core = conns.recv().await.unwrap();
    handshake(&mut core, 1).await;

    core.send(&CoreFrame::VolumeSet {
        task_id: "T3".into(),
        device: "dot2-d".into(),
        value: 1.7, // out of range: device clamps, reports effective truth
        unit: "linear".into(),
    })
    .await;
    let (r, _) = core
        .until(
            |f| matches!(f, GatewayFrame::VolumeReceipt { .. }),
            Duration::from_secs(2),
        )
        .await;
    match r {
        GatewayFrame::VolumeReceipt {
            effective, stage, ..
        } => {
            assert_eq!(effective, 1.0);
            assert_eq!(stage, "device_confirmed");
        }
        f => panic!("{f:?}"),
    }

    core.send(&CoreFrame::ModeSet {
        device: "dot2-d".into(),
        duplex_requested: "full".into(),
    })
    .await;
    let (r, _) = core
        .until(
            |f| matches!(f, GatewayFrame::ModeReport { .. }),
            Duration::from_secs(2),
        )
        .await;
    match r {
        GatewayFrame::ModeReport { duplex, .. } => {
            assert_eq!(duplex.capability, "half");
            assert_eq!(duplex.requested, "full");
            assert_eq!(duplex.negotiated, "half");
            assert_eq!(duplex.effective, "half");
        }
        f => panic!("{f:?}"),
    }
}

/// Capability mismatch: a play addressed to a device this gateway doesn't
/// host must fail (not fake a confirmation).
#[tokio::test]
async fn play_for_unknown_device_fails() {
    let (port, mut conns) = spawn_fake_core().await;
    let (_dev, _ev, _stop) = start_gateway(port).await;
    let mut core = conns.recv().await.unwrap();
    handshake(&mut core, 1).await;

    let mut f = tiny_wav_play("T4", "voice");
    if let CoreFrame::Play { device, .. } = &mut f {
        *device = "someone-else".into();
    }
    core.send(&f).await;
    let (r, _) = core
        .until(
            |f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "T4" && stage == "failed"),
            Duration::from_secs(2),
        )
        .await;
    assert!(matches!(r, GatewayFrame::PlayReceipt { stage, .. } if stage == "failed"));
}

/// Reconnect: after the fake core drops the socket the gateway re-hellos;
/// the core's generation counter increments; buffered playback from the old
/// generation is gone (no late finish, no replayed events).
#[tokio::test]
async fn reconnect_gets_new_generation_and_clears_state() {
    let (port, mut conns) = spawn_fake_core().await;
    let (dev, _ev, _stop) = start_gateway(port).await;
    let mut core = conns.recv().await.unwrap();
    handshake(&mut core, 7).await;

    // Start a long playback, then kill the connection mid-play.
    let mut long = tiny_wav_play("T5", "media");
    if let CoreFrame::Play {
        audio_b64,
        simulated_ms,
        ..
    } = &mut long
    {
        *audio_b64 = None;
        *simulated_ms = Some(5000);
    }
    core.send(&long).await;
    core.until(
        |f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "T5" && stage == "device_confirmed"),
        Duration::from_secs(2),
    )
    .await;
    drop(core); // closes the socket

    let core2 = conns.recv().await.unwrap();
    let mut core2 = core2;
    handshake(&mut core2, 8).await;

    // The old generation's playback must not produce a finish on the new one.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        dev.lock().await.active().is_empty(),
        "old-generation playback cleared on reconnect"
    );

    // New connection is fully usable.
    core2.send(&tiny_wav_play("T6", "voice")).await;
    core2
        .until(
            |f| matches!(f, GatewayFrame::PlayReceipt { task_id, stage, .. } if task_id == "T6" && stage == "device_confirmed"),
            Duration::from_secs(2),
        )
        .await;
}

/// v1 mechanism (#11): compatibility is interval intersection, so a core
/// that speaks a HIGHER version but still accepts 0 is FINE (this is the
/// mixed activation window); only a DISJOINT interval aborts, with the
/// reason on the error. A core-side HelloReject passes its reason through.
#[tokio::test]
async fn a_higher_core_that_still_accepts_zero_negotiates() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();
        // A future core: speaks 1, accepts >= 0 — intersects [0,0] at 0.
        if let Ok(Some(_hello)) = lines.next_line().await {
            let ack = encode_core(&CoreFrame::HelloAck {
                protocol: 1,
                min_protocol: 0,
                core_version: "9.9.9".into(),
                supported_versions: vec![],
                capabilities: vec![],
                generation: 1,
            })
            .unwrap();
            wr.write_all(ack.as_bytes()).await.unwrap();
            wr.write_all(b"\n").await.unwrap();
            // Hold the socket open; the client proceeds past the handshake.
            let _ = tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    });
    let cfg = GatewayConfig {
        core_addr: format!("127.0.0.1:{port}"),
        min_backoff: Duration::from_millis(20),
        max_backoff: Duration::from_millis(50),
        ..Default::default()
    };
    let device = Arc::new(Mutex::new(VirtualDot2d::new("dot2-d")));
    let (_tx, mut events) = mpsc::channel(1);
    let (_sx, mut shutdown) = mpsc::channel(1);
    let result = client::connect_and_serve(&cfg, device, &mut events, &mut shutdown).await;
    assert!(
        result.is_ok(),
        "a core accepting 0 must negotiate even from a higher version: {result:?}"
    );
}

#[tokio::test]
async fn a_disjoint_version_interval_aborts_with_the_reason() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();
        // A far-future core: accepts only [5,99] — no intersection with [0,0].
        if let Ok(Some(_hello)) = lines.next_line().await {
            let ack = encode_core(&CoreFrame::HelloAck {
                protocol: 99,
                min_protocol: 5,
                core_version: "9.9.9".into(),
                supported_versions: vec![],
                capabilities: vec![],
                generation: 1,
            })
            .unwrap();
            wr.write_all(ack.as_bytes()).await.unwrap();
            wr.write_all(b"\n").await.unwrap();
        }
    });
    let cfg = GatewayConfig {
        core_addr: format!("127.0.0.1:{port}"),
        min_backoff: Duration::from_millis(20),
        max_backoff: Duration::from_millis(50),
        ..Default::default()
    };
    let device = Arc::new(Mutex::new(VirtualDot2d::new("dot2-d")));
    let (_tx, mut events) = mpsc::channel(1);
    let (_sx, mut shutdown) = mpsc::channel(1);
    let result = client::connect_and_serve(&cfg, device, &mut events, &mut shutdown).await;
    let err = result.expect_err("a disjoint interval must abort the handshake");
    assert!(
        format!("{err}").contains("protocol_no_intersection"),
        "the refusal must carry the reason: {err}"
    );
}

#[tokio::test]
async fn a_hello_reject_from_the_core_aborts_with_the_reason() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();
        if let Ok(Some(_hello)) = lines.next_line().await {
            let reject = encode_core(&CoreFrame::HelloReject {
                protocol: 0,
                min_protocol: 0,
                core_version: "0.0.1".into(),
                reason: "duplicate device id 'dot2-d'".into(),
            })
            .unwrap();
            wr.write_all(reject.as_bytes()).await.unwrap();
            wr.write_all(b"\n").await.unwrap();
        }
    });
    let cfg = GatewayConfig {
        core_addr: format!("127.0.0.1:{port}"),
        min_backoff: Duration::from_millis(20),
        max_backoff: Duration::from_millis(50),
        ..Default::default()
    };
    let device = Arc::new(Mutex::new(VirtualDot2d::new("dot2-d")));
    let (_tx, mut events) = mpsc::channel(1);
    let (_sx, mut shutdown) = mpsc::channel(1);
    let result = client::connect_and_serve(&cfg, device, &mut events, &mut shutdown).await;
    let err = result.expect_err("a HelloReject must abort");
    assert!(
        format!("{err}").contains("duplicate device id"),
        "the reason must reach the client: {err}"
    );
}
