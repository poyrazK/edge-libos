//! getrandom. Needed for hash seed / secrets / uuid4 (spec §4.1).

use wasmtime::Caller;

use crate::errno::to_ret;
use crate::kernel::Kernel;

pub const NR_GETRANDOM: u32 = 318;

pub async fn getrandom(_c: &mut Caller<'_, Kernel>, _a: [i64; 6]) -> i64 {
    to_ret(crate::errno::ENOSYS)
}
