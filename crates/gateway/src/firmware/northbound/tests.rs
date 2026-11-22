use super::*;
use crate::protocol::EchoPathHealth;
use std::{
    pin::Pin,
    task::{Context, Poll},
};

fn tx() -> NorthTx {
    NorthTx::new(
        "fixture-dot".into(),
        1,
        Arc::new(NorthboundStats::default()),
        Arc::new(AtomicU64::new(0)),
    )
}
fn receipt(text: String) -> GatewayFrame {
    GatewayFrame::PlayReceipt {
        task_id: "p".into(),
        stage: "unknown".into(),
        detail: Some(text),
        ts: 1,
    }
}
fn health(reason: String) -> GatewayFrame {
    GatewayFrame::EchoHealth {
        device: "fixture-dot".into(),
        health: EchoPathHealth::Healthy,
        degraded_ms: 0,
        reason,
    }
}
fn presence(at: u64) -> GatewayFrame {
    GatewayFrame::NearEndPresence {
        device: "fixture-dot".into(),
        ts_ms: at,
        duration_ms: 80,
        rms_dbfs: -30.0,
        noise_floor_dbfs: -80.0,
        snr_db: 50.0,
        clipped: false,
        echo_health: EchoPathHealth::Healthy,
        profile_version: "fixture".into(),
    }
}
fn audio() -> GatewayFrame {
    GatewayFrame::AudioChunk {
        device: "fixture-dot".into(),
        ts: 1,
        seq: 1,
        pcm_b64: "A".repeat(3416),
        format: "s16le".into(),
        sample_rate: 16000,
        channels: 1,
        layout: "mono".into(),
        vad_speech: None,
        vad_prob: None,
    }
}
fn decoded(entry: &Entry) -> serde_json::Value {
    serde_json::from_slice(&entry.bytes).unwrap()
}

#[test]
fn exact_count_and_byte_guards_include_inflight_critical_reservation() {
    let count = tx();
    for _ in 0..64 {
        assert_eq!(count.send_ctrl(receipt(String::new())), Admission::Admitted);
    }
    let inflight = count.connection.take().unwrap();
    assert_eq!(count.connection.stats.snapshot().critical_current_count, 64);
    assert_eq!(count.send_ctrl(receipt(String::new())), Admission::Closed);
    assert_eq!(count.reason(), "critical_count_full");
    assert_eq!(count.connection.stats.snapshot().critical_current_count, 1);
    drop(inflight);
    assert_eq!(count.connection.stats.snapshot().critical_current_count, 0);
    let bytes = tx();
    let overhead = encode(&receipt(String::new()), CRITICAL_BYTE_CAP)
        .unwrap()
        .len();
    assert_eq!(
        bytes.send_ctrl(receipt("x".repeat(CRITICAL_BYTE_CAP - overhead))),
        Admission::Admitted
    );
    assert_eq!(
        bytes.connection.stats.snapshot().critical_current_bytes,
        CRITICAL_BYTE_CAP as u64
    );
    assert_eq!(bytes.connection.stats.snapshot().critical_current_count, 1);
    let inflight = bytes.connection.take().unwrap();
    assert_eq!(
        bytes.connection.stats.snapshot().critical_current_bytes,
        CRITICAL_BYTE_CAP as u64
    );
    assert_eq!(bytes.send_ctrl(receipt(String::new())), Admission::Closed);
    assert_eq!(bytes.reason(), "critical_bytes_full");
    assert_eq!(
        bytes.connection.stats.snapshot().critical_current_bytes,
        CRITICAL_BYTE_CAP as u64
    );
    drop(inflight);
    assert_eq!(bytes.connection.stats.snapshot().critical_current_bytes, 0);
    let over = tx();
    assert_eq!(
        over.send_ctrl(receipt("x".repeat(CRITICAL_BYTE_CAP - overhead + 1))),
        Admission::Closed
    );
    assert_eq!(over.reason(), "oversized_frame");
}

#[test]
fn bounded_encoder_counts_utf8_escaping_and_newline_and_reports_serialize_error() {
    let frame = receipt("中文\"\\\n".into());
    let expected = (serde_json::to_string(&frame).unwrap() + "\n").into_bytes();
    assert_eq!(encode(&frame, expected.len()).unwrap(), expected);
    assert_eq!(
        encode(&frame, expected.len() - 1).unwrap_err(),
        "oversized_frame"
    );
    struct Bad;
    impl serde::Serialize for Bad {
        fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("fixture serializer failure"))
        }
    }
    assert_eq!(encode(&Bad, 64).unwrap_err(), "encode_failed");
}

