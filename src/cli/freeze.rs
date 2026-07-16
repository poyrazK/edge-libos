//! `edge-cli freeze <wasm> [--] [args...] --out <path>`.
//!
//! P2-D3.5: replaces the D3.3 stub with the full freeze flow.
//!
//! Drive the guest until at least one socket listener is materialized,
//! rewrite the ephemeral-port drift into the kernel state, then write
//! a postcard snapshot via `try_to_snapshot` + `write_snapshot_file`.
//!
//! Quiescence: poll every 10 ms for up to 10 s for `gs.listener.is_some()`
//! (NOT `is_listening()` — that fires at `listen()` time, before the
//! kernel-assigned port is known). Once materialized, capture
//! `listener.local_addr()?.port()` and overwrite `SocketInner.bound.port`
//! for any IPv4 listener with `port == 0`. This is the ephemeral-port-drift
//! fix: without it, snapshots taken from `bind(0.0.0.0:0)` would record
//! `bound.port = 0` and `apply_snapshot` would bind a DIFFERENT ephemeral
//! port than the snapshot says, breaking `serve`.

use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use wasmtime::Linker;

use crate::cli::error::{CliError, CliResult};
use crate::cli::util::call_start;
use crate::host::{add_to_linker, build_engine, build_store};
use crate::kernel::Kernel;
use crate::snapshot::{try_to_snapshot, write_snapshot_file, KernelSnapshot};

/// Entry point for `edge-cli freeze`. Argv layout:
///
/// - `--out <path>` (required): destination snapshot path.
/// - Positional wasm path (required).
/// - Optional `--` separator; the rest become the guest argv
///   (forwarded to `Kernel::new`).
pub async fn run_main(args: &[String]) -> CliResult<i32> {
    let mut out: Option<PathBuf> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--out" {
            let p = it.next().ok_or_else(|| {
                CliError::Args("freeze: --out requires a path argument".to_string())
            })?;
            out = Some(PathBuf::from(p));
        } else {
            positional.push(a.clone());
        }
    }
    let out = out.ok_or_else(|| {
        CliError::Args("usage: edge-cli freeze <wasm> [--] [args...] --out <path>".to_string())
    })?;
    if positional.is_empty() {
        return Err(CliError::Args(
            "usage: edge-cli freeze <wasm> [--] [args...] --out <path>".to_string(),
        ));
    }
    let wasm_path = positional[0].clone();
    let guest_args: Vec<String> = if positional.get(1).map(String::as_str) == Some("--") {
        positional[2..].to_vec()
    } else {
        positional[1..].to_vec()
    };

    let snap = freeze_snapshot(&wasm_path, &guest_args).await?;
    write_snapshot_file(&out, &snap)?;
    eprintln!(
        "edge-cli freeze: wrote {} ({} pages, {} fds)",
        out.display(),
        snap.pages.len(),
        snap.fds.entries.len()
    );
    Ok(0)
}

