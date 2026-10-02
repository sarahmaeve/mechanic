//! macOS GUI process CPU measurement. Launches one app with an idle Rust PTY peer.
//! Usage: app_cpu APP_BINARY OUTPUT.json [--animate] [--settle-secs N]
//!        [--sample-secs N] [--label LABEL]
//! CPU is percent of one core, excluding the peer, WindowServer, and GPU work.

#[cfg(target_os = "macos")]
mod macos {
    use serde::Serialize;
    use std::env;
    use std::fs::{self, File, OpenOptions};
    use std::io::{self, Write};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant, UNIX_EPOCH};

    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
    const PEER_ENV: &str = "MECHANIC_BENCH_IDLE_PEER";

    struct Options {
        app: PathBuf,
        output: PathBuf,
        animate: bool,
        settle: u64,
        sample: u64,
        label: Option<String>,
    }

    impl Options {
        fn parse() -> Result<Self> {
            let mut args = env::args_os().skip(1);
            let usage = "usage: app_cpu APP_BINARY OUTPUT.json [--animate] [--settle-secs N] [--sample-secs N] [--label LABEL]";
            let app = args.next().ok_or(usage)?;
            if app == "--help" || app == "-h" {
                println!(
                    "{usage}\nKeep window size and focus unchanged during sampling; animation requires focus.\nDefaults: settle 3s, sample 5s. Existing output files are never overwritten."
                );
                std::process::exit(0);
            }
            let mut options = Self {
                app: fs::canonicalize(app)?,
                output: args.next().ok_or(usage)?.into(),
                animate: false,
                settle: 3,
                sample: 5,
                label: None,
            };
            while let Some(arg) = args.next() {
                match arg.to_str().ok_or("non-UTF8 option")? {
                    "--animate" => options.animate = true,
                    "--settle-secs" => {
                        options.settle = args
                            .next()
                            .ok_or("missing settle seconds")?
                            .to_str()
                            .ok_or("non-UTF8 settle seconds")?
                            .parse()?;
                    }
                    "--sample-secs" => {
                        options.sample = args
                            .next()
                            .ok_or("missing sample seconds")?
                            .to_str()
                            .ok_or("non-UTF8 sample seconds")?
                            .parse()?;
                    }
                    "--label" => {
                        options.label = Some(
                            args.next()
                                .ok_or("missing label")?
                                .into_string()
                                .map_err(|_| "non-UTF8 label")?,
                        );
                    }
                    other => return Err(format!("unknown option {other}; {usage}").into()),
                }
            }
            if !(1..=20).contains(&options.settle) || !(1..=30).contains(&options.sample) {
                return Err("settle seconds must be 1..=20; sample seconds must be 1..=30".into());
            }
            Ok(options)
        }
    }

    // Drop always closes our app's PTY. The peer exits on hangup, with a 60s fallback.
    struct OwnedApp(Child);
    impl Drop for OwnedApp {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[derive(Clone, Copy, Serialize)]
    struct CpuCounters {
        user_mach_ticks: u64,
        system_mach_ticks: u64,
        user_ns: u128,
        system_ns: u128,
    }

    #[derive(Clone, Copy, Serialize)]
    struct CpuTimebase {
        numerator: u32,
        denominator: u32,
    }

    #[allow(deprecated)]
    fn cpu_timebase() -> Result<CpuTimebase> {
        let mut info = libc::mach_timebase_info { numer: 0, denom: 0 };
        let result = unsafe { libc::mach_timebase_info(&mut info) };
        if result != 0 || info.numer == 0 || info.denom == 0 {
            return Err(format!(
                "invalid Mach timebase: status={result}, {}/{}",
                info.numer, info.denom
            )
            .into());
        }
        Ok(CpuTimebase { numerator: info.numer, denominator: info.denom })
    }

    fn ticks_to_ns(ticks: u128, timebase: CpuTimebase) -> Option<u128> {
        // Widen before multiplication: u64 ticks * u32 numerator can exceed u64.
        ticks
            .checked_mul(u128::from(timebase.numerator))?
            .checked_div(u128::from(timebase.denominator))
    }

    fn cpu_counters(pid: u32, timebase: CpuTimebase) -> Result<CpuCounters> {
        // proc_pid_rusage writes the full RUSAGE_INFO_V2 struct to this buffer.
        let mut usage: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::proc_pid_rusage(
                pid.try_into()?,
                libc::RUSAGE_INFO_V2,
                (&mut usage as *mut libc::rusage_info_v2).cast(),
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error().into());
        }
        // These native CPU counters use Mach absolute ticks, not nanoseconds.
        Ok(CpuCounters {
            user_mach_ticks: usage.ri_user_time,
            system_mach_ticks: usage.ri_system_time,
            user_ns: ticks_to_ns(u128::from(usage.ri_user_time), timebase)
                .ok_or("CPU time conversion overflow or invalid timebase")?,
            system_ns: ticks_to_ns(u128::from(usage.ri_system_time), timebase)
                .ok_or("CPU time conversion overflow or invalid timebase")?,
        })
    }

    fn wait_alive(app: &mut OwnedApp, duration: Duration) -> Result<()> {
        let deadline = Instant::now() + duration;
        loop {
            if let Some(status) = app.0.try_wait()? {
                return Err(format!("app exited prematurely: {status}").into());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            thread::sleep(remaining.min(Duration::from_millis(100)));
        }
    }

    fn focus_events(log: &str) -> Vec<bool> {
        log.lines()
            .filter_map(|line| {
                let app_target =
                    line.contains("mechanic::app") || line.contains("mechanic_app::app");
                if app_target && line.contains(" focused: true") {
                    Some(true)
                } else if app_target && line.contains(" focused: false") {
                    Some(false)
                } else {
                    None
                }
            })
            .collect()
    }

    #[derive(Serialize)]
    struct Report {
        schema_version: u32,
        status: &'static str,
        error: Option<String>,
        app_binary: PathBuf,
        app_binary_bytes: u64,
        app_binary_modified_unix_seconds: Option<u64>,
        label: Option<String>,
        benchmark_version: &'static str,
        benchmark_debug_assertions: bool,
        mode: &'static str,
        settle_seconds: u64,
        requested_sample_seconds: u64,
        pid: Option<u32>,
        cpu_start: Option<CpuCounters>,
        cpu_end: Option<CpuCounters>,
        cpu_timebase: CpuTimebase,
        cpu_delta_mach_ticks: Option<u128>,
        cpu_delta_ns: Option<u128>,
        elapsed_seconds: Option<f64>,
        cpu_percent_one_core: Option<f64>,
        focused_at_start: Option<bool>,
        focus_events_during_sample: Vec<bool>,
        settings: serde_json::Value,
        scope: &'static str,
        app_stderr: String,
    }

    fn sample(options: &Options, report: &mut Report, log_path: &Path) -> Result<()> {
        let isolated = tempfile::tempdir()?;
        let config_dir = isolated.path().join("mechanic");
        fs::create_dir(&config_dir)?;
        let peer = env::current_exe()?;
        // JSON string escaping also gives a valid TOML basic string for this path.
        let peer_string = serde_json::to_string(peer.to_str().ok_or("non-UTF8 peer path")?)?;
        fs::write(
            config_dir.join("mechanic.toml"),
            format!(
                "[font]\nfamily = \"Menlo\"\nsize = 14.0\n[shell]\nprogram = {peer_string}\n[theme.opacity]\ntitle_bar_opacity = 1.0\ncontent_active_opacity = 1.0\ncontent_idle_opacity = 1.0\ntext_idle_opacity = 1.0\n"
            ),
        )?;
        let mut command = Command::new(&options.app);
        command
            .env("XDG_CONFIG_HOME", isolated.path())
            .env(PEER_ENV, "1")
            .env("RUST_LOG", "mechanic::app=debug,mechanic_app::app=debug")
            .env("RUST_LOG_STYLE", "never")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(log_path)?);
        if options.animate {
            command.arg("--hot-cpu");
        }
        let mut app = OwnedApp(command.spawn()?);
        let pid = app.0.id();
        report.pid = Some(pid);
        wait_alive(&mut app, Duration::from_secs(options.settle))?;
        let before_log = fs::read_to_string(log_path)?;
        let before_events = focus_events(&before_log);
        report.focused_at_start = before_events.last().copied();
        let start = cpu_counters(pid, report.cpu_timebase)?;
        let started = Instant::now();
        report.cpu_start = Some(start);
        wait_alive(&mut app, Duration::from_secs(options.sample))?;
        let end = cpu_counters(pid, report.cpu_timebase)?;
        let elapsed = started.elapsed().as_secs_f64();
        report.cpu_end = Some(end);
        report.elapsed_seconds = Some(elapsed);
        report.app_stderr = fs::read_to_string(log_path)?;
        report.focus_events_during_sample =
            focus_events(&report.app_stderr).into_iter().skip(before_events.len()).collect();
        let user_ticks = end
            .user_mach_ticks
            .checked_sub(start.user_mach_ticks)
            .ok_or("user CPU counter regressed")?;
        let system_ticks = end
            .system_mach_ticks
            .checked_sub(start.system_mach_ticks)
            .ok_or("system CPU counter regressed")?;
        let cpu_ticks = u128::from(user_ticks) + u128::from(system_ticks);
        let cpu_ns = ticks_to_ns(cpu_ticks, report.cpu_timebase)
            .ok_or("CPU time conversion overflow or invalid timebase")?;
        report.cpu_delta_mach_ticks = Some(cpu_ticks);
        report.cpu_delta_ns = Some(cpu_ns);
        report.cpu_percent_one_core = Some(cpu_ns as f64 / 1e9 / elapsed * 100.0);
        let focused_at_start = report.focused_at_start.ok_or(
            "focus state was not observed before sampling; rerun with a longer settle time",
        )?;
        if options.animate && !focused_at_start {
            return Err("animation requires confirmed focus at sample start; focus the launched window and rerun".into());
        }
        if report.focus_events_during_sample.iter().any(|focused| *focused != focused_at_start) {
            return Err(
                "focus changed during sampling; keep the launched window's focus stable and rerun"
                    .into(),
            );
        }
        Ok(())
    }

    fn idle_peer() -> Result<()> {
        // No timer-driven output or redraws. poll sleeps until PTY hangup or timeout.
        // Reset SIGHUP in case the parent inherited an ignored disposition.
        unsafe {
            libc::signal(libc::SIGHUP, libc::SIG_DFL);
        }
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            let mut fd = libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 };
            let ready = unsafe {
                libc::poll(&mut fd, 1, remaining.as_millis().min(i32::MAX as u128) as i32)
            };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error.into());
            }
            if fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                return Ok(());
            }
            if fd.revents & libc::POLLIN != 0 {
                let mut bytes = [0u8; 128];
                let read = unsafe {
                    libc::read(libc::STDIN_FILENO, bytes.as_mut_ptr().cast(), bytes.len())
                };
                if read <= 0 {
                    return Ok(());
                }
            }
        }
    }

    pub fn run() -> Result<()> {
        if env::var_os(PEER_ENV).as_deref() == Some(std::ffi::OsStr::new("1")) {
            return idle_peer();
        }
        let options = Options::parse()?;
        let metadata = fs::metadata(&options.app)?;
        let timebase = cpu_timebase()?;
        // Reserve output before launching a window; never truncate earlier results.
        let mut output = OpenOptions::new().write(true).create_new(true).open(&options.output)?;
        let log = tempfile::NamedTempFile::new()?;
        let mut report = Report {
            schema_version: 2,
            status: "inconclusive",
            error: None,
            app_binary: options.app.clone(),
            app_binary_bytes: metadata.len(),
            app_binary_modified_unix_seconds: metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|time| time.as_secs()),
            label: options.label.clone(),
            benchmark_version: env!("CARGO_PKG_VERSION"),
            benchmark_debug_assertions: cfg!(debug_assertions),
            mode: if options.animate { "focused_animation" } else { "idle" },
            settle_seconds: options.settle,
            requested_sample_seconds: options.sample,
            pid: None,
            cpu_start: None,
            cpu_end: None,
            cpu_timebase: timebase,
            cpu_delta_mach_ticks: None,
            cpu_delta_ns: None,
            elapsed_seconds: None,
            cpu_percent_one_core: None,
            focused_at_start: None,
            focus_events_during_sample: Vec::new(),
            settings: serde_json::json!({
                "font_family": "Menlo", "font_size_points": 14,
                "opacity": 1.0, "pty_peer": "idle Rust peer",
                "requested_default_window_logical_width": 1024,
                "requested_default_window_logical_height": 768,
                "actual_window_dimensions": "not measured; keep window unchanged",
                "app_build_revision": "not inferred; use --label to identify the build"
            }),
            scope: "App process user + system CPU only; 100% is one CPU core. Excludes PTY peer, WindowServer, GPU usage and energy. Compare identical hardware, display, window size and app build profile.",
            app_stderr: String::new(),
        };
        eprintln!(
            "Launching {}: settle {}s, sample {}s. Keep window size and focus unchanged{}.",
            options.app.display(),
            options.settle,
            options.sample,
            if options.animate { " (animation requires focus)" } else { "" }
        );
        let result = sample(&options, &mut report, log.path());
        if let Err(error) = &result {
            report.error = Some(error.to_string());
            report.app_stderr = fs::read_to_string(log.path()).unwrap_or_default();
        } else {
            report.status = "ok";
        }
        serde_json::to_writer_pretty(&mut output, &report)?;
        writeln!(output)?;
        output.flush()?;
        if result.is_ok() {
            println!(
                "{:.3}% of one core; {}",
                report.cpu_percent_one_core.unwrap(),
                options.output.display()
            );
        }
        // Keep an inconclusive JSON report, while making automation see failure.
        result
    }

    #[cfg(test)]
    mod tests {
        use super::{CpuTimebase, ticks_to_ns};

        #[test]
        fn converts_fractional_mach_timebase_to_nanoseconds() {
            let timebase = CpuTimebase { numerator: 125, denominator: 3 };
            assert_eq!(ticks_to_ns(3, timebase), Some(125));
            assert_eq!(ticks_to_ns(1, timebase), Some(41));
            assert_eq!(ticks_to_ns(5_997_804, timebase), Some(249_908_500));
        }

        #[test]
        fn conversion_widens_before_multiplication() {
            let timebase = CpuTimebase { numerator: u32::MAX, denominator: 2 };
            let expected = u128::from(u64::MAX) * u128::from(u32::MAX) / 2;
            assert!(expected > u128::from(u64::MAX));
            assert_eq!(ticks_to_ns(u128::from(u64::MAX), timebase), Some(expected));
            assert_eq!(ticks_to_ns(u128::MAX, timebase), None);
        }

        #[test]
        fn conversion_rejects_zero_denominator() {
            assert_eq!(ticks_to_ns(1, CpuTimebase { numerator: 125, denominator: 0 }), None);
        }
    }
}

#[cfg(target_os = "macos")]
fn main() {
    if let Err(error) = macos::run() {
        eprintln!("app_cpu: {error}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("app_cpu requires macOS (proc_pid_rusage)");
    std::process::exit(1);
}