#[test]
fn telemetry_latest_slots_audio_drop_and_critical_fairness_are_independent() {
    let tx = tx();
    for n in 0..10000 {
        tx.send_ctrl(health(format!("health-{n}")));
        tx.send_ctrl(presence(n));
    }
    for _ in 0..25 {
        assert_eq!(tx.try_send_audio(audio()), Admission::Admitted);
    }
    assert_eq!(tx.try_send_audio(audio()), Admission::DroppedAudio);
    for n in 0..16 {
        tx.send_ctrl(receipt(n.to_string()));
    }
    for n in 0..8 {
        let entry = tx.connection.take().unwrap();
        assert_eq!(decoded(&entry)["detail"], n.to_string());
        drop(entry);
    }
    let h = tx.connection.take().unwrap();
    assert_eq!(decoded(&h)["reason"], "health-9999");
    drop(h);
    let p = tx.connection.take().unwrap();
    assert_eq!(decoded(&p)["ts_ms"], 9999);
    drop(p);
    let a = tx.connection.take().unwrap();
    assert_eq!(decoded(&a)["type"], "audio_chunk");
    drop(a);
    let c = tx.connection.take().unwrap();
    assert_eq!(decoded(&c)["detail"], "8");
    drop(c);
    let status = tx.connection.stats.snapshot();
    assert_eq!(
        (status.health_coalesced, status.presence_coalesced),
        (9999, 9999)
    );
    assert_eq!(
        (status.audio_high_water_count, status.audio_dropped),
        (25, 1)
    );
    assert_eq!(tx.connection.stats.probe()["health_pending"], 0);
    tx.fail("peer_closed");
    assert_eq!(tx.connection.stats.snapshot().critical_current_count, 0);
    assert_eq!(tx.try_send_audio(audio()), Admission::Closed);
}

#[test]
fn telemetry_exact_cap_invalid_source_and_wrong_lane_are_terminal() {
    let overhead = encode(&health(String::new()), TELEMETRY_BYTE_CAP)
        .unwrap()
        .len();
    let exact = tx();
    assert_eq!(
        exact.send_ctrl(health("x".repeat(TELEMETRY_BYTE_CAP - overhead))),
        Admission::Admitted
    );
    let over = tx();
    assert_eq!(
        over.send_ctrl(health("x".repeat(TELEMETRY_BYTE_CAP - overhead + 1))),
        Admission::Closed
    );
    assert_eq!(over.reason(), "oversized_frame");
    let other = tx();
    let mut frame = presence(1);
    if let GatewayFrame::NearEndPresence { device, .. } = &mut frame {
        *device = "other-device".into();
    }
    assert_eq!(other.send_ctrl(frame), Admission::Closed);
    assert_eq!(other.reason(), "invalid_source");
    let wrong = tx();
    assert_eq!(wrong.send_ctrl(audio()), Admission::Closed);
    assert_eq!(wrong.reason(), "invalid_frame");
}

