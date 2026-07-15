//! P3 Tier-8 v2 — M1 integration smoke: the child-thread skeleton
//! (defined in `src/sys/process.rs::run_child_pub`) drives a
//! WAT guest to completion and delivers the exit code back via mpsc.
//!
//! The full `fork_syscall → spawn_child_thread` plumbing lands in M2
//! once `Kernel` carries `Arc<Engine>` + `Arc<Module>` (M3 introduces
//! `ProcessState`). For M1 we hand-build the engine/module on the
//! parent thread, snapshot the parent kernel+memory, hand the
//! snapshot to the child thread via `run_child_pub`, and assert the
//! child delivered its exit code through the mpsc.
//!
//! The WAT fixture is `tests/guests/child_thread_exit.wat`: it
//! calls `NR_EXIT(42)` immediately. Isolating the fixture from a
//! nested `fork()` syscall lets M1 prove the thread-per-child
//! skeleton without depending on v1's deferred-fork work. A
//! separate round-trip fixture (`fork_child_runs.wat`) lands in
//! M7 once the full fork() path works on the child thread.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Notify};
use tokio::time::timeout;

const CHILD_WAT: &str = include_str!("guests/child_thread_exit.wat");

#[tokio::test(flavor = "current_thread")]
async fn child_thread_runs_wasm_and_delivers_exit_code_via_mpsc() {
    // 1. Build engine + module on the parent thread (this would
    //    normally be done by `cli/run.rs` once at startup).
    let mut cfg = wasmtime::Config::new();
    cfg.wasm_component_model(true);
    cfg.wasm_reference_types(true);
    cfg.wasm_simd(true);
    cfg.wasm_threads(true);
    cfg.shared_memory(true);
    cfg.wasm_shared_everything_threads(true);
    cfg.consume_fuel(true);
    let engine = Arc::new(wasmtime::Engine::new(&cfg).expect("engine"));
    let module = Arc::new(
        wasmtime::Module::new(&engine, wat::parse_str(CHILD_WAT).expect("parse wat"))
            .expect("module"),
    );

    // 2. Build parent kernel + linker + store. Attach memory after
    //    instantiate (the canonical order).
    let mut linker: wasmtime::Linker<edge_libos::Kernel> = wasmtime::Linker::new(&engine);
    edge_libos::host::add_to_linker(&mut linker).expect("add_to_linker");
    let parent_kernel = edge_libos::Kernel::new_without_stdio(vec![], vec![]);
    let mut parent_store = edge_libos::build_store(&engine, parent_kernel);
    let instance = linker
        .instantiate_async(&mut parent_store, &module)
        .await
        .expect("instantiate parent");
    if let Some(mem) = instance.get_memory(&mut parent_store, "memory") {
        parent_store.data_mut().attach_memory(mem);
    }

    // 3. Snapshot the parent kernel+memory while at quiescent point.
    let snap = edge_libos::snapshot::try_to_snapshot(parent_store.data(), &parent_store)
        .expect("snapshot");

    // 4. Spawn the child thread via the v2 helper. The child runs
    //    `apply_snapshot` against a fresh Store, then calls _start.
    //    It sends `(child_pid, exit_code)` back via the mpsc.
    let child_pid: i32 = 42; // chosen for the test
    let (tx, mut rx) = mpsc::unbounded_channel::<(i32, i32)>();
    let child_event = Arc::new(Notify::new());

    // Spawn a fresh OS thread to run the child. `run_child_pub`
    // builds its own multi-thread tokio runtime and calls
    // `block_on`. If we called it synchronously on the test thread
    // (which is already inside a `current_thread` runtime), the
    // runtime-in-runtime check would panic. The OS thread boundary
    // escapes the parent's runtime context cleanly.
    let child_event_for_thread = child_event.clone();
    let children_for_thread = Arc::clone(&parent_store.data().process_state.children);
    std::thread::Builder::new()
        .name(format!("edge-test-fork-{child_pid}"))
        .spawn(move || {
            edge_libos::sys::process::run_child_pub(
                engine.clone(),
                module.clone(),
                snap,
                child_pid,
                tx,
                child_event_for_thread,
                children_for_thread,
            );
        })
        .expect("spawn child thread");

    // 5. Wait for the child's exit-code delivery via mpsc.
    let (pid, exit_code) = timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("child did not deliver within 5s")
        .expect("mpsc closed");

    assert_eq!(pid, child_pid, "child PID must match what we passed");
    assert_eq!(
        exit_code, 42,
        "child called NR_EXIT(42); the kernel preserves that as exit_code on the trap, got {exit_code}"
    );

    // M2: the parent's `Kernel.children` map must carry an entry
    // for `child_pid` with `exited == true, exit_code == 42`. The
    // child thread inserts the entry BEFORE invoking `_start`
    // (exited=false) and updates it on exit. This proves the
    // shared `Arc<Mutex<HashMap>>` plumbing works through the
    // runtime boundary.
    let exit = {
        let map = parent_store.data().process_state.children.lock();
        map.get(&child_pid)
            .map(|s| (s.exited, s.exit_code))
            .unwrap_or((false, -1))
    };
    assert_eq!(
        exit,
        (true, 42),
        "parent.children[{child_pid}] must be (exited=true, exit_code=42) after the child finishes"
    );

    // The mpsc delivery itself proves the child thread ran to
    // completion on its own runtime — that's the load-bearing
    // assertion for M1. (`child_event` was reserved for the parent's
    // any-pid `wait4`; a per-child `Arc<Notify>` lands in M5 and
    // gets its own test there.)
    drop(child_event);
}
