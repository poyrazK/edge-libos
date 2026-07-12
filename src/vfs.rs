//! Hand-rolled VFS. No `wasmtime-wasi` in P0 (user-confirmed decision #3).
//!
//! Skeleton — full path resolution, stat, and getdents64 land in Step 13.

#[allow(dead_code)]
pub struct Vfs {
    pub root: std::path::PathBuf,
    pub cwd: std::path::PathBuf,
}
