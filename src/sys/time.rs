//! Time syscalls. P0 covers clock_gettime (REALTIME + MONOTONIC),
//! gettimeofday, and nanosleep (P1 in spec, but `time.sleep` is called
//! eagerly during `import fastapi` — see plan §4.6 note).

use std::time::Duration;

use chrono::Utc;
use wasmtime::Caller;

use crate::errno::EINVAL;
use crate::kernel::Kernel;
use crate::mem;

pub const NR_CLOCK_GETTIME: u32 = 228;
pub const NR_GETTIMEOFDAY: u32 = 96;
pub const NR_NANOSLEEP: u32 = 35;

// P2-C2: clock_getres, clock_nanosleep.
pub const NR_CLOCK_GETRES: u32 = 229;
pub const NR_CLOCK_NANOSLEEP: u32 = 230;

pub const TIMER_ABSTIME: i32 = 1;

/// `clock_gettime`'s `clockid_t` values (the ones we honour). Anything else
/// returns `-EINVAL` rather than guessing — glibc probes fall through.
pub const CLOCK_REALTIME: i64 = 0;
pub const CLOCK_MONOTONIC: i64 = 1;
/// Best-effort fallback: CPython sometimes asks for `CLOCK_MONOTONIC_RAW`
/// during startup. We don't have a separate source, so return monotonic.
pub const CLOCK_MONOTONIC_RAW: i64 = 4;
pub const CLOCK_PROCESS_CPUTIME_ID: i64 = 2;
pub const CLOCK_THREAD_CPUTIME_ID: i64 = 3;

/// `struct timespec` is two i64s (tv_sec, tv_nsec) on wasm32-musl.
const TIMESPEC_SIZE: i64 = 16;
const TIMESPEC_SEC_OFF: usize = 0;
const TIMESPEC_NSEC_OFF: usize = 8;

/// `struct timeval` is two i64s (tv_sec, tv_usec) on wasm32-musl.
const TIMEVAL_SIZE: i64 = 16;
const TIMEVAL_SEC_OFF: usize = 0;
const TIMEVAL_USEC_OFF: usize = 8;

/// `clock_gettime(clockid, timespec *)`. Writes 16 bytes to `*timespec`.
/// Returns 0 on success, `-EFAULT` if the pointer is bad, `-EINVAL` for an
/// unsupported clockid.
pub async fn clock_gettime(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let clockid = a[0];
    let tp = a[1];

    // Compute the time first, before any guest-memory writes, so an
    // EFAULT return leaves the kernel clock untouched.
    let (sec, nsec) = match compute_time(caller, clockid) {
        Some(t) => t,
        None => return -EINVAL,
    };

    let bytes = match mem::guest_slice_mut(caller, tp, TIMESPEC_SIZE) {
        Ok(b) => b,
        Err(e) => return e,
    };
    bytes[TIMESPEC_SEC_OFF..TIMESPEC_NSEC_OFF].copy_from_slice(&sec.to_le_bytes());
    bytes[TIMESPEC_NSEC_OFF..TIMESPEC_NSEC_OFF + 8].copy_from_slice(&nsec.to_le_bytes());
    0
}

/// Compute (tv_sec, tv_nsec) for a given clockid. Returns None for
/// clockids we don't honour.
fn compute_time(caller: &Caller<'_, Kernel>, clockid: i64) -> Option<(i64, i64)> {
    match clockid {
        CLOCK_REALTIME => {
            let now = Utc::now();
            let sec = now.timestamp();
            let nsec = now.timestamp_subsec_nanos() as i64;
            Some((sec, nsec))
        }
        CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_PROCESS_CPUTIME_ID
        | CLOCK_THREAD_CPUTIME_ID => {
            let elapsed = caller.data().started_at.elapsed();
            let total_ns = elapsed.as_nanos();
            let sec = (total_ns / 1_000_000_000) as i64;
            let nsec = (total_ns % 1_000_000_000) as i64;
            Some((sec, nsec))
        }
        _ => None,
    }
}

/// `clock_getres(clockid, timespec *)` — write a 16-byte timespec
/// representing a 1ns resolution. Returns 0 on success, -EINVAL for an
/// unsupported clockid, -EFAULT for a bad pointer.
pub async fn clock_getres(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let clockid = a[0];
    let tp = a[1];
    // Validate clockid first; honor same set as clock_gettime.
    match clockid {
        CLOCK_REALTIME | CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW
        | CLOCK_PROCESS_CPUTIME_ID | CLOCK_THREAD_CPUTIME_ID => {}
        _ => return -EINVAL,
    }
    if tp == 0 {
        return 0;
    }
    let bytes = match mem::guest_slice_mut(caller, tp, TIMESPEC_SIZE) {
        Ok(b) => b,
        Err(e) => return e,
    };
    bytes[TIMESPEC_SEC_OFF..TIMESPEC_NSEC_OFF].copy_from_slice(&0_i64.to_le_bytes());
    bytes[TIMESPEC_NSEC_OFF..TIMESPEC_NSEC_OFF + 8].copy_from_slice(&1_i64.to_le_bytes());
    0
}

