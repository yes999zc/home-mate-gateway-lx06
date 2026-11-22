//! Production capture/AEC/KWS with semantic interruption disabled.

use crate::support::*;
use home_mate_device_gateway::firmware::FirmwareConfig;
use home_mate_device_gateway::protocol::{
    CoreFrame, EchoPathHealth, GatewayFrame, PROTOCOL_MIN_VERSION, PROTOCOL_VERSION,
};
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const GUARD_PLAY_ID: &str = "guard-music";

#[derive(Debug, Default)]
struct GuardPlaybackTrace {
    monitor_play: bool,
    sent: bool,
    health: Option<(EchoPathHealth, String)>,
    // Production north_keyword currently emits wire seq=0. This counter
    // orders observations in this fixture only; it is not a wire sequence
    // or capture identity. Real sidecar fed_ms supplies the progress fence.
    keyword_observations: u64,
    last_keyword_wire_seq: u64,
    keyword_fed_ms: u64,
}

impl GuardPlaybackTrace {
    fn playing_health(&self, expected: EchoPathHealth, reason: &str) -> bool {
        self.sent
            && self
                .health
                .as_ref()
                .is_some_and(|(health, why)| *health == expected && why == reason)
    }

    fn observe(&mut self, frame: &GatewayFrame, stage_name: &str) {
        match frame {
            GatewayFrame::PlayReceipt { task_id, stage, .. }
                if self.monitor_play && task_id == GUARD_PLAY_ID =>
            {
                assert_ne!(
                    stage, "failed",
                    "stage={stage_name}: guarded play failed: {frame:?}"
                );
                if stage == "sent" {
                    self.sent = true;
                }
            }
            GatewayFrame::PlayFinished { task_id, .. }
            | GatewayFrame::PlayRejected { task_id, .. }
                if self.monitor_play && task_id == GUARD_PLAY_ID =>
            {
                panic!("stage={stage_name}: guarded playback ended or was rejected before acoustic assertions: {frame:?}");
            }
            GatewayFrame::EchoHealth { health, reason, .. } => {
                self.health = Some((*health, reason.clone()))
            }
            GatewayFrame::Keyword { seq, fed_ms, .. } => {
                self.keyword_observations += 1;
                self.last_keyword_wire_seq = *seq;
                self.keyword_fed_ms = self.keyword_fed_ms.max(fed_ms.unwrap_or(0));
            }
            _ => {}
        }
    }
}

/// The deadline is unchanged. A failure names the exact transition and
/// records bounded wire/device facts instead of dumping PCM or guessing.
async fn until_guard_stage(
    core: &FakeCore,
    probes: &Mutex<Probes>,
    mode: &AtomicU64,
    stage: &str,
    trace: &mut GuardPlaybackTrace,
    predicate: impl Fn(&GatewayFrame, &GuardPlaybackTrace) -> bool,
) -> GatewayFrame {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
    let mut recent = std::collections::VecDeque::new();
    loop {
        let next = tokio::time::timeout_at(deadline, core.frames.lock().unwrap().recv()).await;
        let frame = match next {
            Ok(Some(frame)) => frame,
            other => {
                let p = probes.lock().unwrap();
                panic!("guard stage={stage} failed ({other:?}); mode={} written_frames={} nonzero_samples={} flushed={} trace={trace:?} recent={recent:?}",
                    mode.load(Ordering::Relaxed), p.written_frames,
                    p.all_samples.iter().filter(|&&s| s != 0).count(), p.flushed);
            }
        };
        let summary = match &frame {
            GatewayFrame::Keyword {
                keyword,
                self_heard,
                fed_ms,
                ..
            } => format!("keyword={keyword} self_heard={self_heard} fed_ms={fed_ms:?}"),
            GatewayFrame::EchoHealth { health, reason, .. } => {
                format!("echo={health:?} reason={reason}")
            }
            GatewayFrame::PlayReceipt {
                task_id,
                stage,
                detail,
                ..
            } => format!("play={task_id} stage={stage} detail={detail:?}"),
            GatewayFrame::PlayFinished { task_id, .. } => format!("finished={task_id}"),
            GatewayFrame::KwsReport { state, detail, .. } => format!("kws={state} detail={detail}"),
            GatewayFrame::AudioChunk { seq, .. } => format!("audio_seq={seq}"),
            _ => format!("{:?}", std::mem::discriminant(&frame)),
        };
        if recent.len() == 16 {
            recent.pop_front();
        }
        recent.push_back(summary);
        assert!(
            !matches!(frame, GatewayFrame::BargeIn { .. }),
            "stage={stage}: semantic barge is disabled"
        );
        trace.observe(&frame, stage);
        if predicate(&frame, trace) {
            return frame;
        }
    }
}