#[test]
fn runtime_lane_mapping_is_exhaustive_and_kws_transitions_and_receipts_stay_fifo() {
    // Compile-time exhaustive classify() rejects an unclassified future enum variant.
    let mut frames = vec![
        json!({"type":"wake","device":"fixture-dot","ts":0,"seq":1}),
        json!({"type":"utterance","device":"fixture-dot","ts":0,"seq":1,"source":"text","text":"fixture"}),
        json!({"type":"button","device":"fixture-dot","ts":0,"seq":1,"action":"action_press"}),
        json!({"type":"mute_state","device":"fixture-dot","ts":0,"seq":1,"muted":false,"source":"device"}),
        json!({"type":"play_receipt","task_id":"p","stage":"unknown","ts":0}),
        json!({"type":"play_rejected","task_id":"p","device":"fixture-dot","reason":"fixture","ts":0}),
        json!({"type":"stop_receipt","task_id":"p","stage":"unknown"}),
        json!({"type":"volume_receipt","task_id":"p","effective":0.2,"unit":"linear","stage":"device_confirmed"}),
        json!({"type":"mode_report","device":"fixture-dot","duplex":{"capability":"half","requested":"half","negotiated":"half","effective":"half"},"aec":{"owner":"none","reference":"absent"}}),
        json!({"type":"play_finished","device":"fixture-dot","task_id":"p","played_ms":0,"ts":0}),
        json!({"type":"wifi_state","device":"fixture-dot","ts":0,"ssid":"fixture","state":"applied"}),
        json!({"type":"mic_report","device":"fixture-dot","active":false}),
        json!({"type":"device_status","device":"fixture-dot","online":true,"reason":"fixture"}),
        json!({"type":"keyword","device":"fixture-dot","ts":0,"seq":1,"keyword":"wake"}),
        json!({"type":"barge_in","device":"fixture-dot","ts":0,"seq":1,"pcm_b64":"AA==","sample_rate":16000,"channels":1,"corr":0.1,"energy_db":-20.0}),
        json!({"type":"stopped_locally","device":"fixture-dot","task_id":"p","reason":"fixture","played_ms":0,"latency_ms":1,"ahead_ms":0,"ts":0}),
        json!({"type":"kws_report","device":"fixture-dot","state":"starting","detail":"","restarts":0,"detections":0}),
        json!({"type":"kws_report","device":"fixture-dot","state":"running","detail":"","restarts":0,"detections":0}),
        json!({"type":"kws_deploy_receipt","device":"fixture-dot","accepted":true,"hash":"fixture","keywords":1,"detail":""}),
        json!({"type":"local_exit_flushed","device":"fixture-dot","keyword":"exit","ts":0,"latency_ms":1,"channels":["voice"]}),
        json!({"type":"protocol_error","protocol":0,"gateway_version":"fixture","reason":"fixture"}),
    ];
    let tx = tx();
    for frame in &frames {
        let frame: GatewayFrame = serde_json::from_value(frame.clone()).unwrap();
        assert_eq!(classify(&frame), Lane::Critical);
        assert_eq!(tx.send_ctrl(frame), Admission::Admitted);
    }
    for frame in &mut frames {
        if frame["type"] == "device_status" {
            frame["northbound"] =
                decoded(&tx.connection.queue.lock().unwrap().critical[12])["northbound"].clone();
        }
    }
    for frame in frames {
        let entry = tx.connection.take().unwrap();
        let actual = decoded(&entry);
        assert_eq!(actual["type"], frame["type"]);
        if frame["type"] == "kws_report" {
            assert_eq!(actual["state"], frame["state"]);
        }
        drop(entry);
    }
}

