//! Virtual dot2-d terminal: an event-level simulation of the one physical
//! device in scope (FireOS + builtin speaker; see the core's
//! dot-device-baseline). It speaks protocol v0 honestly:
//!
//! - reports `half` duplex capability (full duplex needs a verified AEC
//!   path — the simulation must not claim it);
//! - confirms only what it actually did: `accepted` → `device_confirmed`
//!   receipts, `play_finished` with its own played-ms observation;
//! - writes received WAV payloads to disk (D1 "audio lands on disk"); a
//!   `--afplay` flag additionally plays them on the dev machine.
//!
//! The device is a pure state machine: `apply` takes a core frame and
//! returns immediate frames plus scheduled finishes; the connection actor
//! owns IO and timers. That keeps contract tests free of real time.

use crate::protocol::*;
use std::collections::HashMap;
use std::path::PathBuf;

/// The virtual dot2-d descriptor announced at hello.
pub fn dot2d_descriptor() -> HelloDevice {
    HelloDevice {
        stable_id: "dot2-d".into(),
        model: "AEOBC".into(),
        firmware: Some("virtual (baseline: fireos-5.5.5.4)".into()),
        capabilities: vec![
            "wake_event".into(),
            "text_in".into(),
            "audio_out".into(),
            "volume".into(),
            "stop".into(),
        ],
        audio_profile: "fireos_builtin_speaker".into(),
        wifi_ssid: None,
    }
}

#[derive(Debug, Clone)]
struct ActivePlayback {
    channel: String,
    #[allow(dead_code)] // provenance for diagnostics once activity feeds a status view
    content_ref: String,
    started_ms: u64,
}

/// A finish the caller must schedule: send `play_finished` after `after_ms`.
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduledFinish {
    pub task_id: String,
    pub played_ms: u64,
    pub after_ms: u64,
}

/// Effects of applying one core frame.
#[derive(Debug, Default)]
pub struct Effects {
    pub immediate: Vec<GatewayFrame>,
    pub finishes: Vec<ScheduledFinish>,
    /// Decoded audio the connection actor should persist/play.
    pub audio: Option<ReceivedAudio>,
}

