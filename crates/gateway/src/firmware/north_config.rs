//! `config_apply_v2` handlers (`ConfigStateGet`, `ConfigValidate`, `ConfigApply`)
//! and atomic KWS bundle metadata staging (#112 / #113).

use super::config_journal::{
    self, capability_fingerprint, kws_model_token_hashes, ConfigReceiptData, OpLookup,
};
use super::role::PendingEqConfig;
use super::*;
use crate::protocol::{ConfigBlock, ConfigReceiptStatus, MAX_CONFIG_FRAME_BYTES};

impl Role {
    pub(super) fn stage_kws_bundle_meta(
        &self,
        dir: &std::path::Path,
        kw_hash: &str,
        intents_hash: &str,
        tuning: Option<(f32, f32)>,
        local_exit: u32,
    ) -> Result<(String, u64), String> {
        let tuning_str = match tuning {
            Some((t, s)) => format!("{t:.4}:{s:.4}"),
            None => "default".to_string(),
        };
        let bundle_id =
            fnv1a_hex(format!("{kw_hash}:{intents_hash}:{tuning_str}:{local_exit}").as_bytes());
        let status = lock_or_fault(&self.shared, Some(&self.fault), "role.shared")
            .kws_status
            .clone();
        let next_gen = if let Some(ref st_arc) = status {
            let st = st_arc
                .lock()
                .map_err(|_| "kws_status poisoned".to_string())?;
            st.bundle_generation.saturating_add(1)
        } else {
            1
        };
        kws::write_bundle_meta(dir, &bundle_id, next_gen)?;
        if let Some(st_arc) = status {
            if let Ok(mut st) = st_arc.lock() {
                st.bundle_id = bundle_id.clone();
                st.bundle_generation = next_gen;
            }
        }
        Ok((bundle_id, next_gen))
    }

