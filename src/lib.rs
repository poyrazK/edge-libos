//! edge-libos — Linux-personality libOS kernel for CPython on Wasmtime.
//!
//! See `impelementationplan` for the full design contract. This crate exposes:
//!
//! - [`Kernel`]: the per-store state container
//! - [`add_to_linker`], [`build_engine`], [`build_store`]: the Wasmtime factory
//! - the `sys`, `errno`, `mem`, `mm`, `fd`, `vfs` modules
//!
//! The P0 deliverable is that `python -c "print(2+2)"` and `import fastapi`
//! run to completion inside the guest.

#![allow(clippy::result_large_err)] // our i64 "errors" are kernel-style errnos

pub mod dispatch;
pub mod errno;
pub mod fd;
pub mod host;
pub mod kernel;
pub mod mem;
pub mod mm;
pub mod sys;
pub mod vfs;

pub use dispatch::{dispatch, install_observer, syscall_name, SyscallObserver};
pub use host::{add_to_linker, build_engine, build_store};
pub use kernel::Kernel;
pub use sys::signal::SigAction;
