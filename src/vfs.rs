//! Hand-rolled VFS. No `wasmtime-wasi` in P0 (user-confirmed decision #3).
//!
//! The VFS owns:
//!   - `root`: a preopen directory that bounds all path resolution. Any
//!     `..` escape that lands above `root` is rejected with `-EACCES`.
//!   - `cwd`: the current working directory for `AT_FDCWD` resolution.
//!
//! In P0 the VFS is read-mostly. Open files bypass it after `openat`; only
//! path-bearing syscalls (openat, newfstatat, getdents64, chdir) consult it.

use std::fs::{self, FileType, Metadata};
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::errno::{EACCES, EFAULT, EIO, ELOOP, ENOENT, ENOTDIR};

/// Result alias — `Ok(T)` or `Err(-errno)`.
pub type VfsResult<T> = Result<T, i64>;

/// Per-process VFS state.
///
/// P2-D1: derives `Serialize`/`Deserialize` so `KernelSnapshot` can
/// record cwd/root without a custom impl.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vfs {
    /// Preopen root. All paths resolve under this. Cannot escape via `..`.
    pub root: PathBuf,
    /// Current working directory. Always a descendant of `root`.
    pub cwd: PathBuf,
}

impl Vfs {
    /// Build a VFS rooted at `preopen`. The cwd starts at `preopen`.
    pub fn new(preopen: impl Into<PathBuf>) -> VfsResult<Self> {
        let root = preopen.into();
        let root = fs::canonicalize(&root).map_err(io_to_errno)?;
        Ok(Self {
            cwd: root.clone(),
            root,
        })
    }

    /// Resolve `(dirfd, path)` to an absolute path under `root`. The returned
    /// path is guaranteed to be inside `root` (no escape via `..`).
    ///
    /// * `dirfd == AT_FDCWD` → resolve against `cwd`.
    /// * `dirfd < 0` other than AT_FDCWD → `-ENOSYS` (P1 routes dirfd-as-fd
    ///   through `FdTable`).
    /// * `path` is absolute → if it's already under root, use verbatim;
    ///   otherwise treat as root-relative.
    pub fn resolve_path(&self, dirfd: i64, path: &str) -> VfsResult<PathBuf> {
        // If the path is absolute and lives inside the preopen, take it as-is.
        let candidate = Path::new(path);
        if candidate.is_absolute() && candidate.starts_with(&self.root) {
            return self.normalize(candidate.to_path_buf());
        }

        let base = if path.starts_with('/') {
            self.root.clone()
        } else if dirfd == crate::fd::AT_FDCWD {
            self.cwd.clone()
        } else {
            return Err(-(crate::errno::ENOSYS));
        };

        let trimmed = path.trim_start_matches('/');
        let joined = if trimmed.is_empty() {
            base
        } else {
            base.join(trimmed)
        };
        self.normalize(joined)
    }

    /// Walk `path` to collapse `.` / `..`, reject escapes above root.
    fn normalize(&self, joined: PathBuf) -> VfsResult<PathBuf> {
        let mut normalized = PathBuf::new();
        for comp in Path::new(&joined).components() {
            use std::path::Component;
            match comp {
                Component::ParentDir => {
                    if !normalized.pop() {
                        return Err(-EACCES);
                    }
                }
                Component::CurDir => {}
                other => normalized.push(other.as_os_str()),
            }
        }
        if !normalized.starts_with(&self.root) {
            return Err(-EACCES);
        }
        Ok(normalized)
    }

