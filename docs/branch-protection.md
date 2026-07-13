# Branch protection & required checks

This document is human-facing instructions for configuring GitHub branch
protection on `main`. The CI workflow at `.github/workflows/ci.yml` is the
single source of truth for required checks.

## Goal

Every PR landing in `main` must pass **all four** parallel CI jobs on
`verify`. Direct pushes to `main` should be discouraged (the project uses
direct-to-main today per `HANDOFF.md §4.6`, but the protection works
either way).

## CI job graph (P2-CI-2)

```
                          ┌──▶ c-conformance ──┐
tools ──▶ build ──────────┤                     ├── all required
                          └──▶ reproduce ──────┘
```

**Critical design point: `cargo` runs ONCE per CI invocation, inside
`build`.** Each GitHub-hosted runner has its own `/home/runner` filesystem,
no shared target/. Running `cargo build` AND `cargo test` on separate
runners (as the earlier 5-job shape did) means each rebuilds the
dependency graph from cache. By folding both into `build`, cargo runs
once and shares its build artifacts with the test-binary compilation in
the same job.

| Job | What it runs | Cargo? | Catches |
|-----|--------------|--------|---------|
| `tools` | Checkout, rust-toolchain (1.93.0), cargo + wasm32 caches, zig 0.16.0 (tarball), wabt, strace | no | Rust toolchain drift, cache-key errors, zig install breakage |
| `build` | `cargo build --release` (trace-host + edge-python) + `cargo test --release` + `cargo test --release --test strace_baseline_diff`; uploads binaries as a 7-day workflow artifact | **yes — once** | **Everything cargo can catch.** Compilation errors, all 197 Rust tests, strace baseline subset |
| `c-conformance` | Downloads trace-host artifact, reinstalls zig, runs `bash tests/conformance/runner.sh` (marker-enforced) | no | The actual C correctness gate — **this is the P1 false-pass root-cause patch's home** |
| `reproduce` | Downloads trace-host + edge-python artifacts, runs `reproduce_dod.sh` with `SKIP_*` env hooks to avoid duplicating build's work | minimal — only targeted smoke tests | Integration smoke: dev_setup / guest build / DoD smokes / count totals |

## Steps (one-time setup in the GitHub UI)

1. Open the repository on GitHub.
2. **Settings → Branches → Add rule**.
3. **Branch name pattern**: `main`.
4. Enable **Require a pull request before merging** (recommended).
5. Enable **Require status checks to pass before merging**.
6. Under **Status checks that are required**, search for and add **all four** job names:
   - `tools`
   - `build`
   - `c-conformance`
   - `reproduce`
7. Optional but recommended:
   - **Require branches to be up to date before merging**.
   - **Do not allow bypassing the above settings**.
8. Save.

After this is in place, any green PR gets four ✅ checks and is allowed to merge.

## Required-check checklist

A failing or missing check on any one of these blocks the merge button:

| Check | Catches |
|-------|---------|
| `tools` | Toolchain version drift (1.93.0 not pinned), zig install breakage, cache-key errors |
| `build` | Compile errors, ALL Rust tests, strace-baseline subset — the single cargo gate |
| `c-conformance` | C conformance regressions (44/44 marker-enforced, post-P1 fix) |
| `reproduce` | End-to-end integration regressions (dev_setup / guest build / DoD smokes / count totals) |

## Why this design

### Why one cargo run instead of two (the P2-CI-2 revision)

Earlier CI shape (P2-CI-1) had `rust-tests` as a separate job that ran
`cargo test --release` independently of `build`. The user's intuition
was correct: each runner started cold on its own target/, so cargo
rebuilt the dependency graph twice per CI invocation (once in `build`,
once in `rust-tests`). That burned 2-4 minutes of wasted compile time
per run on cache-hit cycles.

The fix: fold `cargo build` + `cargo test` + `cargo test --test
strace_baseline_diff` all into `build`. Cargo's intermediate
artifacts from `cargo build` are reused by `cargo test` (which compiles
only the test binaries that depend on them). One cache key, one
runner, one cargo invocation.

### Why not share `target/` across runners

GitHub-hosted runners have no shared filesystem. The only ways to
share `target/` are:
- `actions/cache@v4` with a shared key — concurrent writes from multiple
  runners corrupt the cache (race condition).
- `actions/upload-artifact` + `actions/download-artifact` — works but
  requires the artifact to be a finished binary (or a tarball of target/),
  both of which add latency that exceeds the savings.

Per-job `Swatinem/rust-cache@v2` keyed on `Cargo.lock` is the right
granularity. The trade-off is "one cargo run per CI invocation" —
which is what we have now.

## When you add a new required check

If you add a new job to `.github/workflows/ci.yml` that should be
required (for example, a fuzz job in P2-G2 or a metering job in P2-F2),
come back here and add it to the "Status checks that are required"
list. Use the **job name** (not the workflow file name or step name) —
GitHub tracks checks by job name.

## Wall-clock target

- **First run (cold cache)**: ≤ 6 min
- **Subsequent runs (cache hit)**: ≤ 4 min

The 5-job fan-out predecessor shape took ~13 min on first run; the
current 4-job shape with a single cargo run should hit ≤ 4 min on
warm cache and ≤ 6 min cold.

## Rolling back

If you need to revert to the single sequential job shape (e.g., for
debugging), the last known-good 14-step shape lives in the git history
as of commit `a9b6ada`. After debugging, restore the parallel shape
from HEAD.
