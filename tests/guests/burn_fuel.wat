;; tests/guests/burn_fuel.wat
;;
;; P2-metering M6: fixture for the integration test
;; `edge_cli_metering_traps_on_out_of_fuel` in
;; `tests/edge_cli_metering_smoke.rs`.
;;
;; Burns a known, large amount of fuel via a tight `loop` that does
;; only local arithmetic and conditional branch (no host calls, no
;; memory ops, no `br_if`). The test passes `--cpu-budget-ms <N>`
;; to `edge-cli run`; with budget too small, the guest traps on
;; `OutOfFuel`; with budget large enough, the loop completes and
;; exits via NR_EXIT.
;;
;; The loop counter is read from linear memory at offset 256
;; (8 bytes, i64 LE). A non-zero value means "do THAT many loop
;; iterations and exit cleanly"; zero (default) means "loop
;; forever — the only way out is OutOfFuel or NR_EXIT from the
;; test driver". The test writes 0 to ensure the trap path.

(module
  (import "kernel" "syscall"
    (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))

  (memory (export "memory") 1)

  ;; ITERATIONS_ADDR = 256 (8 bytes, i64 LE; 0 = "loop forever").
  (data (i32.const 256) "\00\00\00\00\00\00\00\00")

  ;; PASS_MARKER_ADDR = 4096 (the same MARKER_ADDR used by the C
  ;; conformance tests). When the fixture finishes the bounded
  ;; loop, it writes "PASS" here so the test driver can confirm
  ;; the loop completed without needing to inspect a syscall return
  ;; (which the trap from NR_EXIT also collapses).
  (data (i32.const 4096) "PASS")

  (func (export "_start") (result i64)
    (local $iters i64) (local $done i64) (local $bounded i64)

    ;; Read the iteration count. Zero = loop forever.
    (local.set $iters (i64.load (i32.const 256)))
    ;; bounded = (iters != 0) ? 1 : 0. With iters == 0 we never
    ;; decrement, so the exit condition below never fires.
    (local.set $bounded
      (select (i64.const 1) (i64.const 0)
        (i64.ne (local.get $iters) (i64.const 0))))

    (local.set $done (i64.const 0))
    (block $exit
      (loop $forever
        ;; Exit when we had a bounded counter AND we've burned
        ;; exactly that many iters. With bounded==0 (iters==0
        ;; in memory), this br_if never fires — loop forever.
        (br_if $exit
          (i32.and
            (i64.ne (local.get $bounded) (i64.const 0)) ;; i32 from i64.ne
            (i64.eq (local.get $iters) (i64.const 0))))  ;; i32 from i64.eq
        ;; Burn fuel: local arithmetic + a couple of locals.
        (local.set $iters (i64.sub (local.get $iters) (i64.const 1)))
        (local.set $done (i64.add (local.get $done) (i64.const 1)))
        (br $forever)))

    ;; Clean exit (only reached when the bounded counter hit 0).
    (call $syscall (i64.const 60) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))

  ;; Optional helper: the test driver uses `_start` directly.
  )