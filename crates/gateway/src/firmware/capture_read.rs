//! Complete one valid capture block before DSP/heartbeat publication.
//! A partial prefix never survives an error into the next block.

use std::io;

const MAX_ZERO_READS: u32 = 3;
const MAX_READ_CALLS: u32 = 64;

pub(super) fn read_block<F>(buf: &mut [u8], frames: u32, mut read: F) -> io::Result<()>
where
    F: FnMut(&mut [u8], u32) -> io::Result<u32>,
{
    if frames == 0 || buf.is_empty() || !buf.len().is_multiple_of(frames as usize) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid capture block layout",
        ));
    }
    // Make every failure path safe even if the backend filled only a prefix.
    buf.fill(0);
    let stride = buf.len() / frames as usize;
    let mut done = 0;
    let mut zero_reads = 0;
    let result = (|| {
        for _ in 0..MAX_READ_CALLS {
            let count = frames - done;
            let n = read(&mut buf[done as usize * stride..], count)?;
            if n > count {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "capture returned excess frames",
                ));
            }
            if n == 0 {
                zero_reads += 1;
                if zero_reads >= MAX_ZERO_READS {
                    break;
                }
            } else {
                zero_reads = 0;
                done += n;
                if done == frames {
                    return Ok(());
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "capture made insufficient progress",
        ))
    })();
    if result.is_err() {
        buf.fill(0);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_reads_continue_at_frame_offset_without_stale_suffix() {
        let mut buf = [99; 12];
        let mut calls = Vec::new();
        read_block(&mut buf, 4, |bytes, count| {
            calls.push(count);
            let n = if calls.len() == 1 { 1 } else { count };
            bytes[..n as usize * 3].fill(calls.len() as u8);
            Ok(n)
        })
        .unwrap();
        assert_eq!(calls, [4, 3]);
        assert_eq!(buf, [1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2]);
    }

    #[test]
    fn incomplete_or_invalid_reads_never_publish_old_or_partial_data() {
        for fault in 0..3 {
            let mut buf = [99; 12];
            let mut calls = 0;
            let result = read_block(&mut buf, 4, |bytes, count| {
                calls += 1;
                if calls == 1 {
                    bytes[..3].fill(7);
                    return Ok(1);
                }
                match fault {
                    0 => Ok(0),
                    1 => Err(io::Error::from_raw_os_error(libc::EPIPE)),
                    _ => Ok(count + 1),
                }
            });
            assert!(result.is_err());
            assert_eq!(buf, [0; 12]);
            assert!(calls <= 4);
        }
    }

    #[test]
    fn fragmented_backend_has_bounded_work_and_next_block_can_recover() {
        let mut buf = [99; 1280];
        let mut calls = 0;
        assert!(read_block(&mut buf, 1280, |bytes, _| {
            calls += 1;
            bytes[0] = 1;
            Ok(1)
        })
        .is_err());
        assert_eq!(calls, MAX_READ_CALLS);
        assert_eq!(buf, [0; 1280]);
        read_block(&mut buf, 1280, |bytes, frames| {
            bytes.fill(2);
            Ok(frames)
        })
        .unwrap();
        assert_eq!(buf, [2; 1280]);
    }
}
