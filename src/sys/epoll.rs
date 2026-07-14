//! `epoll_create1(2)` + `epoll_ctl(2)` + `epoll_wait(2)` — the async pivot.
//!
//! P1-7: this is what makes `asyncio` work. The plan:
//!
//! 1. `epoll_create1(flags)` allocates an `EpollInner` and returns a new fd.
//!    The instance starts empty; it has a `cancel: Notify` used to wake a
//!    pending wait when `epoll_ctl(DEL)` mutates the registration set.
//!
//! 2. `epoll_ctl(epfd, op, fd, event_ptr)` registers (`ADD`/`MOD`) or
//!    removes (`DEL`) a watched fd. The user's `data` word and event mask
//!    are stored in `EpollEntry`. On `DEL` of an fd that's currently
//!    being awaited, the cancel notify fires so the wait re-scans.
//!
//! 3. `epoll_wait(epfd, events_ptr, maxevents, timeout_ms)` is the async
//!    pivot:
//!    - (a) Snapshot current readiness for each registered fd
//!      (synchronous — same logic as `poll(2)`).
//!    - (b) If any entry has nonzero `revents`, pack them into the
//!      guest's `events` array and return the count immediately.
//!    - (c) Otherwise, build a `tokio::select!` over:
//!      - (i)   a `sleep(timeout_ms)` (or `pending()` if timeout < 0),
//!      - (ii)  the per-fd `Notify::notified()` futures cloned from
//!        each registered fd's `notify_read` / `notify_write`,
//!      - (iii) the epoll instance's own `cancel.notify()`.
//!    - (d) On wake, re-snapshot readiness; if any fd now reports
//!      events, pack and return.
//!
//! P1-7's scope: only `epoll_wait(timeout >= 0)` is fully async-suspending.
//! `epoll_wait(-1)` is also supported (waits indefinitely).

use std::collections::HashMap;
use std::sync::Arc;

use wasmtime::Caller;

use crate::errno::{EBADF, EINVAL};
use crate::fd::{EpollEntry, EpollInner, Resource};
use crate::kernel::Kernel;
use crate::mem;

// Linux x86-64 syscall NRs.
pub const NR_EPOLL_CREATE1: u32 = 291;
pub const NR_EPOLL_CTL: u32 = 233;
pub const NR_EPOLL_WAIT: u32 = 232;
// P2-C3 part 1: epoll_pwait.
pub const NR_EPOLL_PWAIT: u32 = 281;

// epoll_ctl(2) operations.
pub const EPOLL_CTL_ADD: i32 = 1;
pub const EPOLL_CTL_MOD: i32 = 3;
pub const EPOLL_CTL_DEL: i32 = 2;

// epoll event flags (matches <sys/epoll.h> on Linux).
pub const EPOLLIN: u32 = 0x001;
pub const EPOLLPRI: u32 = 0x002;
pub const EPOLLOUT: u32 = 0x004;
pub const EPOLLERR: u32 = 0x008;
pub const EPOLLHUP: u32 = 0x010;
pub const EPOLLET: u32 = 0x80000000;
pub const EPOLLONESHOT: u32 = 0x40000000;

// epoll_create1(2) flags.
pub const EPOLL_CLOEXEC: i32 = 0o2000000;

// sizeof(struct epoll_event) on x86-64 / wasm32: u32 events + u64 data = 12B.
// (We're little-endian on both targets, so layout matches.)
pub const EPOLL_EVENT_SIZE: usize = 12;

/// `epoll_create1(flags)` — allocate a new epoll instance fd.
///
/// Only `EPOLL_CLOEXEC` is accepted (recorded for fidelity; P1 doesn't
/// model exec).
pub async fn epoll_create1(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let _flags = a[0] as i32;
    let inner = EpollInner {
        entries: parking_lot::Mutex::new(HashMap::new()),
        cancel: Arc::new(tokio::sync::Notify::new()),
        self_event_fd: None,
    };
    let fd = caller.data_mut().fds.insert(Resource::Epoll(inner));
    fd as i64
}

