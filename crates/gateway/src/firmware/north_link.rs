use super::*;

// --- northbound link --------------------------------------------------------------

/// v1 mechanism (#11): a handshake the LINK refused — version interval with
/// no intersection or a capability gap. Distinct from an unreachable core so
/// the lamp and the retry pace can differ.
#[derive(Debug)]
pub(super) struct ProtocolMismatch(pub(super) String);
impl std::fmt::Display for ProtocolMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "protocol mismatch: {}", self.0)
    }
}
impl std::error::Error for ProtocolMismatch {}

/// v1 mechanism (#11/#12): paint the protocol-error lamp FIRST (it must be
/// visible even if the peer never lets us say why), then send the
/// `ProtocolError` frame over the handshake socket itself — the writer task
/// does not exist yet — with a bounded write, inside
/// PROTOCOL_FAIL_DISCONNECT_MS. Best-effort: an OLD core logs-and-skips a
/// runtime frame it cannot decode, and a HELLO-stage one disconnects; either
/// way the lamp is already up and the reconnect pace is ours.
pub(super) async fn refuse_link(
    role: &Role,
    wr: &mut tokio::net::tcp::OwnedWriteHalf,
    reason: &str,
) {
    {
        let mut sh = lock_or_fault(&role.shared, Some(&role.fault), "role.shared");
        sh.link_fault = Some(leds::HealthLamp::Protocol);
    }
    render_ring(&role.shared, &role.hw);
    let err = GatewayFrame::ProtocolError {
        protocol: PROTOCOL_VERSION,
        min_protocol: PROTOCOL_MIN_VERSION,
        gateway_version: role.cfg.version.clone(),
        reason: reason.to_string(),
    };
    let _ = northbound::write_handshake(wr, &err, &role.northbound_stats).await;
}

pub(super) async fn north_loop(role: Role) {
    let mut backoff = RECONNECT_MIN_MS;
    let mut attempts: u64 = 0;
    loop {
        let result = north_connect_and_serve(&role).await;
        // M6a: the link IS the generation. Every ended session — clean EOF or
        // error — tears the audio state down before the reconnect, so an old
        // play can neither keep playing into the next generation nor report
        // its terminal facts there. (A fresh connection must start silent.)
        role.teardown_link();
        role.northbound_stats.cleanup();
        // v1 mechanism (#12): the lamp follows the FAULT KIND. A protocol
        // refusal is sticky (only a full handshake clears it) and retries
        // slowly — that pacing is what makes the lamp survivable: when the
        // peer is rolled back to a compatible version, the next attempt
        // succeeds and the lamp clears through the normal render. Everything
        // else is `Offline`, retried with the usual backoff. The compositor
        // keeps the mute latch above any lamp.
        let protocol_fail = result
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<ProtocolMismatch>())
            .map(|p| p.0.clone());
        let was_clean_eof = matches!(&result, Ok(()));
        match result {
            Ok(()) => fwlog!("[firmware] core link closed cleanly"),
            Err(e) => {
                attempts += 1;
                if attempts == 1 || attempts.is_multiple_of(10) {
                    fwlog!("[firmware] core link lost (attempt {attempts}, retrying in {backoff} ms): {e}");
                }
            }
        }
        if was_clean_eof {
            attempts = 0;
            backoff = RECONNECT_MIN_MS;
        }
        if !was_clean_eof {
            {
                let mut sh = lock_or_fault(&role.shared, Some(&role.fault), "role.shared");
                sh.link_fault = Some(match &protocol_fail {
                    Some(_) => leds::HealthLamp::Protocol,
                    None => leds::HealthLamp::Offline,
                });
            }
            render_ring(&role.shared, &role.hw);
        }
        let wait = if protocol_fail.is_some() {
            // v1 (#11): while the lamp shows a protocol error, reconnects are
            // slow and deliberate (PROTOCOL_ERROR_RETRY_MS), not a hot loop.
            PROTOCOL_ERROR_RETRY_MS
        } else {
            backoff
        };
        tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
        backoff = (backoff * 2).min(RECONNECT_MAX_MS);
    }
}

