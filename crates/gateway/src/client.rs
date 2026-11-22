//! Gateway→core connection actor: dial out, handshake, then pump frames in
//! both directions with reconnect + backoff. The core owns the generation
//! counter (hello_ack) — every reconnect is a fresh hello, and this side
//! never resubmits events from a dead connection.

use crate::protocol::*;
use crate::terminal::{dot2d_descriptor, ScheduledFinish, VirtualDot2d};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};

pub struct GatewayConfig {
    pub core_addr: String,
    pub gateway_name: String,
    pub out_dir: Option<PathBuf>,
    pub afplay: bool,
    /// Reconnect backoff bounds.
    pub min_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            core_addr: "127.0.0.1:8932".into(),
            gateway_name: format!("home-mate-device-gateway {}", env!("CARGO_PKG_VERSION")),
            out_dir: None,
            afplay: false,
            min_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(5),
        }
    }
}

#[derive(Debug)]
struct ProtocolMismatch(String);

impl std::fmt::Display for ProtocolMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "protocol mismatch: {}", self.0)
    }
}

impl std::error::Error for ProtocolMismatch {}

/// Run the gateway until `shutdown` fires. `events` injects local device
/// events (wake/utterance/…) from the terminal UI; replies flow back over
/// the connection itself.
pub async fn run(
    cfg: GatewayConfig,
    device: Arc<Mutex<VirtualDot2d>>,
    mut events: mpsc::Receiver<GatewayFrame>,
    mut shutdown: mpsc::Receiver<()>,
) {
    let mut backoff = cfg.min_backoff;
    // connect_and_serve watches `shutdown` itself; it returns Shutdown when
    // fired, so this loop only owns reconnect pacing.
    loop {
        let result = connect_and_serve(&cfg, device.clone(), &mut events, &mut shutdown).await;
        let protocol_fail = result
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<ProtocolMismatch>())
            .is_some();
        match result {
            Ok(CloseReason::Shutdown) => {
                tracing::info!("gateway shutting down");
                return;
            }
            Ok(CloseReason::PeerClosed) => tracing::warn!("core closed the connection"),
            Err(e) => tracing::warn!(error = %e, "connection lost"),
        }
        device.lock().await.disconnected();
        let wait = if protocol_fail {
            Duration::from_millis(PROTOCOL_ERROR_RETRY_MS)
        } else {
            backoff
        };
        tracing::info!(?wait, "reconnecting");
        tokio::select! {
            _ = shutdown.recv() => return,
            _ = tokio::time::sleep(wait) => {}
        }
        if !protocol_fail {
            backoff = (backoff * 2).min(cfg.max_backoff);
        }
    }
}

#[derive(Debug)]
pub enum CloseReason {
    Shutdown,
    PeerClosed,
}