/// `epoll_ctl(epfd, op, fd, event_ptr)` — ADD / MOD / DEL a watched fd.
///
/// On `ADD`/`MOD`, `event_ptr` points to a 12-byte `struct epoll_event`.
/// On `DEL`, `event_ptr` is ignored (Linux semantics).
///
/// Errors: `-EBADF` if epfd/fd aren't valid, `-EINVAL` for bad op,
/// `-ENOENT` if DEL/MOD on a fd not registered, `-EEXIST` if ADD on a
/// fd already registered.
pub async fn epoll_ctl(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let epfd = match u32::try_from(a[0]) {
        Ok(f) => f,
        Err(_) => return -EBADF,
    };
    let op = a[1] as i32;
    let fd = match u32::try_from(a[2]) {
        Ok(f) => f,
        Err(_) => return -EBADF,
    };
    let event_ptr = a[3];

    // Phase 1: validate epfd/fd exist, decide wake primitives. All
    // immutable borrows — must complete before we touch `caller` mutably.
    let (cancel, contains_fd) = {
        let fds = &caller.data().fds;
        // Validate epfd is an Epoll resource.
        match fds.get(epfd) {
            Ok(Resource::Epoll(e)) => {
                let contains = e.entries.lock().contains_key(&fd);
                (e.cancel.clone(), contains)
            }
            Ok(_) => return -EBADF,
            Err(e) => return e,
        }
    };

    // Phase 1b: pull the actual wake primitives from the target fd type.
    // P2-B5: lock briefly to read the Arc<Notify> handles out.
    let (wake_read, wake_write) = {
        let fds = &caller.data().fds;
        match fds.get(fd) {
            Ok(Resource::Socket(s)) => {
                let gs = s.lock();
                (gs.notify_read.clone(), gs.notify_write.clone())
            }
            _ => {
                let placeholder = Arc::new(tokio::sync::Notify::new());
                (placeholder.clone(), placeholder)
            }
        }
    };

    match op {
        EPOLL_CTL_ADD => {
            // Read event struct from guest memory (immutable borrow of caller).
            let event = match mem::guest_slice(caller, event_ptr, EPOLL_EVENT_SIZE as i64) {
                Ok(b) => b,
                Err(e) => return e,
            };
            let events = u32::from_le_bytes([event[0], event[1], event[2], event[3]]);
            let data = u64::from_le_bytes([
                event[4], event[5], event[6], event[7], event[8], event[9], event[10], event[11],
            ]);
            if contains_fd {
                return -crate::errno::EEXIST;
            }
            // Pick a wake primitive.
            let wake = if events & EPOLLIN != 0 {
                wake_read
            } else if events & EPOLLOUT != 0 {
                wake_write
            } else {
                cancel.clone()
            };
            // Mutate the entries table.
            let fds = &mut caller.data_mut().fds;
            match fds.get_mut(epfd) {
                Ok(Resource::Epoll(e)) => {
                    e.entries.lock().insert(
                        fd,
                        EpollEntry {
                            fd,
                            events,
                            data,
                            wake,
                        },
                    );
                }
                _ => return -EBADF,
            }
            cancel.notify_waiters();
            0
        }
        EPOLL_CTL_MOD => {
            let event = match mem::guest_slice(caller, event_ptr, EPOLL_EVENT_SIZE as i64) {
                Ok(b) => b,
                Err(e) => return e,
            };
            let events = u32::from_le_bytes([event[0], event[1], event[2], event[3]]);
            let data = u64::from_le_bytes([
                event[4], event[5], event[6], event[7], event[8], event[9], event[10], event[11],
            ]);
            if !contains_fd {
                return -crate::errno::ENOENT;
            }
            let wake = if events & EPOLLIN != 0 {
                wake_read
            } else if events & EPOLLOUT != 0 {
                wake_write
            } else {
                cancel.clone()
            };
            let fds = &mut caller.data_mut().fds;
            match fds.get_mut(epfd) {
                Ok(Resource::Epoll(e)) => {
                    e.entries.lock().insert(
                        fd,
                        EpollEntry {
                            fd,
                            events,
                            data,
                            wake,
                        },
                    );
                }
                _ => return -EBADF,
            }
            cancel.notify_waiters();
            0
        }
        EPOLL_CTL_DEL => {
            if !contains_fd {
                return -crate::errno::ENOENT;
            }
            let fds = &mut caller.data_mut().fds;
            match fds.get_mut(epfd) {
                Ok(Resource::Epoll(e)) => {
                    e.entries.lock().remove(&fd);
                }
                _ => return -EBADF,
            }
            cancel.notify_waiters();
            0
        }
        _ => -EINVAL,
    }
}