    /// Open `abs` (already resolved) with the given flags + mode.
    pub fn open(&self, abs: &Path, flags: i32, _mode: u32) -> VfsResult<std::fs::File> {
        use std::fs::OpenOptions;
        let mut opt = OpenOptions::new();
        let accmode = flags & crate::sys::file::O_ACCMODE;
        match accmode {
            crate::sys::file::O_RDONLY => {
                opt.read(true);
            }
            crate::sys::file::O_WRONLY => {
                opt.write(true);
            }
            crate::sys::file::O_RDWR => {
                opt.read(true).write(true);
            }
            _ => return Err(-(crate::errno::EINVAL)),
        };
        if flags & crate::sys::file::O_CREAT != 0 {
            opt.create(true);
            if flags & crate::sys::file::O_EXCL != 0 {
                opt.create_new(true);
            }
        }
        if flags & crate::sys::file::O_TRUNC != 0 {
            opt.truncate(true);
        }
        if flags & crate::sys::file::O_APPEND != 0 {
            opt.append(true);
        }

        let f = opt.open(abs).map_err(io_to_errno)?;

        if flags & crate::sys::file::O_DIRECTORY != 0 {
            let meta = f.metadata().map_err(io_to_errno)?;
            if !meta.is_dir() {
                return Err(-(crate::errno::ENOTDIR));
            }
        }

        Ok(f)
    }

    /// `fstat(abs)` — build a `Stat` from `Metadata`.
    pub fn stat(&self, abs: &Path) -> VfsResult<Stat> {
        let meta = fs::symlink_metadata(abs).map_err(io_to_errno)?;
        Ok(Stat::from_metadata(&meta))
    }

    /// `getdents64(abs, len)`. Reads the directory entries in sorted order
    /// and packs them into `linux_dirent64` records, capped at `len` bytes.
    /// Read the full directory listing into pre-encoded dirent64 records
    /// (sorted by name, no offset slicing). Used by `getdents_at` and the
    /// dir-stream position path in `sys::file::getdents64`.
    pub fn readdir_all(&self, abs: &Path) -> VfsResult<Vec<u8>> {
        let mut out: Vec<u8> = Vec::new();
        let entries = fs::read_dir(abs).map_err(io_to_errno)?;
        let mut names: Vec<(String, Option<FileType>, u64)> = entries
            .filter_map(|r| r.ok())
            .map(|e| {
                let name: String = e.file_name().to_string_lossy().into_owned();
                let ftype: Option<FileType> = e.file_type().ok();
                let ino: u64 = e.metadata().ok().map(|m| m.ino()).unwrap_or(0);
                (name, ftype, ino)
            })
            .collect();
        names.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, ftype, ino) in names {
            let rec = Dirent64Record {
                ino,
                off: out.len() as i64 + name.len() as i64 + 24,
                type_: dirent_type(ftype.as_ref()),
                name: name.into_bytes(),
            };
            let bytes = rec.encode();
            out.extend_from_slice(&bytes);
        }
        Ok(out)
    }

    /// Return the dirent64 slice starting at byte offset `start` (relative
    /// to the dir's own encoding, not the guest buffer). Caller controls
    /// the buffer size via `len`. Returns the encoded slice and the total
    /// length of the dir's encoding so the caller can advance its position.
    pub fn getdents_at(&self, abs: &Path, start: usize, len: usize) -> VfsResult<(Vec<u8>, usize)> {
        if len == 0 {
            return Err(-(crate::errno::EINVAL));
        }
        let all = self.readdir_all(abs)?;
        let total = all.len();
        if start >= total {
            return Ok((Vec::new(), total));
        }
        // Slice `all[start..]` but cap to `len` bytes.
        let end = (start + len).min(total);
        Ok((all[start..end].to_vec(), total))
    }

    /// Backwards-compatible wrapper: full dirent64 encoding (start = 0).
    /// Used by callers that don't track position across calls.
    pub fn getdents(&self, abs: &Path, len: usize) -> VfsResult<Vec<u8>> {
        let (mut out, _total) = self.getdents_at(abs, 0, len)?;
        // Mirror the old behavior: stop at len if the encoding exceeds it.
        if out.len() > len {
            out.truncate(len);
        }
        Ok(out)
    }

    /// Update cwd to `abs` (must already be resolved).
    pub fn chdir(&mut self, abs: &Path) -> VfsResult<()> {
        if !abs.starts_with(&self.root) {
            return Err(-EACCES);
        }
        let meta = fs::metadata(abs).map_err(io_to_errno)?;
        if !meta.is_dir() {
            return Err(-ENOTDIR);
        }
        self.cwd = abs.to_path_buf();
        Ok(())
    }

    /// Set `abs` as the new root + cwd. P2-C1 `chroot(2)`: there is no
    /// saved-root model; the change is permanent for the process.
    /// `abs` must exist and be a directory.
    pub fn chroot(&mut self, abs: &Path) -> VfsResult<()> {
        let meta = fs::metadata(abs).map_err(io_to_errno)?;
        if !meta.is_dir() {
            return Err(-ENOTDIR);
        }
        self.root = abs.to_path_buf();
        self.cwd = abs.to_path_buf();
        Ok(())
    }
}

