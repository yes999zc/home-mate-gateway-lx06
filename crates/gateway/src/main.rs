//! Home Mate LX06 Device Gateway binary entry point.
//!
//! Subcommands:
//!   hm-gateway firmware --core 127.0.0.1:8932 --device-id lx06 ...
//!   hm-gateway lx06-wake-contract
//!   hm-gateway lx06-switch-watch [/dev/input/event0] [/data/hm/mode]
//!   hm-gateway led-cal 0..11|off [--frame /sys/.../led_rgb]

use home_mate_device_gateway::firmware::loudness;
use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_else(|| "firmware".into());
    match cmd.as_str() {
        "firmware" => run_firmware(args),
        "led-cal" => run_led_cal(args),
        "lx06-wake-contract" => println!("lx06-idle-wake-v1"),
        "lx06-switch-watch" => run_lx06_switch_watch(args),
        other => {
            eprintln!(
                "unknown subcommand '{other}' (firmware | lx06-wake-contract | lx06-switch-watch | led-cal)"
            );
            std::process::exit(2);
        }
    }
}

// --- LED calibration (D/T5 debt #6) -----------------------------------------------
//
// Light exactly ONE ring LED and hold it. The sweep/timing lives in the
// host-side tool (tools/led_calibrate.py over the serial console), NOT in
// this binary: a device-side sleep loop could reach a time64 syscall this
// 3.18 kernel lacks (the P0 watchdog root cause). One process = one lamp.
//
//   gateway led-cal 0..11 [--frame /sys/.../frame] [--map /data/.../led-map.json]
//   gateway led-cal off                     (dark ring)
fn run_led_cal(mut args: impl Iterator<Item = String>) {
    use home_mate_device_gateway::firmware::leds::{self, LedRing};
    let which = args.next().unwrap_or_else(|| {
        eprintln!("led-cal needs a ring index 0..11 or 'off'");
        std::process::exit(2);
    });
    let mut frame_path = "/sys/devices/soc/11007000.i2c/i2c-0/0-003f/frame".to_string();
    let mut map_path: Option<PathBuf> = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--frame" => frame_path = args.next().expect("--frame needs a path"),
            "--map" => map_path = Some(PathBuf::from(args.next().expect("--map needs a path"))),
            other => {
                eprintln!("unknown led-cal flag {other}");
                std::process::exit(2);
            }
        }
    }
    let mut ring = match map_path {
        Some(p) => LedRing::with_calibrated_map(&frame_path, &p),
        None => LedRing::new(&frame_path),
    };
    let rgb = if which == "off" {
        [(0u8, 0u8, 0u8); leds::RING_LEDS]
    } else {
        let idx: usize = which.parse().unwrap_or_else(|_| {
            eprintln!("led-cal index must be 0..11 or 'off' (got '{which}')");
            std::process::exit(2);
        });
        if idx >= leds::RING_LEDS {
            eprintln!(
                "led-cal index must be 0..{} (got {idx})",
                leds::RING_LEDS - 1
            );
            std::process::exit(2);
        }
        leds::calibration_frame(idx)
    };
    if let Err(e) = if frame_path == home_mate_device_gateway::firmware::lx06::LX06_LED_PATH
        || (unsafe { libc::access(c"/sys/devices/i2c-0/0-003a/led_rgb".as_ptr(), libc::W_OK) == 0 }
            && frame_path == "/sys/devices/soc/11007000.i2c/i2c-0/0-003f/frame")
    {
        frame_path = home_mate_device_gateway::firmware::lx06::LX06_LED_PATH.to_string();
        home_mate_device_gateway::firmware::lx06::Aw20054Ring::new(&frame_path).write(&rgb)
    } else {
        ring.write(&rgb)
    } {
        eprintln!("led-cal write to {frame_path} failed: {e}");
        std::process::exit(1);
    }
    println!("[led-cal] {which} written to {frame_path}");
}