/// One connection lifetime. Errors propagate to the reconnect loop; the
/// two Ok variants distinguish "we were told to stop" from "core went away".
/// Public for contract tests that drive a single attempt directly.
pub async fn connect_and_serve(
    cfg: &GatewayConfig,
    device: Arc<Mutex<VirtualDot2d>>,
    events: &mut mpsc::Receiver<GatewayFrame>,
    shutdown: &mut mpsc::Receiver<()>,
) -> Result<CloseReason, Box<dyn std::error::Error + Send + Sync>> {
    let stream = TcpStream::connect(&cfg.core_addr).await?;
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd);

    // Handshake: hello → hello_ack. v1 mechanism (#11): interval
    // intersection, both versions and the accepted floor printed, and a
    // refusal EXITS with the reason instead of retrying into the same wall.
    let hello = GatewayFrame::Hello {
        protocol: PROTOCOL_VERSION,
        min_protocol: PROTOCOL_MIN_VERSION,
        supported_versions: SUPPORTED_PROTOCOLS.to_vec(),
        gateway: cfg.gateway_name.clone(),
        gateway_version: env!("CARGO_PKG_VERSION").to_string(),
        aec_tail_ms: None,
        profile_version: None,
        devices: vec![dot2d_descriptor()],
    };
    write_line(&mut wr, &encode_gateway(&hello)?).await?;
    let line = match tokio::select! {
        _ = shutdown.recv() => return Ok(CloseReason::Shutdown),
        r = read_line_capped(&mut lines) => r?,
    } {
        LineRead::Line(l) => l,
        LineRead::Oversized { .. } => {
            return Err(format!("frame exceeds {MAX_FRAME_BYTES} bytes").into())
        }
        LineRead::Eof => return Err("core closed during handshake".into()),
    };
    let generation = match decode_core(&line)? {
        CoreFrame::HelloAck {
            protocol,
            min_protocol,
            supported_versions,
            capabilities: _,
            core_version,
            generation,
        } => {
            let Some(operating) = protocol_operating_version_with_supported(
                (PROTOCOL_MIN_VERSION, PROTOCOL_VERSION),
                SUPPORTED_PROTOCOLS,
                (min_protocol, protocol),
                &supported_versions,
            ) else {
                let reason = protocol_no_intersection_reason(
                    (PROTOCOL_MIN_VERSION, PROTOCOL_VERSION),
                    (min_protocol, protocol),
                );
                let err_frame = GatewayFrame::ProtocolError {
                    protocol: PROTOCOL_VERSION,
                    min_protocol: PROTOCOL_MIN_VERSION,
                    gateway_version: env!("CARGO_PKG_VERSION").to_string(),
                    reason: reason.clone(),
                };
                if let Ok(encoded) = encode_gateway(&err_frame) {
                    let _ = tokio::time::timeout(
                        Duration::from_millis(1000),
                        write_line(&mut wr, &encoded),
                    )
                    .await;
                }
                return Err(ProtocolMismatch(reason).into());
            };
            tracing::info!(
                operating,
                core_version,
                "handshake complete (this side speaks {PROTOCOL_VERSION}, accepts >= {PROTOCOL_MIN_VERSION})"
            );
            generation
        }
        CoreFrame::HelloReject {
            reason,
            core_version,
            ..
        } => {
            return Err(ProtocolMismatch(format!(
                "core {core_version} refused the handshake: {reason}"
            ))
            .into())
        }
        f => return Err(format!("expected hello_ack, got {f:?}").into()),
    };
    tracing::info!(generation, "handshake complete");
    println!(
        "[gateway] connected to {} (generation {generation})",
        cfg.core_addr
    );

    // Play-finish timers created by apply() send their finish here.
    let (finish_tx, mut finish_rx) = mpsc::unbounded_channel::<ScheduledFinish>();

    loop {
        tokio::select! {
            _ = shutdown.recv() => return Ok(CloseReason::Shutdown),
            Some(finish) = finish_rx.recv() => {
                let frame = device.lock().await.finish_fired(&finish);
                if let Some(f) = frame {
                    write_line(&mut wr, &encode_gateway(&f)?).await?;
                    log_out(&f);
                }
            }
            Some(ev) = events.recv() => {
                write_line(&mut wr, &encode_gateway(&ev)?).await?;
                log_out(&ev);
            }
            read = read_line_capped(&mut lines) => {
                let line = match read? {
                    LineRead::Line(l) => l,
                    LineRead::Oversized { .. } => {
                        return Err(format!("frame exceeds {MAX_FRAME_BYTES} bytes").into())
                    }
                    LineRead::Eof => return Ok(CloseReason::PeerClosed),
                };
                let frame = decode_core(&line)?;
                let fx = {
                    // Hold the lock only for the state transition; timers and
                    // IO run outside it.
                    let mut d = device.lock().await;
                    let fx = d.apply(&frame, cfg.out_dir.as_ref())?;
                    for fin in fx.finishes.clone() {
                        let tx = finish_tx.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(fin.after_ms)).await;
                            let _ = tx.send(fin);
                        });
                    }
                    fx
                };
                if let Some(audio) = &fx.audio {
                    println!(
                        "[device] {} on {}: {} bytes (~{} ms){}",
                        audio.task_id,
                        audio.channel,
                        audio.wav.len(),
                        audio.duration_ms,
                        if cfg.afplay { " → afplay" } else { "" }
                    );
                    if cfg.afplay {
                        if let Some(dir) = &cfg.out_dir {
                            let path = dir.join(format!("latest-{}.wav", audio.task_id));
                            if std::fs::write(&path, &audio.wav).is_ok() {
                                let _ = std::process::Command::new("afplay").arg(&path).spawn();
                            }
                        }
                    }
                }
                for f in &fx.immediate {
                    write_line(&mut wr, &encode_gateway(f)?).await?;
                    log_out(f);
                }
            }
            else => return Ok(CloseReason::Shutdown),
        }
    }
}

async fn write_line(
    wr: &mut tokio::net::tcp::OwnedWriteHalf,
    line: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    wr.write_all(line.as_bytes()).await?;
    wr.write_all(b"\n").await?;
    Ok(())
}

fn log_out(f: &GatewayFrame) {
    match f {
        GatewayFrame::PlayReceipt { task_id, stage, .. } => {
            println!("[device] play {task_id}: {stage}");
        }
        GatewayFrame::StopReceipt { task_id, stage, .. } => {
            println!("[device] stop {task_id}: {stage}");
        }
        GatewayFrame::VolumeReceipt {
            task_id, effective, ..
        } => {
            println!("[device] volume {task_id}: {effective}");
        }
        GatewayFrame::ModeReport { duplex, .. } => {
            println!(
                "[device] mode report: cap={} req={} neg={} eff={}",
                duplex.capability, duplex.requested, duplex.negotiated, duplex.effective
            );
        }
        GatewayFrame::PlayFinished {
            task_id, played_ms, ..
        } => {
            println!("[device] finished {task_id} after {played_ms} ms");
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_line_rejects_oversized_unterminated_line_without_oom() {
        let (mut tx, rx) = tokio::io::duplex(64 * 1024);
        let writer = tokio::spawn(async move {
            let chunk = vec![b'a'; 64 * 1024];
            let total = MAX_FRAME_BYTES + 128 * 1024;
            let mut sent = 0usize;
            while sent < total {
                let n = (total - sent).min(chunk.len());
                if tx.write_all(&chunk[..n]).await.is_err() {
                    break;
                }
                sent += n;
            }
        });
        let mut reader = BufReader::new(rx);
        let res = read_line_capped(&mut reader).await.unwrap();
        assert_eq!(
            res,
            LineRead::Eof,
            "oversized unterminated line drains to Eof"
        );
        drop(reader);
        let _ = writer.await;
    }

    #[tokio::test]
    async fn read_line_reads_normal_frames_and_eof() {
        let (mut tx, rx) = tokio::io::duplex(1024);
        tx.write_all(b"{\"type\":\"hello_ack\"}\r\nsecond\n")
            .await
            .unwrap();
        drop(tx);
        let mut reader = BufReader::new(rx);
        assert_eq!(
            read_line_capped(&mut reader).await.unwrap(),
            LineRead::Line("{\"type\":\"hello_ack\"}".into())
        );
        assert_eq!(
            read_line_capped(&mut reader).await.unwrap(),
            LineRead::Line("second".into())
        );
        assert_eq!(read_line_capped(&mut reader).await.unwrap(), LineRead::Eof);
    }
}