#[tokio::test]
async fn kws_guard_retains_healthy_near_end_and_rejects_zero_reference_without_semantic_barge() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (mut hw, _buttons) = FakeHardware::new(probes.clone());
    hw.pace_playback = true;
    let mode = hw.capture_reference_mode.clone();
    // Start with the real fake-DAC history, including its idle silence.
    // The unrelated default 9ch pattern cannot verify an AEC reference.
    mode.store(1, Ordering::Relaxed);
    let sidecar = state_dir("guard-sidecar");
    std::fs::create_dir_all(sidecar.join("model")).unwrap();
    std::fs::write(sidecar.join("model/tokens.txt"), "n 1\ni 2\n").unwrap();
    let launcher = sidecar.join("run.sh");
    std::fs::write(&launcher,
        "#!/bin/sh\necho '{\"event\":\"ready\",\"load_ms\":1}'\nn=0\nwhile dd bs=2560 count=1 of=/dev/null 2>/dev/null; do\nn=$((n+80))\necho \"{\\\"event\\\":\\\"keyword\\\",\\\"keyword\\\":\\\"hello\\\",\\\"fed_ms\\\":$n,\\\"lag_ms\\\":0}\"\ndone\n"
    ).unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (core, port) = spawn_fake_core().await;
    spawn_firmware(
        FirmwareConfig {
            core_addr: format!("127.0.0.1:{port}"),
            state_dir: state_dir("guard-worker"),
            aec: true,
            barge_in: false,
            kws: true,
            kws_dir: sidecar,
            loudness: false,
            boot_health: false,
            ..Default::default()
        },
        Arc::new(hw),
    );
    let _hello = core.recv().await;
    core.send(&CoreFrame::HelloAck {
        protocol: PROTOCOL_VERSION,
        min_protocol: PROTOCOL_MIN_VERSION,
        core_version: "test".into(),
        supported_versions: vec![],
        capabilities: vec![],
        generation: 1,
    })
    .await;
    core.send(&CoreFrame::MicSession {
        device: "dot2-d".into(),
        action: "start".into(),
        mode: Some("continuous".into()),
    })
    .await;
    let mut trace = GuardPlaybackTrace::default();
    until_guard_stage(&core, &probes, &mode, "idle-kws", &mut trace, |f, _| {
        matches!(
            f,
            GatewayFrame::Keyword {
                self_heard: false,
                ..
            }
        )
    })
    .await;

    // Independent mic sine against random DAC output: low correlation with
    // a sample-exact hardware reference supplied from real fake-DAC writes.
    mode.store(1, Ordering::Relaxed);
    let mut rng = 5u64;
    let mut pcm = Vec::new();
    for _ in 0..48000 * 12 {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        let sample = ((rng >> 33) as i32 % 6000 - 3000) as i16;
        pcm.extend_from_slice(&sample.to_le_bytes());
    }
    let mut wav = wav_48k(pcm.len() / 2, 1);
    wav[44..].copy_from_slice(&pcm);
    trace.monitor_play = true;
    core.send(&play_frame(GUARD_PLAY_ID, &wav, Some("music")))
        .await;
    // No PlayStarted frame exists in this wire version. `sent` is the
    // matching submission fence; ref_locked below additionally proves the
    // capture worker observed a verified reference during actual playback.
    until_guard_stage(
        &core,
        &probes,
        &mode,
        "guard-play-sent",
        &mut trace,
        |frame, trace| {
            trace.sent
                && matches!(frame, GatewayFrame::PlayReceipt { task_id, stage, .. }
            if task_id == GUARD_PLAY_ID && stage == "sent")
        },
    )
    .await;
    // Preserve a playing-specific health event if it raced ahead of the
    // sent receipt; never substitute idle_locked for this evidence.
    if !trace.playing_health(EchoPathHealth::Healthy, "ref_locked") {
        until_guard_stage(
            &core,
            &probes,
            &mode,
            "healthy-reference-during-playback",
            &mut trace,
            |f, trace| {
                matches!(
                    f,
                    GatewayFrame::EchoHealth {
                        health: EchoPathHealth::Healthy, reason,
                        ..
                    } if reason == "ref_locked"
                ) && trace.playing_health(EchoPathHealth::Healthy, "ref_locked")
            },
        )
        .await;
    }
    assert!(
        probes.lock().unwrap().all_samples.iter().any(|&s| s != 0),
        "guarded play must reach the fake DAC"
    );
    let positive_watermark = (trace.keyword_observations, trace.keyword_fed_ms);
    until_guard_stage(
        &core,
        &probes,
        &mode,
        "healthy-independent-kws",
        &mut trace,
        |f, trace| {
            matches!(
                f,
                GatewayFrame::Keyword {
                    self_heard: false, fed_ms: Some(fed_ms),
                    ..
                } if trace.keyword_observations > positive_watermark.0 && *fed_ms > positive_watermark.1
            ) && trace.playing_health(EchoPathHealth::Healthy, "ref_locked")
        },
    )
    .await;

    mode.store(2, Ordering::Relaxed);
    until_guard_stage(
        &core,
        &probes,
        &mode,
        "zero-reference-degraded",
        &mut trace,
        |f, trace| {
            matches!(
                f,
                GatewayFrame::EchoHealth {
                    health: EchoPathHealth::Degraded, reason,
                    ..
                } if reason == "ref_unlocked"
            ) && trace.playing_health(EchoPathHealth::Degraded, "ref_unlocked")
        },
    )
    .await;
    let negative_watermark = (trace.keyword_observations, trace.keyword_fed_ms);
    until_guard_stage(
        &core,
        &probes,
        &mode,
        "zero-reference-guarded-kws",
        &mut trace,
        |f, trace| {
            matches!(
                f,
                GatewayFrame::Keyword {
                    self_heard: true, fed_ms: Some(fed_ms),
                    ..
                } if trace.keyword_observations > negative_watermark.0 && *fed_ms > negative_watermark.1
            ) && trace.playing_health(EchoPathHealth::Degraded, "ref_unlocked")
        },
    )
    .await;
}