/// `epoll_wait(epfd, events_ptr, maxevents, timeout_ms)` — the async
/// pivot. See module docs for the algorithm.
///
/// `events_ptr` is a guest buffer of at least `maxevents * 12` bytes.
pub async fn epoll_wait(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let epfd = match u32::try_from(a[0]) {
        Ok(f) => f,
        Err(_) => return -EBADF,
    };
    let events_ptr = a[1];
    let maxevents = match usize::try_from(a[2]) {
        Ok(n) => n,
        Err(_) => return -EINVAL,
    };
    let timeout_ms = a[3];

    if maxevents == 0 {
        return -EINVAL;
    }

    // Pre-validate the events buffer.
    let total = (maxevents * EPOLL_EVENT_SIZE) as i64;
    if let Err(e) = mem::guest_slice_mut(caller, events_ptr, total) {
        return e;
    }

    // Snapshot the entries list and cancel token. We pull them out of
    // the fds table so we can `.await` outside the &mut caller borrow.
    let (entries_snapshot, cancel) = {
        let fds = &caller.data().fds;
        match fds.get(epfd) {
            Ok(Resource::Epoll(e)) => {
                let entries: HashMap<u32, EpollEntry> = e.entries.lock().clone();
                (entries, e.cancel.clone())
            }
            Ok(_) => return -EBADF,
            Err(e) => return e,
        }
    };

    if entries_snapshot.is_empty() {
        // No registrations — just sleep the timeout (or wait forever).
        if timeout_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(timeout_ms as u64)).await;
        } else if timeout_ms < 0 {
            // Wait forever — but we have nothing to wake us, so we'd hang.
            // Return 0 immediately; this is the conservative behavior for
            // empty epoll_wait with timeout=-1.
        }
        return 0;
    }

    // Phase 1: synchronous snapshot. If anything is ready now, return.
    let immediate = snapshot_readiness(&entries_snapshot, caller);
    if !immediate.is_empty() {
        return pack_events(caller, events_ptr, maxevents, &immediate);
    }

    // Phase 2: build the async wait.
    let timeout_dur = if timeout_ms >= 0 {
        Some(std::time::Duration::from_millis(timeout_ms as u64))
    } else {
        None
    };

    // Clone the wake primitives so they outlive the await.
    let wakes: Vec<Arc<tokio::sync::Notify>> =
        entries_snapshot.values().map(|e| e.wake.clone()).collect();
    let cancel_for_wait = cancel.clone();

    tokio::select! {
        _ = async {
            if let Some(d) = timeout_dur {
                tokio::time::sleep(d).await;
            } else {
                std::future::pending::<()>().await;
            }
        } => {}
        _ = wait_any(&wakes) => {}
        _ = cancel_for_wait.notified() => {}
    }

    // Phase 3: re-snapshot readiness post-wake.
    let ready = snapshot_readiness(&entries_snapshot, caller);
    pack_events(caller, events_ptr, maxevents, &ready)
}