/// v1 mechanism (#11) timeouts. Monotonic (tokio). CONNECT bounds the TCP
/// dial itself; HELLO bounds the whole ack wait (the old code had NO ack
/// timeout — a silent core parked the handshake task forever).
const CONNECT_TIMEOUT_MS: u64 = 3000;
const HELLO_TIMEOUT_MS: u64 = 5000;
/// Budget for writing the refusal frame before the socket closes anyway.
const REJECT_WRITE_TIMEOUT_MS: u64 = 1000;

pub(super) async fn north_connect_and_serve(
    role: &Role,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let stream = tokio::time::timeout(
        std::time::Duration::from_millis(CONNECT_TIMEOUT_MS),
        TcpStream::connect(&role.cfg.core_addr),
    )
    .await
    .map_err(|_| format!("core dial timed out after {CONNECT_TIMEOUT_MS} ms"))??;
    let (rd, wr) = stream.into_split();

    let generation = lock_or_fault(&role.shared, Some(&role.fault), "north.shared")
        .link_gen
        .wrapping_add(1);
    let tx = NorthTx::new(
        role.cfg.device_id.clone(),
        generation,
        role.northbound_stats.clone(),
        role.dropped_audio.clone(),
    );
    let result = serve_link(role, rd, wr, tx.clone()).await;
    {
        let mut sh = lock_or_fault(&role.shared, Some(&role.fault), "north.shared");
        if sh
            .north_tx
            .as_ref()
            .is_some_and(|current| current.same_connection(&tx))
        {
            sh.north_tx = None;
        }
    }
    tx.fail(
        if result
            .as_ref()
            .err()
            .is_some_and(|e| e.downcast_ref::<ProtocolMismatch>().is_some())
        {
            "protocol_mismatch"
        } else {
            "peer_closed"
        },
    );
    result
}

