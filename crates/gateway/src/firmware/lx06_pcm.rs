//! Shared LX06 PCM endpoints. The firmware's ALSA utilities resolve the vendor
//! plug/dsnoop/dmix configuration using its own userspace libraries.
use super::{alsa, LX06_CAP_PERIOD, PLAY_CHANNELS};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const IO_TIMEOUT: Duration = Duration::from_secs(1);
const REAP_TIMEOUT: Duration = Duration::from_millis(500);

pub(super) struct Helper(Child);

impl Helper {
    fn spawn(command: &mut Command) -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt;
            let parent = unsafe { libc::getpid() };
            // A watchdog may SIGKILL the gateway, skipping Rust destructors.
            // Check the parent again after prctl to close the fork/death race.
            unsafe {
                command.pre_exec(move || {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::getppid() != parent {
                        return Err(io::Error::from_raw_os_error(libc::ESRCH));
                    }
                    Ok(())
                });
            }
        }
        command.spawn().map(Self)
    }

    fn wait_for(&mut self, timeout: Duration) -> io::Result<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.0.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "ALSA helper did not exit",
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn terminate(&mut self) -> io::Result<()> {
        if self.0.try_wait()?.is_none() {
            self.0.kill()?;
            self.wait_for(REAP_TIMEOUT)?;
        }
        Ok(())
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

pub(super) enum Capture {
    Direct(alsa::Pcm),
    Shared { pipe: ChildStdout, _process: Helper },
}

impl Capture {
    pub(super) fn shared() -> io::Result<Self> {
        let mut child = Helper::spawn(
            Command::new("arecord")
                .args(["-q", "-D", "noop", "-t", "raw"])
                .args(["-c", "8", "-f", "S32_LE", "-r", "48000"])
                .args(["--period-size", "384", "--buffer-size", "6144"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped()),
        )?;
        let pipe = child.0.stdout.take().expect("piped arecord stdout");
        nonblocking(&pipe)?;
        Ok(Self::Shared {
            pipe,
            _process: child,
        })
    }

    pub(super) fn read_period(&mut self, target: &mut [u8]) -> io::Result<u32> {
        match self {
            Self::Direct(pcm) => pcm.readi(target, LX06_CAP_PERIOD),
            Self::Shared { pipe, .. } => {
                let deadline = Instant::now() + IO_TIMEOUT;
                let mut remaining = target;
                while !remaining.is_empty() {
                    wait_io(pipe, libc::POLLIN, deadline)?;
                    match pipe.read(remaining) {
                        Ok(0) => {
                            return Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "arecord exited",
                            ))
                        }
                        Ok(n) => remaining = &mut remaining[n..],
                        Err(e) if retry_io(&e) => continue,
                        Err(e) => return Err(e),
                    }
                }
                Ok(LX06_CAP_PERIOD)
            }
        }
    }
}

fn retry_io(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
    )
}

fn nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn wait_io(pipe: &impl AsRawFd, events: libc::c_short, deadline: Instant) -> io::Result<()> {
    let mut pollfd = libc::pollfd {
        fd: pipe.as_raw_fd(),
        events,
        revents: 0,
    };
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "ALSA helper pipe stalled",
            ));
        }
        let rc = unsafe { libc::poll(&mut pollfd, 1, remaining.as_millis().max(1) as libc::c_int) };
        if rc > 0 {
            // HUP and ERR also need a final read/write to distinguish a last
            // fragment from EOF/EPIPE.
            return Ok(());
        }
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }
}

struct Aplay {
    process: Helper,
    input: Option<ChildStdin>,
}