#[derive(Debug, Clone)]
pub struct ReceivedAudio {
    pub task_id: String,
    pub channel: String,
    pub wav: Vec<u8>,
    /// Duration estimated from the WAV header metadata.
    pub duration_ms: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum VirtualError {
    #[error("bad audio payload: {0}")]
    BadAudio(String),
}

pub struct VirtualDot2d {
    pub device_id: String,
    seq: u64,
    volume: f64,
    muted: bool,
    playbacks: HashMap<String, ActivePlayback>,
    /// Last duplex state reported (echoes the core's latest request).
    duplex: DuplexState,
}

impl VirtualDot2d {
    pub fn new(device_id: impl Into<String>) -> Self {
        Self {
            device_id: device_id.into(),
            seq: 0,
            volume: 0.6,
            muted: false,
            playbacks: HashMap::new(),
            duplex: DuplexState {
                capability: "half".into(),
                requested: "half".into(),
                negotiated: "half".into(),
                effective: "half".into(),
            },
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    pub fn duplex(&self) -> &DuplexState {
        &self.duplex
    }

    pub fn volume(&self) -> f64 {
        self.volume
    }

    pub fn muted(&self) -> bool {
        self.muted
    }

    pub fn active(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = self
            .playbacks
            .iter()
            .map(|(task, p)| (task.clone(), p.channel.clone()))
            .collect();
        v.sort();
        v
    }

    /// Local user actions on the terminal. Each stamps ts/seq here so seq
    /// stays monotonic across event kinds from this device.
    pub fn wake(&mut self) -> GatewayFrame {
        GatewayFrame::Wake {
            device: self.device_id.clone(),
            ts: now_ms(),
            seq: self.next_seq(),
        }
    }

    pub fn utterance(&mut self, text: impl Into<String>) -> GatewayFrame {
        GatewayFrame::Utterance {
            device: self.device_id.clone(),
            ts: now_ms(),
            seq: self.next_seq(),
            source: "text".into(),
            text: text.into(),
        }
    }

    pub fn button(&mut self, action: &str) -> GatewayFrame {
        GatewayFrame::Button {
            device: self.device_id.clone(),
            ts: now_ms(),
            seq: self.next_seq(),
            action: action.into(),
        }
    }

    pub fn mute(&mut self, muted: bool) -> GatewayFrame {
        self.muted = muted;
        GatewayFrame::MuteState {
            device: self.device_id.clone(),
            ts: now_ms(),
            seq: self.next_seq(),
            muted,
            source: "hardware".into(),
            task_id: None, // a local toggle is a fact, never a command receipt
        }
    }

    /// Apply a core frame. Pure apart from internal state: IO and timers
    /// belong to the caller.
    pub fn apply(
        &mut self,
        frame: &CoreFrame,
        out_dir: Option<&PathBuf>,
    ) -> Result<Effects, VirtualError> {
        let mut fx = Effects::default();
        match frame {
            CoreFrame::TimeSet { .. } => {
                // Clock projection (G6 #2): virtual terminal has no clock.
            }
            CoreFrame::WifiConfig { .. } => {
                // v0.2.3: virtual terminal has no radio; ignored.
            }
            CoreFrame::AudioEq { .. } => {
                // M6c-2: no audio output in the virtual terminal; the tone
                // settings are accepted and play no part here.
            }
            CoreFrame::LedConfig { .. } => {
                // #35: no physical LED ring in the virtual terminal.
            }
            CoreFrame::KwsDeploy { .. } => {
                // R1j/P1: the virtual terminal has no spotter bundle; it
                // answers honestly instead of pretending a deploy landed.
            }
            CoreFrame::PlayBegin { .. }
            | CoreFrame::PlayChunk { .. }
            | CoreFrame::PlayEnd { .. } => {
                // v0.3 streamed playback is a direct-firmware capability
                // (`play_stream`); the virtual terminal does not advertise
                // it, so the core never streams here. Ignored, honestly.
            }
            CoreFrame::HelloAck { generation, .. } => {
                // Handshake completion is connection-actor business; nothing
                // for the device itself.
                tracing::debug!(generation, "hello ack");
            }
            CoreFrame::MuteSet {
                task_id,
                device,
                muted,
            } => {
                // v1 mechanism (#13): the virtual terminal models the same
                // latch the real firmware owns — set it, and answer with a
                // receipt that echoes the task id, so the whole feature is
                // testable without a speaker. A `hello_ack`-shaped `Assistant
                // State: idle` must NEVER do this (contract test in the core).
                if *device != self.device_id {
                    return Ok(fx);
                }
                let prev = self.muted;
                self.muted = *muted;
                fx.immediate.push(GatewayFrame::MuteState {
                    device: device.clone(),
                    ts: now_ms(),
                    seq: self.next_seq(),
                    muted: *muted,
                    source: "core_set".into(),
                    task_id: Some(task_id.clone()),
                });
                tracing::info!(task_id, from = prev, to = *muted, "virtual mute latch");
            }
            CoreFrame::HelloReject {
                reason,
                core_version,
                ..
            } => {
                // v1 (#11): refused mid-session — recorded; the connection
                // actor sees the close and exits with the reason.
                tracing::warn!(%core_version, %reason, "hello_reject received");
            }
            CoreFrame::Play {
                task_id,
                device,
                content_ref,
                audio,
                audio_b64,
                channel,
                simulated_ms,
                ..
            } => {
                if device != &self.device_id {
                    fx.immediate.push(GatewayFrame::PlayReceipt {
                        task_id: task_id.clone(),
                        stage: "failed".into(),
                        detail: Some(format!("unknown device {device} on this gateway")),
                        ts: now_ms(),
                    });
                    return Ok(fx);
                }
                let channel = channel.clone().unwrap_or_else(|| "voice".into());
                let now = now_ms();
                if let Some(b64) = audio_b64 {
                    let wav =
                        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
                            .map_err(|e| VirtualError::BadAudio(e.to_string()))?;
                    let duration_ms = wav_duration_ms(&wav, audio);
                    if let Some(dir) = out_dir {
                        let path = dir.join(format!("{}-{}.wav", now, task_id));
                        if let Err(e) =
                            std::fs::create_dir_all(dir).and_then(|_| std::fs::write(&path, &wav))
                        {
                            tracing::warn!(path = %path.display(), error = %e, "cannot persist received audio");
                        }
                    }
                    fx.audio = Some(ReceivedAudio {
                        task_id: task_id.clone(),
                        channel: channel.clone(),
                        wav,
                        duration_ms,
                    });
                    self.playbacks.insert(
                        task_id.clone(),
                        ActivePlayback {
                            channel,
                            content_ref: content_ref.clone(),
                            started_ms: now,
                        },
                    );
                    // A virtual speaker consumes instantly: accepted then
                    // confirmed, then a scheduled finish with its own count.
                    fx.immediate.push(GatewayFrame::PlayReceipt {
                        task_id: task_id.clone(),
                        stage: "accepted".into(),
                        detail: Some("virtual speaker accepted".into()),
                        ts: now,
                    });
                    fx.immediate.push(GatewayFrame::PlayReceipt {
                        task_id: task_id.clone(),
                        stage: "device_confirmed".into(),
                        detail: Some("virtual speaker started".into()),
                        ts: now,
                    });
                    fx.finishes.push(ScheduledFinish {
                        task_id: task_id.clone(),
                        played_ms: duration_ms,
                        // Compress long media for dev ergonomics? No: keep
                        // real timing; tests use short values.
                        after_ms: duration_ms,
                    });
                } else if let Some(sim_ms) = simulated_ms {
                    // Simulated media (mock library track): no samples, the
                    // channel runs for the declared duration.
                    self.playbacks.insert(
                        task_id.clone(),
                        ActivePlayback {
                            channel,
                            content_ref: content_ref.clone(),
                            started_ms: now,
                        },
                    );
                    fx.immediate.push(GatewayFrame::PlayReceipt {
                        task_id: task_id.clone(),
                        stage: "accepted".into(),
                        detail: Some("simulated playback accepted".into()),
                        ts: now,
                    });
                    fx.immediate.push(GatewayFrame::PlayReceipt {
                        task_id: task_id.clone(),
                        stage: "device_confirmed".into(),
                        detail: Some("simulated playback started".into()),
                        ts: now,
                    });
                    fx.finishes.push(ScheduledFinish {
                        task_id: task_id.clone(),
                        played_ms: *sim_ms,
                        after_ms: *sim_ms,
                    });
                } else {
                    fx.immediate.push(GatewayFrame::PlayReceipt {
                        task_id: task_id.clone(),
                        stage: "failed".into(),
                        detail: Some("play frame carried neither audio nor simulated_ms".into()),
                        ts: now,
                    });
                }
            }
            CoreFrame::Stop { task_id, .. } => {
                match self.playbacks.remove(task_id) {
                    Some(p) => {
                        let played = now_ms().saturating_sub(p.started_ms);
                        fx.immediate.push(GatewayFrame::StopReceipt {
                            task_id: task_id.clone(),
                            stage: "device_confirmed".into(),
                            played_ms: Some(played),
                            consumed_ms: None,
                            progress_basis: None,
                            detail: None,
                        });
                        tracing::info!(task = %task_id, played_ms = played, "stopped");
                    }
                    None => {
                        // Nothing was playing under this id: the device state
                        // IS "stopped" — confirm honestly rather than "unknown".
                        fx.immediate.push(GatewayFrame::StopReceipt {
                            task_id: task_id.clone(),
                            stage: "device_confirmed".into(),
                            played_ms: None,
                            consumed_ms: None,
                            progress_basis: None,
                            detail: None,
                        });
                    }
                }
            }
            CoreFrame::VolumeSet {
                task_id,
                device,
                value,
                unit,
                ..
            } => {
                if device != &self.device_id {
                    // Never confirm an operation for a device we are not.
                    fx.immediate.push(GatewayFrame::VolumeReceipt {
                        task_id: task_id.clone(),
                        effective: 0.0,
                        unit: unit.clone(),
                        stage: "failed".into(),
                        device: Some(self.device_id.clone()),
                    });
                    return Ok(fx);
                }
                // Linear 0..1 on the virtual device; real devices map their
                // own scale here (device-owned conversion per the boundary doc).
                let clamped = value.clamp(0.0, 1.0);
                self.volume = clamped;
                fx.immediate.push(GatewayFrame::VolumeReceipt {
                    task_id: task_id.clone(),
                    effective: clamped,
                    unit: unit.clone(),
                    stage: "device_confirmed".into(),
                    device: Some(self.device_id.clone()),
                });
            }
            CoreFrame::ModeSet {
                device,
                duplex_requested,
            } => {
                if device != &self.device_id {
                    tracing::warn!(%device, "mode_set for a device this gateway does not host; ignored");
                    return Ok(fx);
                }
                // Honest negotiation: a half-capable link never reports full.
                let negotiated = if duplex_requested == "full" && self.duplex.capability == "full" {
                    "full"
                } else {
                    "half"
                };
                self.duplex.requested = duplex_requested.clone();
                self.duplex.negotiated = negotiated.into();
                self.duplex.effective = negotiated.into();
                fx.immediate.push(GatewayFrame::ModeReport {
                    device: self.device_id.clone(),
                    duplex: self.duplex.clone(),
                    aec: AecState {
                        owner: "none".into(),
                        reference: "absent".into(),
                    },
                });
            }
            CoreFrame::MicSession { device, action, .. } => {
                // The virtual terminal announces text_in only — it has no
                // microphone and must not fake a mic session (M2 boundary).
                fx.immediate.push(GatewayFrame::MicReport {
                    device: device.clone(),
                    active: false,
                    layout: None,
                    sample_rate: None,
                    channels: None,
                    detail: Some(format!(
                        "virtual terminal has no microphone; mic_session {action} refused"
                    )),
                });
            }
            CoreFrame::AssistantState { device, state } => {
                // No LEDs on the virtual terminal — record the projection
                // for introspection and move on (a state the terminal can't
                // show must not error the link).
                tracing::debug!(%device, %state, "assistant_state (virtual: no LEDs)");
            }
            CoreFrame::ConfigStateGet { .. }
            | CoreFrame::ConfigValidate { .. }
            | CoreFrame::ConfigApply { .. } => {}
        }
        Ok(fx)
    }

    /// A scheduled finish fired: emit `play_finished` if the playback is
    /// still active (a Stop in between removes it — no late finish).
    pub fn finish_fired(&mut self, finish: &ScheduledFinish) -> Option<GatewayFrame> {
        self.playbacks.remove(&finish.task_id)?;
        Some(GatewayFrame::PlayFinished {
            device: self.device_id.clone(),
            task_id: finish.task_id.clone(),
            played_ms: finish.played_ms,
            consumed_ms: None,
            progress_basis: None,
            ts: now_ms(),
        })
    }

    /// The connection dropped: every active playback is dead. Honest state
    /// = nothing playing (no fabricated finishes).
    pub fn disconnected(&mut self) {
        self.playbacks.clear();
    }
}

/// Estimate playback duration from a RIFF/WAVE header (data-chunk size over
/// byte rate). Falls back to 0 for malformed payloads (the caller still
/// receives the bytes; duration only drives the finish timer).
pub fn wav_duration_ms(wav: &[u8], meta: &AudioMeta) -> u64 {
    // Minimal RIFF parse: find the data chunk and use its size.
    if wav.len() >= 44 && &wav[0..4] == b"RIFF" && &wav[8..12] == b"WAVE" {
        let mut pos = 12;
        while pos + 8 <= wav.len() {
            let id = &wav[pos..pos + 4];
            let size = u32::from_le_bytes([wav[pos + 4], wav[pos + 5], wav[pos + 6], wav[pos + 7]])
                as usize;
            if id == b"data" {
                let bytes = size.min(wav.len() - pos - 8);
                let byte_rate = (meta.sample_rate as usize) * (meta.channels as usize) * 2; // s16 assumed
                if byte_rate > 0 {
                    return (bytes as u64) * 1000 / byte_rate as u64;
                }
            }
            pos += 8 + size + (size & 1); // chunks are word-aligned
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev() -> VirtualDot2d {
        VirtualDot2d::new("dot2-d")
    }

    /// Minimal valid RIFF/WAVE (s16 mono 16 kHz) with `data_len` zero samples.
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

    fn play_voice(task: &str, data_len: usize) -> CoreFrame {
        // 16 kHz mono s16: 32000 bytes of data ≈ 1 s.
        let wav = wav_bytes(data_len);
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
            channel: Some("voice".into()),
            text: None,
            simulated_ms: None,
            barge_in: Some("stop_current".into()),
        }
    }

    #[test]
    fn play_receipts_then_finish() {
        let mut d = dev();
        let fx = d.apply(&play_voice("T1", 32000), None).unwrap();
        let stages: Vec<&str> = fx
            .immediate
            .iter()
            .map(|f| match f {
                GatewayFrame::PlayReceipt { stage, .. } => stage.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(stages, vec!["accepted", "device_confirmed"]);
        assert_eq!(fx.finishes.len(), 1);
        assert_eq!(fx.finishes[0].played_ms, 1000);
        let fin = d.finish_fired(&fx.finishes[0]).unwrap();
        match fin {
            GatewayFrame::PlayFinished {
                task_id, played_ms, ..
            } => {
                assert_eq!(task_id, "T1");
                assert_eq!(played_ms, 1000);
            }
            _ => panic!(),
        }
        assert!(d.active().is_empty());
    }

    #[test]
    fn stop_before_finish_cancels_and_confirms() {
        let mut d = dev();
        let fx = d.apply(&play_voice("T2", 320000), None).unwrap(); // 10 s
        let fx2 = d
            .apply(
                &CoreFrame::Stop {
                    task_id: "T2".into(),
                    flush: true,
                },
                None,
            )
            .unwrap();
        match &fx2.immediate[0] {
            GatewayFrame::StopReceipt { stage, .. } => assert_eq!(stage, "device_confirmed"),
            f => panic!("{f:?}"),
        }
        // The scheduled finish must NOT produce a frame anymore.
        assert!(d.finish_fired(&fx.finishes[0]).is_none());
    }

    #[test]
    fn stop_unknown_task_still_confirms_stopped_state() {
        let mut d = dev();
        let fx = d
            .apply(
                &CoreFrame::Stop {
                    task_id: "T404".into(),
                    flush: true,
                },
                None,
            )
            .unwrap();
        match &fx.immediate[0] {
            GatewayFrame::StopReceipt { stage, .. } => assert_eq!(stage, "device_confirmed"),
            f => panic!("{f:?}"),
        }
    }

    #[test]
    fn play_to_wrong_device_fails() {
        let mut d = dev();
        let mut f = play_voice("T3", 100);
        if let CoreFrame::Play { device, .. } = &mut f {
            *device = "dot2-a".into();
        }
        let fx = d.apply(&f, None).unwrap();
        match &fx.immediate[0] {
            GatewayFrame::PlayReceipt { stage, .. } => assert_eq!(stage, "failed"),
            x => panic!("{x:?}"),
        }
    }

    #[test]
    fn full_duplex_request_on_half_capability_stays_half() {
        let mut d = dev();
        let fx = d
            .apply(
                &CoreFrame::ModeSet {
                    device: "dot2-d".into(),
                    duplex_requested: "full".into(),
                },
                None,
            )
            .unwrap();
        match &fx.immediate[0] {
            GatewayFrame::ModeReport { duplex, .. } => {
                assert_eq!(duplex.capability, "half");
                assert_eq!(duplex.requested, "full");
                assert_eq!(
                    duplex.negotiated, "half",
                    "must not fake full on a half link"
                );
                assert_eq!(duplex.effective, "half");
            }
            x => panic!("{x:?}"),
        }
    }

    #[test]
    fn seq_is_monotonic_across_event_kinds() {
        let mut d = dev();
        let seq_of = |f: &GatewayFrame| match f {
            GatewayFrame::Wake { seq, .. }
            | GatewayFrame::Utterance { seq, .. }
            | GatewayFrame::Button { seq, .. }
            | GatewayFrame::MuteState { seq, .. } => *seq,
            _ => 0,
        };
        let a = seq_of(&d.wake());
        let b = seq_of(&d.utterance("hi"));
        let c = seq_of(&d.button("action_press"));
        let e = seq_of(&d.mute(true));
        assert!(a < b && b < c && c < e);
    }

    #[test]
    fn simulated_media_without_audio() {
        let mut d = dev();
        let fx = d
            .apply(
                &CoreFrame::Play {
                    task_id: "T9".into(),
                    device: "dot2-d".into(),
                    content_ref: "mocklib:t01".into(),
                    audio: AudioMeta {
                        format: "wav".into(),
                        sample_rate: 16000,
                        channels: 1,
                    },
                    audio_b64: None,
                    channel: Some("media".into()),
                    text: Some("晚风信笺".into()),
                    simulated_ms: Some(500),
                    barge_in: None,
                },
                None,
            )
            .unwrap();
        assert!(fx.audio.is_none());
        assert_eq!(fx.finishes[0].after_ms, 500);
        assert_eq!(d.active()[0].1, "media");
    }

    #[test]
    fn disconnect_clears_playbacks_without_fake_finishes() {
        let mut d = dev();
        d.apply(&play_voice("T5", 32000), None).unwrap();
        d.disconnected();
        assert!(d.active().is_empty());
    }

    #[test]
    fn wav_duration_parses_header() {
        // Build a minimal WAV: 44-byte header + 32000 data bytes.
        let mut wav = vec![0u8; 44 + 32000];
        wav[0..4].copy_from_slice(b"RIFF");
        wav[4..8].copy_from_slice(&(36 + 32000u32).to_le_bytes());
        wav[8..12].copy_from_slice(b"WAVE");
        wav[12..16].copy_from_slice(b"fmt ");
        wav[16..20].copy_from_slice(&16u32.to_le_bytes());
        wav[20..22].copy_from_slice(&1u16.to_le_bytes());
        wav[22..24].copy_from_slice(&1u16.to_le_bytes());
        wav[24..28].copy_from_slice(&16000u32.to_le_bytes());
        wav[28..32].copy_from_slice(&32000u32.to_le_bytes());
        wav[32..34].copy_from_slice(&2u16.to_le_bytes());
        wav[34..36].copy_from_slice(&16u16.to_le_bytes());
        wav[36..40].copy_from_slice(b"data");
        wav[40..44].copy_from_slice(&32000u32.to_le_bytes());
        let meta = AudioMeta {
            format: "wav".into(),
            sample_rate: 16000,
            channels: 1,
        };
        assert_eq!(wav_duration_ms(&wav, &meta), 1000);
    }
}