    pub(super) fn handle_config_state_get(
        &self,
        request_id: String,
        device: String,
        direct_tx: &NorthTx,
    ) {
        if device != self.cfg.device_id {
            return;
        }
        let desc = self.descriptor();
        let kws_dir = {
            let sh = lock_or_fault(&self.shared, Some(&self.fault), "config_state.shared");
            if sh.negotiated_protocol < 2 || !sh.config_apply_v2 {
                fwlog!(
                    "[config_v2] ConfigStateGet ignored: protocol={} config_apply_v2={}",
                    sh.negotiated_protocol,
                    sh.config_apply_v2
                );
                return;
            }
            sh.kws_control
                .as_ref()
                .map(|c| c.dir.clone())
                .unwrap_or_else(|| self.cfg.kws_dir.clone())
        };
        let (model_hash, token_hash) = kws_model_token_hashes(&kws_dir);
        let (boot_id, state_rev, blocks, tz_offset, tz_name) = {
            let sh = lock_or_fault(&self.shared, Some(&self.fault), "config_state.shared");
            (
                sh.gateway_boot_id.clone(),
                sh.config_journal.state_revision,
                sh.config_journal.block_states(),
                sh.led_tz_offset_minutes,
                sh.led_timezone.clone(),
            )
        };
        let _ = direct_tx.send_ctrl(GatewayFrame::ConfigState {
            request_id,
            device,
            gateway_boot_id: boot_id,
            state_revision: state_rev,
            capabilities: desc.capabilities,
            block_schemas: serde_json::json!({
                "kws": 1,
                "audio_eq": 1,
                "led": 1,
                "duplex": 1,
                "acoustics": 1,
            }),
            bounds: serde_json::json!({
                "audio_eq": { "max_bands": 10, "freq_hz": [20.0, 16000.0], "gain_db": [-18.0, 18.0], "q": [0.3, 8.0] },
                "led": { "minute": [0, 1439], "brightness_pct": [0, 100], "rotation": [0, 11], "tz_offset_minutes": [-720, 840] },
                "acoustics": { "volume_cap": [0.0, 1.0], "mix_gain": [0.05, 2.0], "onset_threshold_dbfs": [-120.0, 0.0], "mic_gain_offset_db": [-24.0, 24.0] },
                "kws": { "threshold": [0.0, 1.0], "score": [0.0, 20.0] },
            }),
            model_id: Some(self.cfg.model.clone()),
            model_hash,
            token_hash,
            time_basis: Some(serde_json::json!({
                "timezone": tz_name.unwrap_or_else(|| "UTC".to_string()),
                "tz_offset_minutes": tz_offset,
                "utc_ms": now_ms(),
            })),
            blocks,
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn handle_config_validate(
        &self,
        request_id: String,
        device: String,
        expected_gateway_boot_id: Option<String>,
        expected_state_revision: Option<u64>,
        block: ConfigBlock,
        schema_version: u32,
        block_hash: String,
        values: serde_json::Value,
        direct_tx: &NorthTx,
    ) {
        let desc = self.descriptor();
        let cap_fp = capability_fingerprint(&desc.capabilities);
        let (boot_id, state_rev, kws_dir, v2_negotiated) = {
            let sh = lock_or_fault(&self.shared, Some(&self.fault), "config_validate.shared");
            let dir = sh
                .kws_control
                .as_ref()
                .map(|c| c.dir.clone())
                .unwrap_or_else(|| self.cfg.kws_dir.clone());
            (
                sh.gateway_boot_id.clone(),
                sh.config_journal.state_revision,
                dir,
                sh.negotiated_protocol >= 2 && sh.config_apply_v2,
            )
        };
        let (model_hash, token_hash) = kws_model_token_hashes(&kws_dir);
        let mut blockers = Vec::new();
        if !v2_negotiated {
            blockers.push("config_apply_v2_not_negotiated".into());
        }
        if device != self.cfg.device_id {
            blockers.push(format!("device_mismatch: host is {}", self.cfg.device_id));
        }
        if schema_version != 1 {
            blockers.push(format!("unsupported_schema_version:{schema_version}"));
        }
        if let Some(exp) = expected_gateway_boot_id {
            if exp != boot_id {
                blockers.push(format!("boot_id_mismatch: expected {exp}, got {boot_id}"));
            }
        }
        if let Some(exp_rev) = expected_state_revision {
            if exp_rev != state_rev {
                blockers.push(format!(
                    "state_revision_mismatch: expected {exp_rev}, got {state_rev}"
                ));
            }
        }
        if serde_json::to_vec(&values)
            .map(|v| v.len() > MAX_CONFIG_FRAME_BYTES)
            .unwrap_or(true)
        {
            blockers.push("frame_too_large".into());
        }

        let normalized_values = if blockers.is_empty() {
            match block {
                ConfigBlock::Kws => match config_journal::validate_kws(&values, &kws_dir) {
                    Ok(v) => Some(v.normalized),
                    Err(e) => {
                        blockers.extend(e);
                        None
                    }
                },
                ConfigBlock::AudioEq => match config_journal::validate_audio_eq(&values) {
                    Ok(v) => Some(v.normalized),
                    Err(e) => {
                        blockers.extend(e);
                        None
                    }
                },
                ConfigBlock::Led => match config_journal::validate_led(&values) {
                    Ok(v) => Some(v.normalized),
                    Err(e) => {
                        blockers.extend(e);
                        None
                    }
                },
                ConfigBlock::Duplex => match config_journal::validate_duplex(&values) {
                    Ok(v) => {
                        if v.duplex_requested == "full" {
                            blockers.push("full_duplex_unsupported".into());
                        }
                        Some(v.normalized)
                    }
                    Err(e) => {
                        blockers.extend(e);
                        None
                    }
                },
                ConfigBlock::Acoustics => match config_journal::validate_acoustics(&values) {
                    Ok(v) => Some(v.normalized),
                    Err(e) => {
                        blockers.extend(e);
                        None
                    }
                },
            }
        } else {
            None
        };

        let _ = direct_tx.send_ctrl(GatewayFrame::ConfigValidated {
            request_id,
            device,
            gateway_boot_id: boot_id,
            state_revision: state_rev,
            block,
            block_hash,
            supported: blockers.is_empty(),
            normalized_values,
            capability_fingerprint: Some(cap_fp),
            model_hash,
            token_hash,
            blockers,
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn handle_config_apply(
        &self,
        device: String,
        _core_node_id: String,
        data_epoch: u64,
        application_id: String,
        operation_id: String,
        desired_revision: u64,
        block_generation: u64,
        block: ConfigBlock,
        schema_version: u32,
        block_hash: String,
        expected_gateway_boot_id: Option<String>,
        expected_state_revision: Option<u64>,
        capability_fingerprint_arg: Option<String>,
        values: serde_json::Value,
        direct_tx: &NorthTx,
    ) {
        let desc = self.descriptor();
        let cap_fp = capability_fingerprint(&desc.capabilities);

        let make_receipt = |boot_id: String,
                            state_rev: u64,
                            status: ConfigReceiptStatus,
                            persisted_hash: Option<String>,
                            active_hash: Option<String>,
                            active_values: Option<serde_json::Value>,
                            evidence: Option<serde_json::Value>,
                            error_code: Option<String>,
                            retryable: bool| {
            ConfigReceiptData {
                device: self.cfg.device_id.clone(),
                gateway_boot_id: boot_id,
                operation_id: operation_id.clone(),
                application_id: application_id.clone(),
                data_epoch,
                desired_revision,
                block_generation,
                block,
                block_hash: block_hash.clone(),
                state_revision: state_rev,
                status,
                persisted_hash,
                active_hash,
                active_values,
                evidence,
                error_code,
                retryable,
            }
        };

        // 1. Protocol/capability + device identity check.
        let v2_negotiated = {
            let sh = lock_or_fault(&self.shared, Some(&self.fault), "config_apply.shared");
            sh.negotiated_protocol >= 2 && sh.config_apply_v2
        };
        if !v2_negotiated {
            let (boot_id, state_rev) = {
                let sh = lock_or_fault(&self.shared, Some(&self.fault), "config_apply.shared");
                (sh.gateway_boot_id.clone(), sh.config_journal.state_revision)
            };
            let r = make_receipt(
                boot_id,
                state_rev,
                ConfigReceiptStatus::Unsupported,
                None,
                None,
                None,
                None,
                Some("config_apply_v2_not_negotiated".into()),
                false,
            );
            let _ = direct_tx.send_ctrl(r.into_frame());
            return;
        }
        if device != self.cfg.device_id {
            let (boot_id, state_rev) = {
                let sh = lock_or_fault(&self.shared, Some(&self.fault), "config_apply.shared");
                (sh.gateway_boot_id.clone(), sh.config_journal.state_revision)
            };
            let r = make_receipt(
                boot_id,
                state_rev,
                ConfigReceiptStatus::Failed,
                None,
                None,
                None,
                None,
                Some("device_mismatch".into()),
                false,
            );
            let _ = direct_tx.send_ctrl(r.into_frame());
            return;
        }

        // 2. Frame size cap check.
        if serde_json::to_vec(&values)
            .map(|v| v.len() > MAX_CONFIG_FRAME_BYTES)
            .unwrap_or(true)
        {
            let (boot_id, state_rev) = {
                let sh = lock_or_fault(&self.shared, Some(&self.fault), "config_apply.shared");
                (sh.gateway_boot_id.clone(), sh.config_journal.state_revision)
            };
            let r = make_receipt(
                boot_id,
                state_rev,
                ConfigReceiptStatus::Failed,
                None,
                None,
                None,
                None,
                Some("frame_too_large".into()),
                false,
            );
            let _ = direct_tx.send_ctrl(r.into_frame());
            return;
        }

        // 3. Idempotent journal lookup + preconditions.
        let kws_dir;
        let boot_id;
        let state_rev;
        {
            let sh = lock_or_fault(&self.shared, Some(&self.fault), "config_apply.shared");
            boot_id = sh.gateway_boot_id.clone();
            state_rev = sh.config_journal.state_revision;
            kws_dir = sh
                .kws_control
                .as_ref()
                .map(|c| c.dir.clone())
                .unwrap_or_else(|| self.cfg.kws_dir.clone());

            match sh
                .config_journal
                .lookup_operation(&operation_id, block, &block_hash)
            {
                OpLookup::IdempotentReplay(recorded) => {
                    let _ = direct_tx.send_ctrl((*recorded).into_frame());
                    return;
                }
                OpLookup::Conflict => {
                    let r = make_receipt(
                        boot_id,
                        state_rev,
                        ConfigReceiptStatus::Failed,
                        None,
                        None,
                        None,
                        None,
                        Some("operation_conflict".into()),
                        false,
                    );
                    let _ = direct_tx.send_ctrl(r.into_frame());
                    return;
                }
                OpLookup::New => {}
            }

            if let Err(reason) =
                sh.config_journal
                    .check_high_water(block, data_epoch, block_generation)
            {
                let r = make_receipt(
                    boot_id,
                    state_rev,
                    ConfigReceiptStatus::Superseded,
                    None,
                    None,
                    None,
                    Some(serde_json::json!({ "reason": reason })),
                    Some("stale_generation".into()),
                    false,
                );
                let _ = direct_tx.send_ctrl(r.into_frame());
                return;
            }
        }

        if schema_version != 1 {
            let r = make_receipt(
                boot_id,
                state_rev,
                ConfigReceiptStatus::Unsupported,
                None,
                None,
                None,
                None,
                Some(format!("unsupported_schema_version:{schema_version}")),
                false,
            );
            let _ = direct_tx.send_ctrl(r.into_frame());
            return;
        }
        if let Some(exp) = expected_gateway_boot_id {
            if exp != boot_id {
                let r = make_receipt(
                    boot_id,
                    state_rev,
                    ConfigReceiptStatus::Failed,
                    None,
                    None,
                    None,
                    None,
                    Some("boot_id_mismatch".into()),
                    true,
                );
                let _ = direct_tx.send_ctrl(r.into_frame());
                return;
            }
        }
        if let Some(exp_rev) = expected_state_revision {
            if exp_rev != state_rev {
                let r = make_receipt(
                    boot_id,
                    state_rev,
                    ConfigReceiptStatus::Failed,
                    None,
                    None,
                    None,
                    None,
                    Some("state_revision_mismatch".into()),
                    true,
                );
                let _ = direct_tx.send_ctrl(r.into_frame());
                return;
            }
        }
        if let Some(fp) = capability_fingerprint_arg {
            if fp != cap_fp {
                let r = make_receipt(
                    boot_id,
                    state_rev,
                    ConfigReceiptStatus::Failed,
                    None,
                    None,
                    None,
                    None,
                    Some("capability_fingerprint_mismatch".into()),
                    true,
                );
                let _ = direct_tx.send_ctrl(r.into_frame());
                return;
            }
        }

        // 4. Block-specific validation, persistence, and execution.
        match block {
            ConfigBlock::Kws => {
                let v = match config_journal::validate_kws(&values, &kws_dir) {
                    Ok(v) => v,
                    Err(errs) => {
                        let r = make_receipt(
                            boot_id,
                            state_rev,
                            ConfigReceiptStatus::Failed,
                            None,
                            None,
                            None,
                            Some(serde_json::json!({ "errors": errs })),
                            Some("validation_failed".into()),
                            false,
                        );
                        self.record_and_send_receipt(r, None, direct_tx);
                        return;
                    }
                };
                let (accepted, kw_hash, count, detail, local_exit) =
                    deploy_keywords(&kws_dir, &v.keywords, &v.retreat_words, &v.intents_hash);
                if !accepted {
                    let r = make_receipt(
                        boot_id,
                        state_rev,
                        ConfigReceiptStatus::Failed,
                        None,
                        None,
                        None,
                        Some(serde_json::json!({ "detail": detail })),
                        Some("kws_deploy_rejected".into()),
                        false,
                    );
                    self.record_and_send_receipt(r, None, direct_tx);
                    return;
                }
                let tuning = v.threshold.zip(v.score);
                if let Err(e) = kws::write_tuning(&kws_dir, tuning) {
                    let bak = kws_dir.join("kw.txt.bak");
                    if bak.exists() {
                        let _ = std::fs::rename(&bak, kws_dir.join("kw.txt"));
                    }
                    let r = make_receipt(
                        boot_id,
                        state_rev,
                        ConfigReceiptStatus::Failed,
                        None,
                        None,
                        None,
                        Some(serde_json::json!({ "detail": e })),
                        Some("kws_tuning_write_failed".into()),
                        true,
                    );
                    self.record_and_send_receipt(r, None, direct_tx);
                    return;
                }
                let (bundle_id, bundle_gen) = match self.stage_kws_bundle_meta(
                    &kws_dir,
                    &kw_hash,
                    &v.intents_hash,
                    tuning,
                    local_exit,
                ) {
                    Ok(meta) => meta,
                    Err(e) => {
                        let r = make_receipt(
                            boot_id,
                            state_rev,
                            ConfigReceiptStatus::Failed,
                            None,
                            None,
                            None,
                            Some(serde_json::json!({ "detail": e })),
                            Some("kws_bundle_meta_failed".into()),
                            true,
                        );
                        self.record_and_send_receipt(r, None, direct_tx);
                        return;
                    }
                };

                let (control, status_arc) = {
                    let sh = lock_or_fault(&self.shared, Some(&self.fault), "config_apply.kws");
                    (sh.kws_control.clone(), sh.kws_status.clone())
                };
                if let Some(st_arc) = status_arc {
                    if let Ok(mut st) = st_arc.lock() {
                        if let Some((t, s)) = tuning {
                            st.threshold = t;
                            st.score = s;
                        }
                        st.local_exit = local_exit;
                    }
                }
                let restarted = control.as_ref().map(|c| c.restart()).unwrap_or(false);
                let active_vals = serde_json::json!({
                    "keywords_hash": kw_hash,
                    "intents_hash": v.intents_hash,
                    "keywords": count,
                    "local_exit": local_exit,
                    "threshold": v.threshold,
                    "score": v.score,
                    "bundle_id": bundle_id,
                    "kws_generation": bundle_gen,
                });
                let r = make_receipt(
                    boot_id,
                    state_rev.saturating_add(1),
                    ConfigReceiptStatus::Active,
                    Some(block_hash.clone()),
                    Some(block_hash.clone()),
                    Some(active_vals.clone()),
                    Some(serde_json::json!({
                        "restarted": restarted,
                        "detail": detail,
                    })),
                    None,
                    false,
                );
                self.record_and_send_receipt(r, Some(active_vals), direct_tx);
            }
            ConfigBlock::AudioEq => {
                let v = match config_journal::validate_audio_eq(&values) {
                    Ok(v) => v,
                    Err(errs) => {
                        let r = make_receipt(
                            boot_id,
                            state_rev,
                            ConfigReceiptStatus::Failed,
                            None,
                            None,
                            None,
                            Some(serde_json::json!({ "errors": errs })),
                            Some("validation_failed".into()),
                            false,
                        );
                        self.record_and_send_receipt(r, None, direct_tx);
                        return;
                    }
                };
                let saved = config_journal::SavedAudioEq {
                    curve: v.curve_name.clone(),
                    bands: v.raw_bands.clone(),
                };
                if let Err(e) = config_journal::save_audio_eq(&self.cfg.state_dir, &saved) {
                    let r = make_receipt(
                        boot_id,
                        state_rev,
                        ConfigReceiptStatus::Failed,
                        None,
                        None,
                        None,
                        Some(serde_json::json!({ "detail": e })),
                        Some("persistence_failed".into()),
                        true,
                    );
                    self.record_and_send_receipt(r, None, direct_tx);
                    return;
                }
                let playback_alive = {
                    let sh = lock_or_fault(&self.shared, Some(&self.fault), "config_apply.eq");
                    sh.play_tx.is_some() && !self.fault.is_requested()
                };
                if !playback_alive {
                    let r = make_receipt(
                        boot_id,
                        state_rev,
                        ConfigReceiptStatus::Failed,
                        Some(block_hash.clone()),
                        None,
                        None,
                        None,
                        Some("playback_unavailable".into()),
                        true,
                    );
                    self.record_and_send_receipt(r, Some(v.normalized), direct_tx);
                    return;
                }

                // Stage EQ in Shared and defer the `Active` receipt until
                // `playback.rs` rebuilds `Loudness` at the 42 ms period boundary.
                let new_eq_rev = {
                    let mut sh = lock_or_fault(&self.shared, Some(&self.fault), "config_apply.eq");
                    sh.eq_curve = Some(v.curve);
                    sh.eq_bands = v.bands.clone();
                    sh.eq_rev = sh.eq_rev.wrapping_add(1);
                    let rev = sh.eq_rev;
                    let pending_receipt = make_receipt(
                        boot_id,
                        state_rev,
                        ConfigReceiptStatus::Persisted,
                        Some(block_hash.clone()),
                        None,
                        None,
                        None,
                        None,
                        false,
                    );
                    sh.pending_eq_receipt = Some(PendingEqConfig {
                        eq_rev: rev,
                        receipt: pending_receipt,
                        persisted_values: v.normalized.clone(),
                    });
                    rev
                };
                fwlog!(
                    "[loudness] audio_eq staged for 42ms period rebuild (eq_rev={new_eq_rev}, curve={:?}, bands={})",
                    v.curve,
                    v.bands.len()
                );
            }
            ConfigBlock::Led => {
                let v = match config_journal::validate_led(&values) {
                    Ok(v) => v,
                    Err(errs) => {
                        let r = make_receipt(
                            boot_id,
                            state_rev,
                            ConfigReceiptStatus::Failed,
                            None,
                            None,
                            None,
                            Some(serde_json::json!({ "errors": errs })),
                            Some("validation_failed".into()),
                            false,
                        );
                        self.record_and_send_receipt(r, None, direct_tx);
                        return;
                    }
                };
                if let Err(e) = leds::save_led_config(
                    &self.cfg.state_dir,
                    v.night_mode,
                    v.orientation,
                    v.tz_offset_minutes,
                    Some(v.timezone.clone()),
                ) {
                    let r = make_receipt(
                        boot_id,
                        state_rev,
                        ConfigReceiptStatus::Failed,
                        None,
                        None,
                        None,
                        Some(serde_json::json!({ "detail": e })),
                        Some("persistence_failed".into()),
                        true,
                    );
                    self.record_and_send_receipt(r, None, direct_tx);
                    return;
                }
                {
                    let mut sh = lock_or_fault(&self.shared, Some(&self.fault), "config_apply.led");
                    sh.night_mode = v.night_mode;
                    sh.led_orientation = v.orientation;
                    sh.led_tz_offset_minutes = v.tz_offset_minutes;
                    sh.led_timezone = Some(v.timezone.clone());
                }
                render_ring(&self.shared, &self.hw);
                let r = make_receipt(
                    boot_id,
                    state_rev.saturating_add(1),
                    ConfigReceiptStatus::Active,
                    Some(block_hash.clone()),
                    Some(block_hash.clone()),
                    Some(v.normalized.clone()),
                    Some(serde_json::json!({
                        "minute_of_day": leds::minute_of_day_now_with_offset(v.tz_offset_minutes),
                    })),
                    None,
                    false,
                );
                self.record_and_send_receipt(r, Some(v.normalized), direct_tx);
            }
            ConfigBlock::Duplex => {
                let v = match config_journal::validate_duplex(&values) {
                    Ok(v) => v,
                    Err(errs) => {
                        let r = make_receipt(
                            boot_id,
                            state_rev,
                            ConfigReceiptStatus::Failed,
                            None,
                            None,
                            None,
                            Some(serde_json::json!({ "errors": errs })),
                            Some("validation_failed".into()),
                            false,
                        );
                        self.record_and_send_receipt(r, None, direct_tx);
                        return;
                    }
                };
                {
                    let mut sh =
                        lock_or_fault(&self.shared, Some(&self.fault), "config_apply.duplex");
                    sh.duplex_requested = v.duplex_requested.clone();
                    sh.duplex_effective = "half".into();
                }
                let active_vals = serde_json::json!({
                    "capability": "half",
                    "requested": v.duplex_requested,
                    "negotiated": "half",
                    "effective": "half",
                });
                let (status, error_code) = if v.duplex_requested == "full" {
                    (
                        ConfigReceiptStatus::Unsupported,
                        Some("full_duplex_unsupported".into()),
                    )
                } else {
                    (ConfigReceiptStatus::Active, None)
                };
                let r = make_receipt(
                    boot_id,
                    state_rev.saturating_add(1),
                    status,
                    Some(block_hash.clone()),
                    (status == ConfigReceiptStatus::Active).then(|| block_hash.clone()),
                    Some(active_vals.clone()),
                    None,
                    error_code,
                    false,
                );
                self.record_and_send_receipt(r, Some(active_vals), direct_tx);
            }
            ConfigBlock::Acoustics => {
                let v = match config_journal::validate_acoustics(&values) {
                    Ok(v) => v,
                    Err(errs) => {
                        let r = make_receipt(
                            boot_id,
                            state_rev,
                            ConfigReceiptStatus::Failed,
                            None,
                            None,
                            None,
                            Some(serde_json::json!({ "errors": errs })),
                            Some("validation_failed".into()),
                            false,
                        );
                        self.record_and_send_receipt(r, None, direct_tx);
                        return;
                    }
                };
                let saved = config_journal::SavedAcoustics {
                    volume_cap: v.volume_cap,
                    mix_gain: v.mix_gain,
                    onset_threshold_dbfs: v.onset_threshold_dbfs,
                    mic_gain_offset_db: v.mic_gain_offset_db,
                };
                if let Err(e) = config_journal::save_acoustics(&self.cfg.state_dir, &saved) {
                    let r = make_receipt(
                        boot_id,
                        state_rev,
                        ConfigReceiptStatus::Failed,
                        None,
                        None,
                        None,
                        Some(serde_json::json!({ "detail": e })),
                        Some("persistence_failed".into()),
                        true,
                    );
                    self.record_and_send_receipt(r, None, direct_tx);
                    return;
                }
                let barge_arc = {
                    let mut sh =
                        lock_or_fault(&self.shared, Some(&self.fault), "config_apply.acoustics");
                    sh.volume_cap = v.volume_cap;
                    sh.mix_gain = v.mix_gain;
                    sh.vad_threshold = v.onset_threshold_dbfs;
                    sh.mic_gain_offset_db = v.mic_gain_offset_db;
                    sh.barge.clone()
                };
                if let Some(barge) = barge_arc {
                    if let Ok(mut det) = barge.lock() {
                        det.set_onset_db(v.onset_threshold_dbfs);
                    }
                }
                let r = make_receipt(
                    boot_id,
                    state_rev.saturating_add(1),
                    ConfigReceiptStatus::Active,
                    Some(block_hash.clone()),
                    Some(block_hash.clone()),
                    Some(v.normalized.clone()),
                    Some(serde_json::json!({
                        "unit_id": self.cfg.profile.unit_id,
                    })),
                    None,
                    false,
                );
                self.record_and_send_receipt(r, Some(v.normalized), direct_tx);
            }
        }
    }

    fn record_and_send_receipt(
        &self,
        mut receipt: ConfigReceiptData,
        persisted_values: Option<serde_json::Value>,
        direct_tx: &NorthTx,
    ) {
        {
            let mut sh = lock_or_fault(&self.shared, Some(&self.fault), "config_receipt.shared");
            if matches!(
                receipt.status,
                ConfigReceiptStatus::Persisted
                    | ConfigReceiptStatus::Active
                    | ConfigReceiptStatus::Unsupported
            ) {
                sh.config_journal.state_revision =
                    sh.config_journal.state_revision.saturating_add(1);
                receipt.state_revision = sh.config_journal.state_revision;
            }
            sh.config_journal
                .record_terminal(&receipt, persisted_values);
            let _ = sh.config_journal.save(&self.cfg.state_dir);
        }
        let _ = direct_tx.send_ctrl(receipt.into_frame());
    }
}
