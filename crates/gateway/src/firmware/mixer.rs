//! Direct ALSA control (mixer) access via /dev/snd/controlC0 — zero C deps,
//! same discipline as alsa.rs.
//!
//! ABI facts validated on the live device 2026-11-08 (ctl-smoke, both task
//! widths): generic ioctl encoding, elem_list 80B(64-bit)/72B(32-bit) with
//! pids ptr @16, elem_info 272B on both (enum item NAME lands at value+8 —
//! a bionic-uapi field, NOT id.name as mainline docs suggest), elem_value
//! 1224B/712B with the value union @72 (`long` = 8/4 bytes, enum item[0]
//! u32). Request codes per arch below.

use std::fs::OpenOptions;
use std::io;
use std::os::unix::io::AsRawFd;

pub const CTL_TYPE_BOOLEAN: u32 = 1;
pub const CTL_TYPE_INTEGER: u32 = 2;
pub const CTL_TYPE_ENUMERATED: u32 = 3;

// asm-generic encodings; sizes differ by task width (validated on-device).
#[cfg(target_arch = "aarch64")]
mod abi {
    pub const ELEM_LIST: libc::c_ulong = 0xc050_5510; // _IOWR('U',0x10, 80)
    pub const ELEM_VALUE: libc::c_ulong = 0xc4c8_5513; // write, 1224
    pub const LONG: usize = 8;
}
#[cfg(target_arch = "arm")]
mod abi {
    pub const ELEM_LIST: libc::c_ulong = 0xc048_5510; // _IOWR('U',0x10, 72)
    pub const ELEM_VALUE: libc::c_ulong = 0xc2c8_5513; // write, 712
    pub const LONG: usize = 4;
}
#[cfg(not(any(target_arch = "aarch64", target_arch = "arm")))]
mod abi {
    // Non-device build (contract tests run the fake, never the ioctls); use
    // the 64-bit numbers so the code still compiles on the Mac.
    pub const ELEM_LIST: libc::c_ulong = 0xc050_5510;
    pub const ELEM_VALUE: libc::c_ulong = 0xc4c8_5513;
    pub const LONG: usize = 8;
}
// elem_info is 272 bytes on BOTH widths — same request number.
const ELEM_INFO: libc::c_ulong = 0xc110_5511;

const ELEM_ID_SIZE: usize = 64;
const INFO_SIZE: usize = 272;
const VALUE_SIZE: usize = 72 + 1024 + 16 + 112; // worst case (aarch64); arm32 is smaller
const ID_NAME: usize = 16; // char name[44] inside snd_ctl_elem_id
const ID_NAME_LEN: usize = 44;

/// Same request-width split as `alsa.rs`: musl takes `c_int`, glibc and
/// macOS take `c_ulong`.
#[cfg(target_os = "macos")]
fn raw_ioctl(fd: libc::c_int, req: libc::c_ulong, arg: *mut libc::c_void) -> libc::c_int {
    unsafe { libc::ioctl(fd, req, arg) }
}
#[cfg(not(target_os = "macos"))]
fn raw_ioctl(fd: libc::c_int, req: libc::c_ulong, arg: *mut libc::c_void) -> libc::c_int {
    #[cfg(target_env = "musl")]
    let request = req as libc::c_int;
    #[cfg(not(target_env = "musl"))]
    let request = req;
    unsafe { libc::ioctl(fd, request, arg) }
}

pub struct Ctl {
    fd: std::fs::File,
    /// name → numid, filled once at open (ids shift across kernels; names do not).
    ids: std::collections::HashMap<String, u32>,
}

/// One control's shape, from ELEM_INFO.
#[derive(Debug, Clone)]
pub struct Info {
    pub numid: u32,
    pub name: String,
    pub typ: u32,
    pub count: u32,
    pub enum_items: u32,
}

impl Ctl {
    pub fn open(path: &str) -> io::Result<Ctl> {
        let fd = OpenOptions::new().read(true).write(true).open(path)?;
        let mut ctl = Ctl {
            fd,
            ids: Default::default(),
        };
        ctl.enumerate()?;
        Ok(ctl)
    }

    pub fn numid(&self, name: &str) -> Option<u32> {
        self.ids.get(name).copied()
    }

    pub fn known(&self) -> usize {
        self.ids.len()
    }

    fn enumerate(&mut self) -> io::Result<()> {
        // Phase 1: count only (space=0 → kernel fills used/count).
        let mut buf = [0u8; 96];
        let r = raw_ioctl(
            self.fd.as_raw_fd(),
            abi::ELEM_LIST,
            buf.as_mut_ptr() as *mut libc::c_void,
        );
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        let count = u32::from_ne_bytes([buf[12], buf[13], buf[14], buf[15]]) as usize;
        if count == 0 {
            return Ok(());
        }
        // Phase 2: fill the id array. pids points at count × 64B records.
        let mut ids = vec![0u8; count * ELEM_ID_SIZE];
        let mut buf = [0u8; 96];
        buf[..4].copy_from_slice(&0u32.to_ne_bytes()); // offset
        buf[4..8].copy_from_slice(&(count as u32).to_ne_bytes()); // space
        let ptr = ids.as_mut_ptr();
        let ptr_bytes = (ptr as usize).to_ne_bytes();
        buf[16..16 + ptr_bytes.len()].copy_from_slice(&ptr_bytes); // pids @16
        let r = raw_ioctl(
            self.fd.as_raw_fd(),
            abi::ELEM_LIST,
            buf.as_mut_ptr() as *mut libc::c_void,
        );
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        for i in 0..count {
            let rec = &ids[i * ELEM_ID_SIZE..(i + 1) * ELEM_ID_SIZE];
            let numid = u32::from_ne_bytes([rec[0], rec[1], rec[2], rec[3]]);
            let name = cstr_at(rec, ID_NAME, ID_NAME_LEN);
            if !name.is_empty() {
                self.ids.insert(name, numid);
            }
        }
        Ok(())
    }

