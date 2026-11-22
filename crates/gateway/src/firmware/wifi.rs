use super::*;

pub(super) fn spawn_wifi_probe(shared: Arc<Mutex<Shared>>, hw: Arc<dyn Hardware>) {
    std::thread::spawn(move || {
        const WIFI_PROBE_TIMEOUT_MS: u64 = 500;
        const WIFI_PROBE_INTERVAL_SECS: u64 = 2;
        let mut down_streak = 0u32;
        let mut fail_streak = 0u32;
        loop {
            let answer = Role::wifi_status_text_bounded(std::time::Duration::from_millis(
                WIFI_PROBE_TIMEOUT_MS,
            ));
            let mut verdict: Option<bool> = None; // Some(associated?)
            match answer {
                Some(text) => {
                    fail_streak = 0;
                    verdict = Some(Role::ssid_from_status(&text).is_some());
                }
                None => {
                    fail_streak += 1;
                    if fail_streak > 3 {
                        verdict = Some(true); // probe unusable: stop claiming down
                    }
                }
            }
            if let Some(associated) = verdict {
                down_streak = if associated { 0 } else { down_streak + 1 };
                let now_down = down_streak >= 2;
                let changed = {
                    let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
                    let changed = sh.wifi_down != now_down;
                    sh.wifi_down = now_down;
                    changed
                };
                if changed {
                    // The lamp re-renders; if the loopback core is up
                    // too, offline wins and wifi stays unstated —
                    // that precedence lives in ring_frame alone.
                    render_ring(&shared, &hw);
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(WIFI_PROBE_INTERVAL_SECS));
        }
    });
}

impl Role {
    // --- online WiFi change (v0.2.3, stage C replaced-capability #1-2) ----------

    const WPA_CONF: &str = "/data/misc/wifi/wpa_supplicant.conf";
    const WPA_CTRL_DIR: &str = "/data/misc/wifi/sockets";
    const LX06_WPA_CONF: &str = "/data/wifi/wpa.conf";
    const LX06_WPA_CTRL_DIR: &str = "/var/run/wpa_supplicant";
    const WIFI_JOIN_TIMEOUT_SECS: u64 = 20;

    fn wpa_paths() -> (&'static str, &'static str, &'static str) {
        // Use access(2) rather than metadata() to avoid statx(397) traps on
        // 32-bit armv7 kernels, and check /data/wifi/wpa.conf so desktop Linux
        // CI hosts with /usr/sbin/wpa_cli do not mis-select the LX06 path.
        let is_lx06 = unsafe {
            libc::access(c"/data/wifi".as_ptr(), libc::F_OK) == 0
                && libc::access(c"/usr/sbin/wpa_cli".as_ptr(), libc::X_OK) == 0
        };
        if is_lx06 {
            (
                "/usr/sbin/wpa_cli",
                Self::LX06_WPA_CTRL_DIR,
                Self::LX06_WPA_CONF,
            )
        } else {
            ("/system/bin/wpa_cli", Self::WPA_CTRL_DIR, Self::WPA_CONF)
        }
    }

    fn wpa_cli(args: &[&str]) -> Option<String> {
        let (bin, ctrl_dir, _) = Self::wpa_paths();
        let out = std::process::Command::new(bin)
            .arg("-p")
            .arg(ctrl_dir)
            .arg("-i")
            .arg("wlan0")
            .args(args)
            .output()
            .ok()?;
        Some(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// Associated SSID (wpa_state=COMPLETED), for the descriptor and the
    /// watchdog's restore check.
    pub fn wifi_current_ssid() -> Option<String> {
        Self::wifi_status_text().and_then(|out| Self::ssid_from_status(&out))
    }

    /// Raw `wpa_cli status` text — None when the command failed or produced
    /// nothing (v1 mechanism #12: the probe must be able to tell "not
    /// associated" from "I could not ask").
    fn wifi_status_text() -> Option<String> {
        let out = Self::wpa_cli(&["status"])?;
        (!out.is_empty()).then_some(out)
    }

    /// Bounded `wpa_cli status` query for the background probe: spawns the
    /// `Child` directly, polls `try_wait()` up to `timeout`, and `kill()` +
    /// `wait()`s on timeout so a hung `wpa_cli` never leaks a thread or zombie.
    pub(super) fn wifi_status_text_bounded(timeout: std::time::Duration) -> Option<String> {
        let (bin, ctrl_dir, _) = Self::wpa_paths();
        let mut child = std::process::Command::new(bin)
            .arg("-p")
            .arg(ctrl_dir)
            .arg("-i")
            .arg("wlan0")
            .arg("status")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => {
                    use std::io::Read;
                    let mut buf = Vec::new();
                    if let Some(mut stdout) = child.stdout.take() {
                        let _ = stdout.read_to_end(&mut buf);
                    }
                    let text = String::from_utf8_lossy(&buf).to_string();
                    return (!text.is_empty()).then_some(text);
                }
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return None;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        }
    }

    pub(super) fn ssid_from_status(out: &str) -> Option<String> {
        let mut ssid: Option<String> = None;
        let mut completed = false;
        for line in out.lines() {
            if let Some(v) = line.strip_prefix("ssid=") {
                ssid = Some(v.trim().to_string());
            }
            if line.trim() == "wpa_state=COMPLETED" {
                completed = true;
            }
        }
        if completed {
            ssid
        } else {
            None
        }
    }

    fn wifi_joined(target: &str) -> bool {
        Self::wifi_current_ssid().as_deref() == Some(target)
    }

    fn wifi_write_conf(ssid: &str, psk: &str) -> std::io::Result<()> {
        if let Some(why) = wifi_credentials_rejected(ssid, psk) {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, why));
        }
        let (_, ctrl_dir, wpa_conf) = Self::wpa_paths();
        let conf = format!(
            "ctrl_interface={}\nupdate_config=1\nnetwork={{\n\tssid=\"{}\"\n\tpsk=\"{}\"\n\tkey_mgmt=WPA-PSK\n}}\n",
            ctrl_dir, ssid, psk
        );
        std::fs::write(wpa_conf, conf)
    }

    /// Apply a new network with a join watchdog: if the new SSID does not
    /// associate within the timeout, the backup is restored and
    /// reconfigured — an unattended change can never strand the device.
    /// The north link (loopback to the on-speaker core) survives the wlan
    /// reset, so the WifiState receipt reliably reaches the core.
    pub(super) fn wifi_change(&self, ssid: String, psk: String) {
        let (_, _, wpa_conf) = Self::wpa_paths();
        let exit_provisioning_and_report = |state: &str, detail: String, ssid: &str| {
            {
                let mut sh = lock_or_fault(&self.shared, Some(&self.fault), "wifi.shared");
                sh.wifi_provisioning = false;
                sh.wifi_provisioning_until_ms = None;
            }
            render_ring(&self.shared, &self.hw);
            fwlog!("[hm-firmware] wifi {state} ssid={ssid} {detail}");
            self.north(GatewayFrame::WifiState {
                device: self.cfg.device_id.clone(),
                ts: now_ms(),
                ssid: ssid.to_string(),
                state: state.into(),
                detail: Some(detail),
            });
        };
        if let Some(why) = wifi_credentials_rejected(&ssid, &psk) {
            exit_provisioning_and_report("reverted", format!("validation: {why}"), &ssid);
            return;
        }
        let bak = format!("{wpa_conf}.hm-bak");
        if let Err(e) = std::fs::copy(wpa_conf, &bak) {
            exit_provisioning_and_report("reverted", format!("backup failed: {e}"), &ssid);
            return;
        }
        let old_ssid = Self::wifi_current_ssid().unwrap_or_default();
        if let Err(e) = Self::wifi_write_conf(&ssid, &psk) {
            exit_provisioning_and_report("reverted", format!("conf write failed: {e}"), &ssid);
            return;
        }
        let _ = Self::wpa_cli(&["reconfigure"]);
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(Self::WIFI_JOIN_TIMEOUT_SECS);
        while std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_secs(2));
            if Self::wifi_joined(&ssid) {
                exit_provisioning_and_report("applied", "joined".into(), &ssid);
                return;
            }
        }
        // Watchdog fired: restore the backup and re-join the old network.
        let _ = std::fs::copy(&bak, wpa_conf);
        let _ = Self::wpa_cli(&["reconfigure"]);
        let restore_deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(Self::WIFI_JOIN_TIMEOUT_SECS);
        let mut restored = false;
        while std::time::Instant::now() < restore_deadline {
            std::thread::sleep(std::time::Duration::from_secs(2));
            if old_ssid.is_empty() || Self::wifi_joined(&old_ssid) {
                restored = true;
                break;
            }
        }
        exit_provisioning_and_report(
            "reverted",
            format!(
                "join watchdog timeout after {}s; backup restored {}",
                Self::WIFI_JOIN_TIMEOUT_SECS,
                if restored {
                    "and re-joined"
                } else {
                    "(re-join still pending — check link)"
                }
            ),
            if old_ssid.is_empty() {
                &ssid
            } else {
                &old_ssid
            },
        );
    }
}