/// Linux `struct stat` for wasm32-musl (120 bytes).
///
/// This layout uses the `arch/generic/bits/stat.h` shim with 64-bit
/// timestamps and 64-bit nsec fields (matches musl's stat64 shape used
/// on wasm32). Total: 120 bytes.
#[derive(Debug, Clone, Copy)]
pub struct Stat {
    pub st_dev: u64,        //  0
    pub st_ino: u64,        //  8
    pub st_nlink: u64,      // 16
    pub st_mode: u32,       // 24
    pub st_uid: u32,        // 28
    pub st_gid: u32,        // 32
    pub st_rdev: u64,       // 40 (after 4 bytes of pad between gid and rdev)
    pub st_size: i64,       // 48
    pub st_blksize: i64,    // 56
    pub st_blocks: i64,     // 64
    pub st_atime: i64,      // 72
    pub st_atime_nsec: i64, // 80
    pub st_mtime: i64,      // 88
    pub st_mtime_nsec: i64, // 96
    pub st_ctime: i64,      // 104
    pub st_ctime_nsec: i64, // 108
}
pub const STAT_SIZE: usize = 120;

impl Stat {
    pub const SIZE: usize = STAT_SIZE;

    pub fn from_metadata(meta: &Metadata) -> Self {
        let mode = mode_from(meta.file_type(), meta.permissions().mode());
        let (atime, atime_nsec) = time_split(meta.accessed().unwrap_or(UNIX_EPOCH));
        let (mtime, mtime_nsec) = time_split(meta.modified().unwrap_or(UNIX_EPOCH));
        let (ctime, ctime_nsec) = time_split(
            meta.created()
                .unwrap_or(meta.modified().unwrap_or(UNIX_EPOCH)),
        );
        Self {
            st_dev: 0,
            st_ino: meta.ino(),
            st_nlink: meta.nlink(),
            st_mode: mode,
            st_uid: 1000,
            st_gid: 1000,
            st_rdev: 0,
            st_size: meta.len() as i64,
            st_blksize: 4096,
            st_blocks: meta.len().div_ceil(512) as i64,
            st_atime: atime as i64,
            st_atime_nsec: atime_nsec as i64,
            st_mtime: mtime as i64,
            st_mtime_nsec: mtime_nsec as i64,
            st_ctime: ctime as i64,
            st_ctime_nsec: ctime_nsec as i64,
        }
    }

    /// Write the struct into guest memory at `ptr`.
    pub fn write_to_guest(
        self,
        caller: &mut wasmtime::Caller<'_, crate::kernel::Kernel>,
        ptr: i64,
    ) -> Result<(), i64> {
        let bytes = self.encode();
        let slice = crate::mem::guest_slice_mut(caller, ptr, bytes.len() as i64)?;
        slice.copy_from_slice(&bytes);
        Ok(())
    }