    pub fn info(&self, numid: u32) -> io::Result<Info> {
        let mut buf = [0u8; INFO_SIZE];
        buf[0..4].copy_from_slice(&numid.to_ne_bytes());
        let r = raw_ioctl(
            self.fd.as_raw_fd(),
            ELEM_INFO,
            buf.as_mut_ptr() as *mut libc::c_void,
        );
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        let name = cstr_at(&buf, ID_NAME, ID_NAME_LEN);
        Ok(Info {
            numid,
            name,
            typ: u32::from_ne_bytes([buf[64], buf[65], buf[66], buf[67]]),
            count: u32::from_ne_bytes([buf[72], buf[73], buf[74], buf[75]]),
            enum_items: u32::from_ne_bytes([buf[80], buf[81], buf[82], buf[83]]),
        })
    }

    /// The item strings of an enumerated control, in item order.
    pub fn enum_names(&self, numid: u32) -> io::Result<Vec<String>> {
        let info = self.info(numid)?;
        let mut out = Vec::new();
        for item in 0..info.enum_items {
            let mut buf = [0u8; INFO_SIZE];
            buf[0..4].copy_from_slice(&numid.to_ne_bytes());
            // value.enumerated.item is u32 @ value+4 = 84.
            buf[84..88].copy_from_slice(&item.to_ne_bytes());
            let r = raw_ioctl(
                self.fd.as_raw_fd(),
                ELEM_INFO,
                buf.as_mut_ptr() as *mut libc::c_void,
            );
            if r != 0 {
                return Err(io::Error::last_os_error());
            }
            // The item NAME comes back at value+8 = 88 (bionic uapi layout),
            // 64 bytes — not id.name, which the kernel leaves untouched.
            out.push(cstr_at(&buf, 88, 64));
        }
        Ok(out)
    }

    /// Write integer/boolean values (repeated across the control's channels
    /// when fewer values than count are given, like tinyalsa's spread).
    pub fn write_int(&self, numid: u32, values: &[i64]) -> io::Result<()> {
        let info = self.info(numid)?;
        if info.typ == CTL_TYPE_ENUMERATED {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "control is enumerated; use set_enum",
            ));
        }
        let mut buf = vec![0u8; VALUE_SIZE];
        buf[0..4].copy_from_slice(&numid.to_ne_bytes());
        let voff = 72;
        for ch in 0..info.count as usize {
            let v = values[ch.min(values.len() - 1)];
            let at = voff + ch * abi::LONG;
            match abi::LONG {
                8 => buf[at..at + 8].copy_from_slice(&v.to_ne_bytes()),
                _ => buf[at..at + 4].copy_from_slice(&(v as i32).to_ne_bytes()),
            }
        }
        let r = raw_ioctl(
            self.fd.as_raw_fd(),
            abi::ELEM_VALUE,
            buf.as_mut_ptr() as *mut libc::c_void,
        );
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Write an enumerated control by item string.
    pub fn set_enum(&self, numid: u32, item: &str) -> io::Result<()> {
        let names = self.enum_names(numid)?;
        let idx = names.iter().position(|n| n == item).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("control has no item {item:?} (has {names:?})"),
            )
        })? as u32;
        let mut buf = vec![0u8; VALUE_SIZE];
        buf[0..4].copy_from_slice(&numid.to_ne_bytes());
        // value.enumerated.item[0] is u32 @ value+0 = 72.
        buf[72..76].copy_from_slice(&idx.to_ne_bytes());
        let r = raw_ioctl(
            self.fd.as_raw_fd(),
            abi::ELEM_VALUE,
            buf.as_mut_ptr() as *mut libc::c_void,
        );
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Set a control by NAME with typed values ("On"/"Off" resolves through
    /// the enum item list; numbers/bools go the integer path). Returns the
    /// resolved control name for logging.
    pub fn set(&self, name: &str, values: &[&str]) -> io::Result<()> {
        let numid = self.numid(name).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no control named {name:?}"),
            )
        })?;
        let info = self.info(numid)?;
        if info.typ == CTL_TYPE_ENUMERATED {
            return self.set_enum(numid, values[0]);
        }
        let nums: Vec<i64> = values
            .iter()
            .map(|v| match *v {
                "On" | "on" | "1" | "true" => Ok(1i64),
                "Off" | "off" | "0" | "false" => Ok(0),
                n => n.parse::<i64>().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, format!("bad value {n:?}"))
                }),
            })
            .collect::<io::Result<_>>()?;
        self.write_int(numid, &nums)
    }
}

fn cstr_at(buf: &[u8], off: usize, max: usize) -> String {
    let end = buf[off..off + max]
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(max);
    String::from_utf8_lossy(&buf[off..off + end]).into_owned()
}
