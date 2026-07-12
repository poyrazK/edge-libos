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
        let dir =
            std::env::temp_dir().join(format!("edge-libos-vfs-test-{pid}-{id}"));
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
    let mem = store.data().memory().unwrap().clone();
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
        let mem = store.data().memory().unwrap().clone();
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

        let mem = store.data().memory().unwrap().clone();
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

        let mem = store.data().memory().unwrap().clone();
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
    assert_eq!(edge_libos::sys::file::NR_OPENAT, 257);
    assert_eq!(edge_libos::sys::file::NR_CLOSE, 3);
    assert_eq!(edge_libos::sys::file::NR_LSEEK, 8);
    assert_eq!(edge_libos::sys::file::NR_FSTAT, 5);
    assert_eq!(edge_libos::sys::file::NR_GETDENTS64, 217);
    assert_eq!(edge_libos::sys::file::NR_PIPE2, 293);
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

        let mem = store.data().memory().unwrap().clone();
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