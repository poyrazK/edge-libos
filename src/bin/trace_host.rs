//! `trace-host` — syscall tracer + JSON dumper.
//!
//! Loads a CPython wasm32-musl guest, runs it under edge-libos, and emits one
//! JSON line per `kernel.syscall` dispatch to stdout:
//!
//! ```json
//! {"ts_ns": 1234567, "nr": 1, "name": "write", "args": [1,4096,8,0,0,0], "ret": 8}
//! ```
//!
//! ## CLI
//!
//! ```text
//! trace-host <python.wasm> [--diff <baseline>] [--] [args...]
//! ```
//!
//! `--diff <baseline>` reads a baseline file (one syscall name per line,
//! e.g. from `strace -e trace='!all'`) and exits non-zero if any baseline
//! syscall is missing from the host trace. Host-only syscalls (those in
//! the trace but not in the baseline) are reported on stderr but do not
//! cause failure — they are simply backlog for the baseline author.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Context, Result};
use wasmtime::{Caller, FuncType, Linker, Val, ValType};

use edge_libos::errno::{to_ret, ENOSYS};
use edge_libos::{
    build_engine, build_store, dispatch, kernel::Kernel, sys,
};

/// One traced syscall.
#[derive(Debug, Clone)]
struct TraceEntry {
    ts_ns: u128,
    nr: u32,
    name: &'static str,
    args: [i64; 6],
    ret: i64,
}

fn main() -> Result<()> {
    // Parse CLI: pull --diff out of the front.
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut baseline: Option<PathBuf> = None;
    let mut positional: Vec<String> = Vec::with_capacity(raw.len());
    let mut it = raw.into_iter();
    while let Some(a) = it.next() {
        if a == "--diff" {
            baseline = Some(PathBuf::from(
                it.next().context("--diff requires a path argument")?,
            ));
        } else {
            positional.push(a);
        }
    }

    let wasm_path = positional
        .first()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "usage: trace-host <python.wasm> [--diff <baseline>] [--] [args...]"
            )
        })?
        .clone();
    let script_args: Vec<String> = if positional.get(1).map(|s| s.as_str()) == Some("--") {
        positional[2..].to_vec()
    } else {
        positional[1..].to_vec()
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let entries = rt.block_on(run_guest(&wasm_path, &script_args))?;

    // Emit one JSON line per syscall.
    let mut names_seen: BTreeSet<&'static str> = BTreeSet::new();
    for e in &entries {
        names_seen.insert(e.name);
        println!(
            "{{\"ts_ns\":{},\"nr\":{},\"name\":\"{}\",\"args\":[{},{},{},{},{},{}],\"ret\":{}}}",
            e.ts_ns,
            e.nr,
            e.name,
            e.args[0], e.args[1], e.args[2], e.args[3], e.args[4], e.args[5],
            e.ret,
        );
    }
    eprintln!("trace-host: {} syscalls captured", entries.len());

    // --diff mode: ensure every baseline syscall name was seen in the trace.
    if let Some(path) = baseline {
        let baseline_names: BTreeSet<String> = std::fs::read_to_string(&path)?
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        let baseline_static: BTreeSet<&str> =
            baseline_names.iter().map(|s| s.as_str()).collect();
        let missing: Vec<&&str> = baseline_static.difference(&names_seen).collect();
        let extra: Vec<&&str> = names_seen.difference(&baseline_static).collect();
        if missing.is_empty() {
            eprintln!(
                "trace-host: --diff OK — all {} baseline syscalls observed",
                baseline_names.len()
            );
            if !extra.is_empty() {
                eprintln!(
                    "trace-host: --diff backlog (not in baseline, ok): {:?}",
                    extra
                );
            }
            std::process::exit(0);
        } else {
            eprintln!(
                "trace-host: --diff FAIL — missing baseline syscalls: {:?}",
                missing
            );
            std::process::exit(1);
        }
    }
    Ok(())
}

