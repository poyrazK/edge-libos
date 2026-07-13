//! `eventfd2(2)` — eventfd counters.
//!
//! P1-7: provides the wake primitive backing `epoll_wait` self-wake. An
//! eventfd is an 8-byte counter readable via `read(2)` (returns the u64
//! and resets to 0) and writable via `write(2)` (adds to the counter).
//! The associated `Notify` lets `epoll_wait` await counter changes.
//!
//! P2-B1: `read(2)`/`write(2)` on an eventfd fd now work via the
//! `Resource::EventFd` arm in `crate::sys::file::{read,write}`. The
//! helpers `eventfd_read`/`eventfd_write` operate on the inner counter
//! directly (no fd-table borrow), so the caller in `file.rs` can drop
//! the `&mut fds` borrow before invoking them.

use std::sync::Arc;

use wasmtime::Caller;

use crate::errno::EINVAL;
use crate::fd::{EventFdInner, Resource};
use crate::kernel::Kernel;

// Linux x86-64 syscall NR.
pub const NR_EVENTFD2: u32 = 290;
// P2-C3 part 1: legacy eventfd (no flags).
pub const NR_EVENTFD: u32 = 284;

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

/// `eventfd(initval, flags)` — legacy entry (no flags). Implemented as
/// a thin shim over `eventfd2`.
pub async fn eventfd(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    eventfd2(caller, a).await
}

/// Drain the counter into a u64.
///
/// Called from `crate::sys::file::read` after it detects the fd is an
/// EventFd and extracts the inner (so the `&mut fds` borrow is dropped
/// before this call). Returns `Err(-EAGAIN)` when the counter is empty
/// AND the fd is in nonblocking mode (matches Linux eventfd semantics).
/// When blocking, the caller awaits on the inner `Notify` instead.
///
/// `EFD_SEMAPHORE` is not honored: we always drain, not decrement-by-one.
pub(crate) fn eventfd_read(e: &EventFdInner) -> Result<u64, i64> {
    let mut c = e.counter.lock();
    let v = *c;
    if v == 0 {
        return if e
            .nonblock
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            Err(-crate::errno::EAGAIN)
        } else {
            // Blocking read with empty counter: caller should await.
            // Returning Ok(0) with a flag would require a richer return
            // type; instead the caller treats 0 as "should block". See
            // the read path in `file.rs`.
            Ok(0)
        };
    }
    *c = 0;
    Ok(v)
}

/// Add the u64 at `addend` to the counter and notify waiters.
///
/// Returns the new counter value (saturating at u64::MAX). Called from
/// `crate::sys::file::write` after it detects the fd is an EventFd.
pub(crate) fn eventfd_write(e: &EventFdInner, addend: u64) -> u64 {
    let new = e.counter.lock().checked_add(addend).unwrap_or(u64::MAX);
    *e.counter.lock() = new;
    e.notify.notify_waiters();
    new
}

/// Validate that a generic read/write buf is at least 8 bytes for an eventfd.
pub(crate) fn require_u64_buf(buf_len: i64) -> Result<(), i64> {
    if buf_len < 8 { Err(-EINVAL) } else { Ok(()) }
}

/// Backwards-compatible wrapper: `eventfd2` syscall -> `eventfd2_init`
/// no-op (kept for symmetry; not used by the generic read/write path).
#[allow(dead_code)]
pub async fn eventfd_read_compat(
    caller: &mut Caller<'_, Kernel>,
    fd: u32,
    buf_ptr: i64,
    buf_len: i64,
) -> i64 {
    if let Err(e) = require_u64_buf(buf_len) {
        return e;
    }
    let val = {
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::EventFd(e)) => eventfd_read(e),
            Ok(_) => return -crate::errno::EBADF,
            Err(e) => return e,
        }
    };
    let val = match val {
        Ok(v) => v,
        Err(e) => return e,
    };
    let bytes = val.to_ne_bytes();
    let buf = match crate::mem::guest_slice_mut(caller, buf_ptr, 8) {
        Ok(b) => b,
        Err(e) => return e,
    };
    buf[..8].copy_from_slice(&bytes);
    8
}

/// Backwards-compatible wrapper for write.
#[allow(dead_code)]
pub async fn eventfd_write_compat(
    caller: &mut Caller<'_, Kernel>,
    fd: u32,
    buf_ptr: i64,
    buf_len: i64,
) -> i64 {
    if let Err(e) = require_u64_buf(buf_len) {
        return e;
    }
    let addend = {
        let buf = match crate::mem::guest_slice(caller, buf_ptr, 8) {
            Ok(b) => b,
            Err(e) => return e,
        };
        u64::from_ne_bytes([buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7]])
    };
    {
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::EventFd(e)) => {
                let _ = eventfd_write(e, addend);
            }
            Ok(_) => return -crate::errno::EBADF,
            Err(e) => return e,
        }
    }
    8
}

#[cfg(test)]
mod p2_c3_part1_tests {
    //! P2-C3 part 1: legacy eventfd NR + EAGAIN-on-empty-counter semantics.
    use super::*;
    use crate::fd::EventFdInner;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    fn fresh_eventfd() -> EventFdInner {
        EventFdInner {
            counter: parking_lot::Mutex::new(0),
            notify: Arc::new(tokio::sync::Notify::new()),
            nonblock: AtomicBool::new(false),
        }
    }

    #[test]
    fn eventfd_nr_is_linux_284() {
        assert_eq!(NR_EVENTFD, 284);
        assert_eq!(NR_EVENTFD2, 290);
    }

    #[test]
    fn eventfd_read_empty_blocking_returns_zero() {
        let e = fresh_eventfd();
        assert_eq!(eventfd_read(&e), Ok(0));
    }

    #[test]
    fn eventfd_read_empty_nonblock_returns_eagain() {
        let e = EventFdInner {
            counter: parking_lot::Mutex::new(0),
            notify: Arc::new(tokio::sync::Notify::new()),
            nonblock: AtomicBool::new(true),
        };
        assert_eq!(eventfd_read(&e), Err(-crate::errno::EAGAIN));
    }

    #[test]
    fn eventfd_read_with_value_drains_and_returns() {
        let e = fresh_eventfd();
        eventfd_write(&e, 42);
        assert_eq!(eventfd_read(&e), Ok(42));
        // Drained.
        assert_eq!(eventfd_read(&e), Ok(0));
    }
}