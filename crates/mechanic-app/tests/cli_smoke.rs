//! Smoke tests for Mechanic's command-line surface.

use std::process::Command;

/// Cargo supplies the integration-test binary path.
fn mechanic_binary() -> &'static str {
    env!("CARGO_BIN_EXE_mechanic")
}

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
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "expected version {:?} in output, got {stdout:?}",
        env!("CARGO_PKG_VERSION")
    );
}

#[test]
fn short_v_flag_behaves_like_long() {
    let long = Command::new(mechanic_binary()).arg("--version").output().expect("spawn --version");
    let short = Command::new(mechanic_binary()).arg("-V").output().expect("spawn -V");

    assert_eq!(long.status.code(), short.status.code());
    assert_eq!(long.stdout, short.stdout);
}

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
    assert!(stdout.contains("USAGE"), "help output should include a USAGE section: {stdout:?}");
    for expected in ["--hot-cpu", "--no-mouse-tracking", "--help", "--version"] {
        assert!(stdout.contains(expected), "help output missing flag {expected:?}:\n{stdout}");
    }
}

#[test]
fn short_h_flag_behaves_like_long() {
    let long = Command::new(mechanic_binary()).arg("--help").output().expect("spawn --help");
    let short = Command::new(mechanic_binary()).arg("-h").output().expect("spawn -h");

    assert_eq!(long.status.code(), short.status.code());
    assert_eq!(long.stdout, short.stdout);
}

#[test]
fn unknown_flag_exits_two_with_error_on_stderr() {
    let output = Command::new(mechanic_binary())
        .arg("--definitely-not-a-real-flag")
        .output()
        .expect("failed to spawn mechanic with bogus flag");

    assert_eq!(
        output.status.code(),
        Some(2),
        "unknown flag should exit with code 2, got {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown"), "error output should mention 'unknown', got {stderr:?}");
    assert!(stderr.contains("--help"), "error output should reference --help, got {stderr:?}");
}

#[test]
fn unknown_flag_does_not_produce_stdout() {
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

#[test]
fn help_wins_when_combined_with_other_flags() {
    let output = Command::new(mechanic_binary())
        .args(["--hot-cpu", "--help"])
        .output()
        .expect("spawn with --hot-cpu --help");

    assert!(output.status.success(), "combined --hot-cpu --help should still exit zero");
    assert!(String::from_utf8_lossy(&output.stdout).contains("USAGE"));
}

#[test]
fn version_wins_when_combined_with_other_flags() {
    let output = Command::new(mechanic_binary())
        .args(["--no-mouse-tracking", "--version"])
        .output()
        .expect("spawn with --no-mouse-tracking --version");

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains(env!("CARGO_PKG_VERSION")));
}