/// Passive non-grabbing `/dev/input/event0` monitor for LX06 when stock XiaoAi
/// is active: holding Play/Pause (`code = 114`) for >= 1.5s writes `"homemate\n"`
/// to `/data/hm/mode`. The native touchpad continues to receive its events.
#[cfg(target_os = "linux")]
fn run_lx06_switch_watch(mut args: impl Iterator<Item = String>) {
    use std::io::Read;
    use std::os::unix::io::AsRawFd;
    use std::time::Instant;
    let dev = args
        .next()
        .unwrap_or_else(|| "/dev/input/event0".to_string());
    let mode_path = args.next().unwrap_or_else(|| "/data/hm/mode".to_string());
    let mut file = match std::fs::OpenOptions::new().read(true).open(&dev) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[lx06-switch-watch] open {dev} failed: {e}");
            std::process::exit(1);
        }
    };
    let fd = file.as_raw_fd();
    let mut pressed_at: Option<Instant> = None;
    let mut buf = [0u8; 24];
    loop {
        if let Some(start) = pressed_at {
            if start.elapsed().as_millis() >= 1500 {
                break;
            }
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ms = if pressed_at.is_some() { 100 } else { 1000 };
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return;
        }
        if rc == 0 || (pfd.revents & libc::POLLIN) == 0 {
            continue;
        }
        if file.read_exact(&mut buf).is_err() {
            return;
        }
        let kind = u16::from_le_bytes([buf[16], buf[17]]);
        let code = u16::from_le_bytes([buf[18], buf[19]]);
        let val = i32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);
        if kind == 0 && code == 3 {
            // Lost evdev records may include the release of the action key.
            pressed_at = None;
        }
        if kind != 1 || code != 114 {
            continue;
        }
        match val {
            1 => {
                pressed_at = Some(Instant::now());
            }
            0 => {
                if let Some(start) = pressed_at.take() {
                    if start.elapsed().as_millis() >= 1500 {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    if let Err(err) = home_mate_device_gateway::firmware::lx06::select_mode(
        std::path::Path::new(&mode_path),
        "homemate",
    ) {
        eprintln!("[lx06-switch-watch] failed to select dual wake: {err}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "linux"))]
fn run_lx06_switch_watch(_args: impl Iterator<Item = String>) {
    eprintln!("[lx06-switch-watch] only supported on Linux");
    std::process::exit(2);
}

// --- firmware (G5-2) ---------------------------------------------------------------
//
// The direct-occupation audio component: the EchoMuse-server successor with
// the controller role ABSORBED — owns the dot2-d hardware directly (ALSA PCM
// pair, is31fl3236 LED sysfs, evdev buttons, software volume gain,
// wake_lock) and speaks the northbound v0 protocol straight to the core.
// See docs/g5/direct-firmware-design-20261108.md.
//
//   gateway firmware --core 192.168.1.20:8932 [--hw-ref]

fn run_firmware(mut args: impl Iterator<Item = String>) {
    use home_mate_device_gateway::firmware::{Firmware, FirmwareConfig};
    // 2026-11-13: every firmware line goes through the bounded writer thread
    // from the first one — a full /run must cost log lines, never audio (see
    // `fwlog`). CLI errors below still use the direct fallback.
    home_mate_device_gateway::fwlog::init();
    let mut cfg = FirmwareConfig::default();
    let mut explicit_device_id: Option<String> = None;
    let mut explicit_model = false;
    let mut explicit_volume_cap = false;
    let mut explicit_barge_in_db = false;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--core" => cfg.core_addr = args.next().expect("--core needs host:port"),
            "--device-id" => {
                explicit_device_id = Some(args.next().expect("--device-id needs an id"));
            }
            "--model" => {
                cfg.model = args.next().expect("--model needs a name");
                explicit_model = true;
            }
            "--state-dir" => {
                cfg.state_dir =
                    std::path::PathBuf::from(args.next().expect("--state-dir needs a dir"))
            }
            "--volume-cap" => {
                cfg.volume_cap = args
                    .next()
                    .expect("--volume-cap needs 0..1")
                    .parse()
                    .expect("volume cap must be a number");
                explicit_volume_cap = true;
            }
            "--hw-ref" => cfg.hw_ref = true,
            "--aec" => cfg.aec = true,
            "--aec-tail" => {
                let t: usize = args
                    .next()
                    .expect("--aec-tail needs a sample count")
                    .parse()
                    .expect("--aec-tail must be a number");
                if t == 0 || !t.is_multiple_of(home_mate_device_gateway::firmware::aec::FRAME) {
                    eprintln!("--aec-tail must be a positive multiple of FRAME (320)");
                    std::process::exit(2);
                }
                cfg.aec = true;
                cfg.aec_tail = t;
            }
            "--aec-measure" => {
                cfg.aec = true;
                cfg.aec_measure = true;
            }
            "--kws" => {
                // D/T4: local keyword spotting sidecar (post-AEC reflex).
                cfg.kws = true;
            }
            "--kws-dir" => {
                cfg.kws = true;
                cfg.kws_dir =
                    std::path::PathBuf::from(args.next().expect("--kws-dir needs a directory"));
            }
            "--kws-cpu" => {
                cfg.kws_cpu = args
                    .next()
                    .expect("--kws-cpu needs a CPU index")
                    .parse()
                    .expect("--kws-cpu must be a number");
            }
            "--barge-in" => {
                // "--barge-in off" disables the semantic barge-in detector;
                // the detector is on by default (D/T5) and needs no flag.
                match args.next().as_deref() {
                    Some("off") => cfg.barge_in = false,
                    Some(other) => {
                        eprintln!("--barge-in takes 'off' (got '{other}')");
                        std::process::exit(2);
                    }
                    None => {
                        eprintln!("--barge-in needs 'off'");
                        std::process::exit(2);
                    }
                }
            }
            "--barge-in-db" => {
                cfg.barge_in_db = args
                    .next()
                    .expect("--barge-in-db needs a dBFS value")
                    .parse()
                    .expect("--barge-in-db must be a number");
                explicit_barge_in_db = true;
            }
            "--barge-in-margin" => {
                // Near-end criterion: within this many dB OF THE REFERENCE
                // (R1 Step 3, default 15). The old absolute onset is gone.
                let v: f32 = args
                    .next()
                    .expect("--barge-in-margin needs dB")
                    .parse()
                    .expect("--barge-in-margin must be a number");
                assert!(
                    (0.0..=40.0).contains(&v),
                    "--barge-in-margin takes 0..40 dB"
                );
                cfg.barge_in_margin = v;
            }
            "--barge-in-converged" => {
                // Echo-only blocks of demonstrated suppression before a
                // near-end fact is allowed (0 disables the convergence gate
                // — guard calibration only).
                cfg.barge_in_converged = args
                    .next()
                    .expect("--barge-in-converged needs blocks")
                    .parse()
                    .expect("--barge-in-converged must be a number");
            }
            "--barge-in-corr" => {
                let v: f32 = args
                    .next()
                    .expect("--barge-in-corr needs 0..1")
                    .parse()
                    .expect("--barge-in-corr must be a number");
                assert!(
                    (0.0..=1.0).contains(&v),
                    "--barge-in-corr must be within 0..1"
                );
                cfg.barge_in_corr = v;
            }
            "--barge-in-settle" => {
                // Playback settle window (D/T5 R1): onsets this soon after
                // the voice channel starts are the answer's own TTS onset.
                cfg.barge_in_settle = args
                    .next()
                    .expect("--barge-in-settle needs ms")
                    .parse()
                    .expect("--barge-in-settle must be a number");
            }
            "--barge-in-stats" => {
                // Calibration telemetry only (corr distribution); 0 = off.
                cfg.barge_in_stats = args
                    .next()
                    .expect("--barge-in-stats needs ms")
                    .parse()
                    .expect("--barge-in-stats must be a number");
            }
            "--barge-in-protect" => {
                // R1d local stop: the device refuses to cut a voice play
                // younger than this (mirrors the core's 800 ms protection).
                cfg.barge_in_protect = args
                    .next()
                    .expect("--barge-in-protect needs ms")
                    .parse()
                    .expect("--barge-in-protect must be a number");
            }
            "--aec-measure-secs" => {
                cfg.aec = true;
                cfg.aec_measure = true;
                cfg.aec_measure_secs = args
                    .next()
                    .expect("--aec-measure-secs needs seconds")
                    .parse()
                    .expect("--aec-measure-secs must be a number");
            }
            "--loopback-probe" => {
                // Task B: DAC-side write track + ch8 track to /run/hm-loop,
                // compared by the core's playback_loopback diagnostic. Off
                // by default (no /run writes in normal operation).
                cfg.loopback_probe = true;
            }
            "--loudness-bass" => {
                // D 1.1 A/B: scale the loudness bass shelf (0 = bypassed).
                let v: f32 = args
                    .next()
                    .expect("--loudness-bass needs 0..1")
                    .parse()
                    .expect("--loudness-bass must be a number");
                assert!((0.0..=1.0).contains(&v), "--loudness-bass takes 0..1");
                cfg.loudness_bass = v;
            }
            "--loudness" => {
                // "--loudness off" A/Bs the flat chain; the stage is on by
                // default (D stage 2026-11-10).
                match args.next().as_deref() {
                    Some("off") => cfg.loudness = false,
                    // M6c: "on" is explicit so an A/B line can name the
                    // state it wants instead of relying on the default.
                    Some("on") => cfg.loudness = true,
                    Some(other) => {
                        eprintln!("--loudness takes 'off' or 'on' (got '{other}')");
                        std::process::exit(2);
                    }
                    None => {
                        eprintln!("--loudness needs 'off' or 'on'");
                        std::process::exit(2);
                    }
                }
            }
            "--loudness-curve" => {
                // M6c A/B: the stock (official) curve fitted from the
                // device's own EQ_50 FIR, the RUI ladder (default), or a
                // user-defined peaking chain (--eq-band).
                match args.next().as_deref() {
                    Some("official") => cfg.loudness_curve = Some(loudness::Curve::Official),
                    Some("rui") => cfg.loudness_curve = Some(loudness::Curve::Rui),
                    Some("custom") => cfg.loudness_curve = Some(loudness::Curve::Custom),
                    Some("echomuse") => cfg.loudness_curve = Some(loudness::Curve::EchoMuse),
                    Some(other) => {
                        eprintln!("--loudness-curve takes 'rui', 'official', 'custom' or 'echomuse' (got '{other}')");
                        std::process::exit(2);
                    }
                    None => {
                        eprintln!(
                            "--loudness-curve needs 'rui', 'official', 'custom' or 'echomuse'"
                        );
                        std::process::exit(2);
                    }
                }
            }
            "--eq-band" => {
                // A user curve band: freq_hz:gain_db:q (repeatable, max 10).
                // `--loudness-curve custom` is required for these to apply.
                let spec = args.next().unwrap_or_default();
                let parts: Vec<&str> = spec.split(':').collect();
                let parsed = if parts.len() == 3 {
                    match (
                        parts[0].parse::<f32>(),
                        parts[1].parse::<f32>(),
                        parts[2].parse::<f32>(),
                    ) {
                        (Ok(f), Ok(g), Ok(q)) => Some((f, g, q)),
                        _ => None,
                    }
                } else {
                    None
                };
                match parsed {
                    Some((f, g, q))
                        if (20.0..=16000.0).contains(&f)
                            && (-24.0..=24.0).contains(&g)
                            && (0.3..=8.0).contains(&q) =>
                    {
                        cfg.eq_bands.push(loudness::CustomBand {
                            freq: f,
                            gain_db: g,
                            q,
                        });
                        assert!(cfg.eq_bands.len() <= 10, "--eq-band takes at most 10 bands");
                    }
                    _ => {
                        eprintln!("--eq-band takes freq:gain:q with freq 20..16000 Hz, gain -24..24 dB, Q 0.3..8 (got '{spec}')");
                        std::process::exit(2);
                    }
                }
            }
            "--loudness-mbcl" => {
                // M6c A/B: the stock 4-band MBCL replaces the broadband
                // limiter (only with the official curve).
                match args.next().as_deref() {
                    Some("on") => cfg.loudness_mbcl = Some(true),
                    Some("off") => cfg.loudness_mbcl = Some(false),
                    Some(other) => {
                        eprintln!("--loudness-mbcl takes 'on' or 'off' (got '{other}')");
                        std::process::exit(2);
                    }
                    None => {
                        eprintln!("--loudness-mbcl needs 'on' or 'off'");
                        std::process::exit(2);
                    }
                }
            }
            "--play-buffer" => {
                // D/T5 debt #4: A/B the 170ms buffer on ONE binary. Frames
                // must be a positive multiple of the period (2048) and at
                // least two periods — otherwise ALSA refuses hw_params.
                let v: u32 = args
                    .next()
                    .expect("--play-buffer needs a frame count")
                    .parse()
                    .expect("--play-buffer must be a number");
                let period = home_mate_device_gateway::firmware::PLAY_PERIOD;
                assert!(
                    v >= period * 2 && v.is_multiple_of(period),
                    "--play-buffer must be a multiple of {period} and >= {} (got {v})",
                    period * 2
                );
                cfg.play_buffer = v;
            }
            "--play-watchdog" => {
                // 2026-11-13: a play whose DAC write stopped making progress
                // is failed by name, then (M4) the process exits for a
                // supervisor restart. 0 disables the watchdog; default 10 s.
                let v: u64 = args
                    .next()
                    .expect("--play-watchdog needs seconds")
                    .parse()
                    .expect("--play-watchdog must be a number");
                cfg.play_watchdog_ms = v.saturating_mul(1000);
            }
            "--capture-watchdog" => {
                // M4: no successful capture block for this long = audio fault.
                // 0 disables it; default 10 s.
                let v: u64 = args
                    .next()
                    .expect("--capture-watchdog needs seconds")
                    .parse()
                    .expect("--capture-watchdog must be a number");
                cfg.capture_watchdog_ms = v.saturating_mul(1000);
            }
            "--audio-fault-exit" => {
                // M4: contract tests run the fault path in-process and must
                // not lose the test runner; production keeps the default (on).
                match args.next().as_deref() {
                    Some("off") => cfg.exit_on_audio_fault = false,
                    Some(other) => {
                        eprintln!("--audio-fault-exit takes 'off' (got '{other}')");
                        std::process::exit(2);
                    }
                    None => {
                        eprintln!("--audio-fault-exit needs 'off'");
                        std::process::exit(2);
                    }
                }
            }
            "--fault-inject-ms" => {
                // M4 test hook: request an audio fault N ms after boot through
                // the real receipt → exit → supervisor-restart path. 0 = off.
                cfg.fault_inject_ms = args
                    .next()
                    .expect("--fault-inject-ms needs milliseconds")
                    .parse()
                    .expect("--fault-inject-ms must be a number");
            }
            "--boot-health" => {
                // D/T5 debt #1: off disables the boot capture sentinel.
                match args.next().as_deref() {
                    Some("off") => cfg.boot_health = false,
                    Some(other) => {
                        eprintln!("--boot-health takes 'off' (got '{other}')");
                        std::process::exit(2);
                    }
                    None => {
                        eprintln!("--boot-health needs 'off'");
                        std::process::exit(2);
                    }
                }
            }
            "--mic-channels" => {
                let spec = args.next().expect("--mic-channels needs 0,1,2");
                cfg.mic_channels = spec
                    .split(',')
                    .filter_map(|s| s.trim().parse().ok())
                    .collect();
                if cfg.mic_channels.is_empty() {
                    eprintln!("--mic-channels produced no valid indices");
                    std::process::exit(2);
                }
            }
            "--ref-channel" => {
                cfg.ref_channel = args
                    .next()
                    .expect("--ref-channel needs an index")
                    .parse()
                    .expect("ref channel must be a number")
            }
            other => {
                eprintln!("unknown firmware flag {other}");
                std::process::exit(2);
            }
        }
    }
    // #22: Production firmware startup must have an explicit stable_id
    // (--device-id or <state_dir>/device-id); never silently fall back to
    // "dot2-d".
    cfg.device_id = match home_mate_device_gateway::firmware::resolve_production_device_id(
        explicit_device_id.as_deref(),
        &cfg.state_dir,
    ) {
        Ok(id) => id,
        Err(msg) => {
            home_mate_device_gateway::fwlog!("[firmware] {msg}");
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };
    // #25: Load per-unit profile (preset or <state_dir>/profile.json).
    let loaded_profile = home_mate_device_gateway::firmware::profile::DeviceProfile::load_from_dir(
        &cfg.state_dir,
        &cfg.device_id,
    );
    cfg.apply_profile(
        loaded_profile,
        explicit_model,
        explicit_volume_cap,
        explicit_barge_in_db,
    );
    // #35: Load persisted LED night-mode and orientation preferences if present.
    if let Some(saved_led) =
        home_mate_device_gateway::firmware::leds::load_led_config(&cfg.state_dir)
    {
        cfg.night_mode = saved_led.night_mode;
        cfg.led_orientation = saved_led.led_orientation;
    }
    if !(0.0..=1.0).contains(&cfg.volume_cap) {
        eprintln!("--volume-cap must be within 0..1 (got {})", cfg.volume_cap);
        std::process::exit(2);
    }
    if !home_mate_device_gateway::firmware::core_addr_is_loopback(&cfg.core_addr) {
        eprintln!(
            "refusing --core {} — the firmware only dials a core on this speaker (127.0.0.1, ::1, or localhost)",
            cfg.core_addr
        );
        std::process::exit(2);
    }
    if !std::path::Path::new(&cfg.led_frame_path).exists() {
        eprintln!(
            "LED frame node {} not found — is this the dot2-d board?",
            cfg.led_frame_path
        );
        std::process::exit(2);
    }
    // 2026-11-13 (M4): a TERM from init is an INTENTIONAL stop, not a crash.
    // Without a handler the default disposition kills the process with 143,
    // so the supervisor's "exit 0 = intentional" rule could never apply; on
    // shutdown it relied solely on the (previously wrong) /run marker.
    // Installed BEFORE the codec bring-up (which opens PCMs and takes
    // seconds): a TERM that lands inside that window must also be an
    // intentional stop, not a 143 the supervisor counts as a fast exit.
    // The handler is async-signal-safe: it only _exit(0). The supervisor's own
    // TERM trap does amp_off, so no hardware teardown is needed here.
    #[cfg(target_os = "linux")]
    unsafe {
        // NOTE: a nested `extern "C" fn` does NOT inherit the enclosing
        // unsafe block — the `_exit` call needs its own (this only showed up
        // in the armv7 build; the host build never compiles this cfg).
        extern "C" fn on_term(_sig: libc::c_int) {
            unsafe { libc::_exit(0) };
        }
        libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t);
    }
    let hw = match home_mate_device_gateway::firmware::open_hardware(&cfg) {
        Ok(h) => h,
        Err(e) => {
            // EBUSY here usually means the EchoMuse server still holds the
            // PCM devices — the supervisor counts this as a fast-exit.
            eprintln!("cannot open hardware: {e}");
            std::process::exit(1);
        }
    };
    // The firmware builds its own single-thread runtime inside Firmware::run().
    let fw = Firmware::new(cfg, hw);
    if let Err(e) = fw.run() {
        eprintln!("firmware failed: {e}");
        std::process::exit(1);
    }
}
