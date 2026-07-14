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
//! trace-host <python.wasm> [--diff <baseline>] [--no-marker] [--] [args...]
//! ```
//!
//! `--diff <baseline>` reads a baseline file (one syscall name per line,
//! e.g. from `strace -e trace='!all'`) and exits non-zero if any baseline
//! syscall is missing from the host trace. Host-only syscalls (those in
//! the trace but not in the baseline) are reported on stderr but do not
//! cause failure — they are simply backlog for the baseline author.
//!
//! `--no-marker` suppresses the trailing `{"marker":"..."}` JSON line so
//! callers that consume only syscall entries can pipe stdout cleanly.
//!
//! ## P2-A2: dispatch dedup
//!
//! Previously this binary hand-mirrored the entire NR dispatch table.
//! Now it installs a `SyscallObserver` via `install_observer` and calls
//! `add_to_linker` like any other consumer. New syscalls added to
//! `dispatch::dispatch` are picked up automatically.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Context, Result};
use wasmtime::Linker;

use edge_libos::{
    add_to_linker, build_engine, build_store, install_observer, syscall_name, Kernel,
    SyscallObserver,
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

/// P2-A2: the observer that records syscalls into a thread-local buffer.
/// We pair the captured args with the returned ret in `on_exit` using
/// the `nr` as the correlation key. A single-threaded wasm guest cannot
/// have two concurrent syscalls in flight, so a single `PENDING` slot is
/// sufficient.
struct TraceObserver;

thread_local! {
    static TRACE: std::cell::RefCell<Vec<TraceEntry>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Most recent (ts_ns, nr, args) seen by `on_enter` — flushed in
    /// `on_exit` when the matching `ret` arrives.
    static PENDING: std::cell::RefCell<Option<(u128, u32, [i64; 6])>> =
        const { std::cell::RefCell::new(None) };
}

impl SyscallObserver for TraceObserver {
    fn on_enter(&self, nr: u32, args: [i64; 6]) {
        let ts_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        PENDING.with(|p| *p.borrow_mut() = Some((ts_ns, nr, args)));
    }
    fn on_exit(&self, nr: u32, ret: i64) {
        PENDING.with(|p| {
            if let Some((ts_ns, pending_nr, args)) = p.borrow_mut().take() {
                // Only pair if the same syscall (defensive).
                if pending_nr == nr {
                    TRACE.with(|t| {
                        t.borrow_mut().push(TraceEntry {
                            ts_ns,
                            nr,
                            name: syscall_name(nr),
                            args,
                            ret,
                        });
                    });
                }
            }
        });
    }
}

fn main() -> Result<()> {
    // Parse CLI: pull --diff and --no-marker out of the front.
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut baseline: Option<PathBuf> = None;
    let mut emit_marker = true;
    let mut positional: Vec<String> = Vec::with_capacity(raw.len());
    let mut it = raw.into_iter();
    while let Some(a) = it.next() {
        if a == "--diff" {
            baseline = Some(PathBuf::from(
                it.next().context("--diff requires a path argument")?,
            ));
        } else if a == "--no-marker" {
            emit_marker = false;
        } else {
            positional.push(a);
        }
    }

    let wasm_path = positional
        .first()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "usage: trace-host <python.wasm> [--diff <baseline>] [--no-marker] [--] [args...]"
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
    let (entries, marker) = rt.block_on(run_guest(&wasm_path, &script_args))?;

    // Emit one JSON line per syscall.
    let mut names_seen: BTreeSet<&'static str> = BTreeSet::new();
    for e in &entries {
        names_seen.insert(e.name);
        println!(
            "{{\"ts_ns\":{},\"nr\":{},\"name\":\"{}\",\"args\":[{},{},{},{},{},{}],\"ret\":{}}}",
            e.ts_ns,
            e.nr,
            e.name,
            e.args[0],
            e.args[1],
            e.args[2],
            e.args[3],
            e.args[4],
            e.args[5],
            e.ret,
        );
    }
    if emit_marker {
        let escaped = marker.replace('\\', "\\\\").replace('"', "\\\"");
        println!("{{\"marker\":\"{}\"}}", escaped);
    }
    eprintln!("trace-host: {} syscalls captured", entries.len());

    // --diff mode: ensure every baseline syscall name was seen in the trace.
    if let Some(path) = baseline {
        let baseline_names: BTreeSet<String> = std::fs::read_to_string(&path)?
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        let baseline_static: BTreeSet<&str> = baseline_names.iter().map(|s| s.as_str()).collect();
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

/// Read the pass/fail marker that C conformance tests write at
/// `MARKER_ADDR` (offset 4096) before the final `_start` return.
///
/// Returns the marker text. Empty string means "no marker written".
///
/// **Important:** several existing C conformance tests use bytes 4-62 of
/// the marker region as scratch space for sockaddr layouts (e.g. `bind.c`
/// writes `0x02` — the AF_INET family byte — at offset 4100). So we do
/// NOT walk the buffer until the first NUL; we read the literal prefix:
///   - bytes 0..4 == b"PASS"         → "PASS"
///   - bytes 0..5 == b"FAIL:"        → "FAIL:\<reason up to first NUL>"
///   - anything else                 → "" (no marker)
fn read_marker(store: &wasmtime::Store<Kernel>) -> String {
    const MARKER_ADDR: usize = 4096;
    const MARKER_LEN: usize = 64;
    let mem = match store.data().memory.as_ref() {
        Some(m) => m,
        None => return String::new(),
    };
    let mut buf = [0u8; MARKER_LEN];
    if mem.read(store, MARKER_ADDR, &mut buf).is_err() {
        return String::new();
    }
    if &buf[0..4] == b"PASS" {
        return "PASS".to_string();
    }
    if &buf[0..5] == b"FAIL:" {
        let end = buf[5..]
            .iter()
            .position(|&b| b == 0)
            .map(|p| p + 5)
            .unwrap_or(buf.len());
        let reason_bytes = &buf[5..end];
        return format!("FAIL:{}", String::from_utf8_lossy(reason_bytes));
    }
    String::new()
}

/// Drive the guest and return the captured trace.
///
/// P2-A2: we use the standard `add_to_linker` (no custom dispatch) and
/// install a thread-local `TraceObserver` for the duration of the run.
async fn run_guest(wasm_path: &str, _script_args: &[String]) -> Result<(Vec<TraceEntry>, String)> {
    let engine = build_engine()?;
    let mut linker: Linker<Kernel> = Linker::new(&engine);
    add_to_linker(&mut linker)?;

    // Install the observer for this thread. We restore the prior (likely
    // None) observer on return so this binary is re-entrant-safe.
    let prev = install_observer(Some(std::sync::Arc::new(TraceObserver)));

    let kernel = Kernel::new(vec![], vec![]);
    let mut store = build_store(&engine, kernel);
    let bytes =
        std::fs::read(wasm_path).map_err(|e| anyhow::anyhow!("reading {wasm_path}: {e}"))?;
    let module = if bytes.len() >= 4 && &bytes[0..4] == b"\0asm" {
        wasmtime::Module::new(&engine, &bytes)?
    } else {
        unsafe { wasmtime::Module::deserialize(&engine, &bytes)? }
    };
    let instance = linker.instantiate_async(&mut store, &module).await?;
    if let Some(mem) = instance.get_memory(&mut store, "memory") {
        store.data_mut().attach_memory(mem);
    }
    if let Ok(start) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
        let _ = start.call_async(&mut store, ()).await;
    } else if let Ok(start) = instance.get_typed_func::<(), i32>(&mut store, "_start") {
        let _ = start.call_async(&mut store, ()).await;
    }

    let entries = TRACE.with(|t| t.borrow().clone());
    let marker = read_marker(&store);
    install_observer(prev);
    Ok((entries, marker))
}
