;; tests/guests/serve_forever.wat
;;
;; P2-D3.6+review (F.4): fixture for an end-to-end HTTP smoke through
;; `edge-cli serve`. Pairs with the 5th test in
;; `tests/edge_cli_freeze_serve_smoke.rs::serve_handles_http_request_after_apply`.
;;
;; Two-mode `_start`:
;;
;;   FRESH BOOT (memory[300] == 0): socket → bind(:0) → listen →
;;     epoll_create1 → epoll_ctl ADD. Stash the listener fd at
;;     memory[300] so the next apply_snapshot restores it. Park in
;;     `epoll_wait(timeout=10s)` in a loop so the listener stays
;;     materialized (the kernel-assigned port is live); the freeze
;;     driver's 10s timeout fires while we sleep, taking the
;;     snapshot with the listener materialized and the drift fix at
;;     `src/snapshot.rs:564-585` free to rewrite `bound.port`.
;;
;;   RESTORED BOOT (memory[300] != 0): the kernel has reopened a
;;     listener at the port from the snapshot (rewritten by
;;     `serve --port <p>` if used). Skip socket/bind/listen and use
;;     the inherited fd directly. Enter the HTTP loop on it.
;;
;; HTTP loop (both modes): epoll_wait → accept4 → recvfrom →
;;   sendto → close, never exits. The host kills the subprocess.

(module
  (import "kernel" "syscall"
    (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))

  (memory (export "memory") 2)

  ;; sockaddr_in at 4096: AF_INET=2, port=0 (BE), addr=127.0.0.1.
  ;; struct sockaddr_in { sa_family_t sin_family; u16 sin_port; u32 sin_addr; u8 pad[8]; }
  ;; bytes[0..2] = family (LE) = 0x0002
  ;; bytes[2..4] = port   (BE) = 0x0000  → kernel picks ephemeral
  ;; bytes[4..8] = addr   (BE) = 127.0.0.1 = \7f\00\00\01
  ;; bytes[8..16] = padding (zeros)
  (data (i32.const 4096)
    "\02\00\00\00\7f\00\00\01"
    "\00\00\00\00\00\00\00\00")

  ;; HTTP/1.1 200 response at 8192 (85 bytes).
  (data (i32.const 8192)
    "HTTP/1.1 200 OK\r\n"
    "Content-Type: text/plain\r\n"
    "Content-Length: 2\r\n"
    "Connection: close\r\n"
    "\r\n"
    "ok")

  ;; LISTENER_FD_ADDR = 300 (8 bytes, i64 LE; 0 = "fresh boot").
  ;; Initialized to zero by the data segment (default 0).

  ;; epoll_event at 16384: EPOLLIN, data=0
  (data (i32.const 16384)
    "\01\00\00\00\00\00\00\00\00\00\00\00")

  ;; epoll_wait events buffer at 16512 (one 12B event)
  (data (i32.const 16512) "\00\00\00\00\00\00\00\00\00\00\00\00")

  ;; recvfrom buffer at 16640 (256B)

  (func (export "_start")
    (local $listener i64) (local $epfd i64) (local $accepted i64)
    (local $n i64) (local $dummy i64)
    (local $rc i64)
    (local $inherited i64)

    ;; Read the saved listener fd from memory[300]. Non-zero means
    ;; apply_snapshot restored it from a previous freeze — use the
    ;; inherited fd directly. Zero means fresh boot — do the full
    ;; socket/bind/listen and stash the fd.
    (local.set $inherited (i64.load (i32.const 300)))
    (if (i64.eqz (local.get $inherited)) (then
      ;; Fresh boot.
      (local.set $listener (call $syscall (i64.const 41) (i64.const 2) (i64.const 1) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
      (if (i64.lt_s (local.get $listener) (i64.const 0)) (then
        (drop (call $syscall (i64.const 60) (i64.const 21) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))))
      (local.set $rc (call $syscall (i64.const 49) (local.get $listener) (i64.const 4096) (i64.const 16) (i64.const 0) (i64.const 0) (i64.const 0)))
      (if (i64.lt_s (local.get $rc) (i64.const 0)) (then
        (drop (call $syscall (i64.const 60) (i64.const 22) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))))
      (local.set $rc (call $syscall (i64.const 50) (local.get $listener) (i64.const 1) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
      (if (i64.lt_s (local.get $rc) (i64.const 0)) (then
        (drop (call $syscall (i64.const 60) (i64.const 23) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))))
      ;; Stash the fd so a future apply_snapshot restores it.
      (i64.store (i32.const 300) (local.get $listener))
      (local.set $epfd (call $syscall (i64.const 291) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
      (if (i64.lt_s (local.get $epfd) (i64.const 0)) (then
        (drop (call $syscall (i64.const 60) (i64.const 24) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))))
      (local.set $rc (call $syscall (i64.const 233) (local.get $epfd) (i64.const 1) (local.get $listener) (i64.const 16384) (i64.const 0) (i64.const 0)))
      (if (i64.lt_s (local.get $rc) (i64.const 0)) (then
        (drop (call $syscall (i64.const 60) (i64.const 25) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))))
      ;; FRESH MODE: fall through into the HTTP loop. After a few
      ;; accept4 iterations, the listener may be taken out (Phase 1b)
      ;; and not put back until a peer connects. The freeze driver's
      ;; outer timeout fires somewhere in this loop and snapshots
      ;; whatever the kernel state happens to be. The drift fix at
      ;; `src/snapshot.rs:564-585` only fires when `guard.listener` is
      ;; Some — which is timing-dependent. The 5th smoke tolerates
      ;; both outcomes: a snapshot with the listener materialized
      ;; (drift fix rewrites the port) OR a snapshot where the
      ;; listener was taken out (bound.port stays 0; --port override
      ;; then takes over).
      )
    (else
      ;; RESTORED MODE: skip socket/bind/listen; use the inherited fd.
      (local.set $listener (local.get $inherited))
      (local.set $epfd (call $syscall (i64.const 291) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
      (if (i64.lt_s (local.get $epfd) (i64.const 0)) (then
        (drop (call $syscall (i64.const 60) (i64.const 24) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))))
      (local.set $rc (call $syscall (i64.const 233) (local.get $epfd) (i64.const 1) (local.get $listener) (i64.const 16384) (i64.const 0) (i64.const 0)))
      (if (i64.lt_s (local.get $rc) (i64.const 0)) (then
        (drop (call $syscall (i64.const 60) (i64.const 25) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))))))

    ;; HTTP loop — never exits. The fresh-mode park loop above is
    ;; unreachable here because (then ... (loop $park ... br $park))
    ;; never falls through. The restored-mode path falls through to
    ;; this loop and serves requests on the inherited listener.
    (loop $forever
      (local.set $dummy (call $syscall (i64.const 232) (local.get $epfd) (i64.const 16512) (i64.const 1) (i64.const 1000) (i64.const 0) (i64.const 0)))
      (local.set $accepted (call $syscall (i64.const 288) (local.get $listener) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
      (local.set $n (call $syscall (i64.const 45) (local.get $accepted) (i64.const 16640) (i64.const 256) (i64.const 0) (i64.const 0) (i64.const 0)))
      (if (i64.gt_s (local.get $n) (i64.const 0)) (then
        (drop (call $syscall (i64.const 44) (local.get $accepted) (i64.const 8192) (i64.const 85) (i64.const 0) (i64.const 0) (i64.const 0)))
        (drop (call $syscall (i64.const 48) (local.get $accepted) (i64.const 1) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
        (drop (call $syscall (i64.const 3) (local.get $accepted) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))))
      (br $forever))))