/// Instantiate + drive the guest + capture the snapshot. Split out so
/// the in-module tests can exercise argv flow without touching the FS.
async fn freeze_snapshot(wasm_path: &str, guest_args: &[String]) -> CliResult<KernelSnapshot> {
    let engine = build_engine()?;
    let mut linker: Linker<Kernel> = Linker::new(&engine);
    add_to_linker(&mut linker)?;

    let kernel = Kernel::new(guest_args.to_vec(), vec![]);
    let mut store = build_store(&engine, kernel);

    let bytes = std::fs::read(wasm_path)
        .map_err(|e| CliError::Args(format!("freeze: reading {wasm_path}: {e}")))?;
    // P3-D3.5-followup-1 (ADR 0005): SHA-256 the wasm bytes once so
    // `edge-cli serve` can refuse to apply the resulting snapshot
    // onto a mismatched wasm. Hash covers the raw file bytes —
    // for raw `.wasm` files this is the bytes `Module::new` parses;
    // for precompiled wasmtime artifacts (the `Module::deserialize`
    // branch below) this is the bytes the serve side will also
    // deserialize. Same bytes in, same hash out.
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let module_sha256: [u8; 32] = hasher.finalize().into();
    let module = if bytes.len() >= 4 && &bytes[0..4] == b"\0asm" {
        wasmtime::Module::new(&engine, &bytes)?
    } else {
        // SAFETY: callers accept `Module::deserialize` for precompiled
        // artifacts; same precondition as the deleted edge-python driver.
        unsafe { wasmtime::Module::deserialize(&engine, &bytes) }?
    };
    let instance = linker.instantiate_async(&mut store, &module).await?;
    if let Some(mem) = instance.get_memory(&mut store, "memory") {
        store.data_mut().attach_memory(mem);
    }

    // ADR 0007 §6: install a quiesce_notify so a SIGUSR1 to this
    // process can interrupt a parked server-style guest at the next
    // blocking syscall and let freeze take a snapshot. The notify
    // is Send+Sync so the listener thread can fire it without
    // touching the !Send Store.
    let quiesce = Arc::new(Notify::new());
    // `process_state` is `Arc<ProcessState>` (shared across threads
    // post-fork per ADR 0006). Right after `Kernel::new` it has
    // strong_count==1 so we can get_mut; if that ever changes, the
    // right move is to add a `Kernel::set_quiesce_notify` helper
    // that handles the contention case rather than open-coding here.
    Arc::get_mut(&mut store.data_mut().process_state)
        .expect("process_state must be uniquely owned at freeze setup")
        .quiesce_notify = Some(quiesce.clone());

    // Spawn a dedicated thread that listens for SIGUSR1 and fires the
    // notify. The listener never touches the Store; it only pokes
    // the Send+Sync Arc<Notify>.
    spawn_sigusr1_listener(quiesce.clone())?;

    // Drive the guest. Three arms:
    //   1. call_start returns (short-lived guest exited)
    //   2. SIGUSR1 fires the quiesce_notify (operator-driven snapshot)
    //   3. 10-second outer timeout (server-style guest still parked)
    // `build_kernel_snapshot` does the ephemeral-port-drift fix inline
    // (see `src/snapshot.rs:561`), rewriting `bound.port` to the
    // materialized listener's `local_addr().port()` if `bound.port == 0`.
    //
    // We can't poll kernel state mid-_start because `call_start` holds
    // an exclusive borrow of `Store` for the lifetime of the future —
    // see the comment at `src/cli/util.rs`. Doing the rewrite inside
    // `build_kernel_snapshot` (which holds the per-socket mutex briefly
    // and never across `.await`) is the clean place to observe the
    // materialized port without racing the guest task.
    let timeout = Duration::from_secs(10);
    tokio::select! {
        _ = call_start(&instance, &mut store) => {}
        _ = quiesce.notified() => {}
        _ = tokio::time::sleep(timeout) => {}
    };
    let mut snap = try_to_snapshot(store.data(), &store)?;
    // Embed the freeze-side wasm hash on the snapshot. `try_to_snapshot`
    // builds with module_sha256 = [0u8; 32] because the guest-driven
    // `NR_SNAPSHOT` path can't supply one — we overwrite here on the
    // CLI path. (See ADR 0005 D5 design rationale.)
    snap.module_sha256 = module_sha256;
    Ok(snap)
}

