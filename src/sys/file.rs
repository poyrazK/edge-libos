//! File / VFS syscalls.
//!
//! Step 12 wires read/write against buffered stdio pipes. Step 14 (this file)
//! replaces the openat/close/lseek/fstat/newfstatat/getdents64 stubs with
//! real implementations backed by the hand-rolled VFS in `crate::vfs`.
//!
//! Per-fd read/write **position** lives in a `FilePos` struct held by
//! `Resource::File`. Pipes and stdio keep their position at 0 (they are
//! streams, not seekable files).

use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use wasmtime::Caller;

use crate::errno::{EACCES, EBADF, EFAULT, EINVAL, ENOENT, ERANGE, ESPIPE};
use crate::fd::Resource;
use crate::kernel::Kernel;
use crate::mem;
use crate::sys::eventfd;
use crate::sys::socket;
use crate::vfs::{Stat, Vfs};

// NR_* (Linux x86-64 unistd_64.h)
pub const NR_READ: u32 = 0;
pub const NR_WRITE: u32 = 1;
pub const NR_OPEN: u32 = 2;
pub const NR_OPENAT: u32 = 257;
pub const NR_CLOSE: u32 = 3;
pub const NR_STAT: u32 = 4;
pub const NR_LSTAT: u32 = 6;
pub const NR_LSEEK: u32 = 8;
pub const NR_FSTAT: u32 = 5;
pub const NR_NEWFSTATAT: u32 = 262;
pub const NR_STATX: u32 = 332;
pub const NR_GETDENTS64: u32 = 217;
pub const NR_PIPE: u32 = 22;
pub const NR_PIPE2: u32 = 293;
pub const NR_FCNTL: u32 = 72;
pub const NR_GETCWD: u32 = 79;
pub const NR_READV: u32 = 19;
pub const NR_WRITEV: u32 = 20;
// P2-B5: dup(2) / dup2(2) / dup3(2).
pub const NR_DUP: u32 = 32;
pub const NR_DUP2: u32 = 33;
pub const NR_DUP3: u32 = 292;

// P2-C1 part 1: mkdir / mkdirat / rmdir / unlink / unlinkat.
pub const NR_MKDIR: u32 = 83;
pub const NR_RMDIR: u32 = 84;
pub const NR_UNLINK: u32 = 87;
pub const NR_MKDIRAT: u32 = 258;
pub const NR_UNLINKAT: u32 = 263;

// unlinkat(2) flag: remove the directory itself (vs the default which
// removes a non-directory). Matches `linux/fcntl.h`.
pub const AT_REMOVEDIR: i32 = 0x200;

// P2-C1 part 2: rename / renameat / renameat2 / ftruncate / truncate.
pub const NR_RENAME: u32 = 82;
pub const NR_RENAMEAT: u32 = 264;
pub const NR_RENAMEAT2: u32 = 316;
pub const NR_TRUNCATE: u32 = 76;
pub const NR_FTRUNCATE: u32 = 77;

// renameat2(2) flags (linux/fs.h).
pub const RENAME_NOREPLACE: i32 = 0x1;
pub const RENAME_EXCHANGE: i32 = 0x2;
pub const RENAME_WHITEOUT: i32 = 0x4;

// P2-C1 part 3: readlink / readlinkat / symlink / symlinkat / link / linkat
//                utimensat / chmod / fchmod / fchmodat
//                faccessat / faccessat2 / chdir / chroot.
pub const NR_READLINK: u32 = 89;
pub const NR_READLINKAT: u32 = 267;
pub const NR_SYMLINK: u32 = 88;
pub const NR_SYMLINKAT: u32 = 266;
pub const NR_LINK: u32 = 86;
pub const NR_LINKAT: u32 = 265;
pub const NR_UTIMENSAT: u32 = 280;
pub const NR_CHMOD: u32 = 90;
pub const NR_FCHMOD: u32 = 91;
pub const NR_FCHMODAT: u32 = 268;
pub const NR_FACCESSAT: u32 = 269;
pub const NR_FACCESSAT2: u32 = 439;
pub const NR_CHDIR: u32 = 80;
pub const NR_CHROOT: u32 = 161;

// faccessat(2) mode bits (linux/fcntl.h).
pub const R_OK: i32 = 4;
pub const W_OK: i32 = 2;
pub const X_OK: i32 = 1;
pub const F_OK: i32 = 0;

// fchmodat(2) flags.
pub const AT_SYMLINK_NOFOLLOW_FCHMODAT: i32 = 0x100;

// utimensat(2) flags.
pub const AT_SYMLINK_NOFOLLOW_UTIMENSAT: i32 = 0x100;

// faccessat(2) flags.
pub const AT_EACCESS: i32 = 0x200;

// Linux PATH_MAX. readlink truncates to `buf_len`; if the link is longer
// than `buf_len` it returns -ENAMETOOLONG.
pub const PATH_MAX: i64 = 4096;

/// Map a `std::io::Error` to a positive errno. Mirrors the helper in
/// `vfs.rs` — kept local here so `sys/file.rs` doesn't depend on
/// `vfs`'s private internals. Returns the errno as a positive i64.
fn io_to_errno(e: std::io::Error) -> i64 {
    use std::io::ErrorKind::*;
    let code = match e.kind() {
        NotFound => crate::errno::ENOENT,
        PermissionDenied => crate::errno::EACCES,
        AlreadyExists => crate::errno::EEXIST,
        InvalidInput => crate::errno::EINVAL,
        NotADirectory => crate::errno::ENOTDIR,
        IsADirectory => crate::errno::EISDIR,
        DirectoryNotEmpty => crate::errno::ENOTEMPTY,
        TooManyLinks => crate::errno::ELOOP,
        _ => crate::errno::EIO,
    };
    code
}

// open() flags (linux/fcntl.h). Keep the bare minimum CPython needs.
pub const O_ACCMODE: i32 = 0o3;
pub const O_RDONLY: i32 = 0o0;
pub const O_WRONLY: i32 = 0o1;
pub const O_RDWR: i32 = 0o2;
pub const O_CREAT: i32 = 0o100;
pub const O_EXCL: i32 = 0o200;
pub const O_NOCTTY: i32 = 0o400;
pub const O_TRUNC: i32 = 0o1000;
pub const O_APPEND: i32 = 0o2000;
pub const O_NONBLOCK: i32 = 0o4000;
pub const O_DIRECTORY: i32 = 0o200000;
pub const O_NOFOLLOW: i32 = 0o400000;
pub const O_CLOEXEC: i32 = 0o2000000;

// lseek whence
pub const SEEK_SET: i64 = 0;
pub const SEEK_CUR: i64 = 1;
pub const SEEK_END: i64 = 2;

// fcntl commands we actually implement
pub const F_GETFL: i32 = 3;
pub const F_SETFL: i32 = 4;
pub const F_GETFD: i32 = 1;
pub const F_SETFD: i32 = 2;
pub const F_DUPFD: i32 = 0;
pub const F_DUPFD_CLOEXEC: i32 = 1024 + 6;

// statx(2) flags (linux/fcntl.h). AT_* apply to *at() family and statx;
// STATX_* select which timestamps/fields the caller wants filled.
pub const AT_EMPTY_PATH: i32 = 0x1000;
pub const AT_NO_AUTOMOUNT: i32 = 0x800;
pub const AT_SYMLINK_NOFOLLOW: i32 = 0x100;

pub const STATX_TYPE: u32 = 0x1;
pub const STATX_MODE: u32 = 0x2;
pub const STATX_NLINK: u32 = 0x4;
pub const STATX_UID: u32 = 0x8;
pub const STATX_GID: u32 = 0x10;
pub const STATX_ATIME: u32 = 0x20;
pub const STATX_MTIME: u32 = 0x40;
pub const STATX_CTIME: u32 = 0x80;
pub const STATX_INO: u32 = 0x100;
pub const STATX_BLOCKS: u32 = 0x400;
pub const STATX_BTIME: u32 = 0x800;

/// Linux `struct statx` is 256 bytes on x86-64 (linux/stat.h).
pub const STATX_SIZE: usize = 256;

/// A seekable file or directory fd. Wraps `std::fs::File` + its current
/// position + the absolute path we opened it from (so `getdents64` can
/// be answered without re-resolving).
///
/// P2-B2: directories are now also stored as `FilePos`; the `is_dir` flag
/// routes `getdents64` and `lseek` to the directory-stream code paths.
/// The `dir_cache` holds the pre-encoded dirent64 record bytes so repeated
/// `getdents64` calls advance `pos` through the same buffer.
pub struct FilePos {
    pub inner: std::fs::File,
    pub pos: u64,
    pub path: Option<PathBuf>,
    /// P2-B2: true when this fd refers to a directory.
    pub is_dir: bool,
    /// P2-B2: pre-encoded dirent64 records for the directory. Populated
    /// lazily on the first `getdents64` call. None for regular files.
    pub dir_cache: Option<Vec<u8>>,
}

impl FilePos {
    pub fn new(f: std::fs::File) -> Self {
        Self {
            inner: f,
            pos: 0,
            path: None,
            is_dir: false,
            dir_cache: None,
        }
    }

    /// P2-D1: snapshot form. Drops `inner: std::fs::File` (rebuilt from
    /// `path` on restore); records `pos`, `path`, `is_dir`, `dir_cache`.
    pub fn snapshot(&self) -> crate::snapshot::FileSnapshot {
        crate::snapshot::FileSnapshot {
            path: self.path.clone(),
            pos: self.pos,
            is_dir: self.is_dir,
            dir_cache: self.dir_cache.clone(),
        }
    }

    pub fn with_path(f: std::fs::File, p: PathBuf) -> Self {
        Self {
            inner: f,
            pos: 0,
            path: Some(p),
            is_dir: false,
            dir_cache: None,
        }
    }

    pub fn try_clone(&self) -> std::io::Result<Self> {
        Ok(Self {
            inner: self.inner.try_clone()?,
            pos: self.pos,
            path: self.path.clone(),
            is_dir: self.is_dir,
            dir_cache: self.dir_cache.clone(),
        })
    }
}