/// Wait until any of the supplied Notifies fires. Polled once per branch
/// in a `select!`-friendly future.
async fn wait_any(notifies: &[Arc<tokio::sync::Notify>]) {
    if notifies.is_empty() {
        return std::future::pending::<()>().await;
    }
    // Build a future that races all the notifies. We use tokio::select!
    // over a small batch — for the typical epoll set size (< 8 fds) this
    // is fine. For larger sets we'd build a polling loop.
    match notifies.len() {
        1 => notifies[0].notified().await,
        2 => {
            tokio::select! {
                _ = notifies[0].notified() => {},
                _ = notifies[1].notified() => {},
            }
        }
        3 => {
            tokio::select! {
                _ = notifies[0].notified() => {},
                _ = notifies[1].notified() => {},
                _ = notifies[2].notified() => {},
            }
        }
        4 => {
            tokio::select! {
                _ = notifies[0].notified() => {},
                _ = notifies[1].notified() => {},
                _ = notifies[2].notified() => {},
                _ = notifies[3].notified() => {},
            }
        }
        _ => {
            // Larger sets: poll in a loop. Each iteration awaits the
            // first to fire.
            let mut idx = 0;
            while idx < notifies.len() {
                let slice = &notifies[idx..];
                let n = slice.len().min(4);
                match n {
                    1 => slice[0].notified().await,
                    2 => tokio::select! {
                        _ = slice[0].notified() => {},
                        _ = slice[1].notified() => {},
                    },
                    3 => tokio::select! {
                        _ = slice[0].notified() => {},
                        _ = slice[1].notified() => {},
                        _ = slice[2].notified() => {},
                    },
                    _ => tokio::select! {
                        _ = slice[0].notified() => {},
                        _ = slice[1].notified() => {},
                        _ = slice[2].notified() => {},
                        _ = slice[3].notified() => {},
                    },
                }
                idx += n;
                // Re-check readiness after each branch — if any fd became
                // ready in this slice, return.
                // (We can't actually re-check here without the fds table;
                // instead, we let the outer select! poll again.)
            }
        }
    }
}

/// Snapshot current readiness for each registered entry. Returns only
/// entries that have at least one bit set in `revents`.
fn snapshot_readiness(
    entries: &HashMap<u32, EpollEntry>,
    caller: &Caller<'_, Kernel>,
) -> Vec<(EpollEntry, u32)> {
    let mut out: Vec<(EpollEntry, u32)> = Vec::new();
    let fds = &caller.data().fds;
    for entry in entries.values() {
        let revents = compute_revents(fds, entry.fd, entry.events);
        if revents != 0 {
            out.push((entry.clone(), revents));
        }
    }
    out
}

/// Compute `revents` for an fd given the requested events. Mirrors the
/// `poll(2)` logic but in the EPOLL event-flag namespace.
fn compute_revents(fds: &crate::fd::FdTable, fd: u32, requested: u32) -> u32 {
    let res = match fds.get(fd) {
        Ok(r) => r,
        Err(_) => return EPOLLERR | EPOLLHUP,
    };
    let mut r: u32 = 0;
    match res {
        Resource::Socket(s) => {
            let gs = s.lock();
            let has_stream = gs.stream.is_some();
            let is_listener = gs.is_listening();
            drop(gs);
            if (requested & EPOLLIN) != 0 {
                if has_stream {
                    // Connected stream → readable. EPOLLIN fires.
                    r |= EPOLLIN;
                } else if is_listener {
                    // Listening socket: pending connections are readable.
                    r |= EPOLLIN;
                } else {
                    // No stream and not a listener; treat as error.
                    r |= EPOLLERR;
                }
            }
            if (requested & EPOLLOUT) != 0 {
                if has_stream {
                    r |= EPOLLOUT;
                } else {
                    r |= EPOLLERR;
                }
            }
        }
        Resource::PipeRead(p) => {
            if (requested & EPOLLIN) != 0 {
                let buf_empty = p.buf.lock().is_empty();
                let closed = *p.closed.lock();
                if !buf_empty || closed {
                    r |= EPOLLIN;
                }
            }
        }
        Resource::PipeWrite(_) => {
            if (requested & EPOLLOUT) != 0 {
                r |= EPOLLOUT;
            }
        }
        Resource::Stdin(p) => {
            if (requested & EPOLLIN) != 0 {
                let buf_empty = p.buf.lock().is_empty();
                let closed = *p.closed.lock();
                if !buf_empty || closed {
                    r |= EPOLLIN;
                }
            }
        }
        Resource::Stdout(_) | Resource::Stderr(_) => {
            if (requested & EPOLLOUT) != 0 {
                r |= EPOLLOUT;
            }
        }
        Resource::File(_) => {
            // Regular files are always ready in both directions.
            if (requested & EPOLLIN) != 0 {
                r |= EPOLLIN;
            }
            if (requested & EPOLLOUT) != 0 {
                r |= EPOLLOUT;
            }
        }
        Resource::EventFd(_) => {
            // P1-7: eventfd counter changes are observable via read().
            // We don't actually read it here (we'd need the buffer); just
            // signal readable so the caller knows something happened.
            if (requested & EPOLLIN) != 0 {
                r |= EPOLLIN;
            }
        }
        Resource::Epoll(_) => {
            // Epoll fds aren't supported as watch targets in P1-7.
            r |= EPOLLERR;
        }
    }
    r
}

