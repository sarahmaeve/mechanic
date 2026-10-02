use crate::{Measurement, Options, ResultFile};
use mechanic_config::Config;
use mechanic_core::{Terminal, TerminalSize};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::{self, Read, Write},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

pub const CASES: &[&str] = &["consuming", "delayed-reader", "duplex"];
const READY: &str = "mechanic-paste-ready";
const DONE: &str = "mechanic-paste-verified";
const FAILED: &str = "mechanic-paste-failed";
const PROBE: &[u8] = b"AFTER-PASTE-ORDER-PROBE";

#[derive(Deserialize, Serialize, PartialEq)]
pub struct Metadata {
    payload_version: u32,
    reader_delay_ms: u64,
    duplex_bytes: usize,
    timeout_seconds: u64,
    verification: String,
}

#[derive(Deserialize, Serialize)]
pub struct Sample {
    case: String,
    warmup: bool,
    paste_seconds: f64,
    delivery_seconds: Option<f64>,
    outcome: String,
}

fn payload(bytes: usize) -> String {
    let mut data = String::with_capacity(bytes + 64);
    for i in 0..bytes.div_ceil(64) {
        data.push_str(&format!("{i:016x}{}", "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKL"));
    }
    data.truncate(bytes);
    data
}

fn expected(text: &str) -> Vec<u8> {
    let mut bytes = b"\x1b[200~".to_vec();
    bytes.extend_from_slice(text.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    bytes.extend_from_slice(PROBE);
    bytes
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn wait_title(term: &mut Terminal, title: &str, deadline: Instant) -> io::Result<()> {
    loop {
        let outcome = term.process_input();
        if term.title() == title {
            return Ok(());
        }
        if term.title() == FAILED {
            return Err(io::Error::other("peer detected incorrect bytes/order"));
        }
        if outcome.child_exit.is_some() {
            return Err(io::Error::other("peer exited before acknowledgment"));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "peer acknowledgment timed out"));
        }
        thread::sleep(Duration::from_millis(1));
    }
}

fn sample(
    o: &Options,
    case: &str,
    text: &str,
    warmup: bool,
) -> Result<Sample, Box<dyn std::error::Error>> {
    let mut config = Config::default();
    config.shell.program = "/bin/sh".into();
    config.terminal.scrollback_lines = 0;
    let mut term = Terminal::new(
        &config,
        TerminalSize { columns: o.cols, rows: o.rows, cell_width: 8, cell_height: 16 },
        Arc::new(|| {}),
    )?;
    let exe = env::current_exe()?;
    let delay = if case == "consuming" { 0 } else { o.reader_delay.as_millis() as u64 };
    let duplex = if case == "duplex" { o.duplex_bytes } else { 0 };
    // Command construction and peer setup are outside the measured interval.
    let command = format!(
        "exec {} _paste-peer {} {} {} {}\n",
        quote(exe.to_str().ok_or("non-UTF8 executable path")?),
        text.len(),
        delay,
        duplex,
        o.timeout.as_secs()
    );
    term.write_to_pty(command.as_bytes())?;
    wait_title(&mut term, READY, Instant::now() + o.timeout)?;
    if !term.bracketed_paste() {
        return Err("peer did not enable bracketed paste".into());
    }
    let start = Instant::now();
    let pasted = term.paste(text);
    let paste_seconds = start.elapsed().as_secs_f64();
    let delivered = match pasted {
        Ok(()) => term
            .write_to_pty(PROBE)
            .map_err(io::Error::other)
            .and_then(|()| wait_title(&mut term, DONE, start + o.timeout)),
        Err(error) => Err(io::Error::other(error)),
    };
    let delivery_seconds = delivered.as_ref().ok().map(|()| start.elapsed().as_secs_f64());
    let outcome = match delivered {
        Ok(()) => "verified".into(),
        Err(error) => error.to_string(),
    };
    Ok(Sample { case: case.into(), warmup, paste_seconds, delivery_seconds, outcome })
}