    pub fn encode(self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        let mut o = 0;
        buf[o..o + 8].copy_from_slice(&self.st_dev.to_le_bytes());
        o += 8;
        buf[o..o + 8].copy_from_slice(&self.st_ino.to_le_bytes());
        o += 8;
        buf[o..o + 8].copy_from_slice(&self.st_nlink.to_le_bytes());
        o += 8;
        buf[o..o + 4].copy_from_slice(&self.st_mode.to_le_bytes());
        o += 4;
        buf[o..o + 4].copy_from_slice(&self.st_uid.to_le_bytes());
        o += 4;
        buf[o..o + 4].copy_from_slice(&self.st_gid.to_le_bytes());
        o += 4;
        buf[o..o + 4].copy_from_slice(&[0u8; 4]); // pad before rdev
        o += 4;
        buf[o..o + 8].copy_from_slice(&self.st_rdev.to_le_bytes());
        o += 8;
        buf[o..o + 8].copy_from_slice(&self.st_size.to_le_bytes());
        o += 8;
        buf[o..o + 8].copy_from_slice(&self.st_blksize.to_le_bytes());
        o += 8;
        buf[o..o + 8].copy_from_slice(&self.st_blocks.to_le_bytes());
        o += 8;
        buf[o..o + 8].copy_from_slice(&self.st_atime.to_le_bytes());
        o += 8;
        buf[o..o + 8].copy_from_slice(&self.st_atime_nsec.to_le_bytes());
        o += 8;
        buf[o..o + 8].copy_from_slice(&self.st_mtime.to_le_bytes());
        o += 8;
        buf[o..o + 8].copy_from_slice(&self.st_mtime_nsec.to_le_bytes());
        o += 8;
        buf[o..o + 8].copy_from_slice(&self.st_ctime.to_le_bytes());
        o += 8;
        buf[o..o + 8].copy_from_slice(&self.st_ctime_nsec.to_le_bytes());
        o += 8;
        debug_assert_eq!(o, Self::SIZE, "stat layout must be 112 bytes");
        buf
    }
}

/// Packed `linux_dirent64` record. Assembled from VFS reads, written into
/// the guest buffer in the kernel-spec byte order.
pub struct Dirent64Record {
    pub ino: u64,
    pub off: i64,
    pub type_: u8,
    pub name: Vec<u8>,
}

impl Dirent64Record {
    /// Layout (musl wasm32 / linux generic):
    ///   ino: u64 (8)
    ///   off: i64 (8)
    ///   reclen: u16 (2) — total length including name + NUL
    ///   type: u8  (1)
    ///   name:   [u8; reclen - 19]  — NUL-terminated, padded to 8-byte align
    pub fn encode(&self) -> Vec<u8> {
        const FIXED: usize = 8 + 8 + 2 + 1; // 19
        let name_len = self.name.len() + 1; // include NUL
        let pad = (8 - (name_len % 8)) % 8;
        let reclen = (FIXED + name_len + pad) as u16;
        let mut out = Vec::with_capacity(reclen as usize);
        out.extend_from_slice(&self.ino.to_le_bytes());
        out.extend_from_slice(&self.off.to_le_bytes());
        out.extend_from_slice(&reclen.to_le_bytes());
        out.push(self.type_);
        out.extend_from_slice(&self.name);
        out.push(0); // NUL
        out.resize(reclen as usize, 0); // pad
        out
    }
}

// -- Helpers ----------------------------------------------------------------

fn time_split(t: SystemTime) -> (u64, u32) {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs(), d.subsec_nanos()),
        Err(e) => {
            let d = e.duration();
            (0, d.subsec_nanos())
        }
    }
}

fn dirent_type(ft: Option<&FileType>) -> u8 {
    match ft {
        Some(t) if t.is_dir() => 4,                      // DT_DIR
        Some(t) if t.is_file() => 8,                     // DT_REG
        Some(t) if t.is_symlink() => 10,                 // DT_LNK
        Some(t) if FileTypeExt::is_char_device(t) => 2,  // DT_CHR
        Some(t) if FileTypeExt::is_block_device(t) => 6, // DT_BLK
        Some(t) if t.is_fifo() => 1,                     // DT_FIFO
        Some(t) if t.is_socket() => 12,                  // DT_SOCK
        _ => 0,                                          // DT_UNKNOWN
    }
}

