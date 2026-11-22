use super::*;

impl Role {
    /// #23: Owner-confirmed long-press mute behavior: enter local Wi-Fi
    /// provisioning notice state, stop any current playback, reject incoming
    /// Play/PlayBegin, light the provisioning LED ring, notify core, and
    /// NEVER trigger a factory reset.
    fn enter_wifi_provisioning_notice(&self, ms: u64) {
        {
            let mut sh = lock_or_fault(&self.shared, Some(&self.fault), "wifi_setup.shared");
            sh.wifi_provisioning = true;
            sh.wifi_provisioning_until_ms = if self.cfg.wifi_provisioning_timeout_ms > 0 {
                Some(kws::mono_ms() + self.cfg.wifi_provisioning_timeout_ms)
            } else {
                None
            };
            sh.active = [None, None];
            sh.pending_stop = [None, None];
            sh.local_stop = None;
            sh.play_live = [false, false];
            sh.decode_gen[0] = sh.decode_gen[0].wrapping_add(1);
            sh.decode_gen[1] = sh.decode_gen[1].wrapping_add(1);
        }
        lock_or_fault(&self.streams, Some(&self.fault), "wifi_setup.streams").clear();
        self.progress.clear();
        self.flush_play(Chan::Voice);
        self.flush_play(Chan::Music);
        render_ring(&self.shared, &self.hw);
        fwlog!(
            "[hm-firmware] mute hold ({ms}ms): entered Wi-Fi provisioning mode; playback stopped and new Play rejected (no factory reset)"
        );
        self.north(GatewayFrame::Button {
            device: self.cfg.device_id.clone(),
            ts: now_ms(),
            seq: self.next_seq(),
            action: "mute_hold_wifi_setup".into(),
        });
    }

    pub(super) fn on_button(&self, ev: ButtonEvent) {
        match ev {
            ButtonEvent::Action => {
                self.north(GatewayFrame::Button {
                    device: self.cfg.device_id.clone(),
                    ts: now_ms(),
                    seq: self.next_seq(),
                    action: "action_press".into(),
                });
            }
            ButtonEvent::MuteToggle => {
                // v1 mechanism (#13): the physical key and the MuteSet frame
                // are the ONLY two writers of the latch, and they share this
                // one helper — same hardware gate, same persistence, same
                // receipt shape.
                //
                // LOCK DISCIPLINE (the contract test just caught this): the
                // guard below must DROP before the helper runs — a temporary
                // in a call argument lives until the STATEMENT ends, and
                // std Mutex is not reentrant. One line, whole-device hang.
                let now_muted = lock_or_fault(&self.shared, Some(&self.fault), "role.shared").muted;
                self.set_mute_latch(!now_muted, None, "button");
            }
            ButtonEvent::Hold(code, ms) => {
                // Long-press: reported north as-is, no local destructive
                // action (onepager §3-3 — semantics belong to the core).
                self.north(GatewayFrame::Button {
                    device: self.cfg.device_id.clone(),
                    ts: now_ms(),
                    seq: self.next_seq(),
                    action: format!("{code}_hold:{ms}ms"),
                });
                // #23: Owner-confirmed rule — long-press mute enters local
                // Wi-Fi provisioning notice state, stops current playback,
                // notifies core, and NEVER triggers factory reset.
                if code == buttons::KEY_MUTE || code == self.cfg.profile.key_map.mute {
                    self.enter_wifi_provisioning_notice(ms);
                } else if self.cfg.profile.model == "LX06"
                    && code == self.cfg.profile.key_map.action
                {
                    fwlog!(
                        "[lx06] physical action hold ({ms}ms) -> switching system mode to xiaoai"
                    );
                    if let Err(error) =
                        lx06::select_mode(std::path::Path::new("/data/hm/mode"), "xiaoai-pinned")
                    {
                        fwlog!("[lx06] mode switch failed: {error}");
                        return;
                    }
                    let _ = self.hw.playback_flush();
                    std::process::exit(0);
                }
            }
            ButtonEvent::VolumeDown
            | ButtonEvent::VolumeUp
            | ButtonEvent::VolumeDownRepeat
            | ButtonEvent::VolumeUpRepeat => {
                // A held key repeats at the kernel's typematic rate (typically
                // 10/s after ~500 ms). Stepping a coarse 8 per repeat would
                // cross the whole range in a third of a second, so repeats walk
                // ONE level and are throttled to ~45 ms — a long press is a
                // controllable sweep, a tap is still a coarse step.
                let repeat = matches!(
                    ev,
                    ButtonEvent::VolumeUpRepeat | ButtonEvent::VolumeDownRepeat
                );
                if repeat {
                    let now = tokio::time::Instant::now();
                    if let Some(last) = *self.volume_repeat_at.lock().unwrap() {
                        if now.duration_since(last) < std::time::Duration::from_millis(45) {
                            return; // swallowed: a repeat faster than the drain
                        }
                    }
                    *self.volume_repeat_at.lock().unwrap() = Some(now);
                }
                let step = if repeat { 1 } else { VOLUME_KEY_STEP };
                let level = {
                    let mut sh = lock_or_fault(&self.shared, Some(&self.fault), "role.shared");
                    // The BUTTON obeys the same cap as the API: above it this
                    // driver crackles, and a key press must not smuggle the
                    // level past the limit (user decision 2026-11-14).
                    let cap = (sh.volume_cap.clamp(0.0, 1.0) * 127.0).round() as u64;
                    sh.volume = match ev {
                        ButtonEvent::VolumeUp | ButtonEvent::VolumeUpRepeat => {
                            (sh.volume + step).min(cap.min(127))
                        }
                        _ => sh.volume.saturating_sub(step),
                    };
                    sh.volume
                };
                let muted = lock_or_fault(&self.shared, Some(&self.fault), "role.shared").muted;
                save_state(&self.cfg.state_dir, level, muted);
                // Device-owned volume change: report the fact north.
                self.north(GatewayFrame::VolumeReceipt {
                    task_id: format!("vol:dev:{}", self.next_seq()),
                    effective: level as f64 / 127.0,
                    unit: "linear".into(),
                    stage: "device_confirmed".into(),
                    device: Some(self.cfg.device_id.clone()),
                });
                // EchoMuse volume arc (stage C UX): cyan arc ∝ level for 2 s.
                self.paint_volume_arc(level);
            }
            ButtonEvent::Unknown(code) => {
                fwlog!("[firmware] unmapped key code {code}");
            }
        }
    }