/// Drive the guest and return the captured trace. Mirrors the edge-python
/// stub driver (Step 20 will share this code path). The trace layer wraps
/// the dispatcher via a custom `Linker` definition rather than the default
/// one — see `register_traced_linker` for the wiring.
async fn run_guest(wasm_path: &str, _script_args: &[String]) -> Result<Vec<TraceEntry>> {
    let engine = build_engine()?;
    let mut linker: Linker<Kernel> = Linker::new(&engine);

    // Custom dispatch that records (nr, args, ret) into a buffer that we
    // return by leaking an Arc through Caller::data(). Easier path: stash
    // the buffer in a thread-local we drain at the end.
    thread_local! {
        static TRACE: std::cell::RefCell<Vec<TraceEntry>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    let params: [ValType; 7] = [const { ValType::I64 }; 7];
    let results: [ValType; 1] = [const { ValType::I64 }; 1];
    let func_ty = FuncType::new(&engine, params, results);
    linker.func_new_async("kernel", "syscall", func_ty, move |caller, params, results| {
        Box::new(async move {
            let nr = params[0].unwrap_i64() as u32;
            let a: [i64; 6] = std::array::from_fn(|i| params[i + 1].unwrap_i64());

            let ret = dispatch_dispatch(caller, nr, a).await;
            let name = dispatch::syscall_name(nr);

            let ts_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            TRACE.with(|t| {
                t.borrow_mut().push(TraceEntry {
                    ts_ns,
                    nr,
                    name,
                    args: a,
                    ret,
                });
            });

            results[0] = Val::I64(ret);
            Ok(())
        })
    })?;

    let kernel = Kernel::new(vec![], vec![]);
    let mut store = build_store(&engine, kernel);
    let bytes = std::fs::read(wasm_path)
        .map_err(|e| anyhow::anyhow!("reading {wasm_path}: {e}"))?;
    // Detect precompiled artifact by magic. wasmtime's precompiled artifacts
    // begin with a version tag; raw wasm begins with `\0asm`.
    let module = if bytes.len() >= 4 && &bytes[0..4] == b"\0asm" {
        wasmtime::Module::new(&engine, &bytes)?
    } else {
        // Treat as a precompiled artifact.
        unsafe { wasmtime::Module::deserialize(&engine, &bytes)? }
    };
    let instance = linker.instantiate_async(&mut store, &module).await?;
    if let Some(mem) = instance.get_memory(&mut store, "memory") {
        store.data_mut().attach_memory(mem);
    }
    // Try multiple `_start` signatures. zig cc emits `() -> void` (no
    // return). Emscripten emits `() -> i32`. CPython's `_start` (when
    // landed via Step 19-20) emits `() -> void`.
    if let Ok(start) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
        let _ = start.call_async(&mut store, ()).await; // exit may trap
    } else if let Ok(start) = instance.get_typed_func::<(), i32>(&mut store, "_start") {
        let _ = start.call_async(&mut store, ()).await;
    }

    let entries = TRACE.with(|t| t.borrow().clone());
    Ok(entries)
}