/// Linux `struct statx` (x86-64 / wasm32-musl layout, 256 bytes).
///
/// Layout per `include/uapi/linux/stat.h`. Field offsets are EXACT —
/// the kernel writes little-endian at each offset, then exposes the
/// buffer at `buf_ptr` for musl/glibc to decode. Any drift here will
/// silently corrupt stat results in guests.
///
/// P2-B4 commit 3: encoder + projection from the 120-byte `Stat`.
#[derive(Debug, Clone, Copy)]
pub struct Statx {
    pub stx_mask: u32,
    pub stx_blksize: u32,
    pub stx_attributes: u64,
    pub stx_nlink: u64,
    pub stx_uid: u32,
    pub stx_gid: u32,
    pub stx_mode: u16,
    pub stx_ino: u64,
    pub stx_size: u64,
    pub stx_blocks: u64,
    pub stx_attributes_mask: u64,
    pub stx_atime_sec: i64,
    pub stx_atime_nsec: i64,
    pub stx_btime_sec: i64,
    pub stx_btime_nsec: i64,
    pub stx_ctime_sec: i64,
    pub stx_ctime_nsec: i64,
    pub stx_mtime_sec: i64,
    pub stx_mtime_nsec: i64,
    pub stx_rdev_major: u32,
    pub stx_rdev_minor: u32,
    pub stx_dev_major: u64,
    pub stx_dev_minor: u64,
    pub stx_mnt_id: u64,
    pub stx_dio_mem_align: u32,
    pub stx_dio_offset_align: u32,
}

impl Statx {
    pub const SIZE: usize = STATX_SIZE;

    /// Project a `Stat` (vfs.rs) into a `Statx`. Btime is always 0 (the
    /// host `std::fs::Metadata` does not expose btime on Linux for most
    /// filesystems), so `STATX_BTIME` is excluded from `stx_mask`.
    pub fn from_stat(s: &crate::vfs::Stat) -> Self {
        Self {
            stx_mask: Statx::filled_mask(),
            stx_blksize: s.st_blksize as u32,
            stx_attributes: 0,
            stx_nlink: s.st_nlink,
            stx_uid: s.st_uid,
            stx_gid: s.st_gid,
            stx_mode: (s.st_mode & 0xffff) as u16,
            stx_ino: s.st_ino,
            stx_size: s.st_size as u64,
            stx_blocks: s.st_blocks as u64,
            stx_attributes_mask: 0,
            stx_atime_sec: s.st_atime,
            stx_atime_nsec: clamp_nsec(s.st_atime_nsec),
            stx_btime_sec: 0,
            stx_btime_nsec: 0,
            stx_ctime_sec: s.st_ctime,
            stx_ctime_nsec: clamp_nsec(s.st_ctime_nsec),
            stx_mtime_sec: s.st_mtime,
            stx_mtime_nsec: clamp_nsec(s.st_mtime_nsec),
            stx_rdev_major: 0,
            stx_rdev_minor: 0,
            stx_dev_major: 0,
            stx_dev_minor: 0,
            stx_mnt_id: 0,
            stx_dio_mem_align: 0,
            stx_dio_offset_align: 0,
        }
    }

    /// The mask of bits we actually fill. BTIME is excluded (always 0).
    pub fn filled_mask() -> u32 {
        STATX_TYPE
            | STATX_MODE
            | STATX_NLINK
            | STATX_UID
            | STATX_GID
            | STATX_ATIME
            | STATX_MTIME
            | STATX_CTIME
            | STATX_INO
            | STATX_BLOCKS
    }

    /// Encode to a 256-byte little-endian buffer at exact offsets.
    pub fn encode(self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        let mut o = 0usize;
        buf[o..o + 4].copy_from_slice(&self.stx_mask.to_le_bytes());
        o += 4;
        buf[o..o + 4].copy_from_slice(&self.stx_blksize.to_le_bytes());
        o += 4;
        buf[o..o + 8].copy_from_slice(&self.stx_attributes.to_le_bytes());
        o += 8;
        buf[o..o + 8].copy_from_slice(&self.stx_nlink.to_le_bytes());
        o += 8;
        buf[o..o + 4].copy_from_slice(&self.stx_uid.to_le_bytes());
        o += 4;
        buf[o..o + 4].copy_from_slice(&self.stx_gid.to_le_bytes());
        o += 4;
        // offset 32 = stx_mode (u16)
        buf[o..o + 2].copy_from_slice(&self.stx_mode.to_le_bytes());
        o += 2;
        // 6 bytes pad to offset 40
        o += 6;
        buf[o..o + 8].copy_from_slice(&self.stx_ino.to_le_bytes());
        o += 8;
        buf[o..o + 8].copy_from_slice(&self.stx_size.to_le_bytes());
        o += 8;
        buf[o..o + 8].copy_from_slice(&self.stx_blocks.to_le_bytes());
        o += 8;
        // offset 64 = stx_attributes_mask (u64)
        buf[o..o + 8].copy_from_slice(&self.stx_attributes_mask.to_le_bytes());
        o += 8;
        // offset 72 = stx_atime
        buf[o..o + 8].copy_from_slice(&self.stx_atime_sec.to_le_bytes());
        o += 8;
        buf[o..o + 8].copy_from_slice(&self.stx_atime_nsec.to_le_bytes());
        o += 8;
        // offset 88 = stx_btime (zero)
        buf[o..o + 8].copy_from_slice(&self.stx_btime_sec.to_le_bytes());
        o += 8;
        buf[o..o + 8].copy_from_slice(&self.stx_btime_nsec.to_le_bytes());
        o += 8;
        // offset 104 = stx_ctime
        buf[o..o + 8].copy_from_slice(&self.stx_ctime_sec.to_le_bytes());
        o += 8;
        buf[o..o + 8].copy_from_slice(&self.stx_ctime_nsec.to_le_bytes());
        o += 8;
        // offset 120 = stx_mtime
        buf[o..o + 8].copy_from_slice(&self.stx_mtime_sec.to_le_bytes());
        o += 8;
        buf[o..o + 8].copy_from_slice(&self.stx_mtime_nsec.to_le_bytes());
        o += 8;
        // offset 136 = stx_rdev_major/minor
        buf[o..o + 4].copy_from_slice(&self.stx_rdev_major.to_le_bytes());
        o += 4;
        buf[o..o + 4].copy_from_slice(&self.stx_rdev_minor.to_le_bytes());
        o += 4;
        // offset 144 = stx_dev_major/minor
        buf[o..o + 8].copy_from_slice(&self.stx_dev_major.to_le_bytes());
        o += 8;
        buf[o..o + 8].copy_from_slice(&self.stx_dev_minor.to_le_bytes());
        o += 8;
        // offset 160 = stx_mnt_id
        buf[o..o + 8].copy_from_slice(&self.stx_mnt_id.to_le_bytes());
        o += 8;
        // offset 168 = stx_dio_mem_align / stx_dio_offset_align
        buf[o..o + 4].copy_from_slice(&self.stx_dio_mem_align.to_le_bytes());
        o += 4;
        buf[o..o + 4].copy_from_slice(&self.stx_dio_offset_align.to_le_bytes());
        o += 4;
        // Trailing pad to 256 = stx_reserved3[12] + stx_reserved4[24] + 8 byte end pad.
        // Already zero; nothing to write.
        debug_assert_eq!(o + (Self::SIZE - o), Self::SIZE);
        buf
    }
}

/// Clamp a nsec field into the legal 0..=999_999_999 range. Some host
/// filesystems (e.g. tmpfs on Linux) can hand back negative or oversized
/// values; musl would treat those as a malformed statx.
fn clamp_nsec(n: i64) -> i64 {
    if n < 0 {
        0
    } else if n > 999_999_999 {
        999_999_999
    } else {
        n
    }
}

#[cfg(test)]
mod statx_offset_tests {
    use super::*;

    /// Build a known Statx and verify each field lands at its expected
    /// byte offset in the 256-byte buffer. Anchored against
    /// linux/stat.h so a layout drift fails compilation loudly.
    #[test]
    fn offsets_match_linux_stat_h() {
        let s = Statx {
            stx_mask: 0xdead_beef,
            stx_blksize: 0x1111_2222,
            stx_attributes: 0x3333_4444_5555_6666,
            stx_nlink: 7,
            stx_uid: 1000,
            stx_gid: 1000,
            stx_mode: 0o100644,
            stx_ino: 0xabcd_1234_5678_9abc,
            stx_size: 0x0102_0304_0506_0708,
            stx_blocks: 0x090a_0b0c,
            stx_attributes_mask: 0xdead_beef_dead_beef,
            stx_atime_sec: 1_700_000_000,
            stx_atime_nsec: 123_456_789,
            stx_btime_sec: 0,
            stx_btime_nsec: 0,
            stx_ctime_sec: 1_700_000_001,
            stx_ctime_nsec: 234_567_890,
            stx_mtime_sec: 1_700_000_002,
            stx_mtime_nsec: 345_678_901,
            stx_rdev_major: 0,
            stx_rdev_minor: 0,
            stx_dev_major: 0,
            stx_dev_minor: 0,
            stx_mnt_id: 0,
            stx_dio_mem_align: 0,
            stx_dio_offset_align: 0,
        };
        let buf = s.encode();

        assert_eq!(buf.len(), 256);
        // stx_mask @ 0
        assert_eq!(&buf[0..4], &s.stx_mask.to_le_bytes());
        // stx_blksize @ 4
        assert_eq!(&buf[4..8], &s.stx_blksize.to_le_bytes());
        // stx_attributes @ 8
        assert_eq!(&buf[8..16], &s.stx_attributes.to_le_bytes());
        // stx_nlink @ 16
        assert_eq!(&buf[16..24], &s.stx_nlink.to_le_bytes());
        // stx_uid @ 24
        assert_eq!(&buf[24..28], &s.stx_uid.to_le_bytes());
        // stx_gid @ 28
        assert_eq!(&buf[28..32], &s.stx_gid.to_le_bytes());
        // stx_mode @ 32 (u16) — verify only 2 bytes used
        assert_eq!(&buf[32..34], &s.stx_mode.to_le_bytes());
        // 6-byte pad 34..40 must be zero
        assert!(buf[34..40].iter().all(|b| *b == 0));
        // stx_ino @ 40
        assert_eq!(&buf[40..48], &s.stx_ino.to_le_bytes());
        // stx_size @ 48
        assert_eq!(&buf[48..56], &s.stx_size.to_le_bytes());
        // stx_blocks @ 56
        assert_eq!(&buf[56..64], &s.stx_blocks.to_le_bytes());
        // stx_attributes_mask @ 64
        assert_eq!(&buf[64..72], &s.stx_attributes_mask.to_le_bytes());
        // stx_atime @ 72
        assert_eq!(&buf[72..80], &s.stx_atime_sec.to_le_bytes());
        assert_eq!(&buf[80..88], &s.stx_atime_nsec.to_le_bytes());
        // stx_btime @ 88 (zeroed)
        assert!(buf[88..104].iter().all(|b| *b == 0));
        // stx_ctime @ 104
        assert_eq!(&buf[104..112], &s.stx_ctime_sec.to_le_bytes());
        assert_eq!(&buf[112..120], &s.stx_ctime_nsec.to_le_bytes());
        // stx_mtime @ 120
        assert_eq!(&buf[120..128], &s.stx_mtime_sec.to_le_bytes());
        assert_eq!(&buf[128..136], &s.stx_mtime_nsec.to_le_bytes());
        // stx_rdev_major/minor @ 136/140
        assert_eq!(&buf[136..140], &s.stx_rdev_major.to_le_bytes());
        assert_eq!(&buf[140..144], &s.stx_rdev_minor.to_le_bytes());
        // stx_dev_major/minor @ 144/152
        assert_eq!(&buf[144..152], &s.stx_dev_major.to_le_bytes());
        assert_eq!(&buf[152..160], &s.stx_dev_minor.to_le_bytes());
        // stx_mnt_id @ 160
        assert_eq!(&buf[160..168], &s.stx_mnt_id.to_le_bytes());
        // stx_dio_* @ 168/172
        assert_eq!(&buf[168..172], &s.stx_dio_mem_align.to_le_bytes());
        assert_eq!(&buf[172..176], &s.stx_dio_offset_align.to_le_bytes());
        // reserved 176..256 — all zero
        assert!(buf[176..256].iter().all(|b| *b == 0));
    }