    // --- frames from the core ---------------------------------------------------

    /// Handle one core frame. Returns `Err` when the connection must be
    /// dropped (`ProtocolMismatch` triggers `HealthLamp::Protocol` + slow
    /// `PROTOCOL_ERROR_RETRY_MS` pacing; other errors trigger normal `Offline`
    /// reconnect).
    pub(super) fn on_core_frame(
        &self,
        frame: CoreFrame,
        direct_tx: &NorthTx,
    ) -> Result<Option<DecodeCmd>, Box<dyn std::error::Error + Send + Sync>> {
        // R1d: the voice channel was stopped locally for this play and the
        // core has not caught up yet (its Stop can be seconds behind the
        // fact). Accepting one more chunk would re-open the channel the user
        // just silenced, so a locally stopped play is finished as far as the
        // audio path is concerned. Bounded by LOCAL_STOP_BLOCK_MS so a stale
        // marker cannot eat a later legitimate play.
        if let Some(task_id) = stream_task_of(&frame) {
            let stop_now = {
                let sh = lock_or_fault(&self.shared, Some(&self.fault), "on_core_frame.shared");
                match &sh.local_stop {
                    Some((t, at)) => {
                        t == task_id && now_ms().saturating_sub(*at) <= LOCAL_STOP_BLOCK_MS
                    }
                    None => false,
                }
            };
            if stop_now {
                if matches!(frame, CoreFrame::PlayEnd { .. }) {
                    lock_or_fault(&self.streams, Some(&self.fault), "on_core_frame.streams")
                        .remove(task_id);
                }
                fwlog!("[barge-in] dropped {task_id} chunk: the voice channel was stopped locally");
                return Ok(None);
            }
        }
        // Reject immediately during an existing native turn. An idle native
        // engine is parked on the decoder thread before accepting playback.
        if let CoreFrame::Play {
            device, task_id, ..
        }
        | CoreFrame::PlayBegin {
            device, task_id, ..
        } = &frame
        {
            let sh = lock_or_fault(&self.shared, Some(&self.fault), "play.shared");
            if device == &self.cfg.device_id
                && sh.vendor.as_ref().is_some_and(|v| v.native_blocks_play())
            {
                drop(sh);
                direct_tx.send_ctrl(GatewayFrame::PlayReceipt {
                    task_id: task_id.clone(),
                    stage: "failed".into(),
                    detail: Some("native_audio_busy".into()),
                    ts: now_ms(),
                });
                return Ok(None);
            }
        }
        match frame {
            CoreFrame::AudioEq {
                device,
                curve,
                bands,
            } => {
                if device != self.cfg.device_id {
                    fwlog!(
                        "[loudness] audio_eq for {} ignored (host is {})",
                        device,
                        self.cfg.device_id
                    );
                    return Ok(None);
                }
                let curve = match curve.as_str() {
                    "official" => loudness::Curve::Official,
                    "echomuse" => loudness::Curve::EchoMuse,
                    "custom" => loudness::Curve::Custom,
                    _ => loudness::Curve::Rui,
                };
                let bands: Vec<loudness::CustomBand> = bands
                    .iter()
                    .take(10)
                    .map(|b| loudness::CustomBand {
                        freq: b.freq.clamp(20.0, 16000.0),
                        gain_db: b.gain_db.clamp(-18.0, 18.0),
                        q: b.q.clamp(0.3, 8.0),
                    })
                    .collect();
                {
                    let mut sh =
                        lock_or_fault(&self.shared, Some(&self.fault), "on_core_frame.shared");
                    sh.eq_curve = Some(curve);
                    sh.eq_bands = bands.clone();
                    sh.eq_rev = sh.eq_rev.wrapping_add(1);
                }
                fwlog!(
                    "[loudness] audio_eq applied: curve={:?} bands={} ({})",
                    curve,
                    bands.len(),
                    bands
                        .iter()
                        .map(|b| format!("{}Hz{:+.1}dB Q{:.2}", b.freq, b.gain_db, b.q))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                return Ok(None);
            }
            CoreFrame::Play {
                task_id,
                device,
                audio_b64,
                simulated_ms,
                audio,
                channel,
                ..
            } => {
                if device != self.cfg.device_id {
                    let _ = direct_tx.send_ctrl(GatewayFrame::PlayReceipt {
                        task_id,
                        stage: "failed".into(),
                        detail: Some(format!("component hosts only {}", self.cfg.device_id)),
                        ts: now_ms(),
                    });
                    return Ok(None);
                }
                if lock_or_fault(&self.shared, Some(&self.fault), "on_core_frame.shared")
                    .wifi_provisioning
                {
                    fwlog!("[hm-firmware] play rejected task={task_id}: reason=wifi_provisioning");
                    let _ = direct_tx.send_ctrl(GatewayFrame::PlayRejected {
                        task_id,
                        device,
                        reason: "wifi_provisioning".into(),
                        ts: now_ms(),
                    });
                    return Ok(None);
                }
                if audio_b64.is_none() && simulated_ms.is_none() {
                    let _ = direct_tx.send_ctrl(GatewayFrame::PlayReceipt {
                        task_id,
                        stage: "failed".into(),
                        detail: Some("no audio payload and no simulated_ms".into()),
                        ts: now_ms(),
                    });
                    return Ok(None);
                }
                let chan = if channel.as_deref() == Some("media") {
                    Chan::Music
                } else {
                    Chan::Voice
                };
                let idx = if chan == Chan::Voice { 0 } else { 1 };
                // A new play on the same channel displaces the active one.
                self.supersede(chan, "superseded by a new play", direct_tx);
                let stamp = {
                    let mut sh =
                        lock_or_fault(&self.shared, Some(&self.fault), "on_core_frame.shared");
                    sh.decode_gen[idx] = sh.decode_gen[idx].wrapping_add(1);
                    sh.active[idx] = Some((task_id.clone(), 0));
                    sh.stamp(chan)
                };
                return Ok(Some(DecodeCmd::Play {
                    task_id,
                    chan,
                    audio_b64,
                    simulated_ms,
                    sample_rate: audio.sample_rate,
                    channels: audio.channels,
                    stamp,
                    north: direct_tx.clone(),
                }));
            }
            CoreFrame::PlayBegin {
                task_id,
                device,
                audio,
                channel,
                ..
            } => {
                if device != self.cfg.device_id {
                    let _ = direct_tx.send_ctrl(GatewayFrame::PlayReceipt {
                        task_id,
                        stage: "failed".into(),
                        detail: Some(format!("component hosts only {}", self.cfg.device_id)),
                        ts: now_ms(),
                    });
                    return Ok(None);
                }
                if lock_or_fault(&self.shared, Some(&self.fault), "on_core_frame.shared")
                    .wifi_provisioning
                {
                    fwlog!(
                        "[hm-firmware] play_begin rejected task={task_id}: reason=wifi_provisioning"
                    );
                    let _ = direct_tx.send_ctrl(GatewayFrame::PlayRejected {
                        task_id,
                        device,
                        reason: "wifi_provisioning".into(),
                        ts: now_ms(),
                    });
                    return Ok(None);
                }
                // v0.3 §1: bare S16LE PCM, mono to start.
                if audio.channels != 1 || audio.sample_rate == 0 {
                    let _ = direct_tx.send_ctrl(GatewayFrame::PlayReceipt {
                        task_id,
                        stage: "failed".into(),
                        detail: Some(format!(
                            "stream audio must be mono pcm_s16le with a rate (got ch={} rate={})",
                            audio.channels, audio.sample_rate
                        )),
                        ts: now_ms(),
                    });
                    return Ok(None);
                }
                let chan = if channel.as_deref() == Some("media") {
                    Chan::Music
                } else {
                    Chan::Voice
                };
                let idx = if chan == Chan::Voice { 0 } else { 1 };
                // A new stream on a channel displaces whatever the channel
                // was doing (same rule as whole plays), and takes over the
                // channel slot so StreamDone still reaches the right task.
                self.supersede(chan, "superseded by a new stream", direct_tx);
                let (played_base, stamp) = {
                    let mut sh =
                        lock_or_fault(&self.shared, Some(&self.fault), "on_core_frame.shared");
                    sh.decode_gen[idx] = sh.decode_gen[idx].wrapping_add(1);
                    sh.active[idx] = Some((task_id.clone(), 0));
                    (sh.played[idx], sh.stamp(chan))
                };
                {
                    let mut streams =
                        lock_or_fault(&self.streams, Some(&self.fault), "on_core_frame.streams");
                    streams.retain(|_, ctx| ctx.chan != chan);
                    streams.insert(
                        task_id.clone(),
                        StreamCtx {
                            stamp,
                            chan,
                            state: Arc::new(Mutex::new(role::StreamDecodeState {
                                next_seq: 0,
                                resampler: resample::StreamResampler::new(
                                    audio.sample_rate,
                                    PLAY_RATE,
                                ),
                                pushed: 0,
                                played_base,
                                streaming_receipt_sent: false,
                            })),
                        },
                    );
                }
                let detail = format!(
                    "stream {}Hz -> 48k [{}]",
                    audio.sample_rate,
                    if chan == Chan::Voice {
                        "voice"
                    } else {
                        "music"
                    }
                );
                if lock_or_fault(&self.shared, Some(&self.fault), "play.shared")
                    .vendor
                    .is_some()
                {
                    return Ok(Some(DecodeCmd::ReserveStream {
                        task_id,
                        chan,
                        stamp,
                        north: direct_tx.clone(),
                        detail,
                    }));
                }
                let _ = direct_tx.send_ctrl(GatewayFrame::PlayReceipt {
                    task_id,
                    stage: "accepted".into(),
                    detail: Some(detail),
                    ts: now_ms(),
                });
            }
            CoreFrame::PlayChunk {
                task_id,
                seq,
                audio_b64,
            } => {
                let sh = lock_or_fault(&self.shared, Some(&self.fault), "on_core_frame.shared");
                let streams =
                    lock_or_fault(&self.streams, Some(&self.fault), "on_core_frame.streams");
                let Some(ctx) = streams.get(&task_id) else {
                    return Ok(None);
                };
                if ctx.stamp != sh.stamp(ctx.chan) {
                    return Ok(None);
                }
                return Ok(Some(DecodeCmd::PlayChunk {
                    task_id,
                    seq,
                    audio_b64,
                    stamp: ctx.stamp,
                    north: direct_tx.clone(),
                }));
            }
            CoreFrame::PlayEnd { task_id } => {
                let sh = lock_or_fault(&self.shared, Some(&self.fault), "on_core_frame.shared");
                let streams =
                    lock_or_fault(&self.streams, Some(&self.fault), "on_core_frame.streams");
                let Some(ctx) = streams.get(&task_id) else {
                    return Ok(None);
                };
                if ctx.stamp != sh.stamp(ctx.chan) {
                    return Ok(None);
                }
                return Ok(Some(DecodeCmd::PlayEnd {
                    task_id,
                    stamp: ctx.stamp,
                }));
            }
            CoreFrame::Stop { task_id, flush } => {
                // A stop also tears down any live v0.3 stream under it.
                lock_or_fault(&self.streams, Some(&self.fault), "on_core_frame.streams")
                    .remove(&task_id);
                // The stop owns any play with this task id; unknown ids stop
                // cleanly (nothing was playing under that name).
                let flush_chan = {
                    let mut sh =
                        lock_or_fault(&self.shared, Some(&self.fault), "on_core_frame.shared");
                    let idx = sh
                        .active
                        .iter()
                        .position(|a| a.as_ref().map(|(t, _)| t == &task_id) == Some(true));
                    if let Some(i) = idx {
                        sh.decode_gen[i] = sh.decode_gen[i].wrapping_add(1);
                        if flush {
                            sh.pending_stop[i] = Some(task_id.clone());
                            let chan = if i == 0 { Chan::Voice } else { Chan::Music };
                            sh.send_play(&self.play_tx, PlayAction::Flush(chan));
                            Some(chan)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                };
                if flush_chan.is_some() {
                    // The receipt belongs to the hardware fact, so it
                    // leaves from FlushDone (after playback_flush), with
                    // the played_ms the DAC actually took.
                    return Ok(None);
                }
                let _ = direct_tx.send_ctrl(GatewayFrame::StopReceipt {
                    task_id,
                    stage: "device_confirmed".into(),
                    played_ms: None,
                    consumed_ms: None,
                    progress_basis: None,
                    detail: None,
                });
            }
            CoreFrame::KwsDeploy {
                device,
                keywords,
                retreat_words,
                intents_hash,
                threshold,
                score,
            } => {
                if device != self.cfg.device_id {
                    return Ok(None);
                }
                let control = lock_or_fault(&self.shared, Some(&self.fault), "role.shared")
                    .kws_control
                    .clone();
                let dir = control
                    .as_ref()
                    .map(|c| c.dir.clone())
                    .unwrap_or_else(|| self.cfg.kws_dir.clone());
                let (mut accepted, mut hash, mut count, mut detail, mut local_exit) =
                    deploy_keywords(&dir, &keywords, &retreat_words, &intents_hash);
                // Contract v2 #4: the sensitivity applies with the same
                // deploy and the same restart. A REJECTED deploy changes
                // neither (the old tuning stays in force). Fail closed if
                // `write_tuning` fails.
                let tuning = threshold.zip(score);
                if accepted {
                    if let Err(e) = kws::write_tuning(&dir, tuning) {
                        fwlog!("[kws] tuning install failed (failing closed): {e}");
                        let bak = dir.join("kw.txt.bak");
                        if bak.exists() {
                            let _ = std::fs::rename(&bak, dir.join("kw.txt"));
                        }
                        accepted = false;
                        hash.clear();
                        count = 0;
                        local_exit = 0;
                        detail = format!("tuning install failed: {e}");
                    } else if let Err(e) =
                        self.stage_kws_bundle_meta(&dir, &hash, &intents_hash, tuning, local_exit)
                    {
                        fwlog!("[kws] bundle metadata install failed (failing closed): {e}");
                        accepted = false;
                        hash.clear();
                        count = 0;
                        local_exit = 0;
                        detail = format!("bundle metadata install failed: {e}");
                    }
                }
                if accepted {
                    if let Some(status) =
                        lock_or_fault(&self.shared, Some(&self.fault), "role.shared")
                            .kws_status
                            .clone()
                    {
                        if let Ok(mut st) = status.lock() {
                            if let Some((t, s)) = tuning {
                                st.threshold = t;
                                st.score = s;
                            }
                            st.local_exit = local_exit;
                        }
                    }
                }
                let restarted = if accepted {
                    control.as_ref().map(|c| c.restart()).unwrap_or(false)
                } else {
                    false
                };
                // "signalled", not "restarting": a delivered SIGTERM is not
                // a stopped process. The supervisor escalates to SIGKILL, and
                // the only proof of a fresh vocabulary is the `ready` line's
                // loaded_hash reported north afterwards (M6b).
                fwlog!(
                    "[kws] deploy {}: {} words, hash={}{} ({})",
                    if accepted { "accepted" } else { "REJECTED" },
                    count,
                    hash,
                    if restarted {
                        ", spotter restart signalled"
                    } else {
                        ", no live spotter (applies on next start)"
                    },
                    detail
                );
                let _ = direct_tx.send_ctrl(GatewayFrame::KwsDeployReceipt {
                    device,
                    accepted,
                    hash,
                    keywords: count,
                    local_exit,
                    intents_hash: if accepted {
                        intents_hash.clone()
                    } else {
                        String::new()
                    },
                    threshold: if accepted {
                        threshold.unwrap_or(0.0)
                    } else {
                        0.0
                    },
                    score: if accepted { score.unwrap_or(0.0) } else { 0.0 },
                    detail: if restarted {
                        format!("{detail}; spotter restart signalled")
                    } else if accepted {
                        format!("{detail}; no live spotter — applies on next start")
                    } else {
                        detail
                    },
                });
            }
            CoreFrame::VolumeSet {
                task_id,
                device,
                value,
                ..
            } => {
                if device != self.cfg.device_id {
                    return Ok(None);
                }
                // NOTE: the adapter's P3-3 displacement receipt existed
                // because it WAITED for the southbound firmware's answer;
                // absorbed-mode volume applies in-process, so every task is
                // confirmed synchronously — no pending window to displace.
                let (level, muted) = {
                    let mut sh = lock_or_fault(&self.shared, Some(&self.fault), "role.shared");
                    let v = value.clamp(0.0, 1.0).min(sh.volume_cap);
                    let level = (v * 127.0).round() as u64;
                    sh.volume = level;
                    (level, sh.muted)
                };
                save_state(&self.cfg.state_dir, level, muted);
                // Software gain applies inside our own process — an
                // immediate hardware fact, no round-trip to wait for.
                let _ = direct_tx.send_ctrl(GatewayFrame::VolumeReceipt {
                    task_id,
                    effective: level as f64 / 127.0,
                    unit: "linear".into(),
                    stage: "device_confirmed".into(),
                    device: Some(self.cfg.device_id.clone()),
                });
                fwlog!("[firmware] volume_set {value:.3} → level {level}");
            }
            CoreFrame::ModeSet {
                device,
                duplex_requested,
            } => {
                if device != self.cfg.device_id {
                    return Ok(None);
                }
                // Honest negotiation: the transport CAN run mic+speaker
                // concurrently, but product full duplex needs a verified
                // echo path (v0.2 has none) — capability stays half, and the
                // near-end local stop reads the EFFECTIVE value (R1g).
                {
                    let mut sh = lock_or_fault(&self.shared, Some(&self.fault), "role.shared");
                    sh.duplex_requested = duplex_requested.clone();
                    sh.duplex_effective = "half".into();
                }
                let (layout, _) = self.mic_layout();
                let _ = direct_tx.send_ctrl(GatewayFrame::ModeReport {
                    device: self.cfg.device_id.clone(),
                    duplex: DuplexState {
                        capability: "half".into(),
                        requested: duplex_requested,
                        negotiated: "half".into(),
                        effective: "half".into(),
                    },
                    aec: AecState {
                        // M6b: EchoMuse AEC runs in the firmware audio
                        // thread; the core is a consumer, not the owner.
                        owner: "device".into(),
                        reference: if layout == "mic_ref" {
                            "available".into()
                        } else {
                            "absent".into()
                        },
                    },
                });
            }
            CoreFrame::MicSession { device, action, .. } => {
                if device != self.cfg.device_id {
                    return Ok(None);
                }
                let start = action == "start";
                lock_or_fault(&self.shared, Some(&self.fault), "role.shared").mic_on = start;
                let (layout, channels) = self.mic_layout();
                let _ = direct_tx.send_ctrl(GatewayFrame::MicReport {
                    device: self.cfg.device_id.clone(),
                    active: start,
                    layout: Some(layout.into()),
                    sample_rate: Some(CAP_RATE),
                    channels: Some(channels),
                    detail: None,
                });
                fwlog!("[firmware] mic_session {action}");
            }
            CoreFrame::AssistantState { device, state } => {
                if device != self.cfg.device_id {
                    return Ok(None);
                }
                let vendor = lock_or_fault(&self.shared, Some(&self.fault), "state.shared")
                    .vendor
                    .clone();
                if let Some(vendor) = vendor {
                    vendor.assistant_state(&state);
                }
                self.paint_led(&state);
            }
            CoreFrame::MuteSet {
                task_id,
                device,
                muted,
            } => {
                // v1 mechanism (#13): the ONE frame allowed to write the
                // latch. Idempotent by value; the receipt always goes (the
                // core resolves its command against THIS fact, even when the
                // latch already held the requested value). The echo carries
                // the task id so a key press can never settle this command.
                if device != self.cfg.device_id {
                    return Ok(None);
                }
                self.set_mute_latch(muted, Some(task_id), "core_set");
            }
            CoreFrame::LedConfig {
                device,
                night_enabled,
                night_start_minute,
                night_end_minute,
                night_brightness_pct,
                orientation_rotation,
                orientation_reverse,
            } => {
                if device != self.cfg.device_id {
                    return Ok(None);
                }
                let night_mode = leds::NightMode {
                    enabled: night_enabled,
                    start_minute: night_start_minute % 1440,
                    end_minute: night_end_minute % 1440,
                    brightness_pct: night_brightness_pct.min(100),
                };
                let led_orientation = leds::LedOrientation {
                    rotation: orientation_rotation % (leds::RING_LEDS as u8),
                    reverse: orientation_reverse,
                };
                let (tz_offset, tz_name) = {
                    let mut sh =
                        lock_or_fault(&self.shared, Some(&self.fault), "on_core_frame.shared");
                    sh.night_mode = night_mode;
                    sh.led_orientation = led_orientation;
                    (sh.led_tz_offset_minutes, sh.led_timezone.clone())
                };
                if let Err(e) = leds::save_led_config(
                    &self.cfg.state_dir,
                    night_mode,
                    led_orientation,
                    tz_offset,
                    tz_name,
                ) {
                    fwlog!("[leds] save_led_config failed: {e}");
                }
                fwlog!(
                    "[leds] led_config applied: night_enabled={} window={}..{} brightness={}% rot={} rev={}",
                    night_mode.enabled,
                    night_mode.start_minute,
                    night_mode.end_minute,
                    night_mode.brightness_pct,
                    led_orientation.rotation,
                    led_orientation.reverse
                );
                render_ring(&self.shared, &self.hw);
            }
            CoreFrame::WifiConfig { ssid, psk } => {
                // v0.2.3 online WiFi change (stage C, replaced-capability
                // #1-2): apply + join watchdog on a worker thread — a
                // failed join restores the backup so the device can never
                // strand itself on an unattended change.
                //
                // v1 mechanism (#11): privileged-frame gate. A build that
                // does not declare `wifi` must NOT silently execute a WiFi
                // change — that is a protocol error (lamp + disconnect),
                // exactly the acceptance line gateway issue #11 asks for.
                if !self.descriptor().capabilities.iter().any(|c| c == "wifi") {
                    fwlog!("[firmware] WifiConfig refused: wifi not declared in the descriptor");
                    {
                        let mut sh = lock_or_fault(&self.shared, Some(&self.fault), "role.shared");
                        sh.link_fault = Some(leds::HealthLamp::Protocol);
                    }
                    render_ring(&self.shared, &self.hw);
                    let _ = direct_tx.send_ctrl(GatewayFrame::ProtocolError {
                        protocol: PROTOCOL_VERSION,
                        min_protocol: PROTOCOL_MIN_VERSION,
                        gateway_version: self.cfg.version.clone(),
                        reason: "capability_missing:wifi".into(),
                    });
                    return Err(ProtocolMismatch("capability_missing:wifi".into()).into());
                }
                let role = self.clone();
                std::thread::spawn(move || role.wifi_change(ssid, psk));
            }
            CoreFrame::TimeSet { ts_ms } => {
                // G6 replaced-capability #2: emOS has no time service; the
                // clock sits in 2010 (TLS breaks there in stage C). The
                // core projects its clock right after hello; we set what we
                // can (root) and log the outcome — no reply, it's a
                // projection, not a negotiation.
                let ok = self.hw.set_system_time(ts_ms);
                fwlog!(
                    "[time] clock projection ts_ms={ts_ms} {}",
                    if ok { "applied" } else { "FAILED (perm?)" }
                );
            }
            CoreFrame::HelloAck { .. } => {
                // Handled at connect; a repeat is inert.
            }
            CoreFrame::HelloReject { reason, .. } => {
                // v1 (#11): a rejection AFTER a good handshake means the peer
                // changed its mind mid-flight — same lamp, same slow pace.
                fwlog!("[firmware] core sent hello_reject mid-session: {reason}");
                {
                    let mut sh = lock_or_fault(&self.shared, Some(&self.fault), "role.shared");
                    sh.link_fault = Some(leds::HealthLamp::Protocol);
                }
                render_ring(&self.shared, &self.hw);
                return Err(ProtocolMismatch(format!("refused by core: {reason}")).into());
            }
            CoreFrame::ConfigStateGet { request_id, device } => {
                self.handle_config_state_get(request_id, device, direct_tx);
            }
            CoreFrame::ConfigValidate {
                request_id,
                device,
                expected_gateway_boot_id,
                expected_state_revision,
                block,
                schema_version,
                block_hash,
                values,
            } => {
                self.handle_config_validate(
                    request_id,
                    device,
                    expected_gateway_boot_id,
                    expected_state_revision,
                    block,
                    schema_version,
                    block_hash,
                    values,
                    direct_tx,
                );
            }
            CoreFrame::ConfigApply {
                device,
                core_node_id,
                data_epoch,
                application_id,
                operation_id,
                desired_revision,
                block_generation,
                block,
                schema_version,
                block_hash,
                expected_gateway_boot_id,
                expected_state_revision,
                capability_fingerprint,
                values,
            } => {
                self.handle_config_apply(
                    device,
                    core_node_id,
                    data_epoch,
                    application_id,
                    operation_id,
                    desired_revision,
                    block_generation,
                    block,
                    schema_version,
                    block_hash,
                    expected_gateway_boot_id,
                    expected_state_revision,
                    capability_fingerprint,
                    values,
                    direct_tx,
                );
            }
        }
        Ok(None)
    }

    /// v1 mechanism (#13): the shared latch writer for BOTH the physical key
    /// and the `MuteSet` frame. Applies the ADC gate immediately, persists
    /// atomically, answers with a `MuteState` fact (echoing the command's
    /// task id when one caused it), and repaints through the compositor —
    /// where the red ring outranks every lamp and animation.
    pub(super) fn set_mute_latch(&self, muted: bool, task_id: Option<String>, source: &str) {
        let changed = {
            let mut sh = lock_or_fault(&self.shared, Some(&self.fault), "role.shared");
            let changed = sh.muted != muted;
            sh.muted = muted;
            sh.mute_revision = sh.mute_revision.wrapping_add(1);
            if let Some(vendor) = &sh.vendor {
                vendor.set_privacy(muted);
            }
            // Mute sovereignty (deliberate deviation from the upstream
            // arc-over-mute ordering): the privacy ring is immediate —
            // a 2 s vanity arc never masks the mic state.
            sh.arc_until = None;
            changed
        };
        let volume = lock_or_fault(&self.shared, Some(&self.fault), "role.shared").volume;
        save_state(&self.cfg.state_dir, volume, muted);
        if changed {
            if let Err(e) = self.hw.set_adc_mute(muted) {
                fwlog!("[firmware] adc mute {muted} failed: {e} (uplink gate still holds)");
            }
        }
        self.report_mute(muted, task_id, source);
        if changed {
            fwlog!("[firmware] mute latch -> {muted} (source {source})");
        }
        render_ring(&self.shared, &self.hw);
    }

    /// A positive dual-mode mute also needs the native mic engine stopped.
    /// Keep service waits off the transport pump and retire stale receipts.
    pub(super) fn report_mute(&self, managed: bool, task_id: Option<String>, source: &str) {
        let (vendor, revision, generation) = {
            let sh = lock_or_fault(&self.shared, Some(&self.fault), "mute.shared");
            (sh.vendor.clone(), sh.mute_revision, sh.link_gen)
        };
        let confirm_native = managed && vendor.is_some();
        let source = source.to_owned();
        let role = self.clone();
        let publish = move || {
            if confirm_native && !vendor.as_ref().is_some_and(|v| v.wait_privacy_applied()) {
                fwlog!("[lx06] native mic mute unconfirmed");
                return;
            }
            let mut sh = lock_or_fault(&role.shared, Some(&role.fault), "mute.shared");
            if generation != sh.link_gen || revision != sh.mute_revision || sh.muted != managed {
                return;
            }
            let muted = managed || vendor.as_ref().is_some_and(|v| v.native_muted_now());
            let Some(tx) = sh.north_tx.clone() else {
                return;
            };
            sh.last_reported_mute = Some(muted);
            tx.send_ctrl(GatewayFrame::MuteState {
                device: role.cfg.device_id.clone(),
                ts: now_ms(),
                seq: role.next_seq(),
                muted,
                source,
                task_id,
            });
        };
        if confirm_native {
            tokio::task::spawn_blocking(publish);
        } else {
            publish();
        }
    }

    /// Paint one assistant state on the ring (palette in leds.rs — reserved
    /// green only reachable via state "listening" by construction). The
    /// device-owned mute overrides every core state: privacy is local.
    /// Suppression (EchoMuse mechanism): while a volume arc is showing, the
    /// state is RECORDED but not painted; arc expiry repaints this latest
    /// frame, so an animation suppressed mid-flight resumes seamlessly.
    pub(super) fn paint_led(&self, state: &str) {
        let (muted, _suppressed, changed) = {
            let mut sh = lock_or_fault(&self.shared, Some(&self.fault), "role.shared");
            let changed = sh.led_state.as_deref() != Some(state);
            // LED B: leaving the green family starts the close fade. The tick
            // paints the ramp and then the new state; painting it here would
            // cut the fade at frame one.
            let leaving_green = sh
                .led_state
                .as_deref()
                .map(leds::is_green_family)
                .unwrap_or(false)
                && !leds::is_green_family(state)
                && !sh.muted
                && sh.arc_until.is_none();
            if leaving_green {
                sh.led_fade = Some((leds::state_rgb("listening"), tokio::time::Instant::now()));
                sh.led_state = Some(state.to_string());
                (sh.muted, true, changed)
            } else {
                sh.led_state = Some(state.to_string());
                (sh.muted, sh.arc_until.is_some(), changed)
            }
        };
        // One line per CHANGE (not per repaint): the serial log is how the
        // "semantic state → palette" link is verified without eyes
        // ([docs/led-states.md] §3 in the core repo).
        if changed {
            let (r, g, b) = leds::state_rgb(if muted { "muted" } else { state })[0];
            fwlog!(
                "[led] state={} rgb={r:02x}{g:02x}{b:02x}",
                if muted { "muted" } else { state }
            );
        }
        // v1 mechanism #12: the recorded state above is the ONLY thing this
        // function decides; what the ring SHOWS is ring_frame's call (the
        // ladder: mute > protocol lamp > arc > link lamps > this state).
        render_ring(&self.shared, &self.hw);
    }

    /// Light the cyan volume arc for VOLUME_ARC_MS (physical-button feedback
    /// only — never driven from the network). The arc sits above the state
    /// ladder for its window; the return is mute-aware.
    fn paint_volume_arc(&self, level: u64) {
        {
            let mut sh = lock_or_fault(&self.shared, Some(&self.fault), "role.shared");
            sh.arc_until = Some(
                tokio::time::Instant::now() + std::time::Duration::from_millis(leds::VOLUME_ARC_MS),
            );
            sh.arc_level = level;
        }
        render_ring(&self.shared, &self.hw);
    }

    /// LED B animation tick (10 Hz). Owns exactly two behaviours:
    ///  - the window-open breath (breathing green) while `listening`;
    ///  - the close fade (green → black over CLOSE_FADE_MS), after which the
    ///    recorded state is painted normally.
    ///
    /// Every other state is painted on change by paint_led, as before.
    pub(super) fn led_tick(&self) {
        let native_mute_changed = {
            let sh = lock_or_fault(&self.shared, Some(&self.fault), "mute.shared");
            !sh.muted
                && sh.vendor.as_ref().is_some_and(|v| {
                    sh.last_reported_mute
                        .is_some_and(|last| last != v.native_muted_now())
                })
        };
        if native_mute_changed {
            self.report_mute(false, None, "native_mute");
        }
        // v1 mechanism #12: the tick is a pure re-render.
        // Breathing, the close fade, fault persistence and arc expiry all
        // fall out of ring_frame; the only stateful bit is retiring a
        // FINISHED fade so it cannot resurrect.
        {
            let mut sh = lock_or_fault(&self.shared, Some(&self.fault), "role.shared");
            if let Some((_, started)) = sh.led_fade {
                let elapsed = tokio::time::Instant::now()
                    .saturating_duration_since(started)
                    .as_millis() as u64;
                if elapsed >= leds::CLOSE_FADE_MS {
                    sh.led_fade = None;
                }
            }
        }
        // v1 mechanism #12: the tick ALWAYS re-renders — the compositor owns
        // the whole ladder (fade retirement, fault persistence, arc expiry),
        // so an idle skip here would strand the ring on a stale frame (the
        // post-fade dark frame was exactly what a bisect leftover broke).
        render_ring(&self.shared, &self.hw);
    }

    /// Arc expiry: clear the window and repaint the latest recorded state,
    /// mute-aware (muted returns the red ring, not the underlying animation).
    pub(super) fn expire_volume_arc(&self) {
        {
            let mut sh = lock_or_fault(&self.shared, Some(&self.fault), "role.shared");
            sh.arc_until = None;
        }
        // Ring repaint (v1 #12): the latest recorded state comes back
        // mute-aware through the compositor, exactly as before.
        render_ring(&self.shared, &self.hw);
    }
}

/// THE ring compositor entry (v1 mechanism #12, audit fix): every paint
/// path mutates `Shared` and ends here — `ring_frame` owns the ladder
/// (mute > protocol lamp > volume arc > offline > wifi > reboot > fade >
/// breath > semantic state), so no call site can bypass it. Free function:
/// both the `Firmware` methods and the northbound tasks (which hold a
/// `Role`) call it.
pub(super) fn render_ring(shared: &std::sync::Mutex<Shared>, hw: &std::sync::Arc<dyn Hardware>) {
    let (frame, fade_done) = {
        let sh = shared.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(vendor) = &sh.vendor {
            let active =
                sh.active.iter().any(Option::is_some) || sh.play_live.iter().any(|live| *live);
            if !active || vendor.home_owns() {
                vendor.set_playback_active(active);
            }
            hw.set_home_indicator(vendor.indicator_owner());
        }
        let fade = sh.led_fade.map(|(from, started)| {
            (
                from,
                tokio::time::Instant::now()
                    .saturating_duration_since(started)
                    .as_millis() as u64,
            )
        });
        let lamp = if sh.link_fault == Some(leds::HealthLamp::Protocol) {
            Some(leds::HealthLamp::Protocol)
        } else if sh.wifi_provisioning {
            Some(leds::HealthLamp::WifiProvisioning)
        } else {
            sh.link_fault
                .or_else(|| sh.wifi_down.then_some(leds::HealthLamp::WifiDown))
        };
        let frame = leds::ring_frame_with_prefs(
            sh.muted,
            lamp,
            sh.arc_until.map(|_| sh.arc_level),
            fade,
            sh.led_state.as_deref().unwrap_or("idle"),
            kws::mono_ms(),
            sh.night_mode,
            leds::minute_of_day_now_with_offset(sh.led_tz_offset_minutes),
            sh.led_orientation,
        );
        let fade_done = matches!(fade, Some((_, e)) if e >= leds::CLOSE_FADE_MS);
        (frame, fade_done)
    };
    if fade_done {
        shared.lock().unwrap_or_else(|e| e.into_inner()).led_fade = None;
    }
    if let Err(e) = hw.led_write(&frame) {
        fwlog!("[firmware] led write failed: {e}");
    }
}
