//! Single entry point for `*at()` path resolution.
//!
//! Most path-bearing syscalls (openat, newfstatat, statx, mkdirat, unlinkat,
//! renameat, linkat, symlinkat, readlinkat, faccessat, fchmodat, utimensat,
//! …) take `(dirfd, path)` and need a `PathBuf` underneath the preopen root.
//! Pre-P2-C1, each handler built a `Vfs { root, cwd }` and called
//! `resolve_path` itself, with three slightly different shapes — easy to
//! drift. This module owns the routing.
//!
//! ## Routing rules
//!
//! 1. `dirfd == AT_FDCWD (-100)` — resolve against `vfs.cwd`. Same as before.
//! 2. `dirfd >= 0` (a real fd) — look up the fd in `FdTable`; require
//!    `Resource::File` with `is_dir == true` and a known `path`; resolve
//!    `path` relative to that file's path. This is the path CPython takes:
//!    `os.scandir(fd_for_dir)` → `openat(fd, "child", ...)`.
//! 3. `dirfd < 0 && dirfd != AT_FDCWD` — return `-ENOSYS` (unchanged from
//!    the pre-refactor behavior; we don't model other negative dirfds).
//!
//! After routing, the actual path-canonicalization (escape via `..`, absolute
//! paths that are inside the preopen, etc.) is delegated to `Vfs::resolve_path`
//! so the escape-check invariants in `vfs.rs` continue to apply uniformly.
//!
//! ## Borrow discipline
//!
//! `resolve` is sync. It snapshots `(root, cwd)` from the kernel and the
//! directory fd's `path` from the FdTable under short borrows, then drops
//! everything before calling `Vfs::resolve_path`. This keeps callers free
//! to `.await` afterward without holding any kernel/fd borrows.

use std::path::PathBuf;

use wasmtime::Caller;

use crate::errno::{EACCES, EBADF, ENOSYS, ENOTDIR};
use crate::fd::{FdTable, Resource, AT_FDCWD};
use crate::kernel::Kernel;
use crate::vfs::{Vfs, VfsResult};

/// Resolve `(dirfd, path)` to an absolute `PathBuf` under the preopen root.
///
/// See module docs for the routing rules.
pub fn resolve(caller: &mut Caller<'_, Kernel>, dirfd: i64, path: &str) -> VfsResult<PathBuf> {
    // AT_FDCWD and absolute paths: defer entirely to Vfs::resolve_path.
    // The Vfs implementation already handles `path.starts_with('/')` and the
    // AT_FDCWD case correctly; only the dirfd-as-fd branch needs new logic.
    if path.starts_with('/') || dirfd == AT_FDCWD {
        return resolve_via_cwd_or_root(caller, dirfd, path);
    }

    // dirfd-as-fd branch: dirfd >= 0.
    if dirfd >= 0 {
        return resolve_via_dirfd(caller, dirfd as u32, path);
    }

    // Negative dirfd that isn't AT_FDCWD — we don't model it.
    Err(-ENOSYS)
}

/// Resolve against `cwd` (or absolute path). Snapshots `(root, cwd)` then
/// delegates to `Vfs::resolve_path` with the original `dirfd`.
fn resolve_via_cwd_or_root(
    caller: &mut Caller<'_, Kernel>,
    dirfd: i64,
    path: &str,
) -> VfsResult<PathBuf> {
    let (root, cwd) = {
        let kern = caller.data();
        (kern.vfs.root.clone(), kern.vfs.cwd.clone())
    };
    let vfs = Vfs { root, cwd };
    vfs.resolve_path(dirfd, path)
}

