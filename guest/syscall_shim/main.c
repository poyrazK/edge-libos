/*
 * main.c — CPython entry point.
 *
 * CPython's "main" function is named `Py_Main` in libpython. We provide
 * a wasm `_start` symbol that parses argv, sets up envp, and calls into
 * libpython's Py_Main. The host driver (src/bin/edge_python.rs) loads
 * this wasm, instantiates it, and calls `_start`.
 *
 * CPython expects argv as `int argc, char **argv` and envp as
 * `char **envp`. wasmtime supports both via the wasi-style globals or
 * via custom data passed through the linker. For P0 we use a minimal
 * bridge: argv is hard-coded to ["python"], envp is hard-coded to
 * ["PYTHONUNBUFFERED=1", "HOME=/"].
 *
 * Real argv/envp passing lands in Step 20 (edge-python driver).
 */

#include <stdint.h>

extern int Py_Main(int argc, char **argv);

static char *argv_buf[] = {
    (char *)"python",
    (char *)"-c",
    (char *)"print(2+2)",  /* placeholder; real argv from host in Step 20 */
    0
};

/*
 * _start: wasm entry. libpython's Py_Main never returns — it calls
 * exit() internally. We never reach the `return 0;` below.
 */
__attribute__((visibility("default")))
int _start(void) {
    return Py_Main(3, argv_buf);
}