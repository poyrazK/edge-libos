//! `ioctl(2)` — minimum set: FIONBIO, FIONREAD, TIOCGWINSZ. Everything
//! else returns -ENOTTY.
//!
//! `FIONBIO` flips the `nonblock` flag on the underlying resource via
//! `crate::sys::file::set_nonblock` (extracted from the existing
//! `fcntl(F_SETFL)` handler in C1-B5).
//!
//! We do NOT deref the guest pointer for `FIONBIO` (the value of `arg` is
//! the on/off state). This keeps the surface EFAULT-free for that case.

use wasmtime::Caller;

use crate::errno::{EINVAL, ENOTTY};
use crate::fd::Resource;
use crate::kernel::Kernel;
use crate::mem;
use crate::sys::file;

pub const NR_IOCTL: u32 = 16;

// ioctl(2) opcodes we handle (asm-generic/ioctls.h).
pub const FIONBIO: u32 = 0x5421;
pub const FIONREAD: u32 = 0x541B;
pub const TIOCGWINSZ: u32 = 0x5413;

// `struct winsize` on wasm32-musl: ws_row(2)+ws_col(2)+ws_xpixel(2)+ws_ypixel(2) = 8.
pub const WINSIZE_SIZE: i64 = 8;

/// `ioctl(fd, op, arg)`. Recognized ops: FIONBIO, FIONREAD, TIOCGWINSZ.
/// All others return -ENOTTY.
pub async fn ioctl(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fd = a[0] as u32;
    let op = a[1] as u32;
    let arg = a[2];

    match op {
        FIONBIO => {
            // arg != 0 → set nonblock; arg == 0 → clear.
            let want = arg != 0;
            // Look up the resource briefly; we don't need a clone.
            let fds = &mut caller.data_mut().fds;
            let r = match fds.get_mut(fd) {
                Ok(r) => r,
                Err(e) => return e,
            };
            match r {
                Resource::File(_) | Resource::Socket(_) => {
                    file::set_nonblock(r, want);
                    0
                }
                _ => -crate::errno::EBADF,
            }
        }
        FIONREAD => {
            // Always write 0 (we don't track the buffered count for all
            // resource types). arg may be 0 (no write).
            if arg != 0 {
                let bytes = match mem::guest_slice_mut(caller, arg, 4) {
                    Ok(b) => b,
                    Err(e) => return e,
                };
                bytes[0..4].copy_from_slice(&0_i32.to_le_bytes());
            }
            0
        }
        TIOCGWINSZ => {
            if arg == 0 {
                return -EINVAL;
            }
            let bytes = match mem::guest_slice_mut(caller, arg, WINSIZE_SIZE) {
                Ok(b) => b,
                Err(e) => return e,
            };
            // ws_row=24, ws_col=80, ws_xpixel=0, ws_ypixel=0.
            let ws: [u16; 4] = [24, 80, 0, 0];
            for (i, v) in ws.iter().enumerate() {
                bytes[i * 2..i * 2 + 2].copy_from_slice(&v.to_le_bytes());
            }
            0
        }
        _ => -ENOTTY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nr_ioctl_matches_linux() {
        assert_eq!(NR_IOCTL, 16);
    }

    #[test]
    fn winsize_layout() {
        assert_eq!(WINSIZE_SIZE, 8);
    }
}