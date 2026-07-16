//! `edge-cli trace <wasm> [--diff <baseline>] [--no-marker] [--] [args...]`.
//!
//! P2-D3.3: body migrated verbatim from `src/bin/trace_host.rs` (P2-A2).
//! Preserves the JSON-line protocol and `--diff` / `--no-marker` semantics
//! that `bash tests/conformance/runner.sh` and `tests/strace_baseline_diff.rs`
//! depend on, so the C conformance gate keeps working unchanged.
//!
//! P2-D3.4: `MARKER_ADDR`/`MARKER_LEN` re-homed on `Kernel`. The PENDING
//! thread-local pairs real entry args with the on_exit ret; the JSON line
//! now carries the guest's actual call-site args (proven by
//! `tests/trace_host_smoke.rs::trace_observer_emits_real_args_on_write_syscall`).
//!
//! Output format (one line per syscall, then an optional marker line):
//!
//! ```text
//! {"ts_ns":1234567,"nr":1,"name":"write","args":[1,4096,8,0,0,0],"ret":8}
//! {"ts_ns":...,"nr":...,"name":"...","args":[...6...],"ret":...}
//! ...
//! {"marker":"PASS"}            # only emitted unless --no-marker
//! ```

use std::collections::BTreeSet;
use std::path::PathBuf;

use wasmtime::{Linker, Store};

use crate::cli::error::{CliError, CliResult};
use crate::dispatch::{install_observer, syscall_name, SyscallObserver};
use crate::host::{add_to_linker, build_engine, build_store};
use crate::kernel::{Kernel, MARKER_ADDR, MARKER_LEN};

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

/// Entry point for `edge-cli trace`. Argv layout:
///
/// - `--diff <path>` reads a baseline file (one syscall name per line);
///   exits non-zero if any baseline syscall is missing from the host trace.
/// - `--no-marker` suppresses the trailing `{"marker":"..."}` JSON line.
/// - Rest = positional wasm path + optional `--` separator + guest argv.
pub async fn run_main(args: &[String]) -> CliResult<i32> {
    let mut baseline: Option<PathBuf> = None;
    let mut emit_marker = true;
    let mut positional: Vec<String> = Vec::with_capacity(args.len());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--diff" {
            let p = it
                .next()
                .ok_or_else(|| CliError::Args("--diff requires a path argument".to_string()))?;
            baseline = Some(PathBuf::from(p));
        } else if a == "--no-marker" {
            emit_marker = false;
        } else {
            positional.push(a.clone());
        }
    }

    let wasm_path = positional
        .first()
        .ok_or_else(|| {
            CliError::Args(
                "usage: edge-cli trace <wasm> [--diff <baseline>] [--no-marker] [--] [args...]"
                    .to_string(),
            )
        })?
        .clone();
    let script_args: Vec<String> = if positional.get(1).map(String::as_str) == Some("--") {
        positional[2..].to_vec()
    } else {
        positional[1..].to_vec()
    };

    let (entries, marker) = run_guest(&wasm_path, &script_args).await?;

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
    eprintln!("edge-cli trace: {} syscalls captured", entries.len());

    // --diff mode: ensure every baseline syscall name was seen in the trace.
    if let Some(path) = baseline {
        let baseline_names: BTreeSet<String> = std::fs::read_to_string(&path)
            .map_err(CliError::Io)?
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        let baseline_static: BTreeSet<&str> = baseline_names.iter().map(String::as_str).collect();
        let missing: Vec<&&str> = baseline_static.difference(&names_seen).collect();
        let extra: Vec<&&str> = names_seen.difference(&baseline_static).collect();
        if missing.is_empty() {
            eprintln!(
                "edge-cli trace: --diff OK — all {} baseline syscalls observed",
                baseline_names.len()
            );
            if !extra.is_empty() {
                eprintln!(
                    "edge-cli trace: --diff backlog (not in baseline, ok): {:?}",
                    extra
                );
            }
            return Ok(0);
        } else {
            eprintln!(
                "edge-cli trace: --diff FAIL — missing baseline syscalls: {:?}",
                missing
            );
            return Ok(1);
        }
    }
    Ok(0)
}

