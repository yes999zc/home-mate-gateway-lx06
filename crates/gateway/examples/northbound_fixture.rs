//! Isolated host fixture: actual Firmware.run, period-paced bounded fake Hardware.
//! stdin: `button action|mute_toggle|volume_up|volume_down`,
//! `capture periodic|silence|speech`, or `shutdown`.
//! Optional --kws-dir runs an external deterministic sidecar protocol partner;
//! it does not prove model recognition or physical acoustics.
//! Shutdown flushes a final observation then exits this child process; it is
//! not evidence of cooperative Firmware/audio-thread termination.
use home_mate_device_gateway::firmware::{ButtonEvent, Firmware, FirmwareConfig, Hardware};
use serde_json::json;
use std::{
    io::{self, BufRead, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

#[derive(Default)]
struct Counters {
    capture_calls: AtomicU64,
    capture_mode: AtomicU64,
    capture_commands: AtomicU64,
    playback_calls: AtomicU64,
    nonzero_playback_calls: AtomicU64,
    written_frames: AtomicU64,
    flushes: AtomicU64,
    capture_max_gap_us: AtomicU64,
    playback_max_gap_us: AtomicU64,
    button_input_dropped: AtomicU64,
    capture_at: Mutex<Option<Instant>>,
    playback_at: Mutex<Option<Instant>>,
}
fn call_at(at: &Mutex<Option<Instant>>, max: &AtomicU64) {
    let now = Instant::now();
    let mut previous = at.lock().unwrap();
    if let Some(previous) = *previous {
        max.fetch_max(
            now.duration_since(previous)
                .as_micros()
                .min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
    }
    *previous = Some(now);
}
struct FakeHardware {
    counters: Arc<Counters>,
    buttons: Mutex<std::sync::mpsc::Receiver<ButtonEvent>>,
    capture_pace_ms: u64,
}
impl Hardware for FakeHardware {
    fn capture_read(&self, buffer: &mut [u8], frames: u32) -> io::Result<u32> {
        call_at(&self.counters.capture_at, &self.counters.capture_max_gap_us);
        let call = self.counters.capture_calls.fetch_add(1, Ordering::Relaxed) + 1;
        let mode = self.counters.capture_mode.load(Ordering::Acquire);
        for (index, row) in buffer
            .as_chunks_mut::<27>()
            .0
            .iter_mut()
            .take(frames as usize)
            .enumerate()
        {
            let mic =
                ((index as f32 * 440.0 * std::f32::consts::TAU / 16000.0).sin() * 12000.0) as i32;
            for (channel, sample) in row.as_chunks_mut::<3>().0.iter_mut().enumerate() {
                let value =
                    if channel == 8 || mode == 1 || (mode == 0 && (call <= 4 || call % 50 < 4)) {
                        0
                    } else {
                        mic << 4
                    };
                sample.copy_from_slice(&value.to_le_bytes()[..3]);
            }
        }
        std::thread::sleep(Duration::from_millis(self.capture_pace_ms));
        Ok(frames)
    }
    fn playback_write(&self, stereo: &[i16], frames: u32) -> io::Result<u32> {
        call_at(
            &self.counters.playback_at,
            &self.counters.playback_max_gap_us,
        );
        self.counters.playback_calls.fetch_add(1, Ordering::Relaxed);
        if stereo.iter().any(|sample| *sample != 0) {
            self.counters
                .nonzero_playback_calls
                .fetch_add(1, Ordering::Relaxed);
        }
        self.counters
            .written_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
        std::thread::sleep(Duration::from_nanos(frames as u64 * 1_000_000_000 / 48000));
        Ok(frames)
    }
    fn playback_flush(&self) -> io::Result<()> {
        self.counters.flushes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    fn playback_drain(&self) -> io::Result<()> {
        Ok(())
    }
    fn playback_underruns(&self) -> u64 {
        0
    }
    fn playback_delay_frames(&self) -> Option<u64> {
        Some(0)
    }
    fn led_write(&self, _rgb: &[(u8, u8, u8); 12]) -> io::Result<()> {
        Ok(())
    }
    fn button_next(&self) -> io::Result<ButtonEvent> {
        self.buttons
            .lock()
            .unwrap()
            .recv()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "fixture button input closed"))
    }
    fn set_adc_mute(&self, _on: bool) -> io::Result<()> {
        Ok(())
    }
    fn set_system_time(&self, _ts: u64) -> bool {
        false
    }
    fn wake_lock(&self) -> io::Result<()> {
        Ok(())
    }
}
fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut core = None;
    let mut state_dir = None;
    let mut device = None;
    let mut probe_file = None;
    let mut pace = 80u64;
    let mut duration = None;
    let mut kws_dir = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = args.next().ok_or("every fixture option requires a value")?;
        match arg.as_str() {
            "--core" => core = Some(value),
            "--state-dir" => state_dir = Some(PathBuf::from(value)),
            "--device" => device = Some(value),
            "--probe-file" => probe_file = Some(PathBuf::from(value)),
            "--capture-pace-ms" => pace = value.parse()?,
            "--duration-secs" => duration = Some(value.parse::<u64>()?),
            "--kws-dir" => kws_dir = Some(PathBuf::from(value).canonicalize()?),
            _ => return Err(format!("unknown fixture option: {arg}").into()),
        }
    }
    if !(1..=80).contains(&pace) || duration.is_some_and(|v| v == 0 || v > 1800) {
        return Err("capture pace must be1..80ms; duration1..1800s".into());
    }
    let core = core.ok_or("--core is required")?;
    if !core.parse::<std::net::SocketAddr>()?.ip().is_loopback() {
        return Err("fixture Core must be loopback".into());
    }
    let state_dir = state_dir.ok_or("--state-dir is required")?;
    std::fs::create_dir_all(&state_dir)?;
    let device = device.ok_or("--device is required")?;
    let probe_file = probe_file.ok_or("--probe-file is required")?;
    let counters = Arc::new(Counters::default());
    let shutdown = Arc::new(AtomicBool::new(false));
    let (buttons, buttons_rx) = std::sync::mpsc::sync_channel(64);
    let hardware = Arc::new(FakeHardware {
        counters: counters.clone(),
        buttons: Mutex::new(buttons_rx),
        capture_pace_ms: pace,
    });
    let firmware = Firmware::new(
        FirmwareConfig {
            core_addr: core,
            device_id: device,
            state_dir: state_dir.clone(),
            wifi: false,
            aec: false,
            kws: kws_dir.is_some(),
            kws_dir: kws_dir.unwrap_or_else(|| state_dir.join("no-kws")),
            loudness: false,
            boot_health: false,
            barge_in: false,
            ..Default::default()
        },
        hardware,
    );
    // BEGIN OPTIONAL NORTHBOUND OBSERVER (baseline copy removes this one line).
    let observer = firmware.northbound_observer();
    // END OPTIONAL NORTHBOUND OBSERVER.
    let boot = Instant::now();
    {
        let shutdown = shutdown.clone();
        let counters = counters.clone();
        std::thread::spawn(move || {
            for line in io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if line == "shutdown" {
                    shutdown.store(true, Ordering::Release);
                    return;
                }
                let mode = match line.as_str() {
                    "capture periodic" => Some(0),
                    "capture silence" => Some(1),
                    "capture speech" => Some(2),
                    _ => None,
                };
                if let Some(mode) = mode {
                    counters.capture_mode.store(mode, Ordering::Release);
                    counters.capture_commands.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                let event = match line.as_str() {
                    "button action" => Some(ButtonEvent::Action),
                    "button mute_toggle" => Some(ButtonEvent::MuteToggle),
                    "button volume_up" => Some(ButtonEvent::VolumeUp),
                    "button volume_down" => Some(ButtonEvent::VolumeDown),
                    _ => None,
                };
                if let Some(event) = event {
                    if buttons.try_send(event).is_err() {
                        counters
                            .button_input_dropped
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            shutdown.store(true, Ordering::Release);
        });
    }
    std::thread::spawn(move || {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(probe_file)
            .expect("exclusive probe file");
        loop {
            let mut record = json!({"elapsed_ms":boot.elapsed().as_millis() as u64,"capture_calls":counters.capture_calls.load(Ordering::Relaxed),"playback_calls":counters.playback_calls.load(Ordering::Relaxed),"nonzero_playback_calls":counters.nonzero_playback_calls.load(Ordering::Relaxed),"written_frames":counters.written_frames.load(Ordering::Relaxed),"flushes":counters.flushes.load(Ordering::Relaxed),"capture_max_gap_us":counters.capture_max_gap_us.load(Ordering::Relaxed),"playback_max_gap_us":counters.playback_max_gap_us.load(Ordering::Relaxed),"button_input_dropped":counters.button_input_dropped.load(Ordering::Relaxed)});
            record["capture_mode"] = json!(counters.capture_mode.load(Ordering::Acquire));
            record["capture_commands"] = json!(counters.capture_commands.load(Ordering::Relaxed));
            // BEGIN OPTIONAL NORTHBOUND SNAPSHOT (baseline copy removes this block).
            let probe = observer.probe();
            record["northbound"] = probe["status"].clone();
            record["producer_max_us"] = json!(probe["producer_max_ns"]
                .as_u64()
                .unwrap_or(0)
                .div_ceil(1000));
            for (key, source) in [
                ("pending_health_count", "health_pending"),
                ("pending_presence_count", "presence_pending"),
                ("pending_health_bytes", "health_pending_bytes"),
                ("pending_presence_bytes", "presence_pending_bytes"),
                ("inflight_bytes", "inflight_bytes"),
                ("producer_calls", "producer_calls"),
                ("last_progress_uptime_ms", "last_progress_uptime_ms"),
            ] {
                record[key] = probe[source].clone();
            }
            for key in [
                "reader_tasks",
                "writer_tasks",
                "decode_active",
                "decode_pending",
                "decode_waiters",
                "write_started_us",
                "write_deadline_us",
                "last_fault_write_started_us",
                "last_fault_write_deadline_us",
                "last_fault_us",
                "last_cleanup_us",
                "last_fault_generation",
                "last_cleanup_generation",
                "handshakes",
                "handshake_completed",
            ] {
                record[key] = probe[key].clone();
            }
            // END OPTIONAL NORTHBOUND SNAPSHOT.
            writeln!(file, "{record}").expect("write bounded probe observation");
            file.flush().expect("flush probe observation");
            if shutdown.load(Ordering::Acquire)
                || duration.is_some_and(|seconds| boot.elapsed() >= Duration::from_secs(seconds))
            {
                std::process::exit(0);
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
    firmware.run()
}
