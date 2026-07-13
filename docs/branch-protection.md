# Branch protection & required checks

This document is human-facing instructions for configuring GitHub branch
protection on `main`. The CI workflow at `.github/workflows/ci.yml` is the
single source of truth for required checks.

## Goal

Every PR landing in `main` must pass **all five** of the parallel CI
jobs on `verify`. Direct pushes to `main` should be discouraged (the
project uses direct-to-main today per `HANDOFF.md §4.6`, but the
protection works either way).

## CI job graph (P2-CI-2)

```
                    ┌──▶ rust-tests  ──┐
tools ──▶ build ────┤                   │
                    ├──▶ c-tests ──────┤── all required to pass
                    └──▶ reproduce ────┘
```

| Job | What it does | Why it must be green |
|-----|--------------|----------------------|
| `tools` | Checkout, rust-toolchain (1.93.0), cargo + wasm32 caches, zig 0.16.0 (tarball), wabt, strace | Catch Rust toolchain drift, cache key errors, zig install breakage |
| `build` | `cargo build --release` for trace-host + edge-python; uploads both as a workflow artifact | Catches `cargo build` regressions BEFORE the three downstream jobs waste minutes rebuilding |
| `rust-tests` | `cargo test --release` (≈197 tests across unit + integration + EFAULT fuzzer) | The actual Rust correctness gate |
| `c-tests` | `bash tests/conformance/runner.sh` (44/44 marker-enforced) using zig cc + downloaded trace-host | The actual C correctness gate — **this is the P1 false-pass root-cause patch's home** |
| `reproduce` | `bash scripts/reproduce_dod.sh` (with `SKIP_*` env hooks to avoid duplicating cargo test + c-tests) | End-to-end integration: dev_setup, guest build (if submodule), DoD smoke tests, count totals |

## Steps (one-time setup in the GitHub UI)

1. Open the repository on GitHub.
2. **Settings → Branches → Add rule**.
3. **Branch name pattern**: `main`.
4. Enable **Require a pull request before merging** (recommended).
5. Enable **Require status checks to pass before merging**.
6. Under **Status checks that are required**, search for and add **all five** job names:
   - `tools`
   - `build`
   - `rust-tests`
   - `c-tests`
   - `reproduce`
7. Optional but recommended:
   - **Require branches to be up to date before merging**.
   - **Do not allow bypassing the above settings**.
8. Save.

After this is in place, any green PR gets five ✅ checks and is allowed to merge; a red or missing `c-tests` blocks the merge, etc.

## Required-check checklist

A failing or missing check on any one of these blocks the merge button:

| Check | Catches |
|-------|---------|
| `tools` | Toolchain version drift (1.93.0 not pinned), zig install breakage, cache-key errors |
| `build` | Compilation errors BEFORE the long tests run (faster signal than letting rust-tests discover them) |
| `rust-tests` | Rust unit + integration + EFAULT fuzzer regressions |
| `c-tests` | C conformance regressions (marker-enforced, post-P1 fix) |
| `reproduce` | End-to-end DoD regressions (dev_setup / guest build / DoD smokes / count totals) |

## When you add a new required check

If you add a new job to `.github/workflows/ci.yml` that should be
required (for example, a fuzz job in P2-G2 or a metering job in P2-F2),
come back here and add it to the "Status checks that are required" list.
Use the **job name** (not the workflow file name or step name) — GitHub
tracks checks by job name.

## Wall-clock target

After P2-CI-2's parallelism landed, the verified goal is:

- **First run (cold cache)**: ≤ 6 min
- **Subsequent runs (cache hit)**: ≤ 4 min

The previous single-job shape took ~13 min wall-clock on the same
machine. Parallelism shaves ~7 min on cache-hit runs, and amortizes
against the 2 vCPU GitHub-hosted runner's under-utilization.

## Rolling back

If you need to revert to a single-job shape temporarily, replace the
contents of `.github/workflows/ci.yml` with the version at commit
`a9b6ada` (the last single-job shape that successfully downloaded zig).
After debugging, restore the parallel shape from HEAD.
