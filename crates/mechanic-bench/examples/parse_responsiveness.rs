//! Finite, concurrent PTY flood. This measures parser-call latency, not GUI latency.
use mechanic_config::Config;
use mechanic_core::{Terminal, TerminalSize};
use serde::Serialize;
use std::{
    env, fs,
    io::{self, Read, Write},
    sync::Arc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const READY: &str = "mechanic-parse-flood-ready";
const DONE: &str = "mechanic-parse-flood-done";
const PREFILL_MS: u64 = 100;
const YIELD_MS: u64 = 1;

#[derive(Serialize)]
struct Call {
    seconds: f64,
    parsed_output: bool,
    observed_exit: bool,
}
#[derive(Serialize)]
struct Sample {
    case: String,
    warmup: bool,
    flood_bytes: usize,
    total_drain_seconds: f64,
    calls: Vec<Call>,
    marker_before_exit_verified: bool,
    error: Option<String>,
}
#[derive(Serialize)]
struct Report {
    schema: u32,
    workload_version: u32,
    build_profile: &'static str,
    os: &'static str,
    arch: &'static str,
    unix_seconds: u64,
    requested_bytes: usize,
    samples: usize,
    warmups_per_case: usize,
    cols: usize,
    rows: usize,
    prefill_ms: u64,
    yield_ms: u64,
    timeout_seconds: u32,
    max_call_seconds: f64,
    p95_busy_call_seconds: f64,
    observations: Vec<Sample>,
}

struct OutputFile<'a> {
    file: fs::File,
    path: &'a str,
    complete: bool,
}
impl Drop for OutputFile<'_> {
    fn drop(&mut self) {
        if !self.complete {
            let _ = fs::remove_file(self.path);
        }
    }
}

fn block(case: &str, requested: usize) -> (Vec<u8>, usize) {
    let record = match case {
        "ascii" => "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\r\n",
        "sgr" => "\x1b[38;2;17;83;149m\x1b[48;5;31m0123456789abcdef\x1b[0m \r\n",
        _ => unreachable!("validated case"),
    };
    let block = record.as_bytes().repeat(65536usize.div_ceil(record.len()));
    let count = requested.div_ceil(block.len());
    (block, count)
}
fn marker(case: &str, bytes: usize) -> String {
    format!("__FLOOD_END_{case}_{bytes}__")
}
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
fn title(value: &str) -> io::Result<()> {
    let mut out = io::stdout().lock();
    write!(out, "\x1b]2;{value}\x07")?;
    out.flush()
}