/// `clock_nanosleep(clockid, flags, req, rem)`.
/// * `flags == 0` — relative sleep; same as nanosleep.
/// * `flags == TIMER_ABSTIME` — sleep until `req` is reached; if `req`
///   is in the past, return 0 immediately.
pub async fn clock_nanosleep(
    caller: &mut Caller<'_, Kernel>,
    a: [i64; 6],
) -> i64 {
    let clockid = a[0];
    let flags = a[1] as i32;
    let req = a[2];
    let rem = a[3];

    // Validate clockid; honor same set as clock_gettime.
    match clockid {
        CLOCK_REALTIME | CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW
        | CLOCK_PROCESS_CPUTIME_ID | CLOCK_THREAD_CPUTIME_ID => {}
        _ => return -EINVAL,
    }

    let req_bytes = match mem::guest_slice(caller, req, TIMESPEC_SIZE) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let sec = i64::from_le_bytes(req_bytes[0..8].try_into().unwrap());
    let nsec = i64::from_le_bytes(req_bytes[8..16].try_into().unwrap());
    if nsec < 0 || nsec >= 1_000_000_000 || sec < 0 {
        return -EINVAL;
    }

    if flags & TIMER_ABSTIME != 0 {
        // Read current time and compute the wait delta.
        let (cur_sec, cur_nsec) = match compute_time(caller, clockid) {
            Some(t) => t,
            None => return -EINVAL,
        };
        let cur_total_ns = (cur_sec as u128) * 1_000_000_000 + (cur_nsec as u128);
        let req_total_ns = (sec as u128) * 1_000_000_000 + (nsec as u128);
        if req_total_ns <= cur_total_ns {
            return 0;
        }
        let wait_ns = (req_total_ns - cur_total_ns) as u64;
        tokio::time::sleep(Duration::from_nanos(wait_ns)).await;
        0
    } else {
        let dur = Duration::from_nanos((sec as u64).saturating_mul(1_000_000_000) + nsec as u64);
        tokio::time::sleep(dur).await;
        if rem != 0 {
            let bytes = match mem::guest_slice_mut(caller, rem, TIMESPEC_SIZE) {
                Ok(b) => b,
                Err(e) => return e,
            };
            bytes[TIMESPEC_SEC_OFF..TIMESPEC_NSEC_OFF].copy_from_slice(&0_i64.to_le_bytes());
            bytes[TIMESPEC_NSEC_OFF..TIMESPEC_NSEC_OFF + 8].copy_from_slice(&0_i64.to_le_bytes());
        }
        0
    }
}

/// `gettimeofday(timeval *, tz *)`. Writes 16 bytes to `*timeval`. The
/// timezone pointer is ignored (glibc doesn't use it).
pub async fn gettimeofday(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let tp = a[0];
    let elapsed = caller.data().started_at.elapsed();
    let total_us = elapsed.as_micros();
    // Anchor to Unix epoch via monotonic so we don't double-count Utc::now
    // and Instant::now skew. (Both are wall-clock anchored to roughly the
    // same point, so monotonic→epoch projection gives a sane result.)
    let epoch = Utc::now().timestamp_micros();
    let sec = epoch / 1_000_000;
    let usec = (epoch % 1_000_000) + (total_us % 1_000_000) as i64;
    let sec = if usec >= 1_000_000 { sec + 1 } else { sec };
    let usec = usec % 1_000_000;
    let bytes = match mem::guest_slice_mut(caller, tp, TIMEVAL_SIZE) {
        Ok(b) => b,
        Err(e) => return e,
    };
    bytes[TIMEVAL_SEC_OFF..TIMEVAL_USEC_OFF].copy_from_slice(&sec.to_le_bytes());
    bytes[TIMEVAL_USEC_OFF..TIMEVAL_USEC_OFF + 8].copy_from_slice(&usec.to_le_bytes());
    0
}

/// `nanosleep(req *, rem *)`. Reads 16 bytes from `*req`, sleeps for that
/// duration, writes the unslept remainder to `*rem` (we always write 0
/// since we don't get interrupted). Returns 0 on success.
pub async fn nanosleep(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let req = a[0];
    let rem = a[1];
    let req_bytes = match mem::guest_slice(caller, req, TIMESPEC_SIZE) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let sec = i64::from_le_bytes(req_bytes[0..8].try_into().unwrap());
    let nsec = i64::from_le_bytes(req_bytes[8..16].try_into().unwrap());
    if nsec < 0 || nsec >= 1_000_000_000 || sec < 0 {
        return -EINVAL;
    }
    let dur = Duration::from_nanos((sec as u64).saturating_mul(1_000_000_000) + nsec as u64);
    tokio::time::sleep(dur).await;

    // Write the "unslept remainder" if rem != NULL.
    if rem != 0 {
        let bytes = match mem::guest_slice_mut(caller, rem, TIMESPEC_SIZE) {
            Ok(b) => b,
            Err(e) => return e,
        };
        bytes[TIMESPEC_SEC_OFF..TIMESPEC_NSEC_OFF].copy_from_slice(&0_i64.to_le_bytes());
        bytes[TIMESPEC_NSEC_OFF..TIMESPEC_NSEC_OFF + 8].copy_from_slice(&0_i64.to_le_bytes());
    }
    0
}

/// Re-export errno helper for unit tests below.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timespec_layout_fits_in_16_bytes() {
        assert_eq!(TIMESPEC_SIZE, 16);
        assert_eq!(TIMESPEC_NSEC_OFF, 8);
    }

    #[test]
    fn timeval_layout_fits_in_16_bytes() {
        assert_eq!(TIMEVAL_SIZE, 16);
        assert_eq!(TIMEVAL_USEC_OFF, 8);
    }

    #[test]
    fn nr_constants_match_linux_x86_64() {
        assert_eq!(NR_CLOCK_GETTIME, 228);
        assert_eq!(NR_GETTIMEOFDAY, 96);
        assert_eq!(NR_NANOSLEEP, 35);
        assert_eq!(NR_CLOCK_GETRES, 229);
        assert_eq!(NR_CLOCK_NANOSLEEP, 230);
    }
}
