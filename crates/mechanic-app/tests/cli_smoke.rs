//! Smoke tests for Mechanic's command-line surface.

use std::io::Write;
use std::process::{Command, Stdio};

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
    for expected in [
        "--animate",
        "--hot-cpu",
        "--no-mouse-tracking",
        "--no-restore",
        "ctl",
        "--help",
        "--version",
    ] {
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

/// Headless paths must return before winit tries to connect to a display server.
fn headless() -> Command {
    let mut command = Command::new(mechanic_binary());
    command.env_remove("DISPLAY").env_remove("WAYLAND_DISPLAY");
    command
}

#[test]
fn control_help_exits_without_starting_the_gui() {
    for args in [vec!["ctl", "--help"], vec!["ctl", "send", "--help"]] {
        let output = headless().args(args).output().expect("spawn control help");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let stdout = String::from_utf8_lossy(&output.stdout);
        for expected in
            ["--socket", "--pane", "--stdin", "--raw", "--enter", "--after", "instances"]
        {
            assert!(stdout.contains(expected), "missing {expected}: {stdout}");
        }
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn invalid_control_commands_return_json_without_starting_the_gui() {
    for args in [
        vec!["ctl"],
        vec!["ctl", "bogus"],
        vec!["ctl", "read"],
        vec!["ctl", "read", "--pane", "17"],
        vec!["ctl", "wait", "--pane", "stale:17"],
        vec!["ctl", "send", "--pane", "stale:17", "--stdin", "--text", "x"],
    ] {
        let output = headless().args(&args).output().expect("spawn invalid control command");
        assert_eq!(
            output.status.code(),
            Some(2),
            "args={args:?} stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        let response: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("JSON control error");
        assert_eq!(response["status"], "error");
        assert_eq!(response["code"], "invalid_arguments");
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn explicit_missing_socket_returns_json_without_starting_the_gui() {
    let absent_socket =
        std::env::temp_dir().join(format!("mechanic-cli-absent-{}.sock", std::process::id()));
    for global in [true, false] {
        let mut command = headless();
        if global {
            command.arg("--socket").arg(&absent_socket).args(["ctl", "list"]);
        } else {
            command.args(["ctl", "list", "--socket"]).arg(&absent_socket);
        }
        let output = command.output().expect("spawn missing socket request");
        assert_eq!(output.status.code(), Some(1));
        let response: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("JSON connection error");
        assert_eq!(response["status"], "error");
        assert!(response["message"].as_str().unwrap().contains("control request failed"));
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn control_send_rejects_invalid_stdin_before_connecting() {
    for (input, expected) in [(vec![0xff], "UTF-8"), (vec![b'x'; 128 * 1024 + 1], "limit")] {
        let mut child = headless()
            .args([
                "ctl",
                "send",
                "--pane",
                "0123456789abcdef0123456789abcdef:17",
                "--stdin",
                "--socket",
                "/does/not/exist.sock",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn stdin validation");
        child.stdin.take().unwrap().write_all(&input).expect("write invalid stdin");
        let output = child.wait_with_output().expect("wait stdin validation");
        assert_eq!(output.status.code(), Some(1));
        let response: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("JSON stdin error");
        assert!(response["message"].as_str().unwrap().contains(expected));
        assert!(output.stderr.is_empty());
    }
}