/// Mirror of `dispatch::dispatch` — kept inline here so the trace layer
/// owns the dispatch decision and can record the return value. New syscalls
/// added to `dispatch::dispatch` must be mirrored here.
async fn dispatch_dispatch(
    mut caller: Caller<'_, Kernel>,
    nr: u32,
    a: [i64; 6],
) -> i64 {
    match nr {
        sys::process::NR_EXIT => sys::process::exit(&mut caller, a).await,
        sys::process::NR_EXIT_GROUP => sys::process::exit_group(&mut caller, a).await,
        sys::process::NR_GETPID => sys::process::getpid(),
        sys::process::NR_GETTID => sys::process::gettid(),
        sys::process::NR_SET_TID_ADDRESS => {
            sys::process::set_tid_address(&mut caller, a)
        }
        sys::process::NR_SET_ROBUST_LIST => sys::process::set_robust_list(),
        sys::process::NR_ARCH_PRCTL => to_ret(ENOSYS),
        sys::process::NR_RSEQ => to_ret(ENOSYS),

        sys::memory::NR_MMAP => sys::memory::mmap(&mut caller, a).await,
        sys::memory::NR_MUNMAP => sys::memory::munmap(&mut caller, a).await,
        sys::memory::NR_MPROTECT => sys::memory::mprotect(),
        sys::memory::NR_MADVISE => sys::memory::madvise(),
        sys::memory::NR_BRK => sys::memory::brk(&mut caller, a),

        sys::file::NR_READ => sys::file::read(&mut caller, a).await,
        sys::file::NR_WRITE => sys::file::write(&mut caller, a).await,
        sys::file::NR_OPEN => sys::file::open(&mut caller, a).await,
        sys::file::NR_OPENAT => sys::file::openat(&mut caller, a).await,
        sys::file::NR_CLOSE => sys::file::close(&mut caller, a).await,
        sys::file::NR_STAT => sys::file::stat(&mut caller, a).await,
        sys::file::NR_LSTAT => sys::file::lstat(&mut caller, a).await,
        sys::file::NR_LSEEK => sys::file::lseek(&mut caller, a).await,
        sys::file::NR_FSTAT => sys::file::fstat(&mut caller, a).await,
        sys::file::NR_NEWFSTATAT => sys::file::newfstatat(&mut caller, a).await,
        sys::file::NR_GETDENTS64 => sys::file::getdents64(&mut caller, a).await,
        sys::file::NR_PIPE => sys::file::pipe(&mut caller, a).await,
        sys::file::NR_PIPE2 => sys::file::pipe2(&mut caller, a).await,
        sys::file::NR_FCNTL => sys::file::fcntl(&mut caller, a).await,
        sys::file::NR_GETCWD => sys::file::getcwd(&mut caller, a).await,
        sys::file::NR_READV => sys::file::readv(&mut caller, a).await,
        sys::file::NR_WRITEV => sys::file::writev(&mut caller, a).await,

        sys::socket::NR_SOCKET => sys::socket::socket(&mut caller, a).await,
        sys::socket::NR_BIND => sys::socket::bind(&mut caller, a).await,
        sys::socket::NR_LISTEN => sys::socket::listen(&mut caller, a).await,
        sys::socket::NR_ACCEPT => sys::socket::accept(&mut caller, a).await,
        sys::socket::NR_ACCEPT4 => sys::socket::accept4(&mut caller, a).await,
        sys::socket::NR_CONNECT => sys::socket::connect(&mut caller, a).await,
        sys::socket::NR_SENDTO => sys::socket::sendto(&mut caller, a).await,
        sys::socket::NR_RECVFROM => sys::socket::recvfrom(&mut caller, a).await,
        sys::socket::NR_SETSOCKOPT => sys::socket::setsockopt(&mut caller, a).await,

        sys::identity::NR_GETUID => sys::identity::getuid(),
        sys::identity::NR_GETEUID => sys::identity::geteuid(),
        sys::identity::NR_GETGID => sys::identity::getgid(),
        sys::identity::NR_GETEGID => sys::identity::getegid(),

        sys::time::NR_CLOCK_GETTIME => sys::time::clock_gettime(&mut caller, a).await,
        sys::time::NR_GETTIMEOFDAY => sys::time::gettimeofday(&mut caller, a).await,
        sys::time::NR_NANOSLEEP => sys::time::nanosleep(&mut caller, a).await,

        sys::random::NR_GETRANDOM => sys::random::getrandom(&mut caller, a).await,

        sys::signal::NR_RT_SIGACTION => sys::signal::rt_sigaction(&mut caller, a),
        sys::signal::NR_RT_SIGPROCMASK => sys::signal::rt_sigprocmask(&mut caller, a),

        _ => to_ret(ENOSYS),
    }
}