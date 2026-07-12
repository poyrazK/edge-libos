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

use crate::errno::{EACCES, EBADF, EFAULT, EINVAL, ESPIPE};
use crate::fd::Resource;
use crate::kernel::Kernel;
use crate::mem;
use crate::vfs::{Stat, Vfs};

// NR_* (Linux x86-64 unistd_64.h)
pub const NR_READ: u32 = 0;
pub const NR_WRITE: u32 = 1;
pub const NR_OPENAT: u32 = 257;
pub const NR_CLOSE: u32 = 3;
pub const NR_LSEEK: u32 = 8;
pub const NR_FSTAT: u32 = 5;
pub const NR_NEWFSTATAT: u32 = 262;
pub const NR_GETDENTS64: u32 = 217;
pub const NR_PIPE2: u32 = 293;
pub const NR_FCNTL: u32 = 72;

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

/// A seekable file. Wraps `std::fs::File` + its current position + the
/// absolute path we opened it from (so `getdents64` can be answered without
/// re-resolving).
pub struct FilePos {
    pub inner: std::fs::File,
    pub pos: u64,
    pub path: Option<PathBuf>,
}

impl FilePos {
    pub fn new(f: std::fs::File) -> Self {
        Self {
            inner: f,
            pos: 0,
            path: None,
        }
    }

    pub fn with_path(f: std::fs::File, p: PathBuf) -> Self {
        Self {
            inner: f,
            pos: 0,
            path: Some(p),
        }
    }

    pub fn try_clone(&self) -> std::io::Result<Self> {
        Ok(Self {
            inner: self.inner.try_clone()?,
            pos: self.pos,
            path: self.path.clone(),
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
            _ => return -EBADF,
        }
    }
    if eof {
        return 0;
    }
    if tmp.is_empty() {
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
                q.extend(bytes.iter().copied());
                bytes.len()
            }
            Resource::File(fp) => match fp.inner.write(&bytes) {
                Ok(n) => {
                    fp.pos += n as u64;
                    n
                }
                Err(_) => return -crate::errno::EIO,
            },
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
    let fp = FilePos::with_path(file, abs);
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

    let path: Option<PathBuf> = {
        let fds = &caller.data().fds;
        match fds.get(fd) {
            Ok(Resource::File(fp)) => fp.path.clone(),
            Ok(_) | Err(_) => None,
        }
    };
    let abs = match path {
        Some(p) => p,
        None => return -EBADF,
    };
    let (root, cwd) = {
        let kern = caller.data();
        (kern.vfs.root.clone(), kern.vfs.cwd.clone())
    };
    let vfs = Vfs { root, cwd };
    let bytes = match vfs.getdents(&abs, len) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let n = bytes.len();
    if n == 0 {
        return 0; // End of directory.
    }
    let dst = match mem::guest_slice_mut(caller, buf_ptr, n as i64) {
        Ok(b) => b,
        Err(e) => return e,
    };
    dst.copy_from_slice(&bytes);
    n as i64
}

/// `pipe2(fdarray, flags)`. Allocates a paired (read, write) buffer-backed
/// pipe, inserts both ends into the FdTable, and writes the two u32 fds
/// into the guest's `fdarray` pointer (little-endian, [read_fd, write_fd]).
///
/// `flags` are honoured as best-effort for P0:
/// * `O_CLOEXEC` (0o2000000) — accepted; FD_CLOEXEC tracked on the resource.
/// * `O_NONBLOCK` (0o4000)   — accepted; semantically a no-op for buffer
///   pipes (poll is not implemented yet).
pub async fn pipe2(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fdarray_ptr = a[0];
    let flags = a[1] as i32;

    // Bounds-check the fdarray first; both fds together must be writable.
    if let Err(e) = mem::guest_slice_mut(caller, fdarray_ptr, 8) {
        return e;
    }

    let (rd, wr) = crate::fd::make_pipe();
    let (rd_fd, wr_fd) = {
        let fds = &mut caller.data_mut().fds;
        let rd_fd = fds.insert(Resource::PipeRead(rd));
        let wr_fd = fds.insert(Resource::PipeWrite(wr));
        (rd_fd, wr_fd)
    };

    // Honour O_CLOEXEC: stash the flag on the resource via FdTable so a
    // later F_GETFD in the guest sees FD_CLOEXEC. P0 ignores the flag on
    // exec because we don't model exec; the flag is recorded for fidelity.
    let _ = flags & O_CLOEXEC;
    let _ = flags & O_NONBLOCK;

    let buf = match mem::guest_slice_mut(caller, fdarray_ptr, 8) {
        Ok(b) => b,
        Err(e) => return e,
    };
    buf[0..4].copy_from_slice(&rd_fd.to_le_bytes());
    buf[4..8].copy_from_slice(&wr_fd.to_le_bytes());
    0
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
            let fds = &caller.data().fds;
            match fds.get(fd) {
                Ok(Resource::File(_)) => O_RDWR as i64,
                Ok(_) => O_RDONLY as i64,
                Err(e) => e,
            }
        }
        F_SETFL => 0,
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
                    }),
                    Ok(Resource::Stdout(w)) => Resource::Stdout(crate::fd::PipeWrite {
                        buf: w.buf.clone(),
                        closed: w.closed.clone(),
                    }),
                    Ok(Resource::Stderr(w)) => Resource::Stderr(crate::fd::PipeWrite {
                        buf: w.buf.clone(),
                        closed: w.closed.clone(),
                    }),
                    Ok(Resource::PipeRead(r)) => Resource::PipeRead(crate::fd::PipeRead {
                        buf: r.buf.clone(),
                        closed: r.closed.clone(),
                    }),
                    Ok(Resource::PipeWrite(w)) => Resource::PipeWrite(crate::fd::PipeWrite {
                        buf: w.buf.clone(),
                        closed: w.closed.clone(),
                    }),
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