/// Drive the guest and return the captured trace.
///
/// P2-A2: we use the standard `add_to_linker` (no custom dispatch) and
/// install a thread-local `TraceObserver` for the duration of the run.
async fn run_guest(
    wasm_path: &str,
    _script_args: &[String],
) -> CliResult<(Vec<TraceEntry>, String)> {
    let engine = build_engine().map_err(CliError::from)?;
    let mut linker: Linker<Kernel> = Linker::new(&engine);
    add_to_linker(&mut linker).map_err(CliError::from)?;

    // Install the observer for this thread. We restore the prior (likely
    // None) observer on return so this binary is re-entrant-safe.
    let prev = install_observer(Some(std::sync::Arc::new(TraceObserver)));

    let kernel = Kernel::new(vec![], vec![]);
    let mut store = build_store(&engine, kernel);
    let bytes = std::fs::read(wasm_path)
        .map_err(|e| CliError::Args(format!("reading {wasm_path}: {e}")))?;
    let module = if bytes.len() >= 4 && &bytes[0..4] == b"\0asm" {
        wasmtime::Module::new(&engine, &bytes).map_err(CliError::from)?
    } else {
        // SAFETY: callers accept `Module::deserialize` for precompiled
        // artifacts. Same precondition as the deleted `trace-host` driver.
        unsafe { wasmtime::Module::deserialize(&engine, &bytes).map_err(CliError::from)? }
    };
    let instance = linker
        .instantiate_async(&mut store, &module)
        .await
        .map_err(CliError::from)?;
    if let Some(mem) = instance.get_memory(&mut store, "memory") {
        store.data_mut().attach_memory(mem);
    }
    if let Ok(start) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
        let _ = start.call_async(&mut store, ()).await;
    } else if let Ok(start) = instance.get_typed_func::<(), i32>(&mut store, "_start") {
        let _ = start.call_async(&mut store, ()).await;
    } else if let Ok(start) = instance.get_typed_func::<(), i64>(&mut store, "_start") {
        let _ = start.call_async(&mut store, ()).await;
    }

    let entries = TRACE.with(|t| t.borrow().clone());
    let marker = read_marker(&store);
    install_observer(prev);
    Ok((entries, marker))
}

/// Read the pass/fail/skip marker that C conformance tests write at
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
///   - bytes 0..5 == b"SKIP:"        → "SKIP:\<reason up to first NUL>"
///   - anything else                 → "" (no marker)
fn read_marker(store: &Store<Kernel>) -> String {
    let mem = match store.data().memory() {
        Ok(m) => m,
        Err(_) => return String::new(),
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
    if &buf[0..5] == b"SKIP:" {
        let end = buf[5..]
            .iter()
            .position(|&b| b == 0)
            .map(|p| p + 5)
            .unwrap_or(buf.len());
        let reason_bytes = &buf[5..end];
        return format!("SKIP:{}", String::from_utf8_lossy(reason_bytes));
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_positional_is_usage_error() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt.block_on(run_main(&[])).unwrap_err();
        assert!(matches!(err, CliError::Args(_)));
    }

    #[test]
    fn diff_flag_without_path_is_args_error() {
        let a: Vec<String> = vec!["--diff".into()];
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt.block_on(run_main(&a)).unwrap_err();
        assert!(matches!(err, CliError::Args(_)));
    }

    #[test]
    fn extracts_double_dash_separator() {
        // After argv parsing, positional[1..] should be `["a","b"]`.
        let positional: Vec<String> = vec!["foo.wasm".into(), "--".into(), "a".into(), "b".into()];
        let split = if positional.get(1).map(String::as_str) == Some("--") {
            &positional[2..]
        } else {
            &positional[1..]
        };
        assert_eq!(split, &["a".to_string(), "b".to_string()]);
    }
}
