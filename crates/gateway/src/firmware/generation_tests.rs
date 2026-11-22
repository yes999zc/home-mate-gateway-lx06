//! Negative controls against production admission, decoder, writer and event paths.
use super::playback::{decode_one, PlayAction, PlayCmd, PlayEvent};
use super::*;

#[derive(Default)]
struct HardwareProbe {
    shared_sink: bool,
    drain_allowed: AtomicBool,
    drain_calls: AtomicU64,
    silence_writes: AtomicU64,
    nonzero: AtomicU64,
    flushes: AtomicU64,
}
impl Hardware for HardwareProbe {
    fn capture_read(&self, _: &mut [u8], _: u32) -> io::Result<u32> {
        Ok(0)
    }
    fn playback_write(&self, samples: &[i16], frames: u32) -> io::Result<u32> {
        if samples.iter().any(|v| *v != 0) {
            self.nonzero.fetch_add(1, Ordering::Relaxed);
        } else {
            self.silence_writes.fetch_add(1, Ordering::Relaxed);
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
        Ok(frames)
    }
    fn playback_flush(&self) -> io::Result<()> {
        self.flushes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    fn playback_releases_when_idle(&self) -> bool {
        self.shared_sink
    }
    fn playback_drain(&self) -> io::Result<()> {
        self.drain_calls.fetch_add(1, Ordering::Release);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while self.shared_sink && !self.drain_allowed.load(Ordering::Acquire) {
            if std::time::Instant::now() >= until {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "test drain gate"));
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Ok(())
    }
    fn playback_underruns(&self) -> u64 {
        0
    }
    fn led_write(&self, _: &[(u8, u8, u8); 12]) -> io::Result<()> {
        Ok(())
    }
    fn button_next(&self) -> io::Result<ButtonEvent> {
        Err(io::Error::other("fixture"))
    }
    fn wake_lock(&self) -> io::Result<()> {
        Ok(())
    }
}
fn begin() -> CoreFrame {
    serde_json::from_value(json!({"type":"play_begin","task_id":"same","device":"fixture-dot","content_ref":"fixture","audio":{"format":"pcm_s16le","sample_rate":16000,"channels":1}})).unwrap()
}
fn chunk() -> CoreFrame {
    use base64::Engine;
    serde_json::from_value(json!({"type":"play_chunk","task_id":"same","seq":0,"audio_b64":base64::engine::general_purpose::STANDARD.encode(vec![16;4096])})).unwrap()
}
fn end() -> CoreFrame {
    serde_json::from_value(json!({"type":"play_end","task_id":"same"})).unwrap()
}

#[test]
fn same_task_reconnect_rejects_old_chunk_end_whole_decode_and_terminal_events() {
    let (role, rx) = role::isolated_role(Arc::new(HardwareProbe::default()));
    let old = NorthTx::new(
        "fixture-dot".into(),
        1,
        role.northbound_stats.clone(),
        role.dropped_audio.clone(),
    );
    {
        let mut sh = role.shared.lock().unwrap();
        sh.link_gen = 1;
        sh.north_tx = Some(old.clone());
    }
    role.on_core_frame(begin(), &old).unwrap();
    let stamp = role.shared.lock().unwrap().stamp(Chan::Voice);
    let old_chunk = role.on_core_frame(chunk(), &old).unwrap().unwrap();
    let old_end = role.on_core_frame(end(), &old).unwrap().unwrap();
    let old_whole = DecodeCmd::Play {
        task_id: "same".into(),
        chan: Chan::Voice,
        audio_b64: None,
        simulated_ms: Some(300),
        sample_rate: 48000,
        channels: 1,
        stamp,
        north: old.clone(),
    };
    old.fail("peer_closed");
    role.teardown_link();
    while rx.try_recv().is_ok() {}
    let fresh = NorthTx::new(
        "fixture-dot".into(),
        3,
        role.northbound_stats.clone(),
        role.dropped_audio.clone(),
    );
    {
        let mut sh = role.shared.lock().unwrap();
        sh.link_gen = 3;
        sh.north_tx = Some(fresh.clone());
    }
    role.on_core_frame(begin(), &fresh).unwrap();
    let fresh_stamp = role.shared.lock().unwrap().stamp(Chan::Voice);
    decode_one(&role, old_chunk);
    decode_one(&role, old_end);
    decode_one(&role, old_whole);
    assert!(
        rx.try_recv().is_err(),
        "stale decode published PCM/flush/EOS"
    );
    {
        let streams = role.streams.lock().unwrap();
        let state = streams["same"].state.lock().unwrap();
        assert_eq!((state.next_seq, state.pushed), (0, 0));
    }
    {
        let mut sh = role.shared.lock().unwrap();
        sh.pending_stop[0] = Some("same".into());
    }
    role.on_play_event(PlayEvent::StreamDone {
        stamp,
        chan: Chan::Voice,
        bytes: 9600,
        periods: 2,
        underruns: 0,
        delay_frames: None,
    });
    role.on_play_event(PlayEvent::FlushDone {
        stamp,
        chan: Chan::Voice,
        played_ms: 100,
        delay_frames: None,
        flush_err: None,
        at_ms: now_ms(),
    });
    role.on_play_event(PlayEvent::WriteFailed {
        stamps: [stamp, stamp],
        detail: "old writer".into(),
    });
    {
        let sh = role.shared.lock().unwrap();
        assert_eq!(sh.active[0].as_ref().unwrap().0, "same");
        assert_eq!(sh.pending_stop[0].as_deref(), Some("same"));
    }
    assert!(role.streams.lock().unwrap().contains_key("same"));
    assert!(
        rx.try_recv().is_err(),
        "stale completion flushed the new execution"
    );
    let current = role.on_core_frame(chunk(), &fresh).unwrap().unwrap();
    decode_one(&role, current);
    let pcm = rx.try_recv().unwrap();
    assert_eq!(pcm.stamp, fresh_stamp);
    assert!(matches!(pcm.action,PlayAction::Samples{samples,..} if !samples.is_empty()));
    let current = role.on_core_frame(end(), &fresh).unwrap().unwrap();
    decode_one(&role, current);
    assert!(!role.streams.lock().unwrap().contains_key("same"));
    assert!(rx
        .try_iter()
        .any(|cmd| cmd.stamp == fresh_stamp && matches!(cmd.action, PlayAction::Eos(_))));
    role.on_play_event(PlayEvent::FlushDone {
        stamp: fresh_stamp,
        chan: Chan::Voice,
        played_ms: 80,
        delay_frames: None,
        flush_err: None,
        at_ms: now_ms(),
    });
    assert!(
        role.shared.lock().unwrap().active[0].is_none(),
        "current completion must settle its task"
    );
    assert_eq!(
        old.send_ctrl(GatewayFrame::PlayReceipt {
            task_id: "same".into(),
            stage: "failed".into(),
            detail: None,
            ts: 0
        }),
        northbound::Admission::Closed
    );
}

#[test]
fn playback_rejects_old_pcm_end_and_flush_after_fresh_samples_are_installed() {
    let hw = Arc::new(HardwareProbe::default());
    let (role, rx) = role::isolated_role(hw.clone());
    {
        let mut sh = role.shared.lock().unwrap();
        sh.link_gen = 3;
        sh.decode_gen[0] = 7;
    }
    let current = role.shared.lock().unwrap().stamp(Chan::Voice);
    let old = PlayStamp {
        link: 1,
        channel: 7,
    };
    let send = |stamp, action| role.play_tx.send(PlayCmd { stamp, action }).unwrap();
    send(
        current,
        PlayAction::Samples {
            chan: Chan::Voice,
            samples: vec![1000; PLAY_PERIOD as usize * 2],
        },
    );
    send(
        old,
        PlayAction::Samples {
            chan: Chan::Voice,
            samples: vec![30000; PLAY_PERIOD as usize * 2],
        },
    );
    send(old, PlayAction::Eos(Chan::Voice));
    send(old, PlayAction::Flush(Chan::Voice));
    send(current, PlayAction::Eos(Chan::Voice));
    let (events, event_rx) = mpsc::unbounded_channel();
    let observer = hw.clone();
    let shared = role.shared.clone();
    let progress = role.progress.clone();
    let fault = role.fault.clone();
    let alive = role.playback_alive.clone();
    let handle = std::thread::spawn(move || {
        playback_thread(
            hw,
            rx,
            events,
            shared,
            None,
            1.0,
            false,
            0.0,
            None,
            None,
            vec![],
            false,
            None,
            progress,
            fault,
            alive,
        )
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut event_rx = event_rx;
    let mut completed = false;
    while std::time::Instant::now() < deadline {
        if let Ok(PlayEvent::StreamDone { stamp, bytes, .. }) = event_rx.try_recv() {
            assert_eq!(stamp, current);
            assert_eq!(bytes, PLAY_PERIOD as u64 * 4);
            completed = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(completed, "fresh stream failed to finish");
    assert!(observer.nonzero.load(Ordering::Relaxed) > 0);
    assert_eq!(observer.flushes.load(Ordering::Relaxed), 0);
    // Old flush must not clear the fresh PCM or call the hardware at all.
    assert_eq!(role.shared.lock().unwrap().link_gen, 3);
    drop(role);
    handle.join().unwrap();
}

async fn fatal_observer_case(blocked_decode: bool) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let (mut role, _play_rx) = role::isolated_role(Arc::new(HardwareProbe::default()));
    let (decode_tx, _decode_rx) = mpsc::channel(role::DECODE_QUEUE_CAP);
    if blocked_decode {
        for _ in 0..role::DECODE_QUEUE_CAP {
            decode_tx
                .try_send(DecodeCmd::PlayEnd {
                    task_id: "queued".into(),
                    stamp: PlayStamp {
                        link: 0,
                        channel: 0,
                    },
                })
                .ok()
                .unwrap();
        }
    }
    role.decode_tx = decode_tx;
    role.northbound_stats
        .set_decode_queue(role.decode_tx.downgrade());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (peer, _) = listener.accept().await.unwrap();
    let (rd, wr) = gateway.into_split();
    let (peer_rd, mut peer_wr) = peer.into_split();
    let mut lines = BufReader::new(peer_rd).lines();
    let tx = NorthTx::new(
        "fixture-dot".into(),
        1,
        role.northbound_stats.clone(),
        role.dropped_audio.clone(),
    );
    let serve_tx = tx.clone();
    let serve_role = role.clone();
    let serve =
        tokio::spawn(async move { north_link::serve_link(&serve_role, rd, wr, serve_tx).await });
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&lines.next_line().await.unwrap().unwrap())
            .unwrap()["type"],
        "hello"
    );
    peer_wr.write_all(b"{\"type\":\"hello_ack\",\"protocol\":0,\"min_protocol\":0,\"core_version\":\"test\",\"generation\":1}\n").await.unwrap();
    loop {
        let frame: GatewayFrame =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        if matches!(frame, GatewayFrame::MuteState { .. }) {
            break;
        }
    }
    assert_eq!(role.northbound_stats.probe()["handshake_completed"], true);
    if blocked_decode {
        for frame in [begin(), end()] {
            let mut line = serde_json::to_vec(&frame).unwrap();
            line.push(b'\n');
            peer_wr.write_all(&line).await.unwrap();
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        while role.northbound_stats.probe()["decode_waiters"] != 1 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "production decoder capacity select was never entered"
            );
            tokio::task::yield_now().await;
        }
    }
    tx.fail("critical_count_full");
    let result = tokio::time::timeout(std::time::Duration::from_millis(100), serve)
        .await
        .expect("fatal latch failed to end production select")
        .unwrap();
    assert!(result.is_err());
    let stats = role.northbound_stats.probe();
    assert_eq!(stats["reader_tasks"], 0);
    assert_eq!(stats["writer_tasks"], 0);
    assert_eq!(stats["decode_waiters"], 0);
    assert_eq!(tx.reason(), "critical_count_full");
}
#[tokio::test]
async fn fatal_latch_ends_real_outer_reader_select() {
    fatal_observer_case(false).await;
}
#[tokio::test]
async fn fatal_latch_ends_real_nested_decode_capacity_select() {
    fatal_observer_case(true).await;
}

#[tokio::test]
async fn native_turn_rejects_new_play_and_a_claim_lost_before_decode() {
    use super::lx06_vendor::VendorMonitor;
    use tokio::io::AsyncBufReadExt;
    use tokio::time::{timeout, Duration};

    let (role, rx) = role::isolated_role(Arc::new(HardwareProbe::default()));
    let tx = NorthTx::new(
        "fixture-dot".into(),
        1,
        role.northbound_stats.clone(),
        role.dropped_audio.clone(),
    );
    let (sink, reader) = tokio::io::duplex(8192);
    let writer_tx = tx.clone();
    let writer = tokio::spawn(async move { writer_tx.write_to(sink).await });
    let mut lines = BufReader::new(reader).lines();

    let busy = VendorMonitor::test_observer(true, false);
    role.shared.lock().unwrap().vendor = Some(busy.clone());
    assert!(role.on_core_frame(begin(), &tx).unwrap().is_none());
    assert!(role.streams.lock().unwrap().is_empty());

    role.shared.lock().unwrap().vendor = Some(VendorMonitor::test_observer(false, true));
    let reserve = role
        .on_core_frame(begin(), &tx)
        .unwrap()
        .expect("off-thread claim");
    role.shared.lock().unwrap().vendor = Some(busy);
    decode_one(&role, reserve);
    assert!(role.streams.lock().unwrap().is_empty());
    assert!(role.shared.lock().unwrap().active[0].is_none());
    assert!(
        rx.try_recv().is_err(),
        "an unowned play reached the hardware queue"
    );

    for _ in 0..2 {
        let line = timeout(Duration::from_secs(1), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let frame: GatewayFrame = serde_json::from_str(&line).unwrap();
        assert!(
            matches!(frame, GatewayFrame::PlayReceipt {task_id, stage, detail, ..}
            if task_id == "same" && stage == "failed" && detail.as_deref() == Some("native_audio_busy"))
        );
    }
    tx.fail("fixture_done");
    writer.abort();
}

#[test]
fn idle_keyword_only_parks_native_when_core_accepts_the_wake() {
    use super::lx06_vendor::VendorMonitor;
    let (role, _) = role::isolated_role(Arc::new(HardwareProbe::default()));
    let tx = NorthTx::new(
        "fixture-dot".into(),
        1,
        role.northbound_stats.clone(),
        role.dropped_audio.clone(),
    );
    let monitor = VendorMonitor::test_observer(false, false);
    {
        let mut shared = role.shared.lock().unwrap();
        shared.vendor = Some(monitor.clone());
        shared.north_tx = Some(tx.clone());
    }
    let keyword = role::lx06_keyword_admission(role.shared.clone());
    let state = |state: &str| CoreFrame::AssistantState {
        device: "fixture-dot".into(),
        state: state.into(),
    };
    assert!(keyword("下一首", false, false));
    assert!(
        monitor.test_wants_pns(),
        "an idle non-wake stopped native listening"
    );
    role.on_core_frame(state("idle"), &tx).unwrap();
    assert!(monitor.test_wants_pns());
    assert!(keyword("你好高达", false, false));
    assert!(monitor.test_wants_pns(), "a candidate alone took ownership");
    role.on_core_frame(state("wake"), &tx).unwrap();
    assert!(
        !monitor.test_wants_pns(),
        "confirmed Home wake must park the native engine"
    );
}

#[tokio::test]
async fn shared_playback_waits_for_last_writer_before_finish_and_releases_idle_sink() {
    use tokio::time::{sleep, timeout, Duration};
    let hw = Arc::new(HardwareProbe {
        shared_sink: true,
        ..Default::default()
    });
    let (role, rx) = role::isolated_role(hw.clone());
    let send = |action: PlayAction| {
        let stamp = role.shared.lock().unwrap().stamp(action.chan());
        role.play_tx.send(PlayCmd { stamp, action }).unwrap();
    };
    for chan in [Chan::Music, Chan::Voice] {
        send(PlayAction::Samples {
            chan,
            samples: vec![500; PLAY_PERIOD as usize],
        });
    }
    send(PlayAction::Eos(Chan::Voice));

    let (events, mut observed) = mpsc::unbounded_channel();
    let shared = role.shared.clone();
    let worker_hw = hw.clone();
    let progress = role.progress.clone();
    let fault = role.fault.clone();
    let alive = role.playback_alive.clone();
    let worker = std::thread::spawn(move || {
        playback_thread(
            worker_hw,
            rx,
            events,
            shared,
            None,
            1.0,
            false,
            0.0,
            None,
            None,
            vec![],
            false,
            None,
            progress,
            fault,
            alive,
        );
    });
    let next = timeout(Duration::from_secs(1), observed.recv())
        .await
        .unwrap();
    assert!(matches!(
        next,
        Some(PlayEvent::StreamDone {
            chan: Chan::Voice,
            ..
        })
    ));
    assert_eq!(
        hw.drain_calls.load(Ordering::Acquire),
        0,
        "music still owns this writer"
    );
    send(PlayAction::Eos(Chan::Music));
    timeout(Duration::from_secs(1), async {
        while hw.drain_calls.load(Ordering::Acquire) == 0 {
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        observed.try_recv().is_err(),
        "last stream finished before its writer drained"
    );
    hw.drain_allowed.store(true, Ordering::Release);
    let next = timeout(Duration::from_secs(1), observed.recv())
        .await
        .unwrap();
    assert!(matches!(
        next,
        Some(PlayEvent::StreamDone {
            chan: Chan::Music,
            ..
        })
    ));
    let silence = hw.silence_writes.load(Ordering::Relaxed);
    sleep(Duration::from_millis(65)).await;
    assert_eq!(
        hw.silence_writes.load(Ordering::Relaxed),
        silence,
        "idle reopened writer"
    );
    assert_eq!(hw.drain_calls.load(Ordering::Acquire), 1);
    drop(role);
    worker.join().unwrap();
}
