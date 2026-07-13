//! `eventfd2(2)` — eventfd counters.
//!
//! P1-7: provides the wake primitive backing `epoll_wait` self-wake. An
//! eventfd is an 8-byte counter readable via `read(2)` (returns the u64
//! and resets to 0) and writable via `write(2)` (adds to the counter).
//! The associated `Notify` lets `epoll_wait` await counter changes.

use std::sync::Arc;

use wasmtime::Caller;

use crate::errno::EINVAL;
use crate::fd::{EventFdInner, Resource};
use crate::kernel::Kernel;

// Linux x86-64 syscall NR.
pub const NR_EVENTFD2: u32 = 290;

// eventfd2(2) flags (only the documented bits; EFD_CLOEXEC is recorded
// for fidelity but P1 doesn't model exec).
pub const EFD_CLOEXEC: i32 = 0o2000000;
pub const EFD_NONBLOCK: i32 = 0o4000;
pub const EFD_SEMAPHORE: i32 = 0o0001;

/// `eventfd2(initval, flags)` — allocate a fresh eventfd fd.
///
/// P1-7 only honors `EFD_NONBLOCK` (recorded on the resource; the read/
/// write paths in `crate::sys::file` consult it). `EFD_SEMAPHORE` would
/// change the read semantics from "drain" to "decrement by 1"; we don't
/// implement that here. `EFD_CLOEXEC` is accepted but discarded (no
/// exec model in P1).
pub async fn eventfd2(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let initval = a[0] as u64;
    let flags = a[1] as i32;

    let nonblock = flags & EFD_NONBLOCK != 0;
    let _cloexec = flags & EFD_CLOEXEC;
    let _semaphore = flags & EFD_SEMAPHORE; // not honored in P1-7

    let inner = EventFdInner {
        counter: parking_lot::Mutex::new(initval),
        notify: Arc::new(tokio::sync::Notify::new()),
        nonblock: std::sync::atomic::AtomicBool::new(nonblock),
    };

    let fd = caller.data_mut().fds.insert(Resource::EventFd(inner));
    fd as i64
}

/// `read(fd, buf, len, ...)` — when fd is an EventFd, drains the counter
/// into a u64 at `buf`. Called from `crate::sys::file::read` after it
/// detects the fd is an EventFd.
///
/// Returns 8 on success, -EINVAL if buf is too small.
pub async fn eventfd_read(caller: &mut Caller<'_, Kernel>, fd: u32, buf_ptr: i64, buf_len: i64) -> i64 {
    if buf_len < 8 {
        return -EINVAL;
    }
    let val = {
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::EventFd(e)) => {
                let mut c = e.counter.lock();
                let v = *c;
                *c = 0;
                v
            }
            Ok(_) => return -crate::errno::EBADF,
            Err(e) => return e,
        }
    };
    // Write the u64 into the guest buffer.
    let bytes = val.to_ne_bytes(); // LE on x86, BE on wasm — but P1 writes to wasm only
    let buf = match crate::mem::guest_slice_mut(caller, buf_ptr, 8) {
        Ok(b) => b,
        Err(e) => return e,
    };
    buf[..8].copy_from_slice(&bytes);
    8
}

/// `write(fd, buf, len, ...)` — when fd is an EventFd, adds the u64 at
/// `buf` to the counter and notifies waiters.
pub async fn eventfd_write(caller: &mut Caller<'_, Kernel>, fd: u32, buf_ptr: i64, buf_len: i64) -> i64 {
    if buf_len < 8 {
        return -EINVAL;
    }
    let addend = {
        let buf = match crate::mem::guest_slice(caller, buf_ptr, 8) {
            Ok(b) => b,
            Err(e) => return e,
        };
        u64::from_ne_bytes([
            buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
        ])
    };
    let notify = {
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::EventFd(e)) => {
                *e.counter.lock() = e.counter.lock().checked_add(addend).unwrap_or(u64::MAX);
                e.notify.clone()
            }
            Ok(_) => return -crate::errno::EBADF,
            Err(e) => return e,
        }
    };
    notify.notify_waiters();
    8
}