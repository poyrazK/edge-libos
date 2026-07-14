//! VFS / openat / close / lseek / fstat / newfstatat / getdents64 conformance.

mod common;

use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;

use edge_libos::Kernel;

/// Tiny self-cleaning tmpdir for VFS tests. On drop best-effort `rm -rf`.
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("edge-libos-vfs-test-{pid}-{id}"));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio current_thread runtime");
    rt.block_on(f)
}

// WAT modules ---------------------------------------------------------------

/// openat(AT_FDCWD, "/<preopen>/hello.txt", O_RDONLY, 0) → fd
const OPENAT_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (param $path i64) (result i64)
        (call $syscall
          (i64.const 257)            ;; NR_OPENAT
          (i64.const -100)           ;; AT_FDCWD
          (local.get $path)
          (i64.const 0)              ;; O_RDONLY
          (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#;

/// close(fd) → 0
const CLOSE_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (param $fd i64) (result i64)
        (call $syscall
          (i64.const 3)              ;; NR_CLOSE
          (local.get $fd)
          (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#;

/// fstat(fd, statbuf@4096) → 0
const FSTAT_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (param $fd i64) (result i64)
        (call $syscall
          (i64.const 5)              ;; NR_FSTAT
          (local.get $fd)
          (i64.const 4096)
          (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#;

/// lseek(fd, offset, SEEK_SET) → new offset
const LSEEK_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (param $fd i64) (param $off i64) (result i64)
        (call $syscall
          (i64.const 8)              ;; NR_LSEEK
          (local.get $fd)
          (local.get $off)
          (i64.const 0)              ;; SEEK_SET
          (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#;

/// read(fd, buf@4096, 32) → bytes read
const READ_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (param $fd i64) (result i64)
        (call $syscall
          (i64.const 0)              ;; NR_READ
          (local.get $fd)
          (i64.const 4096)
          (i64.const 32)
          (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#;

/// getdents64(fd, buf@4096, 1024) → bytes
const GETDENTS_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (param $fd i64) (result i64)
        (call $syscall
          (i64.const 217)            ;; NR_GETDENTS64
          (local.get $fd)
          (i64.const 4096)
          (i64.const 1024)
          (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#;

/// write(fd, buf@4096, len) → bytes written
const WRITE_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (param $fd i64) (param $len i64) (result i64)
        (call $syscall
          (i64.const 1)              ;; NR_WRITE
          (local.get $fd)
          (i64.const 4096)
          (local.get $len)
          (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#;

/// pipe2(fdarray@4096, flags) → 0. Writes [rd_fd, wr_fd] as i32 little-endian.
const PIPE2_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (result i64)
        (call $syscall
          (i64.const 293)            ;; NR_PIPE2
          (i64.const 4096)           ;; fdarray pointer
          (i64.const 0)              ;; flags (no CLOEXEC / NONBLOCK)
          (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#;

/// Legacy `pipe(fdarray)` — shim around NR_PIPE2 with flags=0.
const PIPE_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (result i64)
        (call $syscall
          (i64.const 22)             ;; NR_PIPE
          (i64.const 4096)
          (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#;

/// Legacy `open(path, flags, mode)` — shim around NR_OPENAT(AT_FDCWD, ...).
/// Path lives at offset 4096 in the data segment.
const OPEN_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (data (i32.const 4096) "/file\00")
      (func (export "go") (result i64)
        (call $syscall
          (i64.const 2)              ;; NR_OPEN
          (i64.const 4096)           ;; path
          (i64.const 0)              ;; O_RDONLY
          (i64.const 0)              ;; mode
          (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#;

/// `stat(path, statbuf)` — shim around NR_NEWFSTATAT. statbuf at 8192.
const STAT_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (data (i32.const 4096) "/file\00")
      (func (export "go") (result i64)
        (call $syscall
          (i64.const 4)              ;; NR_STAT
          (i64.const 4096)           ;; path
          (i64.const 8192)           ;; statbuf
          (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#;

/// `lstat(path, statbuf)` — same shape as stat but NR 6.
const LSTAT_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (data (i32.const 4096) "/file\00")
      (func (export "go") (result i64)
        (call $syscall
          (i64.const 6)              ;; NR_LSTAT
          (i64.const 4096)
          (i64.const 8192)
          (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#;

/// `getcwd(buf, size)` — buf at 8192, size = 4096.
const GETCWD_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (param $size i64) (result i64)
        (call $syscall
          (i64.const 79)             ;; NR_GETCWD
          (i64.const 8192)           ;; buf
          (local.get $size)
          (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#;

/// `readv(fd, iov, iovcnt)` — iov at 4096, 2 entries pointing to 12288 and 16384.
/// Both buffers are pre-zeroed by `(memory (data ...))`.
const READV_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      ;; iov[0] = {base=12288, len=3}; iov[1] = {base=16384, len=3}
      (data (i32.const 4096)
        "\00\30\00\00"   ;; base = 12288
        "\03\00\00\00"   ;; len  = 3
        "\00\40\00\00"   ;; base = 16384
        "\03\00\00\00")  ;; len  = 3
      (func (export "go") (param $fd i64) (result i64)
        (call $syscall
          (i64.const 19)             ;; NR_READV
          (local.get $fd)
          (i64.const 4096)           ;; iov
          (i64.const 2)              ;; iovcnt
          (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#;

/// `writev(stdout, iov, iovcnt)` — iov at 4096, 2 entries pointing to 12288 ("foo") and 16384 ("bar").
const WRITEV_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (data (i32.const 4096)
        "\00\30\00\00"
        "\03\00\00\00"
        "\00\40\00\00"
        "\03\00\00\00")
      (data (i32.const 12288) "foo")
      (data (i32.const 16384) "bar")
      (func (export "go") (param $fd i64) (result i64)
        (call $syscall
          (i64.const 20)             ;; NR_WRITEV
          (local.get $fd)
          (i64.const 4096)
          (i64.const 2)
          (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#;

/// `readv(fd, iov, 1)` with a single zero-length iov entry → returns 0.
const READV_ZERO_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      ;; iov[0] = {base=12288, len=0}
      (data (i32.const 4096)
        "\00\30\00\00"   ;; base = 12288
        "\00\00\00\00")  ;; len  = 0
      (func (export "go") (param $fd i64) (result i64)
        (call $syscall
          (i64.const 19)             ;; NR_READV
          (local.get $fd)
          (i64.const 4096)           ;; iov
          (i64.const 1)              ;; iovcnt
          (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#;

/// Helpers -----------------------------------------------------------------

/// Build a WAT with a literal byte payload written at offset 4096.
fn wat_with_payload(payload: &[u8]) -> String {
    let mut s = String::new();
    for &b in payload {
        if (0x20..0x7f).contains(&b) && b != b'"' && b != b'\\' {
            s.push(b as char);
        } else {
            s.push_str(&format!("\\{b:02x}"));
        }
    }
    format!(
        r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (data (i32.const 4096) "{s}")
      (func (export "go") (param $fd i64) (param $len i64) (result i64)
        (call $syscall
          (i64.const 1)
          (local.get $fd)
          (i64.const 4096)
          (local.get $len)
          (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#
    )
}

/// Run a no-arg-extra wasm function and return the result + store.
// (unused — tests instantiate their own stores inline)

/// Place a NUL-terminated copy of `s` at offset 4096, return the ptr (4096).
fn write_cstr(store: &mut wasmtime::Store<Kernel>, s: &str) -> i64 {
    let mem = *store.data().memory().unwrap();
    let bytes = s.as_bytes();
    {
        let mut data = mem.data_mut(store);
        data[4096..4096 + bytes.len()].copy_from_slice(bytes);
        data[4096 + bytes.len()] = 0;
    }
    4096
}

// Tests --------------------------------------------------------------------

#[test]
fn openat_existing_file_returns_nonzero_fd() -> Result<()> {
    let d = TmpDir::new();
    File::create(d.0.join("hello.txt")).unwrap();

    let (engine, linker) = common::engine_and_linker()?;
    let kernel = common::kernel_with_preopen(&d.0);
    let module = common::compile_wat(&engine, OPENAT_WAT)?;
    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, kernel);
        let instance = linker.instantiate_async(&mut store, &module).await?;
        if let Some(mem) = instance.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let path_ptr = write_cstr(&mut store, "/hello.txt");
        let f = instance.get_typed_func::<(i64,), i64>(&mut store, "go")?;
        let r = f.call_async(&mut store, (path_ptr,)).await?;
        Ok::<_, anyhow::Error>(r)
    })?;
    assert!(ret >= 3, "openat must return a new fd (>=3), got {ret}");
    Ok(())
}

#[test]
fn openat_missing_file_returns_enoent() -> Result<()> {
    let d = TmpDir::new();
    let (engine, linker) = common::engine_and_linker()?;
    let kernel = common::kernel_with_preopen(&d.0);
    let module = common::compile_wat(&engine, OPENAT_WAT)?;
    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, kernel);
        let instance = linker.instantiate_async(&mut store, &module).await?;
        if let Some(mem) = instance.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let path_ptr = write_cstr(&mut store, "/nope.txt");
        let f = instance.get_typed_func::<(i64,), i64>(&mut store, "go")?;
        let r = f.call_async(&mut store, (path_ptr,)).await?;
        Ok::<_, anyhow::Error>(r)
    })?;
    assert_eq!(ret, -edge_libos::errno::ENOENT);
    Ok(())
}

#[test]
fn close_then_read_returns_ebadf() -> Result<()> {
    let d = TmpDir::new();
    File::create(d.0.join("hello.txt")).unwrap();

    let (engine, linker) = common::engine_and_linker()?;
    let kernel = common::kernel_with_preopen(&d.0);
    let open_mod = common::compile_wat(&engine, OPENAT_WAT)?;
    let close_mod = common::compile_wat(&engine, CLOSE_WAT)?;
    let read_mod = common::compile_wat(&engine, READ_WAT)?;

    let (fd, ret_after_close) = block_on(async {
        let mut store = edge_libos::build_store(&engine, kernel);
        let open_inst = linker.instantiate_async(&mut store, &open_mod).await?;
        if let Some(mem) = open_inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let path_ptr = write_cstr(&mut store, "/hello.txt");
        let of = open_inst.get_typed_func::<(i64,), i64>(&mut store, "go")?;
        let fd = of.call_async(&mut store, (path_ptr,)).await?;
        assert!(fd >= 3);

        let close_inst = linker.instantiate_async(&mut store, &close_mod).await?;
        let cf = close_inst.get_typed_func::<(i64,), i64>(&mut store, "go")?;
        let _ = cf.call_async(&mut store, (fd,)).await?;

        let read_inst = linker.instantiate_async(&mut store, &read_mod).await?;
        let rf = read_inst.get_typed_func::<(i64,), i64>(&mut store, "go")?;
        let r = rf.call_async(&mut store, (fd,)).await?;
        Ok::<_, anyhow::Error>((fd, r))
    })?;
    assert_eq!(ret_after_close, -edge_libos::errno::EBADF, "fd {fd}");
    Ok(())
}

#[test]
fn fstat_writes_sensible_size() -> Result<()> {
    let d = TmpDir::new();
    {
        let mut f = File::create(d.0.join("data.bin")).unwrap();
        f.write_all(b"hello world").unwrap(); // 11 bytes
    }

    let (engine, linker) = common::engine_and_linker()?;
    let kernel = common::kernel_with_preopen(&d.0);
    let open_mod = common::compile_wat(&engine, OPENAT_WAT)?;
    let fstat_mod = common::compile_wat(&engine, FSTAT_WAT)?;

    let stat_bytes = block_on(async {
        let mut store = edge_libos::build_store(&engine, kernel);
        let open_inst = linker.instantiate_async(&mut store, &open_mod).await?;
        if let Some(mem) = open_inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let path_ptr = write_cstr(&mut store, "/data.bin");
        let of = open_inst.get_typed_func::<(i64,), i64>(&mut store, "go")?;
        let fd = of.call_async(&mut store, (path_ptr,)).await?;

        let fstat_inst = linker.instantiate_async(&mut store, &fstat_mod).await?;
        let ff = fstat_inst.get_typed_func::<(i64,), i64>(&mut store, "go")?;
        let _ = ff.call_async(&mut store, (fd,)).await?;

        // Read back the bytes at offset 4096.
        let mem = *store.data().memory().unwrap();
        let data = mem.data(&store);
        let bytes: Vec<u8> = data[4096..4096 + 120].to_vec();
        Ok::<_, anyhow::Error>(bytes)
    })?;
    // st_size is at offset 48 in our layout.
    let size = i64::from_le_bytes(stat_bytes[48..56].try_into().unwrap());
    assert_eq!(size, 11, "stat st_size should report 11");
    Ok(())
}

#[test]
fn lseek_seek_set_updates_position() -> Result<()> {
    let d = TmpDir::new();
    {
        let mut f = File::create(d.0.join("seek.bin")).unwrap();
        f.write_all(b"0123456789").unwrap(); // 10 bytes
    }

    let (engine, linker) = common::engine_and_linker()?;
    let kernel = common::kernel_with_preopen(&d.0);
    let open_mod = common::compile_wat(&engine, OPENAT_WAT)?;
    let lseek_mod = common::compile_wat(&engine, LSEEK_WAT)?;

    let new_off = block_on(async {
        let mut store = edge_libos::build_store(&engine, kernel);
        let open_inst = linker.instantiate_async(&mut store, &open_mod).await?;
        if let Some(mem) = open_inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let path_ptr = write_cstr(&mut store, "/seek.bin");
        let of = open_inst.get_typed_func::<(i64,), i64>(&mut store, "go")?;
        let fd = of.call_async(&mut store, (path_ptr,)).await?;

        let lseek_inst = linker.instantiate_async(&mut store, &lseek_mod).await?;
        let lf = lseek_inst.get_typed_func::<(i64, i64), i64>(&mut store, "go")?;
        let r = lf.call_async(&mut store, (fd, 5)).await?;
        Ok::<_, anyhow::Error>(r)
    })?;
    assert_eq!(new_off, 5);
    Ok(())
}

#[test]
fn openat_read_lseek_roundtrip() -> Result<()> {
    let d = TmpDir::new();
    {
        let mut f = File::create(d.0.join("rt.bin")).unwrap();
        f.write_all(b"abcdefghij").unwrap();
    }
    let (engine, linker) = common::engine_and_linker()?;
    let kernel = common::kernel_with_preopen(&d.0);
    let open_mod = common::compile_wat(&engine, OPENAT_WAT)?;
    let read_mod = common::compile_wat(&engine, READ_WAT)?;
    let lseek_mod = common::compile_wat(&engine, LSEEK_WAT)?;

    let bytes = block_on(async {
        let mut store = edge_libos::build_store(&engine, kernel);
        let open_inst = linker.instantiate_async(&mut store, &open_mod).await?;
        if let Some(mem) = open_inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let path_ptr = write_cstr(&mut store, "/rt.bin");
        let of = open_inst.get_typed_func::<(i64,), i64>(&mut store, "go")?;
        let fd = of.call_async(&mut store, (path_ptr,)).await?;

        // Skip the first 3 bytes.
        let lseek_inst = linker.instantiate_async(&mut store, &lseek_mod).await?;
        let lf = lseek_inst.get_typed_func::<(i64, i64), i64>(&mut store, "go")?;
        let _ = lf.call_async(&mut store, (fd, 3)).await?;

        let read_inst = linker.instantiate_async(&mut store, &read_mod).await?;
        let rf = read_inst.get_typed_func::<(i64,), i64>(&mut store, "go")?;
        let _ = rf.call_async(&mut store, (fd,)).await?;

        let mem = *store.data().memory().unwrap();
        let data = mem.data(&store);
        Ok::<_, anyhow::Error>(data[4096..4096 + 7].to_vec())
    })?;
    assert_eq!(&bytes, b"defghij");
    Ok(())
}

#[test]
fn getdents64_returns_at_least_one_entry() -> Result<()> {
    let d = TmpDir::new();
    File::create(d.0.join("a.txt")).unwrap();
    File::create(d.0.join("b.txt")).unwrap();
    File::create(d.0.join("c.txt")).unwrap();

    let (engine, linker) = common::engine_and_linker()?;
    let kernel = common::kernel_with_preopen(&d.0);
    let open_mod = common::compile_wat(&engine, OPENAT_WAT)?;
    let gd_mod = common::compile_wat(&engine, GETDENTS_WAT)?;

    let bytes = block_on(async {
        let mut store = edge_libos::build_store(&engine, kernel);
        let open_inst = linker.instantiate_async(&mut store, &open_mod).await?;
        if let Some(mem) = open_inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let path_ptr = write_cstr(&mut store, "/");
        let of = open_inst.get_typed_func::<(i64,), i64>(&mut store, "go")?;
        let fd = of.call_async(&mut store, (path_ptr,)).await?;

        let gd_inst = linker.instantiate_async(&mut store, &gd_mod).await?;
        let gf = gd_inst.get_typed_func::<(i64,), i64>(&mut store, "go")?;
        let _ = gf.call_async(&mut store, (fd,)).await?;

        let mem = *store.data().memory().unwrap();
        let data = mem.data(&store);
        Ok::<_, anyhow::Error>(data[4096..4096 + 1024].to_vec())
    })?;
    // We expect at least 3 entries (a.txt, b.txt, c.txt) plus possibly "."
    // and ".." (skipped by read_dir's filter_map default behavior on most
    // platforms — they're returned by std::fs::read_dir but included).
    let names = parse_dirents(&bytes);
    assert!(
        names.contains(&"a.txt".to_string()),
        "expected a.txt in {names:?}"
    );
    assert!(
        names.contains(&"b.txt".to_string()),
        "expected b.txt in {names:?}"
    );
    assert!(
        names.contains(&"c.txt".to_string()),
        "expected c.txt in {names:?}"
    );
    Ok(())
}

#[test]
fn write_to_opened_file_persists() -> Result<()> {
    let d = TmpDir::new();
    let target = d.0.join("out.txt");
    let (engine, linker) = common::engine_and_linker()?;
    let kernel = common::kernel_with_preopen(&d.0);
    // openat that takes both path and flags as params.
    let open_wat = r#"
        (module
          (import "kernel" "syscall"
            (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
          (memory (export "memory") 1)
          (func (export "go") (param $path i64) (param $flags i64) (result i64)
            (call $syscall
              (i64.const 257) (i64.const -100)
              (local.get $path) (local.get $flags)
              (i64.const 0) (i64.const 0) (i64.const 0)))
        )
    "#;
    let open_mod = common::compile_wat(&engine, open_wat)?;
    let wat = wat_with_payload(b"written!");
    let write_mod = common::compile_wat(&engine, &wat)?;

    block_on(async {
        let mut store = edge_libos::build_store(&engine, kernel);
        let open_inst = linker.instantiate_async(&mut store, &open_mod).await?;
        if let Some(mem) = open_inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        // O_WRONLY(1) | O_CREAT(0x40) | O_TRUNC(0x200) = 0x241 = 577
        let path_ptr = write_cstr(&mut store, "/out.txt");
        let of = open_inst.get_typed_func::<(i64, i64), i64>(&mut store, "go")?;
        let fd = of.call_async(&mut store, (path_ptr, 0x241)).await?;
        assert!(fd >= 3, "openat returned {fd}");

        let write_inst = linker.instantiate_async(&mut store, &write_mod).await?;
        // The write module has its own memory; re-attach so the host sees the
        // payload at offset 4096.
        if let Some(mem) = write_inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let wf = write_inst.get_typed_func::<(i64, i64), i64>(&mut store, "go")?;
        let n = wf.call_async(&mut store, (fd, 8)).await?;
        assert_eq!(n, 8);
        Ok::<_, anyhow::Error>(())
    })?;
    let on_disk = fs::read(&target).unwrap();
    assert_eq!(on_disk, b"written!");
    Ok(())
}

/// Parse linux_dirent64 records from a guest buffer. Walks until a record
/// has reclen == 0 or extends past `buf`.
fn parse_dirents(buf: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let mut off = 0;
    while off + 19 <= buf.len() {
        let reclen = u16::from_le_bytes(buf[off + 16..off + 18].try_into().unwrap()) as usize;
        if reclen == 0 || off + reclen > buf.len() {
            break;
        }
        // NUL-terminated name starting at offset 19.
        let name_start = off + 19;
        let name_end = off + reclen;
        if let Some(nul) = buf[name_start..name_end].iter().position(|&b| b == 0) {
            let s = std::str::from_utf8(&buf[name_start..name_start + nul])
                .unwrap_or("")
                .to_string();
            if !s.is_empty() {
                names.push(s);
            }
        }
        off += reclen;
    }
    names
}

#[test]
fn nr_constants_match_linux_x86_64() {
    assert_eq!(edge_libos::sys::file::NR_READ, 0);
    assert_eq!(edge_libos::sys::file::NR_WRITE, 1);
    assert_eq!(edge_libos::sys::file::NR_OPEN, 2);
    assert_eq!(edge_libos::sys::file::NR_CLOSE, 3);
    assert_eq!(edge_libos::sys::file::NR_STAT, 4);
    assert_eq!(edge_libos::sys::file::NR_FSTAT, 5);
    assert_eq!(edge_libos::sys::file::NR_LSTAT, 6);
    assert_eq!(edge_libos::sys::file::NR_LSEEK, 8);
    assert_eq!(edge_libos::sys::file::NR_READV, 19);
    assert_eq!(edge_libos::sys::file::NR_WRITEV, 20);
    assert_eq!(edge_libos::sys::file::NR_PIPE, 22);
    assert_eq!(edge_libos::sys::file::NR_FCNTL, 72);
    assert_eq!(edge_libos::sys::file::NR_GETCWD, 79);
    assert_eq!(edge_libos::sys::file::NR_GETDENTS64, 217);
    assert_eq!(edge_libos::sys::file::NR_NEWFSTATAT, 262);
    assert_eq!(edge_libos::sys::file::NR_PIPE2, 293);
    assert_eq!(edge_libos::sys::file::NR_OPENAT, 257);
}

#[test]
fn pipe2_writes_pair_into_guest_array() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let pipe2_mod = common::compile_wat(&engine, PIPE2_WAT)?;

    let (ret, rd_fd, wr_fd) = block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        let inst = linker.instantiate_async(&mut store, &pipe2_mod).await?;
        if let Some(mem) = inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let f = inst.get_typed_func::<(), i64>(&mut store, "go")?;
        let r = f.call_async(&mut store, ()).await?;

        let mem = *store.data().memory().unwrap();
        let data = mem.data(&store);
        let rd = u32::from_le_bytes(data[4096..4100].try_into().unwrap());
        let wr = u32::from_le_bytes(data[4100..4104].try_into().unwrap());
        Ok::<_, anyhow::Error>((r, rd, wr))
    })?;
    assert_eq!(ret, 0, "pipe2 should return 0 on success");
    assert!(
        rd_fd >= 3,
        "read fd should be >=3 (after stdin/stdout/stderr), got {rd_fd}"
    );
    assert!(
        wr_fd >= 3,
        "write fd should be >=3 (after stdin/stdout/stderr), got {wr_fd}"
    );
    assert_ne!(rd_fd, wr_fd, "read and write fds must differ");
    Ok(())
}

// ---------------------------------------------------------------------------
// Step 28: pipe (NR 22) — legacy shim over pipe2
// ---------------------------------------------------------------------------

#[test]
fn pipe_writes_pair_into_guest_array() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, PIPE_WAT)?;

    let (ret, rd_fd, wr_fd) = block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        let inst = linker.instantiate_async(&mut store, &module).await?;
        if let Some(mem) = inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let f = inst.get_typed_func::<(), i64>(&mut store, "go")?;
        let r = f.call_async(&mut store, ()).await?;
        let mem = *store.data().memory().unwrap();
        let data = mem.data(&store);
        let rd = u32::from_le_bytes(data[4096..4100].try_into().unwrap());
        let wr = u32::from_le_bytes(data[4100..4104].try_into().unwrap());
        Ok::<_, anyhow::Error>((r, rd, wr))
    })?;
    assert_eq!(ret, 0, "pipe should return 0 on success");
    assert!(rd_fd >= 3, "read fd {rd_fd}");
    assert!(wr_fd >= 3, "write fd {wr_fd}");
    assert_ne!(rd_fd, wr_fd);
    Ok(())
}

// ---------------------------------------------------------------------------
// Step 25: open (NR 2) — legacy shim over openat
// ---------------------------------------------------------------------------

#[test]
fn open_existing_file_returns_nonzero_fd() -> Result<()> {
    let d = TmpDir::new();
    std::fs::File::create(d.0.join("file")).unwrap();
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, OPEN_WAT)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, common::kernel_with_preopen(&d.0));
        let inst = linker.instantiate_async(&mut store, &module).await?;
        if let Some(mem) = inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let f = inst.get_typed_func::<(), i64>(&mut store, "go")?;
        f.call_async(&mut store, ()).await
    })?;
    assert!(
        ret >= 3,
        "open() should return a new fd (>=3 after stdio), got {ret}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Step 27: stat / lstat — shims over newfstatat
// ---------------------------------------------------------------------------

#[test]
fn stat_existing_file_returns_zero() -> Result<()> {
    let d = TmpDir::new();
    std::fs::File::create(d.0.join("file")).unwrap();
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, STAT_WAT)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, common::kernel_with_preopen(&d.0));
        let inst = linker.instantiate_async(&mut store, &module).await?;
        if let Some(mem) = inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let f = inst.get_typed_func::<(), i64>(&mut store, "go")?;
        f.call_async(&mut store, ()).await
    })?;
    assert_eq!(ret, 0, "stat() on existing file should return 0");
    Ok(())
}

#[test]
fn lstat_returns_zero_for_existing_file() -> Result<()> {
    let d = TmpDir::new();
    std::fs::File::create(d.0.join("file")).unwrap();
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, LSTAT_WAT)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, common::kernel_with_preopen(&d.0));
        let inst = linker.instantiate_async(&mut store, &module).await?;
        if let Some(mem) = inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let f = inst.get_typed_func::<(), i64>(&mut store, "go")?;
        f.call_async(&mut store, ()).await
    })?;
    assert_eq!(ret, 0, "lstat() on existing file should return 0");
    Ok(())
}

// ---------------------------------------------------------------------------
// Step 26: getcwd — write cwd into guest buffer
// ---------------------------------------------------------------------------

#[test]
fn getcwd_returns_root_path() -> Result<()> {
    let d = TmpDir::new();
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, GETCWD_WAT)?;

    let (ret, path_bytes) = block_on(async {
        let mut store = edge_libos::build_store(&engine, common::kernel_with_preopen(&d.0));
        let inst = linker.instantiate_async(&mut store, &module).await?;
        if let Some(mem) = inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let f = inst.get_typed_func::<i64, i64>(&mut store, "go")?;
        let r = f.call_async(&mut store, 4096_i64).await?;
        let mem = *store.data().memory().unwrap();
        let data = mem.data(&store);
        // The kernel writes N path bytes then NUL at offset N. Walk forward
        // from 8192 looking for the NUL terminator to find the path end.
        let n = r as usize;
        assert!(n <= 4096, "returned length {n} exceeds buffer");
        let nul_pos = 8192 + n;
        assert_eq!(data[nul_pos], 0, "kernel must NUL-terminate at byte {n}");
        Ok::<_, anyhow::Error>((r, data[8192..nul_pos].to_vec()))
    })?;
    assert_eq!(
        ret as usize,
        path_bytes.len(),
        "returned length must match bytes written"
    );
    assert!(!path_bytes.is_empty(), "cwd should not be empty");
    Ok(())
}

#[test]
fn getcwd_truncates_returns_erange() -> Result<()> {
    let d = TmpDir::new();
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, GETCWD_WAT)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, common::kernel_with_preopen(&d.0));
        let inst = linker.instantiate_async(&mut store, &module).await?;
        if let Some(mem) = inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        // size=4 — too small for any real path → -ERANGE.
        let f = inst.get_typed_func::<i64, i64>(&mut store, "go")?;
        f.call_async(&mut store, 4_i64).await
    })?;
    assert_eq!(
        ret,
        -edge_libos::errno::ERANGE,
        "tiny buf must return -ERANGE"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Step 29: readv / writev — scatter/gather I/O
// ---------------------------------------------------------------------------

#[test]
fn readv_into_two_buffers() -> Result<()> {
    let d = TmpDir::new();
    std::fs::write(d.0.join("file"), b"abcdef").unwrap();
    let (engine, linker) = common::engine_and_linker()?;
    let readv_mod = common::compile_wat(&engine, READV_WAT)?;
    let open_mod = common::compile_wat(&engine, OPEN_WAT)?;

    let (ret, b1, b2) = block_on(async {
        let mut store = edge_libos::build_store(&engine, common::kernel_with_preopen(&d.0));

        // Instantiate open module → call → get fd.
        let open_inst = linker.instantiate_async(&mut store, &open_mod).await?;
        if let Some(mem) = open_inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let open_f = open_inst.get_typed_func::<(), i64>(&mut store, "go")?;
        let fd = open_f.call_async(&mut store, ()).await?;

        // Instantiate readv module → call with the fd.
        let readv_inst = linker.instantiate_async(&mut store, &readv_mod).await?;
        if let Some(mem) = readv_inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let f = readv_inst.get_typed_func::<i64, i64>(&mut store, "go")?;
        let r = f.call_async(&mut store, fd).await?;

        let mem = *store.data().memory().unwrap();
        let data = mem.data(&store);
        let b1 = data[12288..12291].to_vec();
        let b2 = data[16384..16387].to_vec();
        Ok::<_, anyhow::Error>((r, b1, b2))
    })?;
    assert_eq!(ret, 6, "expected 6 bytes total");
    assert_eq!(b1, b"abc");
    assert_eq!(b2, b"def");
    Ok(())
}

#[test]
fn writev_concatenates_to_stdout() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, WRITEV_WAT)?;

    let (ret, captured) = block_on(async {
        let kernel = Kernel::new(vec![], vec![]);
        let stdout_buf = kernel.stdout_buf();
        let mut store = edge_libos::build_store(&engine, kernel);
        let inst = linker.instantiate_async(&mut store, &module).await?;
        if let Some(mem) = inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let f = inst.get_typed_func::<i64, i64>(&mut store, "go")?;
        let r = f.call_async(&mut store, 1 /* STDOUT */).await?;
        let bytes: Vec<u8> = match stdout_buf {
            Some(b) => b.lock().drain(..).collect(),
            None => Vec::new(),
        };
        Ok::<_, anyhow::Error>((r, bytes))
    })?;
    assert_eq!(ret, 6, "writev should report 6 bytes written");
    assert_eq!(captured, b"foobar", "stdout should contain 'foobar'");
    Ok(())
}

// ---------------------------------------------------------------------------
// Step 25-30 PR review gaps: negative-path tests
// ---------------------------------------------------------------------------

/// Regression: `stat(path_ptr, ...)` with an out-of-bounds path pointer must
/// return `-EFAULT`, not `-ENOENT`. The empty-path guard added in this PR
/// runs *after* `mem::guest_str`, so an unreadable path is still surfaced
/// as EFAULT (matches Linux: the pointer is checked before the path is
/// interpreted).
#[test]
fn stat_faulty_path_returns_efault_not_enoent() -> Result<()> {
    let d = TmpDir::new();
    // 0xFFFFFFFC is one page beyond the 1-page (64KiB) linear memory.
    // mem::guest_str clamps length to 4096 so we read uninitialised bytes,
    // but any non-NUL byte yields a non-empty path and any NUL stops
    // reading — either way the pointer is OOB and the kernel must EFAULT
    // before evaluating path semantics. We pick a NUL-only region to also
    // exercise the empty-path code path: the kernel must still EFAULT
    // because the pointer itself is invalid, not because the path is empty.
    let wat = r#"
        (module
          (import "kernel" "syscall"
            (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
          (memory (export "memory") 1)
          (func (export "go") (result i64)
            (call $syscall
              (i64.const 4)              ;; NR_STAT
              (i64.const 4294967292)     ;; 0xFFFFFFFC — beyond linear memory
              (i64.const 8192)           ;; statbuf
              (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
    "#;
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, wat)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, common::kernel_with_preopen(&d.0));
        let inst = linker.instantiate_async(&mut store, &module).await?;
        if let Some(mem) = inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let f = inst.get_typed_func::<(), i64>(&mut store, "go")?;
        f.call_async(&mut store, ()).await
    })?;
    assert_eq!(
        ret,
        -edge_libos::errno::EFAULT,
        "stat() with OOB path pointer must return -EFAULT (not -ENOENT)"
    );
    Ok(())
}

/// `stat("/file")` on a missing file → -ENOENT. STAT_WAT hardcodes
/// `/file\00` at offset 4096; we simply don't create the file.
#[test]
fn stat_existing_missing_file_returns_enoent() -> Result<()> {
    let d = TmpDir::new();
    // No File::create — `/file` does not exist.
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, STAT_WAT)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, common::kernel_with_preopen(&d.0));
        let inst = linker.instantiate_async(&mut store, &module).await?;
        if let Some(mem) = inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let f = inst.get_typed_func::<(), i64>(&mut store, "go")?;
        f.call_async(&mut store, ()).await
    })?;
    assert_eq!(
        ret,
        -edge_libos::errno::ENOENT,
        "stat() on missing file should return -ENOENT"
    );
    Ok(())
}

/// `lstat("/file")` on a missing file → -ENOENT.
#[test]
fn lstat_existing_missing_file_returns_enoent() -> Result<()> {
    let d = TmpDir::new();
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, LSTAT_WAT)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, common::kernel_with_preopen(&d.0));
        let inst = linker.instantiate_async(&mut store, &module).await?;
        if let Some(mem) = inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let f = inst.get_typed_func::<(), i64>(&mut store, "go")?;
        f.call_async(&mut store, ()).await
    })?;
    assert_eq!(
        ret,
        -edge_libos::errno::ENOENT,
        "lstat() on missing file should return -ENOENT"
    );
    Ok(())
}

/// `readv` short read: file has 2 bytes, iovec requests 6. Should return 2
/// (the first read consumes everything, second loop iteration breaks on `r < len`).
#[test]
fn readv_short_read() -> Result<()> {
    let d = TmpDir::new();
    std::fs::write(d.0.join("file"), b"ab").unwrap(); // only 2 bytes
    let (engine, linker) = common::engine_and_linker()?;
    let readv_mod = common::compile_wat(&engine, READV_WAT)?;
    let open_mod = common::compile_wat(&engine, OPEN_WAT)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, common::kernel_with_preopen(&d.0));
        let open_inst = linker.instantiate_async(&mut store, &open_mod).await?;
        if let Some(mem) = open_inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let open_f = open_inst.get_typed_func::<(), i64>(&mut store, "go")?;
        let fd = open_f.call_async(&mut store, ()).await?;

        let readv_inst = linker.instantiate_async(&mut store, &readv_mod).await?;
        if let Some(mem) = readv_inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let f = readv_inst.get_typed_func::<i64, i64>(&mut store, "go")?;
        f.call_async(&mut store, fd).await
    })?;
    assert_eq!(
        ret, 2,
        "readv into 6-byte iov from 2-byte file should return 2"
    );
    Ok(())
}

/// `readv` with a single zero-length iovec entry → returns 0 (the kernel
/// skips `len == 0` entries without calling `read`).
#[test]
fn readv_zero_length_iov_skipped() -> Result<()> {
    let d = TmpDir::new();
    std::fs::write(d.0.join("file"), b"abc").unwrap();
    let (engine, linker) = common::engine_and_linker()?;
    let readv_mod = common::compile_wat(&engine, READV_ZERO_WAT)?;
    let open_mod = common::compile_wat(&engine, OPEN_WAT)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, common::kernel_with_preopen(&d.0));
        let open_inst = linker.instantiate_async(&mut store, &open_mod).await?;
        if let Some(mem) = open_inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let open_f = open_inst.get_typed_func::<(), i64>(&mut store, "go")?;
        let fd = open_f.call_async(&mut store, ()).await?;

        let readv_inst = linker.instantiate_async(&mut store, &readv_mod).await?;
        if let Some(mem) = readv_inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let f = readv_inst.get_typed_func::<i64, i64>(&mut store, "go")?;
        f.call_async(&mut store, fd).await
    })?;
    assert_eq!(ret, 0, "readv with single zero-length iov should return 0");
    Ok(())
}

/// `getcwd` exact-fit boundary: `size == cwd.len()+1` succeeds (room for path + NUL).
/// Two-call pattern: probe with a generous size to learn path length, then re-invoke
/// on the same store so cwd is stable.
#[test]
fn getcwd_exact_size_returns_path() -> Result<()> {
    let d = TmpDir::new();
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, GETCWD_WAT)?;

    let (probe, exact_fit) = block_on(async {
        let mut store = edge_libos::build_store(&engine, common::kernel_with_preopen(&d.0));
        let inst = linker.instantiate_async(&mut store, &module).await?;
        if let Some(mem) = inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let f = inst.get_typed_func::<i64, i64>(&mut store, "go")?;
        let r1 = f.call_async(&mut store, 4096_i64).await?;
        assert!(r1 > 0, "first call must succeed, got {r1}");
        // Re-invoke with exactly path_len + 1 (room for path + NUL).
        let r2 = f.call_async(&mut store, r1 + 1).await?;
        Ok::<_, anyhow::Error>((r1, r2))
    })?;
    assert!(probe > 0, "probe must return positive length, got {probe}");
    assert_eq!(
        exact_fit, probe,
        "exact-fit getcwd must return the same path length as probe"
    );
    Ok(())
}

/// `getcwd` one-byte-short boundary: `size == cwd.len()` returns -ERANGE (no room
/// for the trailing NUL). Same two-call pattern as `getcwd_exact_size_returns_path`.
#[test]
fn getcwd_one_byte_short_returns_erange() -> Result<()> {
    let d = TmpDir::new();
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, GETCWD_WAT)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, common::kernel_with_preopen(&d.0));
        let inst = linker.instantiate_async(&mut store, &module).await?;
        if let Some(mem) = inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let f = inst.get_typed_func::<i64, i64>(&mut store, "go")?;
        let r1 = f.call_async(&mut store, 4096_i64).await?;
        assert!(r1 > 0, "probe must succeed, got {r1}");
        // One byte short — no room for the trailing NUL.
        f.call_async(&mut store, r1).await
    })?;
    assert_eq!(
        ret,
        -edge_libos::errno::ERANGE,
        "size == cwd.len() must return -ERANGE (no room for NUL)"
    );
    Ok(())
}