impl Aplay {
    fn start() -> io::Result<Self> {
        let mut process = Helper::spawn(
            Command::new("aplay")
                .args(["-q", "-D", "plug:dmixer", "-t", "raw"])
                .args(["-c", "2", "-f", "S16_LE", "-r", "48000"])
                .args(["--period-size", "1920", "--buffer-size", "7680", "-"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null()),
        )?;
        let input = process.0.stdin.take().expect("piped aplay stdin");
        nonblocking(&input)?;
        #[cfg(target_os = "linux")]
        unsafe {
            // Bound the userspace lead; dmix has its own hardware ring.
            libc::fcntl(input.as_raw_fd(), libc::F_SETPIPE_SZ, 8192);
        }
        Ok(Self {
            process,
            input: Some(input),
        })
    }

    fn write(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        let pipe = self.input.as_mut().expect("live aplay input");
        let deadline = Instant::now() + IO_TIMEOUT;
        while !bytes.is_empty() {
            wait_io(pipe, libc::POLLOUT, deadline)?;
            match pipe.write(bytes) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "aplay pipe closed",
                    ))
                }
                Ok(n) => bytes = &bytes[n..],
                Err(e) if retry_io(&e) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct SharedPlayback {
    child: Option<Aplay>,
}

impl SharedPlayback {
    pub(super) fn write(&mut self, bytes: &[u8], frames: u32) -> io::Result<u32> {
        let required = (frames as usize)
            .checked_mul(PLAY_CHANNELS as usize * 2)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "stereo frame count overflow")
            })?;
        let bytes = bytes.get(..required).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "short stereo PCM period")
        })?;
        if self.child.is_none() {
            if bytes.iter().all(|&sample| sample == 0) {
                // The firmware clocks silence even at idle. Let dmix and the
                // native player own this idle interval without starting aplay.
                std::thread::sleep(Duration::from_secs_f64(frames as f64 / 48_000.0));
                return Ok(frames);
            }
            self.child = Some(Aplay::start()?);
        }
        self.child.as_mut().expect("live aplay").write(bytes)?;
        Ok(frames)
    }

    pub(super) fn flush(&mut self) -> io::Result<()> {
        if let Some(mut player) = self.child.take() {
            player.process.terminate()?;
            // Killing our writer cannot retract frames already mixed into the
            // shared sink. Its other clients, including Xiaomi, keep playing.
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "aplay stopped; shared DAC tail unconfirmed",
            ));
        }
        Ok(())
    }

    pub(super) fn drain(&mut self) -> io::Result<()> {
        if let Some(mut player) = self.child.take() {
            drop(player.input.take());
            let status = player.process.wait_for(IO_TIMEOUT)?;
            if !status.success() {
                return Err(io::Error::other(format!("aplay drain failed: {status}")));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn capture_from_echo() -> (Capture, ChildStdin) {
        let mut process = Helper::spawn(
            Command::new("cat")
                .stdout(Stdio::piped())
                .stdin(Stdio::piped()),
        )
        .unwrap();
        let pipe = process.0.stdout.take().unwrap();
        nonblocking(&pipe).unwrap();
        let input = process.0.stdin.take().unwrap();
        (
            Capture::Shared {
                pipe,
                _process: process,
            },
            input,
        )
    }

    #[test]
    fn split_capture_writes_form_one_period_and_partial_eof_is_a_fault() {
        let (mut source, mut input) = capture_from_echo();
        let period_bytes = LX06_CAP_PERIOD as usize * 8 * 4;
        let data: Vec<_> = (0..period_bytes).map(|n| (n % 251) as u8).collect();
        let expected = data.clone();
        let sender = thread::spawn(move || {
            input.write_all(&data[..3]).unwrap();
            thread::sleep(Duration::from_millis(10));
            input.write_all(&data[3..]).unwrap();
            input.write_all(&[7, 8, 9]).unwrap();
        });
        let mut output = vec![0; period_bytes];
        assert_eq!(source.read_period(&mut output).unwrap(), LX06_CAP_PERIOD);
        assert_eq!(output, expected);
        assert_eq!(
            source.read_period(&mut output).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        sender.join().unwrap();
    }

    fn player_from_process(mut command: Command) -> SharedPlayback {
        let mut process =
            Helper::spawn(command.stdin(Stdio::piped()).stdout(Stdio::null())).unwrap();
        let input = process.0.stdin.take();
        nonblocking(input.as_ref().unwrap()).unwrap();
        SharedPlayback {
            child: Some(Aplay { process, input }),
        }
    }

    #[test]
    fn graceful_playback_drain_waits_for_eof_and_reports_helper_failure() {
        let mut player = player_from_process(Command::new("cat"));
        player.write(&[1, 2, 3, 4], 1).unwrap();
        player.drain().unwrap();
        let mut player = player_from_process(Command::new("false"));
        assert!(player.drain().is_err());
    }

    #[test]
    fn forced_flush_only_terminates_owned_writer_and_reports_uncertain_tail() {
        let mut other = Helper::spawn(Command::new("sleep").arg("30")).unwrap();
        let mut player = player_from_process(Command::new("cat"));
        player.write(&[1, 2, 3, 4], 1).unwrap();
        assert_eq!(
            player.flush().unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert!(player.child.is_none());
        assert!(other.0.try_wait().unwrap().is_none());
    }

    #[test]
    fn stalled_player_fails_a_write_within_the_io_deadline() {
        let mut stalled = Command::new("sleep");
        stalled.arg("30");
        let mut player = player_from_process(stalled);
        let data = vec![42; 2 * 1024 * 1024];
        let started = Instant::now();
        let error = player.write(&data, (data.len() / 4) as u32).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < IO_TIMEOUT + Duration::from_secs(2));
        assert!(player.flush().is_err());
        assert!(player.child.is_none());
    }

    #[test]
    fn idle_silence_uses_no_player_and_short_frames_are_rejected() {
        let mut player = SharedPlayback::default();
        assert_eq!(player.write(&[0; 4], 1).unwrap(), 1);
        assert!(player.child.is_none());
        assert!(player.flush().is_ok());
        assert_eq!(
            player.write(&[0], 1).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