pub fn run(o: Options) -> Result<(), Box<dyn std::error::Error>> {
    let mut output = ResultFile {
        file: fs::OpenOptions::new().write(true).create_new(true).open(&o.output)?,
        path: o.output.clone(),
        complete: false,
    };
    let mut report = crate::new_report(&o)?;
    report.paste = Some(Metadata {
        payload_version: 1,
        reader_delay_ms: o.reader_delay.as_millis() as u64,
        duplex_bytes: o.duplex_bytes,
        timeout_seconds: o.timeout.as_secs(),
        verification: "exact bracketed payload followed by ordered input probe".into(),
    });
    let text = payload(o.bytes);
    let mut raw = Vec::new();
    let mut failures = 0;
    for case in CASES.iter().filter(|case| {
        o.case.as_ref().map_or(**case != "duplex" || o.duplex_bytes > 0, |chosen| chosen == **case)
    }) {
        let mut call = Measurement {
            case: format!("{case}-paste-call"),
            bytes: text.len(),
            operations: 1,
            seconds: Vec::new(),
        };
        let mut total = Measurement {
            case: format!("{case}-verified-delivery"),
            bytes: text.len(),
            operations: 1,
            seconds: Vec::new(),
        };
        for i in 0..=o.samples {
            let s = sample(&o, case, &text, i == 0)?;
            if s.delivery_seconds.is_none() {
                failures += 1;
                eprintln!("{case}: {}", s.outcome);
            }
            if i > 0 {
                call.seconds.push(s.paste_seconds);
                if let Some(seconds) = s.delivery_seconds {
                    total.seconds.push(seconds);
                }
            }
            raw.push(s);
        }
        println!(
            "{case}: paste() median {:.3} ms; {} / {} deliveries verified",
            call.percentile(0.5) * 1000.0,
            total.seconds.len(),
            o.samples
        );
        if !total.seconds.is_empty() {
            println!("  delivery median {:.3} ms", total.percentile(0.5) * 1000.0);
        }
        report.measurements.extend([call, total]);
    }
    report.paste_samples = Some(raw);
    serde_json::to_writer_pretty(&mut output.file, &report)?;
    writeln!(output.file)?;
    output.file.sync_all()?;
    output.complete = true;
    println!("Saved {}", o.output);
    if failures > 0 {
        return Err(format!("{failures} paste samples failed; raw failures saved, comparison will reject incomplete deliveries").into());
    }
    Ok(())
}

fn title(value: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    write!(stdout, "\x1b]2;{value}\x07")?;
    stdout.flush()
}

/// Private subprocess protocol. SIGALRM terminates this isolated peer even if
/// blocking I/O deadlocks, releasing the PTY and unblocking a baseline paste().
pub fn peer(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() != 4 {
        return Err("private peer expects bytes, delay-ms, duplex-bytes, timeout-seconds".into());
    }
    let bytes: usize = args[0].parse()?;
    let delay_ms: u64 = args[1].parse()?;
    let duplex_bytes: usize = args[2].parse()?;
    let timeout: u32 = args[3].parse()?;
    if !(1..=256 * 1024 * 1024).contains(&bytes)
        || delay_ms > 5000
        || duplex_bytes > 64 * 1024 * 1024
        || !(1..=3600).contains(&timeout)
    {
        return Err("invalid private peer parameters".into());
    }
    let mut raw = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: stdin is the peer's own PTY; raw points to writable termios storage.
    if unsafe { libc::tcgetattr(0, raw.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let mut raw = unsafe { raw.assume_init() };
    // SAFETY: raw is initialized and stdin is open. The peer owns these settings.
    unsafe {
        libc::cfmakeraw(&mut raw);
    }
    if unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let expected = expected(&payload(bytes));
    // SAFETY: SIGALRM default termination is installed only in this exec'd peer.
    unsafe {
        libc::signal(libc::SIGALRM, libc::SIG_DFL);
        libc::alarm(timeout);
    }
    io::stdout().write_all(b"\x1b[?2004h")?;
    title(READY)?;
    thread::sleep(Duration::from_millis(delay_ms));
    if duplex_bytes > 0 {
        let chunk = [b'x'; 65536];
        let mut output = io::stdout().lock();
        for offset in (0..duplex_bytes).step_by(chunk.len()) {
            output.write_all(&chunk[..(duplex_bytes - offset).min(chunk.len())])?;
        }
        output.flush()?;
    }
    let mut input = io::stdin().lock();
    let mut offset = 0;
    let mut chunk = [0u8; 65536];
    while offset < expected.len() {
        let n = input.read(&mut chunk[..(expected.len() - offset).min(65536)])?;
        if n == 0 || chunk[..n] != expected[offset..offset + n] {
            title(FAILED)?;
            return Err(format!("payload mismatch/EOF at offset {offset}").into());
        }
        offset += n;
    }
    title(DONE)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn payload_is_exact_size_ascii_and_order_sensitive() {
        for size in [1, 63, 64, 65, 1048576] {
            let text = payload(size);
            assert_eq!(text.len(), size);
            assert!(text.is_ascii());
            assert_eq!(mechanic_core::paste::filter(&text, true), text);
            let wire = expected(&text);
            assert_eq!(&wire[6..6 + size], text.as_bytes());
            assert!(wire.ends_with(PROBE));
        }
        assert_ne!(&payload(128)[..64], &payload(128)[64..]);
    }
    #[test]
    fn shell_quote_preserves_literal_apostrophes() {
        assert_eq!(quote("a'b"), "'a'\\''b'");
    }
}
