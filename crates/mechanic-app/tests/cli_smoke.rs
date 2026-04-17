//! Smoke tests for Mechanic's command-line surface.
//!
//! These spawn the real `mechanic` binary (via `CARGO_BIN_EXE_mechanic`,
//! which Cargo sets up for integration tests) and assert on its output
//! and exit code.  They cover the paths that exit without creating a
//! window — `--help`, `--version`, and the unknown-flag error — so
//! they run in CI on any Linux/macOS runner without a display.
//!
//! What they guard against:
//! - A refactor that silently breaks argv parsing (e.g. renaming a
//!   flag, swapping exit codes, losing a line of `--help`).
//! - A regression where a usage-error path starts the GUI anyway.
//! - Drift between the `--help` text and the flags actually supported.
//!
//! What they deliberately do NOT cover: anything that opens a window.
//! The main event loop needs a real display server (AppKit / X11 /
//! Wayland) which CI agents don't provide — and this test file is
//! meant to be trustworthy in CI.  Window-opening behaviour belongs
//! in manual smoke scripts or a headless-GPU integration suite.

use std::process::Command;

/// Absolute path to the freshly-built `mechanic` binary for this
/// integration-test run.  Cargo exports this env var automatically
/// for any integration test (`tests/*.rs`) in a crate that has a
/// `[[bin]]` target named `mechanic`.
fn mechanic_binary() -> &'static str {
    env!("CARGO_BIN_EXE_mechanic")
}

// ── --version ─────────────────────────────────────────────────────────────────

#[test]
fn version_flag_prints_name_and_version() {
    let output = Command::new(mechanic_binary())
        .arg("--version")
        .output()
        .expect("failed to spawn mechanic --version");

    assert!(
        output.status.success(),
        "--version must exit zero; got {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("mechanic"), "expected name in output, got {stdout:?}");
    // Version string matches what the crate was built with; any skew
    // between what `--version` prints and what Cargo says was built
    // indicates a packaging bug.
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "expected version {:?} in output, got {stdout:?}",
        env!("CARGO_PKG_VERSION")
    );
}

#[test]
fn short_v_flag_behaves_like_long() {
    // `-V` and `--version` are advertised as equivalent in the help
    // text.  If one grows a divergent code path (different output,
    // different exit) users will have a bad time — pin the
    // equivalence.
    let long = Command::new(mechanic_binary())
        .arg("--version")
        .output()
        .expect("spawn --version");
    let short = Command::new(mechanic_binary())
        .arg("-V")
        .output()
        .expect("spawn -V");

    assert_eq!(long.status.code(), short.status.code());
    assert_eq!(long.stdout, short.stdout);
}

// ── --help ────────────────────────────────────────────────────────────────────

#[test]
fn help_flag_prints_usage_with_known_flags() {
    let output = Command::new(mechanic_binary())
        .arg("--help")
        .output()
        .expect("failed to spawn mechanic --help");

    assert!(
        output.status.success(),
        "--help must exit zero; got {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    // The help block exists in some form.
    assert!(stdout.contains("USAGE"), "help output should include a USAGE section: {stdout:?}");
    // Every advertised flag shows up in the help text.  If a flag is
    // added to `parse_args` but forgotten in `print_help`, this test
    // fails.
    for expected in ["--hot-cpu", "--no-mouse-tracking", "--help", "--version"] {
        assert!(
            stdout.contains(expected),
            "help output missing flag {expected:?}:\n{stdout}"
        );
    }
}

#[test]
fn short_h_flag_behaves_like_long() {
    let long = Command::new(mechanic_binary())
        .arg("--help")
        .output()
        .expect("spawn --help");
    let short = Command::new(mechanic_binary())
        .arg("-h")
        .output()
        .expect("spawn -h");

    assert_eq!(long.status.code(), short.status.code());
    assert_eq!(long.stdout, short.stdout);
}

// ── Unknown flag ──────────────────────────────────────────────────────────────

#[test]
fn unknown_flag_exits_two_with_error_on_stderr() {
    let output = Command::new(mechanic_binary())
        .arg("--definitely-not-a-real-flag")
        .output()
        .expect("failed to spawn mechanic with bogus flag");

    // Exit code 2 is the conventional "usage error" code on Unix
    // (distinct from 1 which is generic failure).  If this drifts to
    // 0 or 1, something has gone wrong in argv handling.
    assert_eq!(
        output.status.code(),
        Some(2),
        "unknown flag should exit with code 2, got {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown"),
        "error output should mention 'unknown', got {stderr:?}"
    );
    // Point the user at --help so they don't have to guess.
    assert!(
        stderr.contains("--help"),
        "error output should reference --help, got {stderr:?}"
    );
}

#[test]
fn unknown_flag_does_not_produce_stdout() {
    // A usage error must not leak half-formed startup chatter to
    // stdout — that would pollute pipelines that expect clean output
    // from successful runs only.  Errors belong on stderr.
    let output = Command::new(mechanic_binary())
        .arg("--bogus")
        .output()
        .expect("spawn mechanic with bogus flag");
    assert!(
        output.stdout.is_empty(),
        "unknown-flag path wrote to stdout: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
}

// ── Flag combinations ─────────────────────────────────────────────────────────

#[test]
fn help_wins_when_combined_with_other_flags() {
    // `mechanic --hot-cpu --help` should print help and exit, not
    // try to start the GUI.  Once --help is seen the binary should
    // exit without opening a window, regardless of what else was on
    // the command line.
    let output = Command::new(mechanic_binary())
        .args(["--hot-cpu", "--help"])
        .output()
        .expect("spawn with --hot-cpu --help");

    assert!(output.status.success(), "combined --hot-cpu --help should still exit zero");
    assert!(String::from_utf8_lossy(&output.stdout).contains("USAGE"));
}

#[test]
fn version_wins_when_combined_with_other_flags() {
    // Same invariant for --version: it's a terminal command, nothing
    // after it should cause the window to open.
    let output = Command::new(mechanic_binary())
        .args(["--no-mouse-tracking", "--version"])
        .output()
        .expect("spawn with --no-mouse-tracking --version");

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains(env!("CARGO_PKG_VERSION")));
}
