# Branch protection & required checks

This document is human-facing instructions for configuring GitHub branch
protection on `main`. The CI workflow at `.github/workflows/ci.yml` is the
single source of truth for the required check.

## Goal

Every PR landing in `main` must pass the `verify` CI workflow. Direct
pushes to `main` should be discouraged (the project uses direct-to-main
today per `HANDOFF.md §4.6`, but the protection works either way).

## Steps (one-time setup in the GitHub UI)

1. Open the repository on GitHub.
2. **Settings → Branches → Add rule**.
3. **Branch name pattern**: `main`.
4. Enable **Require a pull request before merging** (recommended).
5. Enable **Require status checks to pass before merging**.
6. Under **Status checks that are required**, search for and select:
   - `verify (ubuntu-22.04)`
   - (The full check name includes the runner OS; the job name is
     `verify` — search for both forms.)
7. Optional but recommended:
   - **Require branches to be up to date before merging**.
   - **Do not allow bypassing the above settings**.
8. Save.

After this is in place, any green PR gets a `✅ verify` check and is
allowed to merge; a red or missing `verify` blocks the merge button.

## Mapping back to the workflow

| `verify` step | What it catches |
|---|---|
| 9. `cargo test --release` | Rust unit + integration regressions |
| 10. `bash tests/conformance/runner.sh` | C conformance regressions (marker-enforced) |
| 11. `cargo test --release --test strace_baseline_diff` | Syscall-pattern regressions |
| 12. `bash scripts/reproduce_dod.sh` | End-to-end DoD regressions (CPython steps auto-skip until A6 lands) |

Any of these failing fails the `verify` job, which fails the `verify`
check, which blocks the merge.

## When you add a new required check

If you add a new job to `.github/workflows/ci.yml` that should be
required (for example, a fuzz job in P2-G2), come back here and add it to
the "Status checks that are required" list. Use the job name, not the
workflow file name — GitHub tracks checks by job name.
