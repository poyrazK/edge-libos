# examples/print_2_plus_2.py — P0 DoD #1.
#
# The simplest sanity check that CPython is alive inside the guest:
# integer arithmetic + write() of the result to stdout. This exercises:
#   * Py_Main argv parsing
#   * bytecode interpreter (BINARY_ADD)
#   * builtin print() -> stdout write() -> NR_WRITE syscall
#
# Expected output: a single line "4".

print(2 + 2)