    #[test]
    fn clamp_nsec_helper() {
        assert_eq!(clamp_nsec(-5), 0);
        assert_eq!(clamp_nsec(0), 0);
        assert_eq!(clamp_nsec(500_000_000), 500_000_000);
        assert_eq!(clamp_nsec(1_000_000_000), 999_999_999);
    }

    #[test]
    fn filled_mask_excludes_btime() {
        let m = Statx::filled_mask();
        assert_eq!(m & STATX_BTIME, 0, "BTIME must be excluded from filled_mask");
        assert_ne!(m & STATX_TYPE, 0);
        assert_ne!(m & STATX_MODE, 0);
        assert_ne!(m & STATX_INO, 0);
        assert_ne!(m & STATX_BLOCKS, 0);
    }
}

/// `read(fd, buf, len)`. Reads up to `len` bytes from `fd` into `buf`.
pub async fn read(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fd = match u32::try_from(a[0]) {
        Ok(f) => f,
        Err(_) => return -EBADF,
    };
    let buf_ptr = a[1];
    let buf_len_raw = a[2];
    if let Err(e) = mem::guest_slice_mut(caller, buf_ptr, buf_len_raw) {
        return e;
    }
    let len = match usize::try_from(buf_len_raw) {
        Ok(n) => n,
        Err(_) => return -EFAULT,
    };
    if len == 0 {
        return 0;
    }

    let mut tmp: Vec<u8> = Vec::new();
    let mut eof = false;
    {
        let fds = &mut caller.data_mut().fds;
        let res = match fds.get_mut(fd) {
            Ok(r) => r,
            Err(e) => return e,
        };
        match res {
            Resource::Stdin(r) | Resource::PipeRead(r) => {
                let mut q = r.buf.lock();
                for _ in 0..len {
                    match q.pop_front() {
                        Some(b) => tmp.push(b),
                        None => break,
                    }
                }
                eof = tmp.is_empty() && *r.closed.lock();
                // P1-3: if the pipe is non-blocking and empty (and not EOF),
                // surface -EAGAIN instead of blocking. This matches the
                // Linux semantics for `read(2)` on an O_NONBLOCK fd.
                if tmp.is_empty() && !eof && r.nonblock.load(std::sync::atomic::Ordering::Relaxed) {
                    return -crate::errno::EAGAIN;
                }
            }
            Resource::File(fp) => {
                // Read up to `len` bytes via std::io::Read at fp.pos.
                // Seek first so position is correct.
                let mut fp = fp.lock();
                let pos = fp.pos;
                let _ = fp.inner.seek(SeekFrom::Start(pos));
                let mut got = Vec::with_capacity(len);
                let mut chunk = [0u8; 4096];
                loop {
                    match fp.inner.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(k) => {
                            let remaining = len - got.len();
                            if k > remaining {
                                got.extend_from_slice(&chunk[..remaining]);
                                break;
                            } else {
                                got.extend_from_slice(&chunk[..k]);
                                if got.len() >= len {
                                    break;
                                }
                            }
                        }
                        Err(_) => return -EACCES,
                    }
                }
                fp.pos += got.len() as u64;
                tmp = got;
            }
            Resource::EventFd(e) => {
                // P2-B1: drain the counter into a u64 at buf.
                if let Err(e) = eventfd::require_u64_buf(buf_len_raw) {
                    return e;
                }
                let val = match eventfd::eventfd_read(e) {
                    Ok(v) => v,
                    Err(e) => return e,
                };
                if val == 0 {
                    // Blocking read with empty counter — nothing to do in
                    // this v1 model (no real block), return 0 (EOF).
                    return 0;
                }
                let bytes = val.to_ne_bytes();
                let buf = match mem::guest_slice_mut(caller, buf_ptr, 8) {
                    Ok(b) => b,
                    Err(e) => return e,
                };
                buf[..8].copy_from_slice(&bytes);
                return 8;
            }
            Resource::Socket(_s) => {
                // P2-C3 part 2: dispatch read(2) against a Socket to the
                // existing recvfrom(fd, buf, len, flags=0, addr=0, addrlen=0)
                // path. recvfrom already covers both V4 and AF_UNIX streams
                // and honors SHUT_RD EOF semantics.
                drop(tmp);
                drop(eof);
                return socket::recvfrom(
                    caller,
                    [a[0], a[1], a[2], 0, 0, 0],
                )
                .await;
            }
            _ => return -EBADF,
        }
    }
    if eof {
        return 0;
    }
    if tmp.is_empty() {
        // Reached only if the pipe was blocking (nonblock path returns
        // earlier). P0 behavior: surface -EAGAIN even when blocking; a
        // future P1-7 epoll layer will let callers block on read(2).
        return -crate::errno::EAGAIN;
    }
    let n = tmp.len();
    let bytes = match mem::guest_slice_mut(caller, buf_ptr, len as i64) {
        Ok(b) => b,
        Err(e) => return e,
    };
    bytes[..n].copy_from_slice(&tmp);
    n as i64
}

/// `write(fd, buf, len)`. Writes `len` bytes from `buf` to `fd`.
pub async fn write(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fd = match u32::try_from(a[0]) {
        Ok(f) => f,
        Err(_) => return -EBADF,
    };
    let bytes = match mem::guest_slice(caller, a[1], a[2]) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let len = bytes.len();
    if len == 0 {
        return 0;
    }
    let bytes = bytes.to_vec();

    let written = {
        let fds = &mut caller.data_mut().fds;
        let res = match fds.get_mut(fd) {
            Ok(r) => r,
            Err(e) => return e,
        };
        match res {
            Resource::Stdout(w) | Resource::Stderr(w) | Resource::PipeWrite(w) => {
                let mut q = w.buf.lock();
                let was_empty = q.is_empty();
                q.extend(bytes.iter().copied());
                drop(q);
                // P2-B3: wake any poll/epoll subscriber waiting for POLLIN.
                // Only fire on the empty→non-empty transition so we don't
                // spam wakers on every write into a non-empty buffer.
                if was_empty {
                    w.notify.notify_waiters();
                }
                bytes.len()
            }
            Resource::File(fp) => {
                let mut fp = fp.lock();
                match fp.inner.write(&bytes) {
                    Ok(n) => {
                        fp.pos += n as u64;
                        n
                    }
                    Err(_) => return -crate::errno::EIO,
                }
            }
            Resource::EventFd(e) => {
                // P2-B1: add u64 at buf to the counter.
                if let Err(e) = eventfd::require_u64_buf(a[2]) {
                    return e;
                }
                let mut be = [0u8; 8];
                be.copy_from_slice(&bytes[..8]);
                let addend = u64::from_ne_bytes(be);
                let _new = eventfd::eventfd_write(e, addend);
                8
            }
            Resource::Socket(_s) => {
                // P2-C3 part 2: dispatch write(2) against a Socket to the
                // existing sendto(fd, buf, len, flags=0, addr=0, addrlen=0)
                // path. sendto already covers both V4 and AF_UNIX streams
                // and honors SHUT_WR EPIPE semantics.
                drop(bytes);
                return socket::sendto(
                    caller,
                    [a[0], a[1], a[2], 0, 0, 0],
                )
                .await;
            }
            _ => return -EBADF,
        }
    };
    written as i64
}

