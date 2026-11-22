//! Non-blocking log sink for the on-device firmware.
//!
//! 2026-11-13, device hang: the firmware's stdout is the supervisor's file on
//! the 4 MB `/run` tmpfs. When that filesystem ran out of space, `println!`'s
//! write returned a short count and panicked — in the capture thread, between
//! a partial line and a lock guard. The panic poisoned the guards the
//! playback/capture loops take every period, so both audio threads died while
//! the process stayed up: no mic frames, no `play_finished`, the core waiting
//! forever. The last log line on the device was `[barge-in] guard held: e` —
//! half a word, which is what a partial write leaves behind.
//!
//! Every firmware log line now goes through a bounded queue to one writer
//! thread. The audio/protocol threads format and `try_send`; a full queue or
//! a failing write drops the line instead of blocking or panicking. The writer
//! ignores write errors and keeps draining, so a full `/run` costs log lines,
//! never audio.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::OnceLock;

/// Enough for a burst (barge-in telemetry during a barge-in storm) without
/// letting a wedged writer grow memory: 512 lines of a few hundred bytes.
const QUEUE_LINES: usize = 512;

static SINK: OnceLock<SyncSender<String>> = OnceLock::new();
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Start the writer thread. Idempotent: the first call wins (tests start
/// several in-process firmwares; they share one writer, like one stdout).
pub fn init() {
    if SINK.get().is_some() {
        return;
    }
    let (tx, rx) = sync_channel::<String>(QUEUE_LINES);
    if std::thread::Builder::new()
        .name("fwlog".into())
        .spawn(move || {
            use std::io::Write;
            let mut out = std::io::stdout();
            while let Ok(line) = rx.recv() {
                // A failed/short write is a full tmpfs, not a reason to die:
                // drop the line, keep the audio thread alive.
                let _ = out.write_all(line.as_bytes());
                let _ = out.flush();
            }
        })
        .is_err()
    {
        return;
    }
    let _ = SINK.set(tx);
}

/// Write one formatted line. Never blocks the caller and never panics.
pub fn line(args: std::fmt::Arguments<'_>) {
    let mut text = args.to_string();
    text.push('\n');
    if let Some(tx) = SINK.get() {
        if tx.try_send(text).is_err() {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
        return;
    }
    // Before `init` (argument-parse errors): one direct write, errors ignored.
    use std::io::Write;
    let _ = std::io::stdout().write_all(text.as_bytes());
}

/// Lines dropped because the queue was full (telemetry for tests/forensics).
pub fn dropped() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_before_init_is_not_lost_or_panicking() {
        // Direct-write fallback; the test only needs "no panic".
        line(format_args!("early {}", 1));
    }
}
