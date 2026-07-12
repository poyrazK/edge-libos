//! Integration test for the strace baseline diff harness.
//!
//! Verifies that tests/strace_baselines/diff.py correctly:
//!   * accepts a one-name-per-line baseline (with `#` comments)
//!   * accepts a JSON-lines host trace (the format trace-host emits)
//!   * exits 0 when baseline ⊆ host
//!   * exits 1 when host is missing a baseline syscall
//!   * accepts raw strace output (auto-detected by paren presence)
//!
//! This catches regressions in the diff harness without requiring a real
//! strace run or a real CPython wasm — both of which need native Linux.

use std::process::Command;

use anyhow::Result;

fn write_tmp(name: &str, body: &str) -> std::path::PathBuf {
    let final_path = std::env::temp_dir().join(format!(
        "edge-libos-{}-{}",
        std::process::id(),
        name
    ));
    std::fs::write(&final_path, body).expect("write tmp");
    final_path
}

#[test]
fn diff_harness_accepts_one_name_per_line_baseline() -> Result<()> {
    let baseline = "\
# comment line
read
write
openat
";
    let trace = "{\"name\":\"read\"}\n{\"name\":\"write\"}\n{\"name\":\"openat\"}\n";
    let bp = write_tmp("b.txt", baseline);
    let tp = write_tmp("t.json", trace);
    let out = Command::new("python3")
        .args([
            "tests/strace_baselines/diff.py",
            bp.to_str().unwrap(),
            tp.to_str().unwrap(),
        ])
        .output()?;
    assert!(
        out.status.success(),
        "diff.py should exit 0; stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

#[test]
fn diff_harness_reports_missing_baseline_syscall() -> Result<()> {
    let baseline = "read\nwrite\nopenat\n";
    let trace = "{\"name\":\"read\"}\n{\"name\":\"write\"}\n"; // missing openat
    let bp = write_tmp("b2.txt", baseline);
    let tp = write_tmp("t2.json", trace);
    let out = Command::new("python3")
        .args([
            "tests/strace_baselines/diff.py",
            bp.to_str().unwrap(),
            tp.to_str().unwrap(),
        ])
        .output()?;
    assert_eq!(
        out.status.code(),
        Some(1),
        "diff.py should exit 1 when baseline syscall missing; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("openat"),
        "expected missing syscall name in stderr: {stderr}"
    );
    Ok(())
}

#[test]
fn diff_harness_handles_strace_raw_format() -> Result<()> {
    // Raw strace lines instead of one-name-per-line.
    let baseline = "\
openat(AT_FDCWD, \"/etc/hosts\", O_RDONLY) = 3
read(3, \"hello\", 5) = 5
write(1, \"hello\\n\", 6) = 6
close(3) = 0
";
    let trace = "{\"name\":\"openat\"}\n{\"name\":\"read\"}\n{\"name\":\"write\"}\n{\"name\":\"close\"}\n";
    let bp = write_tmp("b3.txt", baseline);
    let tp = write_tmp("t3.json", trace);
    let out = Command::new("python3")
        .args([
            "tests/strace_baselines/diff.py",
            bp.to_str().unwrap(),
            tp.to_str().unwrap(),
        ])
        .output()?;
    assert!(
        out.status.success(),
        "raw strace format should parse; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

#[test]
fn golden_baseline_parses_to_expected_syscall_count() -> Result<()> {
    // Sanity check on the in-repo golden file: it should parse to ~30
    // syscalls (the P0 surface). If somebody adds or removes a syscall
    // this number should be updated deliberately, not accidentally.
    let baseline_text =
        std::fs::read_to_string("tests/strace_baselines/baseline.boot.txt")?;
    let count = baseline_text
        .lines()
        .filter(|l| {
            let s = l.trim();
            !s.is_empty() && !s.starts_with('#')
        })
        .count();
    assert!(
        (25..=40).contains(&count),
        "baseline.boot.txt has {count} syscalls; expected 25..=40"
    );
    Ok(())
}