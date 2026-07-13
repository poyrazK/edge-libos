//! Per-syscall handlers. One submodule per group. Each handler has the
//! signature:
//!
//! ```ignore
//! pub async fn NAME(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64
//! ```
//!
//! Returns the kernel-convention i64: `>= 0` success, `[-4095, -1]` = `-errno`.
//! Pure-stubs (identity) take no caller and return `i64` directly.

pub mod epoll;
pub mod eventfd;
pub mod file;
pub mod futex;
pub mod identity;
pub mod ioctl;
pub mod memory;
pub mod path;
pub mod poll;
pub mod process;
pub mod random;
pub mod signal;
pub mod socket;
pub mod time;