fn peer(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() != 3 {
        return Err("peer expects case, byte count, timeout".into());
    }
    let case = args[0].as_str();
    if !["ascii", "sgr"].contains(&case) {
        return Err("invalid peer case".into());
    }
    let bytes: usize = args[1].parse()?;
    let timeout: u32 = args[2].parse()?;
    if !(1..=256 * 1024 * 1024).contains(&bytes) || !(1..=3600).contains(&timeout) {
        return Err("invalid peer limits".into());
    }
    let (block, count) = block(case, bytes);
    let mut raw = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: this exec'd peer owns stdin, which is its isolated PTY slave.
    if unsafe { libc::tcgetattr(0, raw.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let mut raw = unsafe { raw.assume_init() };
    unsafe {
        libc::cfmakeraw(&mut raw);
    }
    if unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    // A watchdog makes even a blocked/unbounded baseline terminate finitely.
    unsafe {
        libc::signal(libc::SIGALRM, libc::SIG_DFL);
        libc::alarm(timeout);
    }
    title(READY)?;
    let mut start = [0; 1];
    io::stdin().read_exact(&mut start)?;
    if start != *b"G" {
        return Err("incorrect flood-start handshake".into());
    }
    let mut output = io::stdout().lock();
    for _ in 0..count {
        output.write_all(&block)?;
    }
    write!(output, "\x1b[0m\r\n{}\r\n\x1b]2;{DONE}\x07", marker(case, block.len() * count))?;
    output.flush()?;
    // A distinct status lets the parent distinguish successful production from watchdog death.
    std::process::exit(7);
}

fn sample(
    case: &str,
    bytes: usize,
    timeout: u32,
    warmup: bool,
) -> Result<Sample, Box<dyn std::error::Error>> {
    let (block, count) = block(case, bytes);
    let actual = block.len() * count;
    let expected_marker = marker(case, actual);
    let mut config = Config::default();
    config.shell.program = "/bin/sh".into();
    config.terminal.scrollback_lines = 0;
    let mut terminal = Terminal::new(
        &config,
        TerminalSize { columns: 120, rows: 40, cell_width: 8, cell_height: 16 },
        Arc::new(|| {}),
    )?;
    let exe = env::current_exe()?;
    let command = format!(
        "exec {} --peer {case} {bytes} {timeout}\n",
        quote(exe.to_str().ok_or("non-UTF8 executable path")?)
    );
    terminal.write_to_pty(command.as_bytes())?;
    let setup_deadline = Instant::now() + Duration::from_secs(timeout.into());
    while terminal.title() != READY {
        let outcome = terminal.process_input();
        if let Some(error) = outcome.io_error {
            return Err(format!("flood peer transport failed during setup: {error}").into());
        }
        if outcome.child_exit.is_some() {
            return Err("flood peer exited during setup".into());
        }
        if Instant::now() >= setup_deadline {
            return Err("flood readiness timed out".into());
        }
        thread::sleep(Duration::from_millis(1));
    }
    // Child production and setup are separate from timed parser calls. Let the
    // bounded output queue reach backpressure before beginning consumption.
    terminal.write_to_pty(b"G")?;
    thread::sleep(Duration::from_millis(PREFILL_MS));
    let start = Instant::now();
    let deadline = start + Duration::from_secs(timeout.into());
    let mut calls = Vec::new();
    let mut verified = false;
    let error = loop {
        let call_start = Instant::now();
        let outcome = terminal.process_input();
        calls.push(Call {
            seconds: call_start.elapsed().as_secs_f64(),
            parsed_output: outcome.grid_maybe_changed,
            observed_exit: outcome.child_exit.is_some(),
        });
        if let Some(error) = outcome.io_error {
            break Some(format!("flood peer transport failed: {error}"));
        }
        if let Some(status) = outcome.child_exit {
            let visible: String =
                terminal.grid().display_iter().map(|indexed| indexed.cell.c).collect();
            verified = status.is_some_and(|s| s.code() == Some(7))
                && terminal.title() == DONE
                && visible.contains(&expected_marker);
            break if verified {
                None
            } else {
                Some("child exit preceded final marker/title, or peer watchdog failed".into())
            };
        }
        if Instant::now() >= deadline {
            break Some("drain timed out without child exit".into());
        }
        // Yield between calls, without claiming to emulate a GUI event loop.
        thread::sleep(Duration::from_millis(YIELD_MS));
    };
    Ok(Sample {
        case: case.into(),
        warmup,
        flood_bytes: actual,
        total_drain_seconds: start.elapsed().as_secs_f64(),
        calls,
        marker_before_exit_verified: verified,
        error,
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--peer") {
        return peer(&args[1..]);
    }
    if args.is_empty() || args[0] == "--help" {
        println!(
            "parse_responsiveness OUTPUT.json [--mib N] [--samples N] [--timeout SECONDS] [--case ascii|sgr]\nDefaults: both cases, 32 MiB, one warmup + three samples, 20-second watchdog. Run a release build."
        );
        return Ok(());
    }
    let mut bytes = 32 * 1024 * 1024;
    let mut samples = 3;
    let mut timeout = 20;
    let mut case = None;
    let mut flags = args[1..].iter();
    while let Some(flag) = flags.next() {
        let value = flags.next().ok_or("missing flag value")?;
        match flag.as_str() {
            "--mib" => {
                bytes = value.parse::<usize>()?.checked_mul(1024 * 1024).ok_or("size overflow")?
            }
            "--samples" => samples = value.parse::<usize>()?,
            "--timeout" => timeout = value.parse::<u32>()?,
            "--case" => case = Some(value.clone()),
            _ => return Err(format!("unknown flag: {flag}").into()),
        }
    }
    if !(1..=256 * 1024 * 1024).contains(&bytes)
        || !(1..=100).contains(&samples)
        || !(1..=3600).contains(&timeout)
        || case.as_ref().is_some_and(|c| !["ascii", "sgr"].contains(&c.as_str()))
    {
        return Err("invalid benchmark limits/case".into());
    }
    // Refuse overwrite before spending time on the run. The report preserves failed samples too.
    let mut output = OutputFile {
        file: fs::OpenOptions::new().write(true).create_new(true).open(&args[0])?,
        path: &args[0],
        complete: false,
    };
    let mut observations = Vec::new();
    for name in ["ascii", "sgr"]
        .into_iter()
        .filter(|name| case.as_ref().is_none_or(|chosen| chosen == *name))
    {
        for i in 0..=samples {
            observations.push(sample(name, bytes, timeout, i == 0)?);
        }
    }
    let measured: Vec<_> =
        observations.iter().filter(|s| !s.warmup).flat_map(|s| &s.calls).collect();
    let max = measured.iter().map(|c| c.seconds).fold(0.0, f64::max);
    let mut busy: Vec<_> = measured.iter().filter(|c| c.parsed_output).map(|c| c.seconds).collect();
    busy.sort_by(f64::total_cmp);
    let p95 =
        busy.get((busy.len() as f64 * 0.95).ceil().max(1.0) as usize - 1).copied().unwrap_or(0.0);
    let report = Report {
        schema: 1,
        workload_version: 1,
        build_profile: if cfg!(debug_assertions) { "debug" } else { "release" },
        os: env::consts::OS,
        arch: env::consts::ARCH,
        unix_seconds: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        requested_bytes: bytes,
        samples,
        warmups_per_case: 1,
        cols: 120,
        rows: 40,
        prefill_ms: PREFILL_MS,
        yield_ms: YIELD_MS,
        timeout_seconds: timeout,
        max_call_seconds: max,
        p95_busy_call_seconds: p95,
        observations,
    };
    serde_json::to_writer_pretty(&mut output.file, &report)?;
    writeln!(output.file)?;
    output.file.sync_all()?;
    output.complete = true;
    for sample in &report.observations {
        if !sample.warmup {
            println!(
                "{}: {} calls, {:.3} ms drain total, final marker precedes exit verified={}",
                sample.case,
                sample.calls.len(),
                sample.total_drain_seconds * 1000.0,
                sample.marker_before_exit_verified
            );
        }
    }
    println!(
        "max process_input {:.3} ms; p95 busy call {:.3} ms; saved {}",
        max * 1000.0,
        p95 * 1000.0,
        args[0]
    );
    if report.observations.iter().any(|s| !s.marker_before_exit_verified) {
        return Err("flood verification failed; see raw report".into());
    }
    Ok(())
}
