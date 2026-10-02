use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

enum Response {
    Reply,
    Timeout,
    Interrupt,
}

fn exercise(response: Response) {
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("result.json");
    let mut master = -1;
    let mut slave = -1;
    let mut size = libc::winsize { ws_row: 24, ws_col: 80, ws_xpixel: 640, ws_ypixel: 384 };
    // SAFETY: openpty writes two descriptors and reads a valid winsize.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        },
        0
    );
    // SAFETY: openpty returned owned descriptors, each wrapped once.
    let mut master = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    let attributes = || {
        let mut value = std::mem::MaybeUninit::uninit();
        assert_eq!(unsafe { libc::tcgetattr(slave.as_raw_fd(), value.as_mut_ptr()) }, 0);
        unsafe { value.assume_init() }
    };
    let before = attributes();
    // Keep the session leader alive while inspecting restored termios; macOS
    // revokes the slave when its session leader exits.
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "\"$@\"; result=$?; printf '\\nBENCH_STATUS=%s\\n' \"$result\"; read hold; exit \"$result\"", "sh", env!("CARGO_BIN_EXE_mechanic-bench")]);
    command
        .args([
            "terminal",
            "simulated",
            output.to_str().unwrap(),
            "--cols",
            "80",
            "--rows",
            "24",
            "--case",
            "ascii",
            "--mib",
            "1",
            "--samples",
            "1",
            "--timeout",
            if matches!(response, Response::Reply) { "5" } else { "1" },
        ])
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(Stdio::piped());
    // SAFETY: only async-signal-safe libc calls run between fork and exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    let mut pending = Vec::new();
    let mut all = Vec::new();
    let mut signalled = false;
    let mut restored = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("benchmark failed to terminate");
        }
        let mut poll = libc::pollfd { fd: master.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut poll, 1, 20) } <= 0 {
            continue;
        }
        let mut bytes = [0; 65536];
        let n = master.read(&mut bytes).unwrap();
        all.extend_from_slice(&bytes[..n]);
        pending.extend_from_slice(&bytes[..n]);
        if restored.is_none()
            && all[all.len().saturating_sub(n + 13)..]
                .windows(13)
                .any(|part| part == b"BENCH_STATUS=")
        {
            restored = Some(attributes());
            master.write_all(b"\n").unwrap();
        }
        while let Some(i) = pending.windows(4).position(|part| part == b"\x1b[6n") {
            pending.drain(..i + 4);
            match response {
                Response::Reply => {
                    master.write_all(b"\x1b[1;").unwrap();
                    master.write_all(b"1R").unwrap();
                }
                Response::Interrupt if !signalled => {
                    // SIGINT reaches the foreground benchmark; the shell is
                    // waiting for it and does not terminate the session.
                    master.write_all(b"\x03").unwrap();
                    signalled = true;
                }
                _ => {}
            }
        }
        if pending.len() > 4 {
            pending.drain(..pending.len() - 4);
        }
    };
    let after = restored.expect("shell did not observe benchmark completion");
    let local_modes = libc::ICANON | libc::ECHO | libc::ISIG;
    // The kernel can set PENDIN when switching back to canonical mode.
    assert_eq!(
        before.c_lflag & local_modes,
        after.c_lflag & local_modes,
        "terminal local modes must be restored"
    );
    assert_eq!(before.c_iflag, after.c_iflag);
    assert_eq!(before.c_oflag, after.c_oflag);
    let mut errors = String::new();
    child.stderr.take().unwrap().read_to_string(&mut errors).unwrap();
    if matches!(response, Response::Reply) {
        assert!(status.success(), "{errors}");
        let report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(output).unwrap()).unwrap();
        assert_eq!(report["measurements"][0]["seconds"].as_array().unwrap().len(), 1);
        assert_eq!(report["measurements"][1]["seconds"].as_array().unwrap().len(), 100);
        assert!(all.len() > 2 * 1024 * 1024);
    } else {
        assert!(!status.success());
        let expected =
            if matches!(response, Response::Timeout) { "TimedOut" } else { "Interrupted" };
        assert!(errors.contains(expected), "expected {expected}, got {errors}");
        assert!(!output.exists(), "failed runs must remove incomplete results");
    }
}

#[test]
fn terminal_run_waits_for_replies_and_saves_samples() {
    exercise(Response::Reply);
}

#[test]
fn unsupported_queries_timeout_and_restore_terminal() {
    exercise(Response::Timeout);
}

#[test]
fn interruption_restores_terminal() {
    exercise(Response::Interrupt);
}