pub(super) async fn serve_link(
    role: &Role,
    rd: tokio::net::tcp::OwnedReadHalf,
    mut wr: tokio::net::tcp::OwnedWriteHalf,
    tx: NorthTx,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _activity = role
        .northbound_stats
        .activity(northbound::ActivityKind::Reader);
    let mut lines = BufReader::new(rd);
    let hello = GatewayFrame::Hello {
        protocol: PROTOCOL_VERSION,
        min_protocol: PROTOCOL_MIN_VERSION,
        supported_versions: crate::protocol::SUPPORTED_PROTOCOLS.to_vec(),
        gateway: format!(
            "home-mate-device-gateway {} ({})",
            env!("CARGO_PKG_VERSION"),
            role.cfg.profile.gateway_tag
        ),
        gateway_version: role.cfg.version.clone(),
        aec_tail_ms: crate::protocol::aec_tail_ms(role.cfg.aec, role.cfg.aec_tail),
        profile_version: Some(crate::protocol::PROFILE_VERSION.to_string()),
        devices: vec![role.descriptor()],
    };
    if let Err(error) = northbound::write_handshake(&mut wr, &hello, &role.northbound_stats).await {
        tx.fail(northbound::write_error_reason(&error));
        return Err(error.into());
    }

    // v1 (#11): the ack must arrive inside HELLO_TIMEOUT_MS — a core that
    // accepted the TCP connection but never answers is OFFLINE, not healthy.
    let line = match tokio::time::timeout(
        std::time::Duration::from_millis(HELLO_TIMEOUT_MS),
        crate::protocol::read_line_capped(&mut lines),
    )
    .await
    {
        Err(_) => return Err(format!("no hello_ack within {HELLO_TIMEOUT_MS} ms").into()),
        Ok(r) => match r? {
            LineRead::Line(l) => l,
            LineRead::Oversized { .. } => return Err("hello_ack exceeds frame cap".into()),
            LineRead::Eof => return Err("core closed during hello".into()),
        },
    };
    let my_interval = (PROTOCOL_MIN_VERSION, PROTOCOL_VERSION);
    match crate::protocol::decode_core(&line)? {
        CoreFrame::HelloAck {
            protocol,
            min_protocol,
            supported_versions,
            capabilities,
            core_version,
            generation,
        } => {
            // v1 (#11) + v2: interval & explicit supported_versions intersection.
            let Some(operating) = crate::protocol::protocol_operating_version_with_supported(
                my_interval,
                crate::protocol::SUPPORTED_PROTOCOLS,
                (min_protocol, protocol),
                &supported_versions,
            ) else {
                let reason = crate::protocol::protocol_no_intersection_reason(
                    my_interval,
                    (min_protocol, protocol),
                );
                refuse_link(role, &mut wr, &reason).await;
                return Err(ProtocolMismatch(reason).into());
            };
            let config_v2 = operating == 2 && capabilities.iter().any(|c| c == "config_apply_v2");
            // First verdict made: `Reboot` is over for this boot, whatever
            // happens later. A full handshake is the ONLY thing that clears
            // a sticky protocol lamp.
            {
                let mut sh = lock_or_fault(&role.shared, Some(&role.fault), "north.shared");
                sh.link_fault = None;
                sh.negotiated_protocol = operating;
                sh.config_apply_v2 = config_v2;
            }
            fwlog!(
                "[firmware] connected to core {} (generation {generation}, core {core_version}, operating protocol {operating}, config_apply_v2={config_v2})",
                role.cfg.core_addr
            );
        }
        CoreFrame::HelloReject {
            reason,
            core_version,
            ..
        } => {
            // v1 (#11): the CORE refused us and said why (it sent this over
            // the handshake socket before closing). Lamp + log, then stay
            // down at the protocol-error pace.
            fwlog!("[firmware] core refused the handshake (core {core_version}): {reason}");
            refuse_link(role, &mut wr, &format!("refused by core: {reason}")).await;
            return Err(ProtocolMismatch(format!("refused by core: {reason}")).into());
        }
        f => return Err(format!("expected hello_ack, got {f:?}").into()),
    }

    tx.mark_handshake();
    // Historical backoff reset still depends on clean EOF, not this handshake fact.
    // After (re)connect, make the core's device state true again:
    // online + descriptor, device volume, mic truth, mute truth, LED repaint.
    let _link_gen = {
        let mut sh = lock_or_fault(&role.shared, Some(&role.fault), "north.shared");
        sh.link_gen = sh.link_gen.wrapping_add(1);
        let link_gen = sh.link_gen;
        sh.north_tx = Some(tx.clone());
        let descriptor = role.descriptor();
        let volume_level = sh.volume;
        let mic_on = sh.mic_on;
        let muted = sh.muted;
        drop(sh);
        tx.send_ctrl(GatewayFrame::DeviceStatus {
            device: role.cfg.device_id.clone(),
            online: true,
            reason: "direct-component-up".into(),
            descriptor: Some(descriptor),
            northbound: None,
        });
        // Keyword reflex truth on every (re)connect: a core that restarted
        // must not guess whether the spotter is alive.
        let status = {
            lock_or_fault(&role.shared, Some(&role.fault), "north.shared")
                .kws_status
                .clone()
        };
        if let Some(status) = status {
            let report = {
                let st = status.lock().unwrap_or_else(|e| e.into_inner());
                GatewayFrame::KwsReport {
                    device: role.cfg.device_id.clone(),
                    state: st.state.clone(),
                    detail: st.detail.clone(),
                    restarts: st.restarts,
                    detections: st.detections,
                    guarded: st.guarded,
                    epoch: st.epoch_ms,
                    loaded_hash: st.loaded_hash.clone(),
                    loaded_ms: st.loaded_ms,
                    local_exit: st.local_exit,
                    threshold: st.threshold,
                    score: st.score,
                }
            };
            tx.send_ctrl(report);
        }
        tx.send_ctrl(GatewayFrame::VolumeReceipt {
            task_id: format!("vol:boot:{}", role.next_seq()),
            effective: volume_level as f64 / 127.0,
            unit: "linear".into(),
            stage: "device_confirmed".into(),
            device: Some(role.cfg.device_id.clone()),
        });
        if mic_on {
            let (layout, channels) = role.mic_layout();
            tx.send_ctrl(GatewayFrame::MicReport {
                device: role.cfg.device_id.clone(),
                active: true,
                layout: Some(layout.into()),
                sample_rate: Some(CAP_RATE),
                channels: Some(channels),
                detail: if role.cfg.aec {
                    Some(format!(
                        "aec=on:7mic-speex tail={}ms frame={}ms +fusion(dealias armed)",
                        role.cfg.aec_tail * 1000 / CAP_RATE as usize,
                        aec::FRAME * 1000 / CAP_RATE as usize
                    ))
                } else {
                    None
                },
            });
        }
        // Mute truth always goes north (not only when muted): the core's
        // view must match the hardware after any reconnect. No task_id — a
        // boot report is a fact, never a command receipt (v1 #13).
        role.report_mute(muted, None, "device_boot");
        // v1 mechanism #12: the reconnect repaint goes through the compositor
        // like every other paint (mute-aware, lamp-aware, arc-aware).
        render_ring(&role.shared, &role.hw);
        link_gen
    };

    let writer_tx = tx.clone();
    let mut writer = tokio::spawn(async move { writer_tx.write_to(wr).await });

    let mut writer_finished = false;
    let result: Result<(), Box<dyn std::error::Error + Send + Sync>> = async { loop {
        tokio::select! {
            biased;
            _=tx.failed()=>break Err(tx.reason().into()),
            read = crate::protocol::read_line_capped(&mut lines) => {
                match read.inspect_err(|_| {tx.fail("reader_error");})? {
                    LineRead::Eof => break Ok(()),
                    LineRead::Oversized { task_id } => {
                        fwlog!(
                            "[firmware] inbound frame exceeds {} bytes limit — skipped",
                            crate::protocol::MAX_FRAME_BYTES
                        );
                        if let Some(task_id) = task_id {
                            tx.send_ctrl(GatewayFrame::PlayReceipt {
                                task_id,
                                stage: "failed".into(),
                                detail: Some("frame exceeds 8 MiB limit".into()),
                                ts: now_ms(),
                            });
                        }
                    }
                    LineRead::Line(line) => {
                        if line.is_empty() {
                            continue;
                        }
                        let frame: CoreFrame = match serde_json::from_str(&line) {
                            Ok(f) => f,
                            Err(e) => {
                                fwlog!("[firmware] undecodable core frame: {e}");
                                continue;
                            }
                        };
                        if tx.is_terminal() { break Err(tx.reason().into()); }
                        match role.on_core_frame(frame, &tx) {
                            Ok(None) => {}
                            Ok(Some(cmd)) => {
                                let _waiting=role.northbound_stats.activity(northbound::ActivityKind::DecodeWait);
                                tokio::select! {
                                    biased;
                                    _=tx.failed()=>break Err(tx.reason().into()),
                                    res = role.decode_tx.send(cmd) => {
                                        if res.is_err() {
                                            tx.fail("decode_error");
                                            break Err("decode worker exited".into());
                                        }
                                    }
                                    w = &mut writer => {
                                        writer_finished = true;
                                        break match w {
                                            Ok(Ok(())) => Err("north writer exited".into()),
                                            Ok(Err(e)) => Err(format!("north writer failed: {e}").into()),
                                            Err(e) => {tx.fail("writer_panic");Err(format!("north writer panicked: {e}").into())},
                                        };
                                    }

                                }
                            }
                            Err(mismatch) => {
                                fwlog!("[firmware] protocol violation — dropping core connection");
                                break Err(mismatch);
                            }
                        }
                    }
                }
            }
            w = &mut writer => {
                writer_finished = true;
                break match w {
                    Ok(Ok(())) => Err("north writer exited".into()),
                    Ok(Err(e)) => Err(format!("north writer failed: {e}").into()),
                    Err(e) => {tx.fail("writer_panic");Err(format!("north writer panicked: {e}").into())},
                };
            }

        }
    }}.await;
    let protocol_failure = result
        .as_ref()
        .err()
        .is_some_and(|e| e.downcast_ref::<ProtocolMismatch>().is_some());
    if !protocol_failure {
        tx.fail("peer_closed");
    }

    if !writer_finished {
        if result
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<ProtocolMismatch>())
            .is_some()
        {
            if tokio::time::timeout(
                std::time::Duration::from_millis(REJECT_WRITE_TIMEOUT_MS),
                &mut writer,
            )
            .await
            .is_err()
            {
                writer.abort();
                let _ = writer.await;
            }
        } else {
            writer.abort();
            let _ = writer.await;
        }
    }
    result
}