/// Spawn a dedicated OS thread that listens for `SIGUSR1` and fires
/// `notify.notify_waiters()` on receipt. Uses a `signalfd`-style
/// approach via a self-pipe:
///
/// 1. Create a pipe; the read end goes to a tokio listener task
///    (drained on a dedicated current-thread runtime so we don't
///    fight the host runtime's signal driver), the write end is
///    captured by a `libc::sigaction` handler.
/// 2. On SIGUSR1, the handler `write(2)`s 1 byte into the pipe.
/// 3. The listener task reads the byte and calls `notify.notify_waiters()`,
///    waking the freeze `select!` arm.
///
/// We avoid `tokio::signal::unix::signal(SignalKind::user_defined1())`
/// because two tokio runtimes in the same process can't both register
/// SIGUSR1 handlers — tokio's signal driver is keyed off the first
/// runtime to register. Raw `sigaction` + self-pipe sidesteps the
/// driver entirely.
///
/// Returns an `Err` only if the pipe / thread can't be set up; the
/// listener itself is infallible from the caller's perspective.
fn spawn_sigusr1_listener(notify: Arc<Notify>) -> CliResult<()> {
    use std::sync::atomic::{AtomicI32, Ordering};

    // Shared fd storage: set once by the spawning thread (parent)
    // before installing the handler, read on every signal. The
    // spawning fence on `Ordering::Release` pairs with the
    // `Ordering::Acquire` in the handler to ensure the fd is
    // visible.
    static WRITE_FD: AtomicI32 = AtomicI32::new(-1);

    // Create the self-pipe. `pipe2(O_CLOEXEC)` would be cleaner but
    // isn't in the `libc` crate's portable bindings; use plain
    // `pipe(2)` and rely on the fact that nothing in this process
    // forks.
    let mut fds = [0i32; 2];
    // SAFETY: fds is a valid [i32; 2]; pipe(2) initializes it.
    let pr = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if pr != 0 {
        return Err(CliError::Args(format!(
            "freeze: SIGUSR1 self-pipe failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    let read_fd = fds[0];
    let write_fd = fds[1];

    // Publish write_fd to the handler BEFORE installing sigaction.
    WRITE_FD.store(write_fd, Ordering::Release);

    // SAFETY: handler is async-signal-safe (only libc::write to a
    // known-good pipe fd).
    unsafe extern "C" fn sigusr1_handler(_sig: libc::c_int) {
        let fd = WRITE_FD.load(Ordering::Acquire);
        if fd < 0 {
            return;
        }
        let b: u8 = 1;
        // SAFETY: fd is a live pipe write end; ignore short writes
        // and EAGAIN (1-byte writes don't short, and the pipe won't
        // fill from operator-driven signals).
        unsafe {
            libc::write(fd, &b as *const u8 as *const libc::c_void, 1);
        }
    }

    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    sa.sa_sigaction = sigusr1_handler as *const () as libc::sighandler_t;
    // No SA_RESTART — we WANT the signal to interrupt syscalls.
    sa.sa_flags = 0;
    sa.sa_mask = unsafe { std::mem::zeroed() };
    // SAFETY: sa is initialized; sigaction(2) is async-signal-safe.
    let rc = unsafe { libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut()) };
    if rc != 0 {
        let e = std::io::Error::last_os_error();
        // SAFETY: close both fds we own before returning.
        unsafe {
            libc::close(read_fd);
            libc::close(write_fd);
        }
        WRITE_FD.store(-1, Ordering::Release);
        return Err(CliError::Args(format!(
            "freeze: SIGUSR1 sigaction failed: {e}"
        )));
    }

    // Spawn a thread that drives a dedicated current-thread tokio
    // runtime, wrapping read_fd in a `tokio::net::unix::pipe` and
    // draining it. When a byte arrives, fire the notify.
    std::thread::Builder::new()
        .name("edge-cli-freeze-sigusr1".to_string())
        .spawn(move || {
            // Catch any panic so it doesn't take down the process —
            // we'd rather fail the freeze than the entire edge-cli.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // SAFETY: read_fd is a live pipe read end owned by this thread.
                let owned_pipe = unsafe { std::os::fd::OwnedFd::from_raw_fd(read_fd) };
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        eprintln!("edge-cli freeze: SIGUSR1 listener runtime failed: {e}");
                        drop(owned_pipe);
                        return;
                    }
                };
                rt.block_on(async move {
                    let pipe = tokio::net::unix::pipe::Receiver::from_owned_fd(owned_pipe)
                        .expect("tokio pipe from_owned_fd");
                    use tokio::io::AsyncReadExt;
                    let mut pipe = pipe;
                    let mut buf = [0u8; 64];
                    loop {
                        match pipe.read(&mut buf).await {
                            Ok(0) => break, // EOF — write end closed, process exiting.
                            Ok(_) => {
                                notify.notify_waiters();
                            }
                            Err(e) => {
                                eprintln!("edge-cli freeze: SIGUSR1 pipe read failed: {e}");
                                break;
                            }
                        }
                    }
                    // SAFETY: close write_fd on the way out so the
                    // reader hits EOF and exits. The handler is still
                    // installed but fd == -1 short-circuits; if a stray
                    // SIGUSR1 arrives after this, it's a no-op.
                    unsafe {
                        libc::close(write_fd);
                    }
                });
            }));
            if let Err(e) = result {
                eprintln!("edge-cli freeze: SIGUSR1 listener thread panicked: {e:?}");
            }
        })
        .map_err(|e| CliError::Args(format!("freeze: SIGUSR1 listener spawn failed: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// Minimal WAT that calls `exit(0)` immediately on `_start` —
    /// drives the freeze timeout down to the natural exit path
    /// rather than the 10 s server-style fallback. Imports
    /// `kernel.syscall` so the linker can satisfy the import.
    const EXIT_FAST_WAT: &str = r#"
        (module
          (import "kernel" "syscall"
            (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
          (memory (export "memory") 1)
          (func (export "_start") (result i64)
            ;; exit(0) → trap-as-exit, caught by `call_start`.
            (call $syscall
              (i64.const 60) (i64.const 0)
              (i64.const 0) (i64.const 0)
              (i64.const 0) (i64.const 0) (i64.const 0))))
    "#;

    #[test]
    fn argv_requires_out_path() {
        let r = rt();
        let err = r.block_on(run_main(&["foo.wasm".into()])).unwrap_err();
        assert!(matches!(err, CliError::Args(_)), "got {err:?}");
    }

    #[test]
    fn argv_requires_wasm_path() {
        let r = rt();
        let err = r
            .block_on(run_main(&["--out".into(), "/tmp/x".into()]))
            .unwrap_err();
        assert!(matches!(err, CliError::Args(_)), "got {err:?}");
    }

    #[test]
    fn out_flag_without_value_is_args_error() {
        let r = rt();
        let err = r.block_on(run_main(&["--out".into()])).unwrap_err();
        assert!(matches!(err, CliError::Args(_)), "got {err:?}");
    }

    /// P3-D3.5-followup-1 / ADR 0005: `freeze_snapshot` embeds the
    /// SHA-256 of the wasm file bytes onto the returned
    /// `KernelSnapshot.module_sha256` so a serve-side mismatch
    /// becomes a hard error. Use a tmpfile to drive the
    /// file-path entry point — no FS mocking needed.
    #[tokio::test(flavor = "current_thread")]
    async fn freeze_writes_module_sha256_to_snapshot() {
        let wasm_bytes = wat::parse_str(EXIT_FAST_WAT).expect("compile WAT");
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&wasm_bytes);
        let expected: [u8; 32] = hasher.finalize().into();

        let dir = tempdir_in_target();
        let wasm_path = dir.join("guest.wasm");
        std::fs::write(&wasm_path, &wasm_bytes).expect("write wasm");

        let snap = freeze_snapshot(wasm_path.to_str().expect("utf8 path"), &[])
            .await
            .expect("freeze_snapshot must succeed on a fast-exit guest");
        assert_eq!(
            snap.module_sha256, expected,
            "freeze must embed SHA-256 of the wasm bytes"
        );
        // Sanity: the fixture really did `exit(0)` so the snapshot
        // captured a deterministic post-_start state (no listener,
        // just the kernel seed + 1 page of linear memory).
        assert_eq!(snap.exit_code, Some(crate::snapshot::endian::LeI32(0)));
    }

    /// Minimal `tempfile`-style helper scoped to this test module —
    /// returns a fresh dir under `target/ci/freeze-hash-tests-<pid>`
    /// that the caller can drop on test exit. Avoids pulling the
    /// `tempfile` crate as a direct dep just for two tests.
    fn tempdir_in_target() -> std::path::PathBuf {
        let pid = std::process::id();
        let dir = std::path::PathBuf::from(format!("target/ci/freeze-hash-tests-{pid}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create tempdir");
        dir
    }
}