pub(super) fn load_state(state_dir: &std::path::Path) -> (u64, bool) {
    let v = std::fs::read_to_string(state_dir.join("state.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    match v {
        Some(v) => (
            v["volume"].as_u64().unwrap_or(DEFAULT_VOLUME),
            v["muted"].as_bool().unwrap_or(false),
        ),
        None => (DEFAULT_VOLUME, false),
    }
}

pub(super) fn save_state(state_dir: &std::path::Path, level: u64, muted: bool) {
    let _ = std::fs::create_dir_all(state_dir);
    // v1 mechanism (#13): the latch is now writable from the network too, so
    // "persisted" must be honest — tmp+rename (atomic on this filesystem),
    // and a failure is LOGGED, not swallowed: a mute that survives reboot in
    // the log but not on disk is a lie the next boot would tell.
    let body =
        serde_json::to_string(&json!({ "volume": level, "muted": muted })).unwrap_or_default();
    let target = state_dir.join("state.json");
    let tmp = state_dir.join("state.json.tmp");
    if let Err(e) = std::fs::write(&tmp, body) {
        fwlog!("[firmware] state.json tmp write failed (latch still applied): {e}");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &target) {
        fwlog!("[firmware] state.json rename failed (latch still applied): {e}");
    }
}

/// Reject characters that break out of a quoted wpa_supplicant field:
/// controls (newline, carriage return, NUL, tab, …), `"`, and `\`.
pub(crate) fn wifi_credentials_rejected(ssid: &str, psk: &str) -> Option<&'static str> {
    if ssid.is_empty() || psk.len() < 8 {
        return Some("ssid must be non-empty and psk >= 8 chars");
    }
    fn field_safe(s: &str) -> bool {
        !s.chars().any(|c| c.is_control() || c == '"' || c == '\\')
    }
    if !field_safe(ssid) || !field_safe(psk) {
        return Some("ssid and psk must not contain quotes, backslashes, or control characters");
    }
    None
}
