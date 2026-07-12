//! Time syscalls. P0 covers clock_gettime (REALTIME + MONOTONIC), gettimeofday,
//! and nanosleep (P1 in spec, but `time.sleep` is called eagerly during
//! `import fastapi` — see plan §4.6 note).

use wasmtime::Caller;

use crate::errno::to_ret;
use crate::kernel::Kernel;

pub const NR_CLOCK_GETTIME: u32 = 228;
pub const NR_GETTIMEOFDAY: u32 = 96;
pub const NR_NANOSLEEP: u32 = 35;

pub async fn clock_gettime(_c: &mut Caller<'_, Kernel>, _a: [i64; 6]) -> i64 {
    to_ret(crate::errno::ENOSYS)
}
pub async fn gettimeofday(_c: &mut Caller<'_, Kernel>, _a: [i64; 6]) -> i64 {
    to_ret(crate::errno::ENOSYS)
}
pub async fn nanosleep(_c: &mut Caller<'_, Kernel>, _a: [i64; 6]) -> i64 {
    to_ret(crate::errno::ENOSYS)
}