struct Pending;
impl AsyncWrite for Pending {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, _: &[u8]) -> Poll<io::Result<usize>> {
        Poll::Pending
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
struct Trickle {
    delay: Option<Pin<Box<tokio::time::Sleep>>>,
}
impl AsyncWrite for Trickle {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        _: &[u8],
    ) -> Poll<io::Result<usize>> {
        use std::future::Future;
        if let Some(delay) = self.delay.as_mut() {
            if delay.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
        }
        self.delay = Some(Box::pin(tokio::time::sleep(Duration::from_millis(30))));
        Poll::Ready(Ok(1))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
#[tokio::test]
async fn production_writer_deadline_bounds_pending_and_trickle_but_not_idle() {
    let tx = tx();
    let writer_tx = tx.clone();
    let writer = tokio::spawn(async move { writer_tx.write_to(Pending).await });
    tokio::time::sleep(Duration::from_millis(1050)).await;
    assert!(
        !writer.is_finished(),
        "an idle writer has no frame deadline"
    );
    tx.send_ctrl(receipt(String::new()));
    let outcome = tokio::time::timeout(Duration::from_millis(1500), writer)
        .await
        .expect("production frame deadline missing")
        .unwrap();
    assert_eq!(outcome.unwrap_err().kind(), io::ErrorKind::TimedOut);
    assert_eq!(tx.reason(), "writer_timeout");
    assert_eq!(tx.connection.stats.snapshot().critical_current_count, 0);
    let stats = NorthboundStats::default();
    let mut writer = Trickle { delay: None };
    let result = tokio::time::timeout(
        Duration::from_millis(1500),
        write_frame(&mut writer, &[0; 100], &stats),
    )
    .await
    .expect("trickle extended frame deadline");
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
    assert!(stats.snapshot().successful_write_bytes > 0);
}
#[tokio::test]
async fn fatal_latch_wakes_without_queue_room_and_old_handles_stay_closed() {
    let tx = tx();
    let waiter = tx.clone();
    let failed = tokio::spawn(async move {
        waiter.failed().await;
    });
    for _ in 0..64 {
        tx.send_ctrl(receipt(String::new()));
    }
    tx.send_ctrl(receipt(String::new()));
    tokio::time::timeout(Duration::from_millis(100), failed)
        .await
        .unwrap()
        .unwrap();
    let fresh = NorthTx::new(
        "fixture-dot".into(),
        3,
        tx.connection.stats.clone(),
        tx.connection.dropped_audio.clone(),
    );
    assert_eq!(tx.send_ctrl(receipt("old".into())), Admission::Closed);
    assert_eq!(
        fresh.send_ctrl(receipt("fresh".into())),
        Admission::Admitted
    );
    assert_eq!(
        decoded(&fresh.connection.take().unwrap())["detail"],
        "fresh"
    );
    fresh.fail("peer_closed");
}

#[tokio::test]
async fn production_age_highwater_survives_stall_and_reconnect_without_observer_polling() {
    let tx = tx();
    let stats = tx.connection.stats.clone();
    tx.send_ctrl(receipt("blocked".into()));
    let error = tx.write_to(Pending).await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    // No snapshot/probe occurred during the blocked write.
    let fresh = NorthTx::new(
        "fixture-dot".into(),
        3,
        stats.clone(),
        Arc::new(AtomicU64::new(0)),
    );
    let status = stats.snapshot();
    assert_eq!(status.oldest_pending_ms, 0);
    assert!(
        status.oldest_pending_high_water_ms >= 990,
        "terminal discarded the production age"
    );
    let probe = stats.probe();
    let start = probe["last_fault_write_started_us"].as_u64().unwrap();
    let deadline = probe["last_fault_write_deadline_us"].as_u64().unwrap();
    let fault = probe["last_fault_us"].as_u64().unwrap();
    assert_eq!(deadline - start, 1_000_000);
    assert!(fault >= deadline);
    assert!(fault - deadline < 100_000);
    fresh.fail("peer_closed");
}
struct Broken {
    zero: bool,
    panic: bool,
}
impl AsyncWrite for Broken {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, _: &[u8]) -> Poll<io::Result<usize>> {
        assert!(!self.panic, "injected writer panic");
        Poll::Ready(if self.zero {
            Ok(0)
        } else {
            Err(io::Error::other("injected write error"))
        })
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
#[tokio::test]
async fn zero_error_and_panic_release_inflight_reservation_and_writer_activity() {
    for zero in [true, false] {
        let tx = tx();
        tx.send_ctrl(receipt("critical".into()));
        assert!(tx.write_to(Broken { zero, panic: false }).await.is_err());
        assert_eq!(tx.reason(), "writer_error");
        let snapshot = tx.connection.stats.snapshot();
        assert_eq!(snapshot.critical_current_count, 0);
        assert_eq!(snapshot.writer_errors, 1);
        assert_eq!(tx.connection.stats.probe()["writer_tasks"], 0);
    }
    let tx = tx();
    tx.send_ctrl(receipt("critical".into()));
    let writer_tx = tx.clone();
    let result = tokio::spawn(async move {
        writer_tx
            .write_to(Broken {
                zero: false,
                panic: true,
            })
            .await
    })
    .await;
    assert!(result.unwrap_err().is_panic());
    // The production panic guard latches failure before serve-loop scheduling.
    assert!(tx.is_terminal());
    tx.fail("peer_closed");
    assert_eq!(tx.reason(), "writer_panic");
    assert_eq!(tx.connection.stats.snapshot().critical_current_count, 0);
    assert_eq!(tx.connection.stats.probe()["writer_tasks"], 0);
}

#[test]
fn public_schema_fixture_and_legacy_unknown_optional_fields_decode_at_wire_zero() {
    let bytes = include_str!("../../../../../docs/fixtures/northbound-status-v1.json");
    let mut value: serde_json::Value = serde_json::from_str(bytes).unwrap();
    let frame: GatewayFrame = serde_json::from_value(value.clone()).unwrap();
    let GatewayFrame::DeviceStatus {
        northbound: Some(status),
        ..
    } = frame
    else {
        panic!("schema fixture missing");
    };
    let status: crate::protocol::NorthboundStatus = serde_json::from_value(status).unwrap();
    assert_eq!(status.schema, 1);
    value.as_object_mut().unwrap().remove("northbound");
    assert!(matches!(
        serde_json::from_value::<GatewayFrame>(value.clone()).unwrap(),
        GatewayFrame::DeviceStatus {
            northbound: None,
            ..
        }
    ));
    value["northbound"] = json!({"schema":999,"future":{"unknown":true}});
    assert!(serde_json::from_value::<GatewayFrame>(value).is_ok());
    assert_eq!((PROTOCOL_VERSION, PROTOCOL_MIN_VERSION), (2, 0));
}

#[test]
fn cleanup_timing_keeps_first_terminal_generation_boundary_across_failed_redials() {
    let tx = tx();
    tx.fail("writer_timeout");
    let stats = &tx.connection.stats;
    stats.cleanup_at(100);
    assert_eq!(stats.probe()["last_cleanup_us"], 100);
    // A failed dial has no new NorthTx/generation. Its common teardown must
    // not overwrite the already observed terminal→retirement interval.
    stats.cleanup_at(9_000_000);
    assert_eq!(stats.probe()["last_cleanup_us"], 100);
    let fresh = NorthTx::new(
        "fixture-dot".into(),
        3,
        stats.clone(),
        Arc::new(AtomicU64::new(0)),
    );
    stats.cleanup_at(10_000_000);
    assert_eq!(stats.probe()["last_cleanup_us"], 100);
    fresh.fail("peer_closed");
    stats.cleanup_at(11_000_000);
    assert_eq!(stats.probe()["last_cleanup_us"], 11_000_000);
    assert_eq!(stats.probe()["last_cleanup_generation"], 3);
}
#[test]
fn encoder_capacity_is_bounded_separately_from_encoded_payload() {
    for cap in [TELEMETRY_BYTE_CAP, AUDIO_BYTE_CAP, CRITICAL_BYTE_CAP] {
        for len in [0, 1, 4095, 4096, 4097, cap.saturating_sub(256)] {
            let text = "x".repeat(len);
            let encoded = encode(&text, cap);
            if let Ok(encoded) = encoded {
                assert!(encoded.len() <= cap);
                assert!(encoded.capacity() <= 4096usize.max(encoded.len() * 2));
                assert!(encoded.capacity() <= cap * 2);
            }
        }
    }
}

#[test]
fn critical_pre_encode_permits_bound_paused_concurrency_and_release_on_failures() {
    let tx = tx();
    let admitted = Arc::new(std::sync::Barrier::new(65));
    let release = Arc::new(std::sync::Barrier::new(65));
    let mut workers = Vec::new();
    for _ in 0..64 {
        let tx = tx.clone();
        let admitted = admitted.clone();
        let release = release.clone();
        workers.push(std::thread::spawn(move || {
            let _permit = tx
                .reserve_critical()
                .expect("available encoder count permit");
            admitted.wait();
            release.wait();
        }));
    }
    admitted.wait();
    assert_eq!(tx.connection.stats.snapshot().critical_current_count, 64);
    assert_eq!(tx.send_ctrl(receipt("65th".into())), Admission::Closed);
    assert_eq!(tx.reason(), "critical_count_full");
    release.wait();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(tx.connection.stats.snapshot().critical_current_count, 0);
    let oversized = super::tests::tx();
    oversized.send_ctrl(receipt("x".repeat(CRITICAL_BYTE_CAP)));
    assert_eq!(oversized.reason(), "oversized_frame");
    assert_eq!(
        oversized.connection.stats.snapshot().critical_current_count,
        0
    );
    let panic = super::tests::tx();
    let worker = panic.clone();
    assert!(std::thread::spawn(move || {
        let _permit = worker.reserve_critical().unwrap();
        panic!("encoder panic");
    })
    .join()
    .is_err());
    assert_eq!(panic.reason(), "encode_failed");
    assert_eq!(panic.connection.stats.snapshot().critical_current_count, 0);
    let failed = super::tests::tx();
    {
        let _permit = failed.reserve_critical().unwrap();
        struct Bad;
        impl serde::Serialize for Bad {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("fixture"))
            }
        }
        assert_eq!(encode(&Bad, CRITICAL_BYTE_CAP), Err("encode_failed"));
        failed.fail("encode_failed");
    }
    assert_eq!(failed.connection.stats.snapshot().critical_current_count, 0);
}

#[test]
fn late_retired_encoder_permit_cannot_overwrite_fresh_current_counters() {
    let old = tx();
    let permit = old.reserve_critical().unwrap();
    old.fail("peer_closed");
    let fresh = NorthTx::new(
        "fixture-dot".into(),
        3,
        old.connection.stats.clone(),
        old.connection.dropped_audio.clone(),
    );
    assert_eq!(fresh.connection.stats.snapshot().critical_current_count, 0);
    fresh.send_ctrl(receipt("fresh".into()));
    assert_eq!(fresh.connection.stats.snapshot().critical_current_count, 1);
    drop(permit);
    assert_eq!(
        fresh.connection.stats.snapshot().critical_current_count,
        1,
        "retired reservation changed fresh generation's projection"
    );
    drop(fresh.connection.take().unwrap());
    assert_eq!(fresh.connection.stats.snapshot().critical_current_count, 0);
}
