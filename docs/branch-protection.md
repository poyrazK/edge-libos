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
| `build` | `cargo build --release` (edge-cli) + `cargo test --release` + `cargo test --release --test strace_baseline_diff`; uploads binaries as a 7-day workflow artifact | **yes — once** | **Everything cargo can catch.** Compilation errors, all 197 Rust tests, strace baseline subset |
| `c-conformance` | Downloads edge-cli artifact, reinstalls zig, runs `bash tests/conformance/runner.sh` (marker-enforced) | no | The actual C correctness gate — **this is the P1 false-pass root-cause patch's home** |
| `reproduce` | Downloads edge-cli artifact, runs `reproduce_dod.sh` with `SKIP_*` env hooks to avoid duplicating build's work | minimal — only targeted smoke tests | Integration smoke: dev_setup / guest build / DoD smokes / count totals |

## Steps (one-time setup in the GitHub UI)

1. Open the repository on GitHub.
2. **Settings → Branches → Add rule**.
3. **Branch name pattern**: `main`.
4. Enable **Require a pull request before merging** (recommended).
5. Enable **Require status checks to pass before merging**.
6. Under **Status checks that are required**, search for and add **all four** job names (extended descriptions in the "Why linters live inside build / tools" section below):
   - `tools` — `toolchain + version pins + non-cargo linters (actionlint + shellcheck + gitleaks + wat2wasm)`
   - `build` — `cargo build + test + cargo linters (fmt + clippy + deny + machete + doc + NR-check) + upload`
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
| `tools` | Toolchain version drift (1.93.0 not pinned); zig / wabt install breakage; `actionlint` workflow-syntax errors; `shellcheck` shell-script warnings (with `.shellcheckrc` allow-list); `gitleaks` leaked secrets; `wat2wasm` WAT syntax errors |
| `build` | Compile errors, ALL Rust tests, strace-baseline subset; plus 7 static checks: NR-table consistency (Rust ↔ C ↔ dispatch), `cargo fmt` formatting drift, `cargo clippy` lint regressions (`-D warnings`), `cargo doc` broken-doc warnings, `cargo-deny` advisory/license/source/bans violations, `cargo-machete` unused dependencies |
| `c-conformance` | C conformance regressions (marker-enforced, post-P1 fix) |
| `reproduce` | End-to-end integration regressions (dev_setup / guest build / DoD smokes / count totals) |

### Why linters live inside build / tools (the 10-job revision)

Per `HANDOFF.md §P2-CI-2`, the CI invariant is: **fan-out jobs MUST NOT invoke cargo** — each runner starts cold on its own `target/`, so an uncached cargo run in `c-conformance` or `reproduce` rebuilds the wasmtime dependency graph (~217 crates, ~3 min cold) for nothing. The same constraint shapes the linter layout:

- **Cargo-required linters** (NR-check, `cargo fmt`, `cargo clippy`, `cargo doc`, `cargo-deny`, `cargo-machete`) live as **steps inside `build`**, which is the single job with a warm `target/ci/` cache. Marginal cost is ~30-60 s per linter on warm cache, ~3-6 min on cold — far cheaper than a separate runner that would cold-compile the whole graph.
- **Non-cargo linters** (`actionlint`, `shellcheck`, `gitleaks`, `wat2wasm` validation) live as **steps inside `tools`** — they have no cache dependency, `tools` is already a toolchain smoke that installs `wabt` and apt packages, and the time budget there is otherwise small.

The local mirror (`scripts/preflight.sh`) runs the same 10 linters in steps 0a-0j before the existing steps 1-5; a contributor who wants to know "will this CI-green?" runs `preflight.sh` locally.

The 10 required checks today (counted as steps across the 4 jobs) are:

1. `bash scripts/check_nr_consistency.sh` — three-way NR table mirror
2. `cargo fmt --all -- --check` — formatting drift
3. `cargo clippy --all-targets -- -D warnings` — lint regressions
4. `cargo doc --no-deps --document-private-items` — broken docs
5. `cargo deny check` — advisories, licenses, sources, bans
6. `cargo machete` — unused dependencies
7. `actionlint` — workflow syntax
8. `shellcheck` scripts/**.sh tests/**.sh tests/conformance/runner.sh tests/strace_baselines/**.sh guest/build.sh
9. `gitleaks detect` — secret leak scan
10. `wat2wasm` validation over `find tests guests -name '*.wat'`

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

- **First run (cold cache)**: ≤ 12 min
- **Subsequent runs (cache hit)**: ≤ 7 min

The 5-job fan-out predecessor shape took ~13 min cold. The current
4-job shape with a single cargo run + 10 static-analysis linters
absorbed ~6 min of additional cargo-linter overhead (clippy, deny,
machete, doc; mostly on first compile — warm cache reuses target/ci/
for nearly all of it).

## Rolling back

If you need to revert to the single sequential job shape (e.g., for
debugging), the last known-good 14-step shape lives in the git history
as of commit `a9b6ada`. After debugging, restore the parallel shape
from HEAD.
