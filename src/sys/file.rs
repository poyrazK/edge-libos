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
use std::path::PathBuf;

use wasmtime::Caller;

use crate::errno::{EACCES, EBADF, EFAULT, EINVAL, ENOENT, ERANGE, ESPIPE};
use crate::fd::Resource;
use crate::kernel::Kernel;
use crate::mem;
use crate::sys::eventfd;
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
pub const NR_GETDENTS64: u32 = 217;
pub const NR_PIPE: u32 = 22;
pub const NR_PIPE2: u32 = 293;
pub const NR_FCNTL: u32 = 72;
pub const NR_GETCWD: u32 = 79;
pub const NR_READV: u32 = 19;
pub const NR_WRITEV: u32 = 20;

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
                let _ = fp.inner.seek(SeekFrom::Start(fp.pos));
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
                let val = eventfd::eventfd_read(e);
                let bytes = val.to_ne_bytes();
                let buf = match mem::guest_slice_mut(caller, buf_ptr, 8) {
                    Ok(b) => b,
                    Err(e) => return e,
                };
                buf[..8].copy_from_slice(&bytes);
                return 8;
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
            Resource::File(fp) => match fp.inner.write(&bytes) {
                Ok(n) => {
                    fp.pos += n as u64;
                    n
                }
                Err(_) => return -crate::errno::EIO,
            },
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

    let (root, cwd) = {
        let kern = caller.data();
        (kern.vfs.root.clone(), kern.vfs.cwd.clone())
    };
    let vfs = Vfs { root, cwd };
    let abs = match vfs.resolve_path(dirfd, &path) {
        Ok(p) => p,
        Err(e) => return e,
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
    let fd = caller.data_mut().fds.insert(Resource::File(fp));
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
            Ok(Resource::File(fp)) => match fp.inner.metadata() {
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

    let (root, cwd) = {
        let kern = caller.data();
        (kern.vfs.root.clone(), kern.vfs.cwd.clone())
    };
    let vfs = Vfs { root, cwd };
    let abs = match vfs.resolve_path(dirfd, &path) {
        Ok(p) => p,
        Err(e) => return e,
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
            Ok(Resource::File(fp)) => fp.is_dir,
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
            Ok(Resource::File(fp)) => match fp.path.clone() {
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
            Ok(Resource::File(fp)) => fp.dir_cache.is_none(),
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
            fp.dir_cache = Some(cached);
        }
    }

    // Slice the cached dirent64 buffer at fp.pos.
    let (slice, new_pos): (Vec<u8>, u64) = {
        let fds = &mut caller.data_mut().fds;
        let fp = match fds.get_mut(fd) {
            Ok(Resource::File(fp)) => fp,
            _ => return -crate::errno::EBADF,
        };
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
                    let nb = s.nonblock.load(std::sync::atomic::Ordering::Relaxed);
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
                Ok(Resource::Stdin(r)) | Ok(Resource::PipeRead(r)) => {
                    r.nonblock.store(want_nonblock, std::sync::atomic::Ordering::Relaxed);
                }
                Ok(Resource::Stdout(w)) | Ok(Resource::Stderr(w)) | Ok(Resource::PipeWrite(w)) => {
                    w.nonblock.store(want_nonblock, std::sync::atomic::Ordering::Relaxed);
                }
                Ok(Resource::Socket(s)) => {
                    s.nonblock.store(want_nonblock, std::sync::atomic::Ordering::Relaxed);
                }
                Ok(Resource::File(_)) => {
                    // Real files have no nonblock semantics on the host
                    // (they're blocking I/O on the std::fs::File). Accept
                    // the call and return 0.
                }
                // P1-7: epoll/eventfd ignore F_SETFL; F_GETFL above already
                // returns O_RDWR for them.
                Ok(Resource::Epoll(_)) | Ok(Resource::EventFd(_)) => {}
                Err(e) => return e,
            }
            0
        }
        F_GETFD => 0,
        F_SETFD => {
            let _ = arg;
            0
        }
        F_DUPFD | F_DUPFD_CLOEXEC => {
            let cloned = {
                let fds = &caller.data().fds;
                match fds.get(fd) {
                    Ok(Resource::File(fp)) => match fp.try_clone() {
                        Ok(c) => Resource::File(c),
                        Err(_) => return -EBADF,
                    },
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
                    // P1-1: socket fds are not yet duplicable; P1-7's epoll
                    // layer is the right place to model shared fds. For now
                    // dup on a socket returns -EBADF (matches Linux: dup on
                    // a socket without SO_ACCEPTCONN semantics is a no-op).
                    Ok(Resource::Socket(_)) => return -EBADF,
                    // P1-7: epoll and eventfd are not dup-able. Linux
                    // allows `dup(epfd)` historically but it's effectively
                    // a no-op; for P1 we just reject.
                    Ok(Resource::Epoll(_)) | Ok(Resource::EventFd(_)) => return -EBADF,
                    Err(e) => return e,
                }
            };
            caller.data_mut().fds.insert(cloned) as i64
        }
        _ => -EINVAL,
    }
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

// (No dead-code silencer needed; everything in this file is used.)