/// Pack the ready entries into the guest's epoll_event array. Returns
/// the number of events written.
fn pack_events(
    caller: &mut Caller<'_, Kernel>,
    events_ptr: i64,
    maxevents: usize,
    ready: &[(EpollEntry, u32)],
) -> i64 {
    let count = ready.len().min(maxevents);
    let buf = match mem::guest_slice_mut(caller, events_ptr, (count * EPOLL_EVENT_SIZE) as i64) {
        Ok(b) => b,
        Err(e) => return e,
    };
    for (i, (entry, revents)) in ready.iter().take(count).enumerate() {
        let off = i * EPOLL_EVENT_SIZE;
        // wasm32-musl `struct epoll_event` is 12 bytes: u32 events + u64 data.
        // `data` is preserved through the round-trip from `EpollEntry::data`
        // (set by the guest's `EPOLL_CTL_ADD`). `revents` is the low 32
        // bits of the kernel-shaped epoll_event.events field.
        buf[off..off + 4].copy_from_slice(&revents.to_le_bytes());
        buf[off + 4..off + 12].copy_from_slice(&entry.data.to_le_bytes());
    }
    count as i64
}

/// `epoll_pwait(epfd, events, maxevents, timeout, sigmask, sigsetsize)` —
/// like `epoll_wait` but takes a `struct timespec` timeout. `sigmask` is
/// ignored (no signal integration in v1).
pub async fn epoll_pwait(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let epfd = match u32::try_from(a[0]) {
        Ok(f) => f,
        Err(_) => return -EBADF,
    };
    let events_ptr = a[1];
    let maxevents = match usize::try_from(a[2]) {
        Ok(n) => n,
        Err(_) => return -EINVAL,
    };
    let tsp = a[3];
    let _sigmask = a[4];
    let _sigsetsize = a[5];
    if maxevents == 0 {
        return -EINVAL;
    }

    let timeout_ms: i64 = if tsp == 0 {
        -1 // wait forever
    } else {
        let bytes = match mem::guest_slice(caller, tsp, 16) {
            Ok(b) => b,
            Err(e) => return e,
        };
        let sec = i64::from_le_bytes(bytes[0..8].try_into().unwrap());
        let nsec = i64::from_le_bytes(bytes[8..16].try_into().unwrap());
        if sec < 0 || nsec < 0 || !(0..1_000_000_000).contains(&nsec) {
            return -EINVAL;
        }
        (sec as i64).saturating_mul(1000) + (nsec as i64) / 1_000_000
    };

    epoll_wait(
        caller,
        [epfd as i64, events_ptr, maxevents as i64, timeout_ms, 0, 0],
    )
    .await
}

#[cfg(test)]
mod p2_c3_part1_tests {
    //! P2-C3 part 1: epoll_pwait NR + EPOLL_EVENT_SIZE constant.
    use super::*;

    #[test]
    fn epoll_pwait_nr_is_linux_281() {
        assert_eq!(NR_EPOLL_PWAIT, 281);
    }

    #[test]
    fn epoll_event_size_is_12_on_wasm32() {
        // u32 events + u64 data = 12 bytes (matches wasm32-musl layout).
        assert_eq!(EPOLL_EVENT_SIZE, 12);
    }
}