#[tokio::test]
async fn idle_kws_remains_eligible_without_aec_or_semantic_barge() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _buttons) = FakeHardware::new(probes);
    let sidecar = state_dir("idle-guard-sidecar");
    std::fs::create_dir_all(sidecar.join("model")).unwrap();
    std::fs::write(sidecar.join("model/tokens.txt"), "n 1\ni 2\n").unwrap();
    let launcher = sidecar.join("run.sh");
    std::fs::write(&launcher, "#!/bin/sh\necho '{\"event\":\"ready\",\"load_ms\":1}'\ndd bs=2560 count=1 of=/dev/null 2>/dev/null\necho '{\"event\":\"keyword\",\"keyword\":\"hello\",\"fed_ms\":80,\"lag_ms\":0}'\nexec sleep 30\n").unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (core, port) = spawn_fake_core().await;
    spawn_firmware(
        FirmwareConfig {
            core_addr: format!("127.0.0.1:{port}"),
            state_dir: state_dir("idle-guard-worker"),
            aec: false,
            barge_in: false,
            kws: true,
            kws_dir: sidecar,
            loudness: false,
            ..Default::default()
        },
        Arc::new(hw),
    );
    let _hello = core.recv().await;
    core.send(&CoreFrame::HelloAck {
        protocol: PROTOCOL_VERSION,
        min_protocol: PROTOCOL_MIN_VERSION,
        core_version: "test".into(),
        supported_versions: vec![],
        capabilities: vec![],
        generation: 1,
    })
    .await;
    core.send(&CoreFrame::MicSession {
        device: "dot2-d".into(),
        action: "start".into(),
        mode: Some("continuous".into()),
    })
    .await;
    let keyword = core
        .until(|f| matches!(f, GatewayFrame::Keyword { .. }))
        .await;
    assert!(matches!(
        keyword,
        GatewayFrame::Keyword {
            self_heard: false,
            ..
        }
    ));
}