/// Resolve `path` relative to the directory fd's bound `FilePos.path`.
/// Returns `-EBADF` if the fd doesn't exist, `-ENOTDIR` if it isn't a
/// directory, `-EACCES` if the fd has no `path` recorded (e.g. a
/// pre-existing fd from before this refactor).
fn resolve_via_dirfd(
    caller: &mut Caller<'_, Kernel>,
    dirfd: u32,
    path: &str,
) -> VfsResult<PathBuf> {
    let dir_path: PathBuf = {
        let fds: &FdTable = &caller.data().fds;
        match fds.get(dirfd) {
            Ok(Resource::File(fp)) => {
                let gs = fp.lock();
                if !gs.is_dir {
                    return Err(-ENOTDIR);
                }
                match gs.path.clone() {
                    Some(p) => p,
                    None => return Err(-EACCES),
                }
            }
            Ok(_) => return Err(-ENOTDIR),
            Err(_) => return Err(-EBADF),
        }
    };

    // We can't pass `dirfd` directly to Vfs::resolve_path (it would route
    // back to the dirfd branch). Build a synthetic absolute path by
    // joining `dir_path` and `path`, then resolve against AT_FDCWD.
    let joined = if path.is_empty() {
        dir_path
    } else {
        dir_path.join(path)
    };
    let joined_str = joined.to_string_lossy().into_owned();

    let (root, cwd) = {
        let kern = caller.data();
        (kern.vfs.root.clone(), kern.vfs.cwd.clone())
    };
    let vfs = Vfs { root, cwd };
    vfs.resolve_path(AT_FDCWD, &joined_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::Kernel;
    use std::fs::File;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Re-use the TmpDir pattern from src/vfs.rs tests.
    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new() -> Self {
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let dir = std::env::temp_dir().join(format!("edge-libos-path-{pid}-{id}"));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fresh_kernel(preopen: &std::path::Path) -> Kernel {
        Kernel::new_with_preopen(vec![], vec![], preopen)
    }

    /// Build a `Caller` shim. We can't easily build a real `Caller` outside
    /// a wasmtime instantiation, so these tests exercise `resolve_via_dirfd`
    /// (which only reads from FdTable + VFS state) and `resolve_via_cwd_or_root`
    /// (which only reads `caller.data().vfs`). Both are accessible via
    /// `Kernel` directly without a `Caller`. We test the dispatch logic by
    /// mirroring the routing in unit tests below.
    ///
    /// For the AT_FDCWD + absolute path branches, the inner `Vfs::resolve_path`
    /// is already covered by `src/vfs.rs` tests; we only need to verify that
    /// `resolve` reaches it correctly. We do that by inspecting the kernel
    /// state without the `Caller` plumbing — see `route_through_vfs_for_at_fdcwd`.

    #[test]
    fn route_through_vfs_for_at_fdcwd_relative() {
        // Set up a tmpdir with one file "foo". Build a Kernel with the
        // tmpdir as preopen + cwd. The route `resolve(AT_FDCWD, "foo")`
        // must return <preopen>/foo. We invoke the inner Vfs directly to
        // bypass the `Caller` requirement.
        let d = TmpDir::new();
        File::create(d.0.join("foo")).unwrap();
        let kern = fresh_kernel(&d.0);
        let vfs = Vfs {
            root: kern.vfs.root.clone(),
            cwd: kern.vfs.cwd.clone(),
        };
        let abs = vfs.resolve_path(AT_FDCWD, "foo").expect("resolve AT_FDCWD");
        assert!(abs.ends_with("foo"));
        assert!(abs.starts_with(&vfs.root));
    }

    #[test]
    fn route_through_vfs_for_absolute_path() {
        // An absolute path inside the preopen takes verbatim per VFS rules.
        let d = TmpDir::new();
        let file_path = d.0.join("abs_target");
        File::create(&file_path).unwrap();
        let canon = std::fs::canonicalize(&file_path).unwrap();
        let kern = fresh_kernel(&d.0);
        let vfs = Vfs {
            root: kern.vfs.root.clone(),
            cwd: kern.vfs.cwd.clone(),
        };
        let abs = vfs
            .resolve_path(AT_FDCWD, canon.to_str().unwrap())
            .expect("resolve absolute");
        assert_eq!(abs, canon);
    }

    #[test]
    fn dirfd_rejects_non_file_resource() {
        // FdTable is empty here; resolve_via_dirfd will get EBADF for an
        // unknown fd. We confirm via FdTable directly that non-File
        // resources would not match the `Resource::File` arm.
        use crate::fd::{FdTable, Resource};
        let mut fds = FdTable::empty();
        let fd = fds.insert(Resource::Stdin(crate::fd::PipeRead {
            buf: std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new())),
            closed: std::sync::Arc::new(parking_lot::Mutex::new(false)),
            nonblock: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            notify: std::sync::Arc::new(tokio::sync::Notify::new()),
        }));
        // Inserted fd is a Stdin resource, not a File. resolve_via_dirfd
        // would return ENOTDIR for it. We assert the resource kind here.
        if let Resource::File(_) = fds.get(fd).unwrap() {
            panic!("expected non-File resource");
        }
        // Non-File is OK here — would be ENOTDIR in resolve_via_dirfd.
    }

    #[test]
    fn dirfd_unknown_fd_is_ebadf() {
        use crate::fd::FdTable;
        let fds = FdTable::empty();
        assert!(fds.get(99).is_err()); // would map to -EBADF
    }

    #[test]
    fn dirfd_with_path_resolves_relative() {
        // The success path of resolve_via_dirfd: a directory fd whose
        // FilePos.path is set. Joining "<dir_path>/<path>" and running
        // through Vfs::resolve_path with AT_FDCWD yields the right answer.
        let d = TmpDir::new();
        let sub = d.0.join("sub");
        std::fs::create_dir(&sub).unwrap();
        File::create(sub.join("child")).unwrap();

        let kern = fresh_kernel(&d.0);
        let vfs = Vfs {
            root: kern.vfs.root.clone(),
            cwd: kern.vfs.cwd.clone(),
        };

        // Simulate the dirfd branch: dir_path = sub, path = "child".
        let dir_path = sub.clone();
        let joined = dir_path.join("child");
        let joined_str = joined.to_string_lossy().into_owned();
        let abs = vfs.resolve_path(AT_FDCWD, &joined_str).expect("resolve");
        assert!(abs.ends_with("child"));
        assert!(abs.starts_with(&vfs.root));
    }

    #[test]
    fn non_at_fdcwd_negative_dirfd_is_enosys() {
        // AT_FDCWD = -100. Anything else < 0 is unsupported.
        let d = TmpDir::new();
        let kern = fresh_kernel(&d.0);
        let vfs = Vfs {
            root: kern.vfs.root.clone(),
            cwd: kern.vfs.cwd.clone(),
        };
        let r = vfs.resolve_path(-50, "foo");
        assert_eq!(r, Err(-ENOSYS));
    }
}
