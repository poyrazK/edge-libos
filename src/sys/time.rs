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

// P2 closing: sysinfo + times stubs.
pub const NR_SYSINFO: u32 = 99;
pub const NR_TIMES: u32 = 100;

/// `struct sysinfo` on x86-64 native (which is what musl was compiled
/// against for `wasm32-musl`): 11 × u64 fields + mem_unit(u32) + pad = 96 B.
/// We size our buffer generously to cover both x86-64 and 32-bit layouts.
pub const SYSINFO_SIZE: i64 = 128;
// Sysinfo offsets are architecture-defined by Linux's `<linux/sysinfo.h>`.
// On x86-64 (the canonical Linux personality): uptime is the first u64.
const SYSINFO_UPTIME_OFF: usize = 0;
// 1×8 (uptime) + 3×8 (loads) = 32, then totalram.
const SYSINFO_TOTALRAM_OFF: usize = 32;
const SYSINFO_FREERAM_OFF: usize = 40;
// 1×8 + 3×8 + 5×8 = 72, then sharedram/bufferram/totalswap/freeswap/procs at 72/80/88/96/104.
const SYSINFO_PROCS_OFF: usize = 104;

/// `struct tms` on x86-64: 4 × clock_t(8) = 32 bytes.
/// We zero-fill a generous 32-byte buffer.
pub const TMS_SIZE: i64 = 32;

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
        CLOCK_MONOTONIC
        | CLOCK_MONOTONIC_RAW
        | CLOCK_PROCESS_CPUTIME_ID
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
        CLOCK_REALTIME
        | CLOCK_MONOTONIC
        | CLOCK_MONOTONIC_RAW
        | CLOCK_PROCESS_CPUTIME_ID
        | CLOCK_THREAD_CPUTIME_ID => {}
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
pub async fn clock_nanosleep(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let clockid = a[0];
    let flags = a[1] as i32;
    let req = a[2];
    let rem = a[3];

    // Validate clockid; honor same set as clock_gettime.
    match clockid {
        CLOCK_REALTIME
        | CLOCK_MONOTONIC
        | CLOCK_MONOTONIC_RAW
        | CLOCK_PROCESS_CPUTIME_ID
        | CLOCK_THREAD_CPUTIME_ID => {}
        _ => return -EINVAL,
    }

    let req_bytes = match mem::guest_slice(caller, req, TIMESPEC_SIZE) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let sec = i64::from_le_bytes(req_bytes[0..8].try_into().unwrap());
    let nsec = i64::from_le_bytes(req_bytes[8..16].try_into().unwrap());
    if nsec < 0 || !(0..1_000_000_000).contains(&nsec) || sec < 0 {
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
    if nsec < 0 || !(0..1_000_000_000).contains(&nsec) || sec < 0 {
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

/// `sysinfo(info)` — stub returning fake uptime/memory. Per
/// impelementationplan §4.6, P2 stubs return plausible values; CPython
/// reads this on startup but tolerates dummy data.
///
/// Returns 0 on success. `info == NULL` → -EFAULT.
pub async fn sysinfo(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    use crate::errno::EFAULT;
    let info = a[0];
    if info == 0 {
        return -EFAULT;
    }
    let bytes = match mem::guest_slice_mut(caller, info, SYSINFO_SIZE) {
        Ok(b) => b,
        Err(e) => return e,
    };
    // Zero the whole struct, then set a few plausible fields:
    //   uptime: 1 second since boot (so libs don't divide by zero)
    //   totalram / freeram: 1 GiB each (sane defaults for a sandbox)
    //   procs: 1 (ourselves)
    for b in bytes.iter_mut() {
        *b = 0;
    }
    bytes[SYSINFO_UPTIME_OFF..SYSINFO_UPTIME_OFF + 8].copy_from_slice(&1_i64.to_le_bytes());
    bytes[SYSINFO_TOTALRAM_OFF..SYSINFO_TOTALRAM_OFF + 8]
        .copy_from_slice(&(1u64 << 30).to_le_bytes());
    bytes[SYSINFO_FREERAM_OFF..SYSINFO_FREERAM_OFF + 8]
        .copy_from_slice(&(1u64 << 30).to_le_bytes());
    bytes[SYSINFO_PROCS_OFF..SYSINFO_PROCS_OFF + 8].copy_from_slice(&1_i64.to_le_bytes());
    0
}

/// `times(buf)` — stub returning all zeros in the tms struct. The plan
/// lists this as a P2 stub; CPython and most guests don't read it.
///
/// Returns the wall-clock time in clock ticks (use 0 — we don't model
/// `CLK_TCK`). `buf == NULL` → -EFAULT.
pub async fn times(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    use crate::errno::EFAULT;
    let buf = a[0];
    if buf == 0 {
        return -EFAULT;
    }
    let bytes = match mem::guest_slice_mut(caller, buf, TMS_SIZE) {
        Ok(b) => b,
        Err(e) => return e,
    };
    for b in bytes.iter_mut() {
        *b = 0;
    }
    // Return 0 clock ticks. We don't model CLK_TCK in v1.
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

    #[test]
    fn p2_closing_nr_constants_match_linux() {
        // sysinfo(99) and times(100) — both P2 stubs per the plan.
        assert_eq!(NR_SYSINFO, 99);
        assert_eq!(NR_TIMES, 100);
    }

    #[test]
    fn sysinfo_struct_size_covers_x86_64() {
        // x86-64 native layout is 11×u64 + u32 + pad = 96 bytes. Our
        // buffer is sized generously to cover both x86-64 and 32-bit.
        assert!(SYSINFO_SIZE >= 96, "SYSINFO_SIZE={SYSINFO_SIZE} < 96");
    }

    #[test]
    fn tms_struct_size_covers_x86_64() {
        // x86-64 native: 4 × clock_t(u64) = 32 bytes.
        assert!(TMS_SIZE >= 32, "TMS_SIZE={TMS_SIZE} < 32");
    }
}
