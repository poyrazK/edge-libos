//! getrandom. Needed for hash seed / secrets / uuid4 (spec §4.1).
//!
//! P0 implementation fills the guest buffer from `Kernel::rng` (a SmallRng
//! seeded once at boot from `SmallRng::from_entropy`). This is sufficient
//! for CPython's hash randomization and `os.urandom` — not for any
//! security-critical use case.

use rand::RngCore;
use wasmtime::Caller;

use crate::errno::{EINVAL, ENOSYS};
use crate::kernel::Kernel;
use crate::mem;

pub const NR_GETRANDOM: u32 = 318;

/// Flags we honour; everything else is rejected so callers learn early.
#[allow(dead_code)]
pub const GRND_NONBLOCK: u32 = 0x01;
pub const GRND_RANDOM: u32 = 0x02;

/// Cap a single getrandom request. CPython typically asks for 16-32 bytes
/// (hash seed) or 32 bytes (uuid4). 4 KiB is generous and prevents an
/// accidental huge allocation from a buggy guest.
const MAX_LEN: usize = 4096;

/// `getrandom(buf, buflen, flags)`. Returns the number of bytes written
/// on success, `-EFAULT` for a bad pointer, `-EINVAL` for negative length,
/// `-ENOSYS` for unknown flags.
pub async fn getrandom(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let buf = a[0];
    let buflen = a[1];
    let flags = a[2] as u32;

    if flags & !(GRND_NONBLOCK | GRND_RANDOM) != 0 {
        return -ENOSYS;
    }
    let len = match usize::try_from(buflen) {
        Ok(n) => n,
        Err(_) => return -EINVAL,
    };
    if len > MAX_LEN {
        // Match Linux: per-call cap, not a hard EFAULT. Callers can retry.
        return -EINVAL;
    }
    if len == 0 {
        return 0;
    }

    // Fill via a stack buffer so we don't hold a &mut caller (via the
    // returned slice) at the same time as `&mut caller.data_mut().rng`.
    let mut tmp = vec![0u8; len];
    {
        let rng = &mut caller.data_mut().rng;
        rng.fill_bytes(&mut tmp);
    }

    // Now copy into guest memory via the (Copy) Memory handle.
    let mem = match caller.data().memory() {
        Ok(m) => *m,
        Err(e) => return e,
    };
    let bytes = match mem::guest_slice_mut_via(&mem, caller, buf, len as i64) {
        Ok(b) => b,
        Err(e) => return e,
    };
    bytes.copy_from_slice(&tmp);
    len as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nr_constants_match_linux_x86_64() {
        assert_eq!(NR_GETRANDOM, 318);
    }

    #[test]
    fn max_len_is_sane() {
        const _: () = assert!(MAX_LEN >= 64); // CPython hash-seed request
        const _: () = assert!(MAX_LEN <= 65536); // not runaway
    }
}
