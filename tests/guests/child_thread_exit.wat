;; P3 Tier-8 v2 — child-thread child-only fixture (M1).
;;
;; The simplest possible fixture that proves `run_child_pub` drives
;; a fresh Store<Kernel> to completion. The child calls
;; `NR_EXIT(42)` immediately; no nested fork/wait4. This isolates
;; the thread-per-child skeleton from v1's deferred-fork work —
;; a separate fixture (`fork_child_runs.wat`) is added in M7 for
;; the full fork round-trip.
(module
  (import "kernel" "syscall"
    (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (memory (export "memory") 1)

  (func $exit (param $code i64)
    (call $syscall
      (i64.const 60) (local.get $code) (i64.const 0) (i64.const 0)
      (i64.const 0) (i64.const 0) (i64.const 0))
    unreachable)

  (func (export "_start") (result i64)
    (call $exit (i64.const 42))
    (i64.const 0)))