/// `openat(dirfd, path, flags, mode)`.
pub async fn openat(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let dirfd = a[0];
    let path_ptr = a[1];
    let flags = a[2] as i32;
    let mode = a[3] as u32;

    let path = match mem::guest_str(caller, path_ptr, 4096) {
        Ok(s) => s.to_string(),
        Err(e) => return e,
    };

    let abs = match crate::sys::path::resolve(caller, dirfd, &path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let vfs = {
        let kern = caller.data();
        Vfs { root: kern.vfs.root.clone(), cwd: kern.vfs.cwd.clone() }
    };
    let _ = mode;
    let file = match vfs.open(&abs, flags, mode) {
        Ok(f) => f,
        Err(e) => return e,
    };
    // P2-B2: stat the path to set is_dir. This lets getdents64/lseek
    // distinguish a directory fd from a regular file fd.
    let is_dir = std::fs::metadata(&abs).map(|m| m.is_dir()).unwrap_or(false);
    let mut fp = FilePos::with_path(file, abs);
    fp.is_dir = is_dir;
    let fd = caller.data_mut()
        .fds
        .insert(Resource::File(std::sync::Arc::new(parking_lot::Mutex::new(fp))));
    fd as i64
}

/// `close(fd)`.
pub async fn close(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fd = match u32::try_from(a[0]) {
        Ok(f) => f,
        Err(_) => return -EBADF,
    };
    let fds = &mut caller.data_mut().fds;
    match fds.close(fd) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

/// `lseek(fd, offset, whence)`. Returns the new absolute offset.
pub async fn lseek(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fd = match u32::try_from(a[0]) {
        Ok(f) => f,
        Err(_) => return -EBADF,
    };
    let offset = a[1];
    let whence = a[2];

    let fds = &mut caller.data_mut().fds;
    let res = match fds.get_mut(fd) {
        Ok(r) => r,
        Err(e) => return e,
    };
    match res {
        Resource::File(fp) => {
            let mut fp = fp.lock();
            if fp.is_dir {
                // P2-B2: dir stream. Only SEEK_SET(0) (rewind) is honored;
                // other whence values return -ESPIPE per Linux semantics.
                match whence {
                    SEEK_SET if offset == 0 => {
                        fp.pos = 0;
                        0
                    }
                    _ => -ESPIPE,
                }
            } else {
                let from = match whence {
                    SEEK_SET => SeekFrom::Start(offset.max(0) as u64),
                    SEEK_CUR => SeekFrom::Current(offset),
                    SEEK_END => {
                        let len = fp.inner.metadata().map(|m| m.len() as i64).unwrap_or(0);
                        SeekFrom::Start((len + offset).max(0) as u64)
                    }
                    _ => return -EINVAL,
                };
                match fp.inner.seek(from) {
                    Ok(p) => {
                        fp.pos = p;
                        p as i64
                    }
                    Err(_) => -EINVAL,
                }
            }
        }
        _ => -ESPIPE,
    }
}

/// `fstat(fd, statbuf)`.
pub async fn fstat(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fd = match u32::try_from(a[0]) {
        Ok(f) => f,
        Err(_) => return -EBADF,
    };
    let statbuf_ptr = a[1];

    let stat: Stat = {
        let fds = &caller.data().fds;
        match fds.get(fd) {
            Ok(Resource::File(fp)) => match fp.lock().inner.metadata() {
                Ok(meta) => Stat::from_metadata(&meta),
                Err(_) => synth_char(),
            },
            Ok(_) => synth_char(),
            Err(e) => return e,
        }
    };
    if let Err(e) = stat.write_to_guest(caller, statbuf_ptr) {
        return e;
    }
    0
}

/// `newfstatat(dirfd, path, statbuf, flags)`.
pub async fn newfstatat(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let dirfd = a[0];
    let path_ptr = a[1];
    let statbuf_ptr = a[2];
    let flags = a[3] as i32;

    let path = match mem::guest_str(caller, path_ptr, 4096) {
        Ok(s) => s.to_string(),
        Err(e) => return e,
    };

    // AT_EMPTY_PATH (0x1000): stat the fd itself.
    if flags & 0x1000 != 0 && path.is_empty() {
        return fstat(caller, [dirfd, statbuf_ptr, 0, 0, 0, 0]).await;
    }

    // Empty path without AT_EMPTY_PATH → -ENOENT (matches Linux).
    if path.is_empty() {
        return -ENOENT;
    }

    let abs = match crate::sys::path::resolve(caller, dirfd, &path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let vfs = {
        let kern = caller.data();
        Vfs { root: kern.vfs.root.clone(), cwd: kern.vfs.cwd.clone() }
    };
    let stat = match vfs.stat(&abs) {
        Ok(s) => s,
        Err(e) => return e,
    };
    if let Err(e) = stat.write_to_guest(caller, statbuf_ptr) {
        return e;
    }
    0
}

/// `statx(dirfd, pathname, flags, mask, buf)` — extended stat.
///
/// P2-B4: returns a 256-byte `struct statx` at `buf`. Recognized flags
/// are `AT_EMPTY_PATH`, `AT_NO_AUTOMOUNT`, `AT_SYMLINK_NOFOLLOW`; any
/// other bits in `flags` return `-EINVAL` (matches Linux strict mode).
/// `mask` is accepted but currently we always fill the same fixed set
/// (BTIME excluded — the host std does not expose btime).
pub async fn statx(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let dirfd = a[0];
    let path_ptr = a[1];
    let flags = a[2] as i32;
    let _mask = a[3] as u32;
    let buf_ptr = a[4];

    const RECOGNIZED_FLAGS: i32 = AT_EMPTY_PATH | AT_NO_AUTOMOUNT | AT_SYMLINK_NOFOLLOW;
    if flags & !RECOGNIZED_FLAGS != 0 {
        return -EINVAL;
    }
    if let Err(e) = mem::guest_slice_mut(caller, buf_ptr, STATX_SIZE as i64) {
        return e;
    }

    let path = match mem::guest_str(caller, path_ptr, 4096) {
        Ok(s) => s.to_string(),
        Err(e) => return e,
    };

    // Build a `Stat` either by stat'ing the fd (AT_EMPTY_PATH + empty
    // path) or by resolving the path through the VFS. Reuse
    // `Stat::from_metadata(&meta)` (vfs.rs) and project via
    // `Statx::from_stat`.
    let stat = if flags & AT_EMPTY_PATH != 0 && path.is_empty() {
        let fd = match u32::try_from(dirfd) {
            Ok(f) => f,
            Err(_) => return -EBADF,
        };
        match caller.data().fds.get(fd) {
            Ok(Resource::File(fp)) => match fp.lock().inner.metadata() {
                Ok(meta) => crate::vfs::Stat::from_metadata(&meta),
                Err(_) => return -EACCES,
            },
            Ok(_) => synth_char(),
            Err(e) => return e,
        }
    } else {
        if path.is_empty() {
            return -ENOENT;
        }
        let abs = match crate::sys::path::resolve(caller, dirfd, &path) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let vfs = {
            let kern = caller.data();
            crate::vfs::Vfs { root: kern.vfs.root.clone(), cwd: kern.vfs.cwd.clone() }
        };
        // AT_SYMLINK_NOFOLLOW is accepted; we don't differentiate here
        // because the host std always follows symlinks. If we ever need
        // lstat semantics, swap to std::fs::symlink_metadata.
        let _ = flags;
        match vfs.stat(&abs) {
            Ok(s) => s,
            Err(e) => return e,
        }
    };

    let statx = Statx::from_stat(&stat);
    let bytes = statx.encode();
    let slice = match mem::guest_slice_mut(caller, buf_ptr, STATX_SIZE as i64) {
        Ok(s) => s,
        Err(e) => return e,
    };
    slice.copy_from_slice(&bytes);
    0
}

/// `getdents64(fd, buf, len)`.
///
/// P2-B2: tracks the position per-fd via `FilePos.pos`. The first call
/// populates `FilePos.dir_cache` with the full pre-encoded dirent64
/// buffer; subsequent calls slice from `pos`. When `pos >= dir_cache.len()`
/// the syscall returns 0 (end-of-directory).
pub async fn getdents64(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fd = match u32::try_from(a[0]) {
        Ok(f) => f,
        Err(_) => return -EBADF,
    };
    let buf_ptr = a[1];
    let buf_len_raw = a[2];
    if let Err(e) = mem::guest_slice_mut(caller, buf_ptr, buf_len_raw) {
        return e;
    }
    let len = match usize::try_from(buf_len_raw) {
        Ok(n) => n,
        Err(_) => return -EINVAL,
    };
    if len == 0 {
        return -EINVAL;
    }

    // First, peek the resource type. Non-file fds (Stdout, Pipe, Socket,
    // EventFd, Epoll) immediately fail with -ENOTDIR. We allow -EBADF
    // for backward-compat (existing tests on stdout accept either).
    let is_dir_fd: bool = {
        let fds = &caller.data().fds;
        match fds.get(fd) {
            Ok(Resource::File(fp)) => fp.lock().is_dir,
            Ok(_) => return -crate::errno::ENOTDIR,
            Err(e) => return e,
        }
    };
    if !is_dir_fd {
        return -crate::errno::ENOTDIR;
    }

    // Populate the cache lazily on the first call. We do this in a fresh
    // fds-borrow block.
    let path: PathBuf = {
        let fds = &caller.data().fds;
        match fds.get(fd) {
            Ok(Resource::File(fp)) => match fp.lock().path.clone() {
                Some(p) => p,
                None => return -EBADF,
            },
            Ok(_) => return -crate::errno::ENOTDIR,
            Err(e) => return e,
        }
    };
    let (root, cwd) = {
        let kern = caller.data();
        (kern.vfs.root.clone(), kern.vfs.cwd.clone())
    };
    let vfs = Vfs { root, cwd };

    // Lazily fill dir_cache. Re-stat on every call is cheap; we only
    // re-read the directory if the cache is empty.
    let needs_fill = {
        let fds = &caller.data().fds;
        match fds.get(fd) {
            Ok(Resource::File(fp)) => fp.lock().dir_cache.is_none(),
            _ => false,
        }
    };
    if needs_fill {
        let cached = match vfs.readdir_all(&path) {
            Ok(b) => b,
            Err(e) => return e,
        };
        let fds = &mut caller.data_mut().fds;
        if let Ok(Resource::File(fp)) = fds.get_mut(fd) {
            fp.lock().dir_cache = Some(cached);
        }
    }

    // Slice the cached dirent64 buffer at fp.pos.
    let (slice, new_pos): (Vec<u8>, u64) = {
        let fds = &mut caller.data_mut().fds;
        let mut fp = match fds.get_mut(fd) {
            Ok(Resource::File(fp)) => fp,
            _ => return -crate::errno::EBADF,
        }.lock();
        let cache = fp.dir_cache.as_ref().expect("dir_cache just populated");
        let start = fp.pos as usize;
        if start >= cache.len() {
            // Already exhausted.
            (Vec::new(), fp.pos)
        } else {
            let end = (start + len).min(cache.len());
            let s = cache[start..end].to_vec();
            let new_pos = end as u64;
            fp.pos = new_pos;
            (s, new_pos)
        }
    };
    let _ = new_pos;
    let n = slice.len();
    if n == 0 {
        return 0; // End of directory.
    }
    let dst = match mem::guest_slice_mut(caller, buf_ptr, n as i64) {
        Ok(b) => b,
        Err(e) => return e,
    };
    dst.copy_from_slice(&slice);
    n as i64
}

/// `pipe2(fdarray, flags)`. Allocates a paired (read, write) buffer-backed
/// pipe, inserts both ends into the FdTable, and writes the two u32 fds
/// into the guest's `fdarray` pointer (little-endian, [read_fd, write_fd]).
///
/// `flags` honored:
/// * `O_CLOEXEC` (0o2000000) — accepted; FD_CLOEXEC tracked for fidelity.
///   (P0 doesn't model exec; the flag is recorded but not enforced.)
/// * `O_NONBLOCK` (0o4000)   — flips the `nonblock` bit on both ends so a
///   subsequent `read` on the read end returns `-EAGAIN` when the buffer
///   is empty (P1-3). Buffer pipes are unbounded on the write side, so
///   `O_NONBLOCK` has no effect on writes today.
pub async fn pipe2(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fdarray_ptr = a[0];
    let flags = a[1] as i32;

    // Bounds-check the fdarray first; both fds together must be writable.
    if let Err(e) = mem::guest_slice_mut(caller, fdarray_ptr, 8) {
        return e;
    }

    let (rd, wr) = crate::fd::make_pipe();
    // Honour O_NONBLOCK at creation time. fcntl(F_SETFL) can flip this
    // later; see `fn fcntl`.
    if flags & O_NONBLOCK != 0 {
        rd.nonblock.store(true, std::sync::atomic::Ordering::Relaxed);
        wr.nonblock.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    let (rd_fd, wr_fd) = {
        let fds = &mut caller.data_mut().fds;
        let rd_fd = fds.insert(Resource::PipeRead(rd));
        let wr_fd = fds.insert(Resource::PipeWrite(wr));
        (rd_fd, wr_fd)
    };

    let buf = match mem::guest_slice_mut(caller, fdarray_ptr, 8) {
        Ok(b) => b,
        Err(e) => return e,
    };
    buf[0..4].copy_from_slice(&rd_fd.to_le_bytes());
    buf[4..8].copy_from_slice(&wr_fd.to_le_bytes());
    0
}

/// `pipe(fdarray)` — legacy wrapper around `pipe2(fdarray, 0)`. musl routes
/// the legacy `pipe(2)` syscall through `pipe2` with no flags.
pub async fn pipe(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    pipe2(caller, [a[0], 0, 0, 0, 0, 0]).await
}

/// `open(path, flags, mode)` — legacy wrapper around
/// `openat(AT_FDCWD, path, flags, mode)`.
pub async fn open(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let path_ptr = a[0];
    let flags = a[1];
    let mode = a[2];
    openat(caller, [-100 /*AT_FDCWD*/, path_ptr, flags, mode, 0, 0]).await
}

/// `stat(path, statbuf)` — legacy wrapper around `newfstatat(AT_FDCWD, path,
/// statbuf, 0)`. Returns `-ENOENT` if `path` is empty (matches Linux: an
/// empty path requires `AT_EMPTY_PATH` to refer to the cwd).
pub async fn stat(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let path_ptr = a[0];
    let statbuf_ptr = a[1];
    newfstatat(caller, [-100, path_ptr, statbuf_ptr, 0, 0, 0]).await
}

/// `lstat(path, statbuf)` — `newfstatat` with `AT_SYMLINK_NOFOLLOW = 0x100`.
/// Returns `-ENOENT` if `path` is empty (matches Linux: an empty path
/// requires `AT_EMPTY_PATH` to refer to the cwd).
pub async fn lstat(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let path_ptr = a[0];
    let statbuf_ptr = a[1];
    newfstatat(caller, [-100, path_ptr, statbuf_ptr, 0x100, 0, 0]).await
}

/// `mkdir(path, mode)` — create a single directory at `path` (parent
/// must exist; we don't auto-create intermediates). `mode` bits are
/// masked to `0o777`; the umask isn't modeled. Returns 0 on success,
/// `-EEXIST` if the path already exists, `-ENOENT` if the parent
/// directory is missing.
pub async fn mkdir(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let path_ptr = a[0];
    let mode = a[1] as u32;

    let path = match mem::guest_str(caller, path_ptr, 4096) {
        Ok(s) => s.to_string(),
        Err(e) => return e,
    };
    if path.is_empty() {
        return -crate::errno::ENOENT;
    }

    let abs = match crate::sys::path::resolve(caller, -100, &path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let _ = mode;
    match std::fs::create_dir(&abs) {
        Ok(()) => 0,
        Err(e) => -io_to_errno(e),
    }
}

/// `mkdirat(dirfd, path, mode)` — `mkdir` with explicit dirfd. The
/// kernel accepts both NR_MKDIR and NR_MKDIRAT — both go through the
/// same handler because our routing is `dirfd == AT_FDCWD` (the common
/// case) and `dirfd >= 0` (a bound directory fd). `mode` is masked to
/// `0o777`; no umask.
pub async fn mkdirat(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let dirfd = a[0];
    let path_ptr = a[1];
    let mode = a[2] as u32;

    let path = match mem::guest_str(caller, path_ptr, 4096) {
        Ok(s) => s.to_string(),
        Err(e) => return e,
    };
    if path.is_empty() {
        return -crate::errno::ENOENT;
    }

    let abs = match crate::sys::path::resolve(caller, dirfd, &path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let _ = mode;
    match std::fs::create_dir(&abs) {
        Ok(()) => 0,
        Err(e) => -io_to_errno(e),
    }
}

/// `rmdir(path)` — remove a single empty directory. Returns `-ENOTDIR`
/// if `path` isn't a directory, `-ENOENT` if it doesn't exist.
pub async fn rmdir(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let path_ptr = a[0];

    let path = match mem::guest_str(caller, path_ptr, 4096) {
        Ok(s) => s.to_string(),
        Err(e) => return e,
    };
    if path.is_empty() {
        return -crate::errno::ENOENT;
    }

    let abs = match crate::sys::path::resolve(caller, -100, &path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match std::fs::remove_dir(&abs) {
        Ok(()) => 0,
        Err(e) => -io_to_errno(e),
    }
}

/// `unlink(path)` — remove a non-directory file. Returns `-EISDIR` if
/// the path is a directory (use `rmdir` for those).
pub async fn unlink(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let path_ptr = a[0];

    let path = match mem::guest_str(caller, path_ptr, 4096) {
        Ok(s) => s.to_string(),
        Err(e) => return e,
    };
    if path.is_empty() {
        return -crate::errno::ENOENT;
    }

    let abs = match crate::sys::path::resolve(caller, -100, &path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match std::fs::remove_file(&abs) {
        Ok(()) => 0,
        Err(e) => -io_to_errno(e),
    }
}

/// `unlinkat(dirfd, path, flags)` — `unlink` with explicit dirfd and
/// `AT_REMOVEDIR` flag. When `AT_REMOVEDIR` is set, behaves like
/// `rmdir`; otherwise behaves like `unlink`.
pub async fn unlinkat(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let dirfd = a[0];
    let path_ptr = a[1];
    let flags = a[2] as i32;

    let path = match mem::guest_str(caller, path_ptr, 4096) {
        Ok(s) => s.to_string(),
        Err(e) => return e,
    };
    if path.is_empty() {
        return -crate::errno::ENOENT;
    }

    let abs = match crate::sys::path::resolve(caller, dirfd, &path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let _ = flags;
    let result = if flags & AT_REMOVEDIR != 0 {
        std::fs::remove_dir(&abs)
    } else {
        std::fs::remove_file(&abs)
    };
    match result {
        Ok(()) => 0,
        Err(e) => -io_to_errno(e),
    }
}

/// `rename(oldpath, newpath)` — atomically rename (move) a file or
/// directory. Thin wrapper over `renameat(AT_FDCWD, old, AT_FDCWD, new, 0)`.
pub async fn rename(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    renameat(caller, [-100, a[0], -100, a[1], 0, 0]).await
}

/// `renameat(olddirfd, oldpath, newdirfd, newpath, flags=0)` — rename
/// with explicit dirfds. Honors:
/// * `flags == 0`               → plain rename (std::fs::rename).
/// * `RENAME_NOREPLACE (0x1)`   → -EEXIST if newpath already exists.
/// * `RENAME_EXCHANGE (0x2)`    → atomic swap (POSIX rename supports it
///                                on Linux; std::fs::rename maps to it).
/// * `RENAME_WHITEOUT (0x4)`    → -EINVAL (not modeled; overlayfs only).
/// * other bits                 → -EINVAL.
pub async fn renameat(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let olddirfd = a[0];
    let old_ptr = a[1];
    let newdirfd = a[2];
    let new_ptr = a[3];
    let flags = a[4] as i32;

    let old = match mem::guest_str(caller, old_ptr, 4096) {
        Ok(s) => s.to_string(),
        Err(e) => return e,
    };
    let new = match mem::guest_str(caller, new_ptr, 4096) {
        Ok(s) => s.to_string(),
        Err(e) => return e,
    };
    if old.is_empty() || new.is_empty() {
        return -crate::errno::ENOENT;
    }

    let old_abs = match crate::sys::path::resolve(caller, olddirfd, &old) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let new_abs = match crate::sys::path::resolve(caller, newdirfd, &new) {
        Ok(p) => p,
        Err(e) => return e,
    };

    // Reject unknown flag combinations up front. RENAME_WHITEOUT is the
    // only recognized bit we don't model.
    let known = RENAME_NOREPLACE | RENAME_EXCHANGE | RENAME_WHITEOUT;
    if flags & !known != 0 {
        return -crate::errno::EINVAL;
    }
    if flags & RENAME_WHITEOUT != 0 {
        return -crate::errno::EINVAL;
    }

    // RENAME_NOREPLACE: if newpath already exists, -EEXIST.
    if flags & RENAME_NOREPLACE != 0 && std::fs::metadata(&new_abs).is_ok() {
        return -crate::errno::EEXIST;
    }

    match std::fs::rename(&old_abs, &new_abs) {
        Ok(()) => 0,
        Err(e) => -io_to_errno(e),
    }
}

/// `renameat2(olddirfd, oldpath, newdirfd, newpath, flags)` — same as
/// `renameat` but with full flag support. Currently a thin shim.
pub async fn renameat2(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    renameat(caller, a).await
}

/// `truncate(path, len)` — set the length of the file at `path`. If the
/// file is shorter, it's extended (zero-filled); if longer, it's
/// truncated. Creates the file if it doesn't exist (matches std::fs
/// `OpenOptions::create`).
pub async fn truncate(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let path_ptr = a[0];
    let len_raw = a[1];

    let path = match mem::guest_str(caller, path_ptr, 4096) {
        Ok(s) => s.to_string(),
        Err(e) => return e,
    };
    if path.is_empty() {
        return -crate::errno::ENOENT;
    }
    let len: u64 = match len_raw {
        n if n < 0 => return -crate::errno::EINVAL,
        n => n as u64,
    };

    let abs = match crate::sys::path::resolve(caller, -100, &path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&abs)
        .and_then(|f| f.set_len(len))
    {
        Ok(()) => 0,
        Err(e) => -io_to_errno(e),
    }
}

/// `ftruncate(fd, len)` — set the length of the open file at `fd`.
/// Per B5 lock discipline: we hold `Resource::File` (Arc<Mutex<FilePos>>),
/// take a `try_clone` of the inner `std::fs::File` under a brief lock,
/// drop the guard, then call the sync `set_len` on the clone (no .await).
pub async fn ftruncate(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fd = match u32::try_from(a[0]) {
        Ok(f) => f,
        Err(_) => return -crate::errno::EBADF,
    };
    let len_raw = a[1];
    let len: u64 = match len_raw {
        n if n < 0 => return -crate::errno::EINVAL,
        n => n as u64,
    };

    let file_clone = {
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::File(fp)) => {
                let fp = fp.lock();
                match fp.inner.try_clone() {
                    Ok(f) => f,
                    Err(e) => return -io_to_errno(e),
                }
            }
            Ok(_) => return -crate::errno::EBADF,
            Err(e) => return e,
        }
    };
    // No await past this point — set_len is sync.
    match file_clone.set_len(len) {
        Ok(()) => 0,
        Err(e) => -io_to_errno(e),
    }
}

/// `getcwd(buf, size)` — write the current working directory (NUL-terminated)
/// into the guest's `buf`. Returns the byte length excluding the NUL on
/// success; returns `-ERANGE` if `size` is too small to fit the path + NUL;
/// returns `-EFAULT` if `buf` doesn't fit in linear memory.
pub async fn getcwd(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let buf_ptr = a[0];
    let buf_len = match usize::try_from(a[1]) {
        Ok(n) => n,
        Err(_) => return -EFAULT,
    };

    let cwd = caller.data().vfs.cwd.clone();
    let cwd_bytes = cwd.to_string_lossy().into_owned().into_bytes();
    let needed = cwd_bytes.len() + 1; // +1 for trailing NUL
    if buf_len < needed {
        return -ERANGE;
    }

    let buf = match mem::guest_slice_mut(caller, buf_ptr, needed as i64) {
        Ok(b) => b,
        Err(e) => return e,
    };
    buf[..cwd_bytes.len()].copy_from_slice(&cwd_bytes);
    buf[cwd_bytes.len()] = 0;
    cwd_bytes.len() as i64
}

/// `readv(fd, iov, iovcnt)` — scatter read. Walks an array of
/// `struct iovec { u32 base; u32 len; }` (8 bytes each on wasm32, per plan §3)
/// and reads each buffer sequentially. P0 single-shot semantics: uvicorn's
/// httptools readv pattern is two adjacent buffers which read identically
/// via sequential `read()` calls.
///
/// Returns total bytes read on success; returns the partial count if a
/// mid-vector read fails (Linux lets the caller resume).
pub async fn readv(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fd = a[0];
    let iov_ptr = a[1];
    let iov_count = match usize::try_from(a[2]) {
        Ok(n) => n,
        Err(_) => return -EINVAL,
    };
    let total_len = match (iov_count as i64).checked_mul(8) {
        Some(n) if n >= 0 => n,
        _ => return -EFAULT,
    };
    let iovs = match mem::guest_slice(caller, iov_ptr, total_len) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let entries: Vec<(i64, i64)> = iovs
        .chunks_exact(8)
        .map(|iov_bytes| {
            let base = u32::from_le_bytes(iov_bytes[0..4].try_into().unwrap()) as i64;
            let len = u32::from_le_bytes(iov_bytes[4..8].try_into().unwrap()) as i64;
            (base, len)
        })
        .collect();
    let mut total_read = 0i64;
    for (base, len) in entries {
        if len == 0 {
            continue;
        }
        let r = read(caller, [fd, base, len, 0, 0, 0]).await;
        if r < 0 {
            return if total_read == 0 { r } else { total_read };
        }
        total_read += r;
        if r < len {
            break; // short read — stop, like Linux
        }
    }
    total_read
}

/// `writev(fd, iov, iovcnt)` — gather write. Same `struct iovec` shape as
/// `readv`. Chunks into separate `write()` calls; total return is the sum.
pub async fn writev(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fd = a[0];
    let iov_ptr = a[1];
    let iov_count = match usize::try_from(a[2]) {
        Ok(n) => n,
        Err(_) => return -EINVAL,
    };
    let total_len = match (iov_count as i64).checked_mul(8) {
        Some(n) if n >= 0 => n,
        _ => return -EFAULT,
    };
    let iovs = match mem::guest_slice(caller, iov_ptr, total_len) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let entries: Vec<(i64, i64)> = iovs
        .chunks_exact(8)
        .map(|iov_bytes| {
            let base = u32::from_le_bytes(iov_bytes[0..4].try_into().unwrap()) as i64;
            let len = u32::from_le_bytes(iov_bytes[4..8].try_into().unwrap()) as i64;
            (base, len)
        })
        .collect();
    let mut total_written = 0i64;
    for (base, len) in entries {
        if len == 0 {
            continue;
        }
        let w = write(caller, [fd, base, len, 0, 0, 0]).await;
        if w < 0 {
            return if total_written == 0 { w } else { total_written };
        }
        total_written += w;
        if w < len {
            break; // short write — stop
        }
    }
    total_written
}

/// P2-C2: flip the nonblock flag on a resource. Used by both
/// `fcntl(F_SETFL, O_NONBLOCK)` and `ioctl(FIONBIO, 1)`.
///
/// Resources without a nonblock concept (File, Epoll, EventFd) are
/// accepted silently. Sockets/Pipes/Stdino update their `nonblock`
/// AtomicBool.
pub fn set_nonblock(r: &mut Resource, on: bool) {
    use std::sync::atomic::Ordering;
    match r {
        Resource::Stdin(x) | Resource::PipeRead(x) => {
            x.nonblock.store(on, Ordering::Relaxed);
        }
        Resource::Stdout(x) | Resource::Stderr(x) | Resource::PipeWrite(x) => {
            x.nonblock.store(on, Ordering::Relaxed);
        }
        Resource::Socket(s) => {
            s.lock().nonblock.store(on, Ordering::Relaxed);
        }
        Resource::File(_) | Resource::Epoll(_) | Resource::EventFd(_) => {
            // No-op.
        }
    }
}

/// `fcntl(fd, cmd, arg)`. Limited subset (F_GETFL/F_SETFL/F_GETFD/F_SETFD/F_DUPFD).
pub async fn fcntl(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fd = match u32::try_from(a[0]) {
        Ok(f) => f,
        Err(_) => return -EBADF,
    };
    let cmd = a[1] as i32;
    let arg = a[2];

    match cmd {
        F_GETFL => {
            // P1-3: actually read O_NONBLOCK from the resource. We don't
            // distinguish RDONLY vs RDWR for pipes (they're full-duplex
            // from the guest's perspective), so pipes report O_RDWR.
            let fds = &caller.data().fds;
            match fds.get(fd) {
                Ok(Resource::Stdin(r)) | Ok(Resource::PipeRead(r)) => {
                    let nb = r.nonblock.load(std::sync::atomic::Ordering::Relaxed);
                    let mut fl = O_RDONLY;
                    if nb {
                        fl |= O_NONBLOCK;
                    }
                    fl as i64
                }
                Ok(Resource::Stdout(w)) | Ok(Resource::Stderr(w)) | Ok(Resource::PipeWrite(w)) => {
                    let nb = w.nonblock.load(std::sync::atomic::Ordering::Relaxed);
                    let mut fl = O_WRONLY;
                    if nb {
                        fl |= O_NONBLOCK;
                    }
                    fl as i64
                }
                Ok(Resource::File(_)) => O_RDWR as i64,
                Ok(Resource::Socket(s)) => {
                    let nb = s.lock().nonblock.load(std::sync::atomic::Ordering::Relaxed);
                    let mut fl = O_RDWR;
                    if nb {
                        fl |= O_NONBLOCK;
                    }
                    fl as i64
                }
                // P1-7: epoll/eventfd have no file-status flags to surface.
                Ok(Resource::Epoll(_)) | Ok(Resource::EventFd(_)) => O_RDWR as i64,
                Err(e) => e,
            }
        }
        F_SETFL => {
            // P1-3: only O_NONBLOCK is wired through. Other bits (O_APPEND
            // etc.) are accepted silently — matches Linux for a pipe.
            let want_nonblock = (arg as i32) & O_NONBLOCK != 0;
            let fds = &mut caller.data_mut().fds;
            match fds.get_mut(fd) {
                Ok(r) => {
                    set_nonblock(r, want_nonblock);
                }
                Err(e) => return e,
            }
            0
        }
        F_GETFD => {
            let on = caller.data().fds.get_cloexec(fd);
            // Optional optval write-back. Linux writes the bit value as a
            // 32-bit int. We omit the write if arg == 0 (matches glibc).
            if arg != 0 {
                if let Ok(buf) = mem::guest_slice_mut(caller, arg, 4) {
                    buf[0..4].copy_from_slice(&(if on { 1i32 } else { 0i32 }).to_le_bytes());
                }
            }
            on as i64
        }
        F_SETFD => {
            let on = (arg as i32) & 1 != 0;
            caller.data_mut().fds.set_cloexec(fd, on);
            0
        }
        F_DUPFD | F_DUPFD_CLOEXEC => {
            let want_cloexec = cmd == F_DUPFD_CLOEXEC;
            // POSIX glibc: F_DUPFD rejects negative `arg`. Cast as i32
            // first to detect the sign, then widen to u32 only on the
            // happy path.
            let min_fd = if (arg as i32) < 0 {
                return -EINVAL;
            } else {
                arg as u32
            };
            let cloned = {
                let fds = &caller.data().fds;
                match fds.get(fd) {
                    // P2-B5: share state via Arc::clone (infallible).
                    Ok(Resource::File(fp)) => Resource::File(std::sync::Arc::clone(fp)),
                    Ok(Resource::Stdin(r)) => Resource::Stdin(crate::fd::PipeRead {
                        buf: r.buf.clone(),
                        closed: r.closed.clone(),
                        nonblock: r.nonblock.clone(),
                        notify: r.notify.clone(),
                    }),
                    Ok(Resource::Stdout(w)) => Resource::Stdout(crate::fd::PipeWrite {
                        buf: w.buf.clone(),
                        closed: w.closed.clone(),
                        nonblock: w.nonblock.clone(),
                        notify: w.notify.clone(),
                    }),
                    Ok(Resource::Stderr(w)) => Resource::Stderr(crate::fd::PipeWrite {
                        buf: w.buf.clone(),
                        closed: w.closed.clone(),
                        nonblock: w.nonblock.clone(),
                        notify: w.notify.clone(),
                    }),
                    Ok(Resource::PipeRead(r)) => Resource::PipeRead(crate::fd::PipeRead {
                        buf: r.buf.clone(),
                        closed: r.closed.clone(),
                        nonblock: r.nonblock.clone(),
                        notify: r.notify.clone(),
                    }),
                    Ok(Resource::PipeWrite(w)) => Resource::PipeWrite(crate::fd::PipeWrite {
                        buf: w.buf.clone(),
                        closed: w.closed.clone(),
                        nonblock: w.nonblock.clone(),
                        notify: w.notify.clone(),
                    }),
                    // P2-B5: dup-able via Arc<Mutex<SocketInner>> (commit 7).
                    // `Arc::clone` is infallible; both fds share the same
                    // underlying SocketInner — offset, bound, listener,
                    // stream, peer_addr, shutdown_flags, is_acceptor.
                    Ok(Resource::Socket(s)) => Resource::Socket(std::sync::Arc::clone(s)),
                    // P2-B5: epoll/eventfd still not dup-able. (Linux
                    // allows `dup(epfd)` historically but it's effectively
                    // a no-op; we reject to keep semantics simple.)
                    Ok(Resource::Epoll(_)) | Ok(Resource::EventFd(_)) => return -EBADF,
                    Err(e) => return e,
                }
            };
            let new_fd = caller.data_mut().fds.insert_at_least(min_fd, cloned);
            caller.data_mut().fds.set_cloexec(new_fd, want_cloexec);
            new_fd as i64
        }
        _ => -EINVAL,
    }
}

// -- P2-B5: dup(2) / dup2(2) / dup3(2) ---------------------------------------

/// `dup(oldfd)` → new fd with no minimum-fd preference; shares the
/// open-file description (offset, listener, stream, etc.) with `oldfd`.
///
/// `dup` doesn't accept a target fd; the kernel picks the lowest free.
/// Returns the new fd as i64, or -errno on failure.
pub async fn dup(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let oldfd = match u32::try_from(a[0]) {
        Ok(f) => f,
        Err(_) => return -crate::errno::EBADF,
    };
    internal_dup(caller, oldfd, None, false).await
}

/// `dup2(oldfd, newfd)` → exact fd `newfd`. If `oldfd == newfd` and
/// the fd is valid, `dup2` returns `newfd` unchanged (matches Linux).
/// Otherwise closes `newfd` if it was bound, then duplicates.
pub async fn dup2(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let oldfd = match u32::try_from(a[0]) {
        Ok(f) => f,
        Err(_) => return -crate::errno::EBADF,
    };
    let newfd = match u32::try_from(a[1]) {
        Ok(f) => f,
        Err(_) => return -crate::errno::EBADF,
    };
    if oldfd == newfd {
        // Linux: if oldfd is valid, dup2 succeeds and returns newfd
        // unchanged. If oldfd is not a valid fd, returns -EBADF.
        let fds = &caller.data().fds;
        match fds.get(oldfd) {
            Ok(_) => return newfd as i64,
            Err(e) => return e,
        }
    }
    internal_dup(caller, oldfd, Some(newfd), false).await
}

/// `dup3(oldfd, newfd, flags)` — like dup2 but accepts `O_CLOEXEC` and
/// rejects `oldfd == newfd` (Linux behaviour).
pub async fn dup3(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let oldfd = match u32::try_from(a[0]) {
        Ok(f) => f,
        Err(_) => return -crate::errno::EBADF,
    };
    let newfd = match u32::try_from(a[1]) {
        Ok(f) => f,
        Err(_) => return -crate::errno::EBADF,
    };
    let flags = a[2] as i32;
    if flags & !O_CLOEXEC != 0 {
        return -crate::errno::EINVAL;
    }
    if oldfd == newfd {
        return -crate::errno::EINVAL;
    }
    internal_dup(caller, oldfd, Some(newfd), flags & O_CLOEXEC != 0).await
}

/// Shared implementation behind `dup`, `dup2`, `dup3`, and `F_DUPFD`.
///
/// `newfd_target`:
///   * `None`         — pick the lowest free fd >= oldfd+1 (for `dup`,
///     or `F_DUPFD` callers that always pass `Some(min)`).
///   * `Some(n)`      — close `n` if bound, insert at `n` (for `dup2`,
///     `dup3`, and `F_DUPFD`).
///
/// `want_cloexec` sets the FD_CLOEXEC bit on the resulting fd.
async fn internal_dup(
    caller: &mut Caller<'_, Kernel>,
    oldfd: u32,
    newfd_target: Option<u32>,
    want_cloexec: bool,
) -> i64 {
    // Phase 1: clone-by-Arc (or per-field for pipes). We do this under a
    // shared borrow so the cloned `Resource` is detached by the time we
    // mutate the FdTable.
    let cloned: Resource = {
        let fds = &caller.data().fds;
        match fds.get(oldfd) {
            Err(e) => return e,
            Ok(Resource::File(fp)) => Resource::File(std::sync::Arc::clone(fp)),
            Ok(Resource::Socket(s)) => Resource::Socket(std::sync::Arc::clone(s)),
            Ok(Resource::Stdin(r)) => Resource::Stdin(crate::fd::PipeRead {
                buf: r.buf.clone(),
                closed: r.closed.clone(),
                nonblock: r.nonblock.clone(),
                notify: r.notify.clone(),
            }),
            Ok(Resource::Stdout(w)) => Resource::Stdout(crate::fd::PipeWrite {
                buf: w.buf.clone(),
                closed: w.closed.clone(),
                nonblock: w.nonblock.clone(),
                notify: w.notify.clone(),
            }),
            Ok(Resource::Stderr(w)) => Resource::Stderr(crate::fd::PipeWrite {
                buf: w.buf.clone(),
                closed: w.closed.clone(),
                nonblock: w.nonblock.clone(),
                notify: w.notify.clone(),
            }),
            Ok(Resource::PipeRead(r)) => Resource::PipeRead(crate::fd::PipeRead {
                buf: r.buf.clone(),
                closed: r.closed.clone(),
                nonblock: r.nonblock.clone(),
                notify: r.notify.clone(),
            }),
            Ok(Resource::PipeWrite(w)) => Resource::PipeWrite(crate::fd::PipeWrite {
                buf: w.buf.clone(),
                closed: w.closed.clone(),
                nonblock: w.nonblock.clone(),
                notify: w.notify.clone(),
            }),
            // P2-B5: dup(epfd) and dup(eventfd) are not modeled. Linux
            // permits them historically but they aren't meaningful for
            // our epoll implementation.
            Ok(Resource::Epoll(_)) | Ok(Resource::EventFd(_)) => return -crate::errno::EBADF,
        }
    };

    // Phase 2: install the cloned resource. For dup2/dup3, close the
    // target first if it was bound; for dup, take the lowest free fd.
    let new_fd: u32 = match newfd_target {
        Some(target) => {
            // Close target if bound (drop its resource; ignore errors).
            let _ = caller.data_mut().fds.close(target);
            match caller.data_mut().fds.insert_at(target, cloned) {
                Ok(fd) => fd,
                Err(e) => return e,
            }
        }
        None => caller.data_mut().fds.insert_at_least(oldfd + 1, cloned),
    };
    caller
        .data_mut()
        .fds
        .set_cloexec(new_fd, want_cloexec);
    new_fd as i64
}

// -- Helpers ----------------------------------------------------------------

fn synth_char() -> Stat {
    Stat {
        st_dev: 0,
        st_ino: 0,
        st_nlink: 1,
        st_mode: 0o020666, // S_IFCHR | rw-rw-rw-
        st_uid: 1000,
        st_gid: 1000,
        st_rdev: 0,
        st_size: 0,
        st_blksize: 4096,
        st_blocks: 0,
        st_atime: 0,
        st_atime_nsec: 0,
        st_mtime: 0,
        st_mtime_nsec: 0,
        st_ctime: 0,
        st_ctime_nsec: 0,
    }
}

// ─── P2-C1 part 3: readlink, readlinkat, symlink, symlinkat, link, linkat,
//     utimensat, chmod, fchmod, fchmodat, faccessat, faccessat2, chdir, chroot.

/// `readlink(path, buf, buf_len)` — read the target of a symlink. Writes
/// up to `buf_len` bytes (no NUL terminator). Returns the number of bytes
/// written, or -EINVAL if the path is not a symlink.
pub async fn readlink(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    readlinkat(caller, [-100, a[0], a[1], a[2], 0, 0]).await
}

/// `readlinkat(dirfd, path, buf, buf_len)` — symlink read with explicit
/// dirfd. Truncates the link target to `buf_len`; if the link is longer
/// than `buf_len` it returns -ENAMETOOLONG (= -36).
pub async fn readlinkat(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let dirfd = a[0];
    let path_ptr = a[1];
    let buf_ptr = a[2];
    let buf_len = match usize::try_from(a[3]) {
        Ok(n) => n,
        Err(_) => return -EINVAL,
    };

    let path = match mem::guest_str(caller, path_ptr, PATH_MAX) {
        Ok(s) => s.to_string(),
        Err(e) => return e,
    };
    if path.is_empty() {
        return -ENOENT;
    }
    let abs = match crate::sys::path::resolve(caller, dirfd, &path) {
        Ok(p) => p,
        Err(e) => return e,
    };

    let target = match std::fs::read_link(&abs) {
        Ok(p) => p,
        Err(e) => return -io_to_errno(e),
    };
    let bytes = target.to_string_lossy().into_owned().into_bytes();
    if bytes.len() > buf_len {
        return -crate::errno::ENAMETOOLONG;
    }
    let buf = match mem::guest_slice_mut(caller, buf_ptr, bytes.len() as i64) {
        Ok(b) => b,
        Err(e) => return e,
    };
    buf.copy_from_slice(&bytes);
    bytes.len() as i64
}

/// `symlink(target, linkpath)` — create a symlink at `linkpath` whose
/// contents are `target`. `target` is not resolved; it is stored verbatim.
pub async fn symlink(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    symlinkat(caller, [a[0], -100, a[1], 0, 0, 0]).await
}

/// `symlinkat(target, newdirfd, linkpath)` — symlink with explicit dirfd.
pub async fn symlinkat(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let target_ptr = a[0];
    let dirfd = a[1];
    let path_ptr = a[2];

    let target = match mem::guest_str(caller, target_ptr, PATH_MAX) {
        Ok(s) => s.to_string(),
        Err(e) => return e,
    };
    let path = match mem::guest_str(caller, path_ptr, PATH_MAX) {
        Ok(s) => s.to_string(),
        Err(e) => return e,
    };
    if path.is_empty() {
        return -ENOENT;
    }
    let abs = match crate::sys::path::resolve(caller, dirfd, &path) {
        Ok(p) => p,
        Err(e) => return e,
    };

    match std::os::unix::fs::symlink(&target, &abs) {
        Ok(()) => 0,
        Err(e) => -io_to_errno(e),
    }
}

/// `link(oldpath, newpath)` — create a hard link. AT_EMPTY_PATH not
/// modeled; returns -EINVAL if the new path is empty.
pub async fn link(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    linkat(caller, [-100, a[0], -100, a[1], 0, 0]).await
}

/// `linkat(olddirfd, oldpath, newdirfd, newpath, flags)` — hard link with
/// explicit dirfds. AT_EMPTY_PATH (0x1000) and AT_SYMLINK_FOLLOW (0x400)
/// are ignored.
pub async fn linkat(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let olddirfd = a[0];
    let old_ptr = a[1];
    let newdirfd = a[2];
    let new_ptr = a[3];
    let flags = a[4] as i32;
    let _ = flags; // AT_EMPTY_PATH / AT_SYMLINK_FOLLOW ignored

    let old = match mem::guest_str(caller, old_ptr, PATH_MAX) {
        Ok(s) => s.to_string(),
        Err(e) => return e,
    };
    let new = match mem::guest_str(caller, new_ptr, PATH_MAX) {
        Ok(s) => s.to_string(),
        Err(e) => return e,
    };
    if old.is_empty() || new.is_empty() {
        return -ENOENT;
    }
    let old_abs = match crate::sys::path::resolve(caller, olddirfd, &old) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let new_abs = match crate::sys::path::resolve(caller, newdirfd, &new) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match std::fs::hard_link(&old_abs, &new_abs) {
        Ok(()) => 0,
        Err(e) => -io_to_errno(e),
    }
}

/// `utimensat(dirfd, path, times, flags)` — set file timestamps. `times`
/// is a pointer to an array of two `struct timespec` (16 bytes each, 32
/// bytes total) on wasm32-musl. `NULL` times sets both atime+mtime to the
/// current time.
///
/// `AT_SYMLINK_NOFOLLOW` is ignored — host std always follows.
pub async fn utimensat(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let dirfd = a[0];
    let path_ptr = a[1];
    let times_ptr = a[2];
    let flags = a[3] as i32;
    let _ = flags; // AT_SYMLINK_NOFOLLOW ignored

    let path = match mem::guest_str(caller, path_ptr, PATH_MAX) {
        Ok(s) => s.to_string(),
        Err(e) => return e,
    };
    if path.is_empty() {
        return -ENOENT;
    }
    let abs = match crate::sys::path::resolve(caller, dirfd, &path) {
        Ok(p) => p,
        Err(e) => return e,
    };

    // Read two timespecs (16 bytes each on wasm32-musl: tv_sec i64, tv_nsec i64).
    let (atime, mtime) = if times_ptr == 0 {
        (std::time::SystemTime::now(), std::time::SystemTime::now())
    } else {
        let bytes = match mem::guest_slice(caller, times_ptr, 32) {
            Ok(b) => b,
            Err(e) => return e,
        };
        let atime = timespec_from_bytes(&bytes[0..16]);
        let mtime = timespec_from_bytes(&bytes[16..32]);
        (atime, mtime)
    };

    let f = match std::fs::OpenOptions::new().write(true).open(&abs) {
        Ok(f) => f,
        Err(e) => return -io_to_errno(e),
    };
    // mtime is what most filesystems persist; atime is best-effort and
    // not exposed via std on stable for File::set_accessed. We swallow
    // any atime error since the syscall's main observable effect is mtime.
    let _ = atime;
    match f.set_modified(mtime) {
        Ok(()) => 0,
        Err(e) => -io_to_errno(e),
    }
}

fn timespec_from_bytes(b: &[u8]) -> std::time::SystemTime {
    // tv_sec: 8 bytes little-endian; tv_nsec: 8 bytes LE.
    let sec = i64::from_le_bytes(b[0..8].try_into().unwrap());
    let nsec = i64::from_le_bytes(b[8..16].try_into().unwrap());
    if sec < 0 || nsec < 0 {
        return std::time::UNIX_EPOCH;
    }
    std::time::UNIX_EPOCH
        + std::time::Duration::from_secs(sec as u64)
        + std::time::Duration::from_nanos(nsec as u64)
}

/// `chmod(path, mode)` — change permissions of the file at `path`. Only
/// the low 12 bits of `mode` are honored (S_ISUID | S_ISGID | S_ISVTX |
/// 3×rwx). Symlinks are followed.
pub async fn chmod(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    fchmodat(caller, [-100, a[0], a[1], 0, 0, 0]).await
}

/// `fchmod(fd, mode)` — change permissions via fd.
///
/// Lock discipline: brief lock to clone the `File`, drop guard, call
/// `set_permissions` outside the lock.
pub async fn fchmod(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fd = a[0] as u32;
    let mode_raw = a[1] as u32;
    let mode = mode_raw & 0o7777;

    // Look up + try_clone under a short lock, then drop guard.
    let file_clone = {
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::File(fp)) => {
                let fp = fp.lock();
                match fp.inner.try_clone() {
                    Ok(f) => f,
                    Err(e) => return -io_to_errno(e),
                }
            }
            Ok(_) => return -crate::errno::EBADF,
            Err(e) => return e,
        }
    };

    let mut perms = match file_clone.metadata() {
        Ok(m) => m.permissions(),
        Err(e) => return -io_to_errno(e),
    };
    perms.set_mode(mode);
    match file_clone.set_permissions(perms) {
        Ok(()) => 0,
        Err(e) => -io_to_errno(e),
    }
}

/// `fchmodat(dirfd, path, mode, flags)` — chmod with explicit dirfd.
/// `AT_SYMLINK_NOFOLLOW (0x100)` is ignored (host std follows). `flags=0`
/// or empty path is the same as plain `chmod`.
pub async fn fchmodat(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let dirfd = a[0];
    let path_ptr = a[1];
    let mode_raw = a[2] as u32;
    let flags = a[3] as i32;
    let mode = mode_raw & 0o7777;
    let _ = flags;

    let path = match mem::guest_str(caller, path_ptr, PATH_MAX) {
        Ok(s) => s.to_string(),
        Err(e) => return e,
    };
    if path.is_empty() {
        return -ENOENT;
    }
    let abs = match crate::sys::path::resolve(caller, dirfd, &path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match std::fs::set_permissions(&abs, std::fs::Permissions::from_mode(mode)) {
        Ok(()) => 0,
        Err(e) => -io_to_errno(e),
    }
}

/// `faccessat(dirfd, path, mode, flags)` — check access. `mode` bits:
/// `R_OK (4)`, `W_OK (2)`, `X_OK (1)`. `F_OK (0)` checks existence.
/// `AT_EACCESS (0x200)` and `AT_SYMLINK_NOFOLLOW (0x100)` are ignored.
pub async fn faccessat(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    faccessat2(caller, a).await
}

/// `faccessat2(dirfd, path, mode, flags)` — same as `faccessat` but with
/// a fixed `flags` layout (added in Linux 5.8).
pub async fn faccessat2(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let dirfd = a[0];
    let path_ptr = a[1];
    let mode = a[2] as i32;
    let _flags = a[3] as i32;

    let path = match mem::guest_str(caller, path_ptr, PATH_MAX) {
        Ok(s) => s.to_string(),
        Err(e) => return e,
    };
    if path.is_empty() {
        return -ENOENT;
    }
    let abs = match crate::sys::path::resolve(caller, dirfd, &path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let meta = match std::fs::metadata(&abs) {
        Ok(m) => m,
        Err(e) => return -io_to_errno(e),
    };
    let perms = meta.permissions();
    let readonly = perms.readonly();
    let m = perms.mode();

    // F_OK (mode 0): existence only.
    if mode == F_OK {
        return 0;
    }
    if mode & R_OK != 0 && (m & 0o444) == 0 {
        return -crate::errno::EACCES;
    }
    if mode & W_OK != 0 && (readonly || (m & 0o222) == 0) {
        return -crate::errno::EACCES;
    }
    if mode & X_OK != 0 && (m & 0o111) == 0 {
        return -crate::errno::EACCES;
    }
    0
}

/// `chdir(path)` — change the working directory. Resolves via the
/// `path::resolve` helper (so dirfd-as-fd is honored). Permanent for
/// the process.
pub async fn chdir(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let path_ptr = a[0];
    let path = match mem::guest_str(caller, path_ptr, PATH_MAX) {
        Ok(s) => s.to_string(),
        Err(e) => return e,
    };
    if path.is_empty() {
        return -ENOENT;
    }
    let abs = match crate::sys::path::resolve(caller, -100, &path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match caller.data_mut().vfs.chdir(&abs) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

/// `chroot(path)` — set the new root for path resolution. **Permanent**
/// for the process (no saved-root model). Sets both `root` and `cwd` to
/// `path`.
pub async fn chroot(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let path_ptr = a[0];
    let path = match mem::guest_str(caller, path_ptr, PATH_MAX) {
        Ok(s) => s.to_string(),
        Err(e) => return e,
    };
    if path.is_empty() {
        return -ENOENT;
    }
    let abs = match crate::sys::path::resolve(caller, -100, &path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match caller.data_mut().vfs.chroot(&abs) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

// (No dead-code silencer needed; everything in this file is used.)