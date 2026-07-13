;; tests/guests/serve_one_request.wat
;;
;; P1-8 DoD: serve one HTTP request using the SAME kernel syscall sequence
;; that uvicorn's asyncio event loop drives.

(module
  (import "kernel" "syscall"
    (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))

  (memory (export "memory") 2)

  ;; sockaddr_in at 4096: AF_INET=2, port=18080 (BE=0x46a0), addr=127.0.0.1
  ;; 0x46a0 = 18080.
  (data (i32.const 4096)
    "\02\00\46\a0\7f\00\00\01"
    "\00\00\00\00\00\00\00\00")

  ;; HTTP/1.1 200 response at 8192 (80 bytes)
  (data (i32.const 8192)
    "HTTP/1.1 200 OK\r\n"
    "Content-Type: text/plain\r\n"
    "Content-Length: 2\r\n"
    "Connection: close\r\n"
    "\r\n"
    "ok")

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

    ;; socket
    (local.set $listener (call $syscall (i64.const 41) (i64.const 2) (i64.const 1) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
    (if (i64.lt_s (local.get $listener) (i64.const 0)) (then
      (drop (call $syscall (i64.const 60) (i64.const 11) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))))

    ;; bind
    (local.set $rc (call $syscall (i64.const 49) (local.get $listener) (i64.const 4096) (i64.const 16) (i64.const 0) (i64.const 0) (i64.const 0)))
    (if (i64.lt_s (local.get $rc) (i64.const 0)) (then
      (drop (call $syscall (i64.const 60) (i64.const 12) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))))

    ;; listen
    (local.set $rc (call $syscall (i64.const 50) (local.get $listener) (i64.const 1) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
    (if (i64.lt_s (local.get $rc) (i64.const 0)) (then
      (drop (call $syscall (i64.const 60) (i64.const 13) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))))

    ;; epoll_create1
    (local.set $epfd (call $syscall (i64.const 291) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
    (if (i64.lt_s (local.get $epfd) (i64.const 0)) (then
      (drop (call $syscall (i64.const 60) (i64.const 14) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))))

    ;; epoll_ctl ADD listener
    (local.set $rc (call $syscall (i64.const 233) (local.get $epfd) (i64.const 1) (local.get $listener) (i64.const 16384) (i64.const 0) (i64.const 0)))
    (if (i64.lt_s (local.get $rc) (i64.const 0)) (then
      (drop (call $syscall (i64.const 60) (i64.const 15) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))))

    ;; event loop — serve ONE request then exit. After exit we keep
    ;; looping but the test driver observes `exit_code == 0` and stops us.
    (block $done
      (loop $loop
        ;; epoll_wait(timeout=100ms) — listener is "always ready" in P1-7
        ;; so this returns immediately the first time around. The timeout
        ;; bounds the spin if the host never connects.
        (local.set $dummy (call $syscall (i64.const 232) (local.get $epfd) (i64.const 16512) (i64.const 1) (i64.const 100) (i64.const 0) (i64.const 0)))
        (br_if $done (i64.le_s (local.get $dummy) (i64.const 0)))
        ;; accept4 — suspends until a connection arrives
        (local.set $accepted (call $syscall (i64.const 288) (local.get $listener) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
        (br_if $done (i64.lt_s (local.get $accepted) (i64.const 0)))
        ;; recvfrom — block until the request bytes arrive
        (local.set $n (call $syscall (i64.const 45) (local.get $accepted) (i64.const 16640) (i64.const 256) (i64.const 0) (i64.const 0) (i64.const 0)))
        ;; If we got any bytes, send the canned 200 and exit.
        (if (i64.gt_s (local.get $n) (i64.const 0)) (then
          ;; 85 = "HTTP/1.1 200 OK\r\n" + "Content-Type: text/plain\r\n"
          ;;     + "Content-Length: 2\r\n" + "Connection: close\r\n"
          ;;     + "\r\n" + "ok"
          (drop (call $syscall (i64.const 44) (local.get $accepted) (i64.const 8192) (i64.const 85) (i64.const 0) (i64.const 0) (i64.const 0)))
          (drop (call $syscall (i64.const 48) (local.get $accepted) (i64.const 1) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
          (drop (call $syscall (i64.const 3) (local.get $accepted) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
          (drop (call $syscall (i64.const 60) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
          (br $done)))
        ;; Otherwise, loop again.
        (br $loop)))

    ;; Halt — wasmtime will report a trap since we have no return value,
    ;; which the test driver treats as "guest exited". The exit_code
    ;; above (set by NR_EXIT) is what the driver inspects.
    (unreachable)))
