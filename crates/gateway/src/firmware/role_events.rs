//! Playback facts are scoped to the execution that produced them.
use super::*;
impl Role {
    pub(super) fn on_play_event(&self, ev: PlayEvent) {
        match ev {
            PlayEvent::StreamDone {
                stamp,
                chan,
                bytes,
                periods,
                underruns,
                delay_frames,
            } => {
                let played_ms = bytes * 1000 / (PLAY_RATE as u64 * 2);
                // M6a: "written" is the wire count; the DAC's own backlog at
                // drain time is the only thing that can turn it into
                // "consumed". Never claim consumption the hardware did not
                // report.
                let (consumed_ms, progress_basis) = progress_facts(played_ms, delay_frames);
                let (task, north) = {
                    let mut sh = lock_or_fault(&self.shared, Some(&self.fault), "role.shared");
                    if sh.stamp(chan) != stamp {
                        return;
                    }
                    (sh.chan_slot(chan).take(), sh.north_tx.clone())
                };
                let send = |frame| {
                    if let Some(tx) = &north {
                        tx.send_ctrl(frame);
                    }
                };
                if let Some((task_id, _)) = task {
                    send(GatewayFrame::PlayReceipt {
                        task_id: task_id.clone(),
                        stage: "device_confirmed".into(),
                        detail: Some(format!(
                            "periods={periods} underruns={underruns} bytes={bytes}"
                        )),
                        ts: now_ms(),
                    });
                    send(GatewayFrame::PlayFinished {
                        device: self.cfg.device_id.clone(),
                        task_id,
                        played_ms,
                        consumed_ms,
                        progress_basis,
                        ts: now_ms(),
                    });
                }
            }
            PlayEvent::FlushDone {
                stamp,
                chan,
                played_ms,
                delay_frames,
                flush_err,
                at_ms,
            } => {
                // The hardware flush is done: now the stop can be confirmed,
                // and only for the task the stop named (a new play may have
                // taken the channel slot in between — it owns its own facts).
                //
                // A flush is NOT a finish (device log 2026-11-12): reporting
                // it as PlayFinished either marked a stopped cue as "ended"
                // with the cue's partial played_ms, or — when the next stream
                // had already taken the slot — marked the NEXT play finished
                // 0 s in. The core then dropped that play's record and
                // ignored a barge-in as `no_voice_playback` while audio was
                // still coming out. The stop receipt is the whole fact; the
                // new play's slot is left alone.
                let (outcome, exit_report, north) = {
                    let mut sh = lock_or_fault(&self.shared, Some(&self.fault), "role.shared");
                    if sh.stamp(chan) != stamp {
                        return;
                    }
                    let i = if chan == Chan::Voice { 0 } else { 1 };
                    let stop_task = sh.pending_stop[i].take();
                    // R1d: a local stop is its own fact. It has no pending
                    // `Stop` (the core never asked), so the StopReceipt path
                    // must not be the only reader of this flush.
                    let local = sh.local_stop.take().filter(|_| chan == Chan::Voice);
                    let cur = sh.chan_slot(chan).as_ref().map(|(t, _)| t.clone());
                    let outcome = flush_outcome(
                        stop_task.as_deref(),
                        local.as_ref().map(|(t, at)| (t.as_str(), *at)),
                        cur.as_deref(),
                    );
                    if outcome.takes_slot() {
                        let _ = sh.chan_slot(chan).take();
                    }
                    // Contract v2 §2.5 (#5): is this flush part of a local
                    // exit? The episode holds the channels still owed a
                    // FlushDone, the tasks it cut and the first failure; it
                    // becomes a fact when the last one lands (after the
                    // hardware flush, so "silent" is a hardware truth).
                    let mut report = None;
                    let mut drop_slot = false;
                    let mut finished = false;
                    if let Some(ex) = sh.local_exit.as_mut() {
                        if ex.pending & (1u8 << i) != 0 {
                            ex.pending &= !(1u8 << i);
                            if ex.flush_err.is_none() {
                                ex.flush_err = flush_err.clone();
                            }
                            // Drop the slot we cut: a Stop the core sends for
                            // this task must be answered immediately instead
                            // of re-flushing an empty channel and reporting a
                            // bogus `played_ms: 0`.
                            drop_slot = ex.clears_slot(i, cur.as_deref()) && !outcome.takes_slot();
                            finished = ex.pending == 0;
                        }
                    }
                    if drop_slot {
                        if i == 0 {
                            sh.active[0] = None;
                        } else {
                            sh.active[1] = None;
                        }
                    }
                    if finished {
                        report = sh.local_exit.take();
                    }
                    (outcome, report, sh.north_tx.clone())
                };
                let send = |frame| {
                    if let Some(tx) = &north {
                        tx.send_ctrl(frame);
                    }
                };
                if let Some(ex) = exit_report {
                    // The DAC was dropped at `at_ms`; the hit was at
                    // `fired_ms`. Both are device clocks, so this is the
                    // device's own hit→silence, not the core's lag.
                    let latency_ms = at_ms.saturating_sub(ex.fired_ms);
                    let channels = ex.channels();
                    fwlog!(
                        "[firmware] local_exit_flushed keyword={} channels={:?} latency_ms={latency_ms}{}",
                        ex.keyword,
                        channels,
                        ex.flush_err
                            .as_ref()
                            .map(|e| format!(" flush_failed={e}"))
                            .unwrap_or_default()
                    );
                    send(GatewayFrame::LocalExitFlushed {
                        device: self.cfg.device_id.clone(),
                        keyword: ex.keyword,
                        ts: ex.fired_ms,
                        latency_ms,
                        channels,
                        detail: ex.flush_err.map(|e| format!("flush_failed: {e}")),
                    });
                }
                match outcome {
                    FlushOutcome::LocallyStopped {
                        task_id, fired_ms, ..
                    } => {
                        // M6a: a flush that failed may have left the cue
                        // audible — that is not a stop fact, so it is
                        // reported as a failed play (never as StoppedLocally,
                        // which the core acts on as "the device cut it").
                        if let Some(e) = &flush_err {
                            fwlog!("[firmware] local stop flush failed: {e}");
                            send(GatewayFrame::PlayReceipt {
                                task_id,
                                stage: "failed".into(),
                                detail: Some(format!("flush_failed: {e}")),
                                ts: now_ms(),
                            });
                            return;
                        }
                        // How much audio the core had queued ahead when the
                        // reflex cut this play — the pacing number, measured
                        // at the one moment it is free.
                        let ahead_ms = {
                            let played = {
                                let sh =
                                    lock_or_fault(&self.shared, Some(&self.fault), "role.shared");
                                sh.played[if chan == Chan::Voice { 0 } else { 1 }]
                            };
                            let streams =
                                lock_or_fault(&self.streams, Some(&self.fault), "role.streams");
                            streams
                                .get(&task_id)
                                .and_then(|ctx| ctx.state.try_lock().ok())
                                .map(|state| {
                                    let played_since = played.saturating_sub(state.played_base);
                                    state.pushed.saturating_sub(played_since) * 1000
                                        / PLAY_RATE as u64
                                })
                        };
                        send(GatewayFrame::StoppedLocally {
                            device: self.cfg.device_id.clone(),
                            task_id,
                            reason: "near_end".into(),
                            played_ms,
                            // The device's own fire→flush time: the number a
                            // slow north link can never inflate (criterion
                            // <=300 ms).
                            latency_ms: now_ms().saturating_sub(fired_ms),
                            ahead_ms: ahead_ms.unwrap_or(0),
                            ts: fired_ms,
                        });
                    }
                    FlushOutcome::NamedStop { task_id, .. } => {
                        let (consumed_ms, progress_basis) = progress_facts(played_ms, delay_frames);
                        if let Some(e) = &flush_err {
                            // Not confirmed: the channel may still be playing.
                            fwlog!("[firmware] stop flush failed for {task_id}: {e}");
                            send(GatewayFrame::StopReceipt {
                                task_id,
                                stage: "unknown".into(),
                                played_ms: Some(played_ms),
                                consumed_ms: None,
                                progress_basis: None,
                                detail: Some(format!("flush_failed: {e}")),
                            });
                        } else {
                            send(GatewayFrame::StopReceipt {
                                task_id,
                                stage: "device_confirmed".into(),
                                played_ms: Some(played_ms),
                                consumed_ms,
                                progress_basis,
                                detail: None,
                            });
                        }
                    }
                    // A flush nobody named (a displaced play): the channel's
                    // new owner keeps its slot and owes its own facts.
                    FlushOutcome::Unclaimed => {}
                }
            }
            PlayEvent::WriteFailed { stamps, detail: e } => {
                fwlog!("[firmware] playback write failed: {e}");
                // The playback thread exits on a data-write failure, so it can
                // never emit the receipt (or the StopReceipt) the core is
                // owed. Report the truth per channel and clear the slots: a
                // later Stop then takes the immediate-receipt path instead of
                // parking a `pending_stop` nobody will ever answer (R0 debt:
                // "writei 写失败后到达的 Stop 拿不到回执").
                let mut owed: Vec<(Chan, String, bool)> = Vec::new();
                let north = {
                    let mut sh = lock_or_fault(&self.shared, Some(&self.fault), "role.shared");
                    for i in 0..2 {
                        let chan = if i == 0 { Chan::Voice } else { Chan::Music };
                        if sh.stamp(chan) != stamps[i] {
                            continue;
                        }
                        sh.send_play(&self.play_tx, PlayAction::Flush(chan));
                        lock_or_fault(&self.streams, Some(&self.fault), "role.streams")
                            .retain(|_, ctx| ctx.stamp != stamps[i]);
                        let pending = sh.pending_stop[i].take();
                        let slot = sh.active[i].take();
                        sh.play_live[i] = false;
                        match (slot, pending) {
                            (Some((task_id, _)), p) => owed.push((chan, task_id, p.is_some())),
                            // A stop whose play already left the slot: still
                            // answer it rather than leave the core waiting.
                            (None, Some(task_id)) => owed.push((chan, task_id, true)),
                            (None, None) => {}
                        }
                    }
                    sh.north_tx.clone()
                };
                let send = |frame| {
                    if let Some(tx) = &north {
                        tx.send_ctrl(frame);
                    }
                };
                for (_chan, task_id, stop_owed) in owed {
                    if stop_owed {
                        send(GatewayFrame::StopReceipt {
                            task_id,
                            stage: "failed".into(),
                            played_ms: None,
                            consumed_ms: None,
                            progress_basis: None,
                            detail: None,
                        });
                    } else {
                        send(GatewayFrame::PlayReceipt {
                            task_id,
                            stage: "failed".into(),
                            detail: Some(format!("playback write failed: {e}")),
                            ts: now_ms(),
                        });
                    }
                }
                // A write that failed at the ioctl layer is a dead DAC path,
                // not a bad stream: the playback thread has already left, so
                // no later play could work. M4: leave after a bounded drain
                // so the supervisor can bring the codec up from scratch.
                // (A data-write failure does not block the thread — it
                // RETURNS — so this exit is the ordinary, reliable case.)
                self.fault.request(format!("playback write failed: {e}"));
            }
            PlayEvent::EqApplied { eq_rev } => {
                let (receipt_frame, north) = {
                    let mut sh = lock_or_fault(&self.shared, Some(&self.fault), "role.shared");
                    let Some(pending) = sh.pending_eq_receipt.take() else {
                        return;
                    };
                    if pending.eq_rev != eq_rev {
                        sh.pending_eq_receipt = Some(pending);
                        return;
                    }
                    let mut receipt = pending.receipt;
                    sh.config_journal.state_revision =
                        sh.config_journal.state_revision.saturating_add(1);
                    receipt.state_revision = sh.config_journal.state_revision;
                    receipt.status = crate::protocol::ConfigReceiptStatus::Active;
                    receipt.active_hash = Some(receipt.block_hash.clone());
                    receipt.active_values = Some(pending.persisted_values.clone());
                    receipt.evidence = Some(serde_json::json!({
                        "eq_rev": eq_rev,
                        "applied_at_period_boundary": true,
                    }));
                    sh.config_journal
                        .record_terminal(&receipt, Some(pending.persisted_values));
                    let _ = sh.config_journal.save(&self.cfg.state_dir);
                    (receipt.into_frame(), sh.north_tx.clone())
                };
                if let Some(tx) = north {
                    tx.send_ctrl(receipt_frame);
                }
                fwlog!("[loudness] audio_eq active at 42ms period boundary (eq_rev={eq_rev})");
            }
        }
    }
}