/// `stat("")` returns -ENOENT (matches Linux; without AT_EMPTY_PATH flag,
/// the empty path is treated as "no such file"). Uses an inline WAT that
/// places a single NUL byte at offset 4096.
#[test]
fn stat_empty_path_returns_enoent() -> Result<()> {
    let d = TmpDir::new();
    let wat = r#"
        (module
          (import "kernel" "syscall"
            (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
          (memory (export "memory") 1)
          (data (i32.const 4096) "\00")
          (func (export "go") (result i64)
            (call $syscall
              (i64.const 4)              ;; NR_STAT
              (i64.const 4096)           ;; empty path
              (i64.const 8192)           ;; statbuf
              (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
    "#;
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, wat)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, common::kernel_with_preopen(&d.0));
        let inst = linker.instantiate_async(&mut store, &module).await?;
        if let Some(mem) = inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let f = inst.get_typed_func::<(), i64>(&mut store, "go")?;
        f.call_async(&mut store, ()).await
    })?;
    assert_eq!(
        ret,
        -edge_libos::errno::ENOENT,
        "stat(\"\") should return -ENOENT"
    );
    Ok(())
}

/// `lstat("")` returns -ENOENT. Same reasoning as stat_empty_path.
#[test]
fn lstat_empty_path_returns_enoent() -> Result<()> {
    let d = TmpDir::new();
    let wat = r#"
        (module
          (import "kernel" "syscall"
            (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
          (memory (export "memory") 1)
          (data (i32.const 4096) "\00")
          (func (export "go") (result i64)
            (call $syscall
              (i64.const 6)              ;; NR_LSTAT
              (i64.const 4096)           ;; empty path
              (i64.const 8192)           ;; statbuf
              (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
    "#;
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, wat)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, common::kernel_with_preopen(&d.0));
        let inst = linker.instantiate_async(&mut store, &module).await?;
        if let Some(mem) = inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let f = inst.get_typed_func::<(), i64>(&mut store, "go")?;
        f.call_async(&mut store, ()).await
    })?;
    assert_eq!(
        ret,
        -edge_libos::errno::ENOENT,
        "lstat(\"\") should return -ENOENT"
    );
    Ok(())
}