fn mode_from(ft: FileType, perm: u32) -> u32 {
    let mut m = perm & 0o7777;
    if ft.is_dir() {
        m |= 0o040000;
    } else if ft.is_symlink() {
        m |= 0o120000;
    } else {
        m |= 0o100000;
    }
    m
}

fn io_to_errno(e: io::Error) -> i64 {
    use io::ErrorKind::*;
    let code = match e.kind() {
        NotFound => ENOENT,
        PermissionDenied => EACCES,
        InvalidInput => EFAULT,
        TooManyLinks => ELOOP,
        _ => EIO,
    };
    -code
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// Lightweight tmpdir helper. On drop we best-effort `rm -rf`.
    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new() -> Self {
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let dir = std::env::temp_dir().join(format!("edge-libos-vfs-{pid}-{id}"));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const AT_FDCWD_VAL: i64 = -100;

    #[test]
    fn vfs_resolves_relative_against_cwd() {
        let d = TmpDir::new();
        File::create(d.0.join("hello.txt")).unwrap();
        let v = Vfs::new(&d.0).unwrap();
        let abs = v.resolve_path(AT_FDCWD_VAL, "hello.txt").unwrap();
        assert!(abs.ends_with("hello.txt"));
        assert!(abs.starts_with(&v.root));
    }

    #[test]
    fn vfs_rejects_dotdot_escape() {
        let d = TmpDir::new();
        let v = Vfs::new(&d.0).unwrap();
        let r = v.resolve_path(AT_FDCWD_VAL, "../../../etc/passwd");
        assert!(r.is_err());
    }

    #[test]
    fn vfs_resolves_absolute_path() {
        let d = TmpDir::new();
        File::create(d.0.join("a")).unwrap();
        let v = Vfs::new(&d.0).unwrap();
        let canon = std::fs::canonicalize(d.0.join("a")).unwrap();
        let resolved = v
            .resolve_path(AT_FDCWD_VAL, canon.to_str().unwrap())
            .unwrap();
        assert_eq!(resolved, canon);
    }

    #[test]
    fn stat_encodes_120_bytes() {
        let s = Stat {
            st_dev: 1,
            st_ino: 2,
            st_nlink: 1,
            st_mode: 0o100644,
            st_uid: 1000,
            st_gid: 1000,
            st_rdev: 0,
            st_size: 42,
            st_blksize: 4096,
            st_blocks: 1,
            st_atime: 100,
            st_atime_nsec: 0,
            st_mtime: 200,
            st_mtime_nsec: 0,
            st_ctime: 300,
            st_ctime_nsec: 0,
        };
        let b = s.encode();
        assert_eq!(b.len(), 120);
        // Spot-check st_size @ offset 48.
        let sz = i64::from_le_bytes(b[48..56].try_into().unwrap());
        assert_eq!(sz, 42);
    }

    #[test]
    fn dirent64_packs_nul_and_pad() {
        let rec = Dirent64Record {
            ino: 7,
            off: 32,
            type_: 8,
            name: b"hello".to_vec(),
        };
        let b = rec.encode();
        // 5-byte name + NUL = 6; pad = (8 - 6) % 8 = 2; reclen = 19 + 6 + 2 = 27.
        let reclen = u16::from_le_bytes(b[16..18].try_into().unwrap()) as usize;
        assert_eq!(b.len(), reclen);
        assert_eq!(reclen, 27);
        // Name bytes at offset 19, NUL at 24, pad to 27.
        assert_eq!(&b[19..24], b"hello");
        assert_eq!(b[24], 0);
        assert_eq!(b[25], 0);
        assert_eq!(b[26], 0);
    }
}
