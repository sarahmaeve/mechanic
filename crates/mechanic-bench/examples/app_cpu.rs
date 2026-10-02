//! macOS GUI process CPU measurement with a controlled Rust PTY peer.
//! Usage: app_cpu APP_BINARY OUTPUT.json [--animation off|logo|background|both] [--settle-secs N]
//!        [--sample-secs N] [--label LABEL]
//!        [--workload idle|cell|row|full|scroll|unicode|atlas|text-fixture]
//!        [--render-profile] [--fixture-cursor block|bar|underline|hidden]
//!        [--services] (enable restoration/control in temporary directories)
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
    const WORKLOAD_ENV: &str = "MECHANIC_BENCH_WORKLOAD";
    const FIXTURE_CURSOR_ENV: &str = "MECHANIC_BENCH_FIXTURE_CURSOR";
    const UPDATE_INTERVAL: Duration = Duration::from_millis(50);

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Workload {
        Idle,
        Cell,
        Row,
        Full,
        Scroll,
        Unicode,
        Atlas,
        TextFixture,
    }

    impl Workload {
        fn parse(value: &str) -> Result<Self> {
            match value {
                "idle" => Ok(Self::Idle),
                "cell" => Ok(Self::Cell),
                "row" => Ok(Self::Row),
                "full" => Ok(Self::Full),
                "scroll" => Ok(Self::Scroll),
                "unicode" => Ok(Self::Unicode),
                "atlas" => Ok(Self::Atlas),
                "text-fixture" => Ok(Self::TextFixture),
                _ => Err("workload must be idle, cell, row, full, scroll, unicode, atlas, or text-fixture".into()),
            }
        }
        fn name(self) -> &'static str {
            match self {
                Self::Idle => "idle",
                Self::Cell => "cell",
                Self::Row => "row",
                Self::Full => "full",
                Self::Scroll => "scroll",
                Self::Unicode => "unicode",
                Self::Atlas => "atlas",
                Self::TextFixture => "text-fixture",
            }
        }
    }

    #[derive(Clone, Copy)]
    enum FixtureCursor {
        Block,
        Bar,
        Underline,
        Hidden,
    }

    impl FixtureCursor {
        fn parse(value: &str) -> Result<Self> {
            match value {
                "block" => Ok(Self::Block), "bar" => Ok(Self::Bar),
                "underline" => Ok(Self::Underline), "hidden" => Ok(Self::Hidden),
                _ => Err("fixture cursor must be block, bar, underline, or hidden; unfocus to inspect hollow".into()),
            }
        }
        fn name(self) -> &'static str {
            match self {
                Self::Block => "block",
                Self::Bar => "bar",
                Self::Underline => "underline",
                Self::Hidden => "hidden",
            }
        }
        fn sequence(self) -> &'static [u8] {
            match self {
                Self::Block => b"\x1b[?25h\x1b[2 q",
                Self::Bar => b"\x1b[?25h\x1b[6 q",
                Self::Underline => b"\x1b[?25h\x1b[4 q",
                Self::Hidden => b"\x1b[?25l",
            }
        }
    }

    struct Options {
        app: PathBuf,
        output: PathBuf,
        animate: bool,
        animation: &'static str,
        logo: &'static str,
        settle: u64,
        sample: u64,
        label: Option<String>,
        workload: Workload,
        render_profile: bool,
        services: bool,
        fixture_cursor: Option<FixtureCursor>,
    }

    impl Options {
        fn parse() -> Result<Self> {
            let mut args = env::args_os().skip(1);
            let usage = "usage: app_cpu APP_BINARY OUTPUT.json [--animation off|logo|background|both] [--logo triangle|atom] [--animate] [--settle-secs N] [--sample-secs N] [--label LABEL] [--workload idle|cell|row|full|scroll|unicode|atlas|text-fixture] [--render-profile] [--fixture-cursor block|bar|underline|hidden]";
            let app = args.next().ok_or(usage)?;
            if app == "--help" || app == "-h" {
                println!(
                    "{usage}\n--services enables restoration/control in temporary directories.\nKeep window size and focus unchanged during sampling; animation requires focus.\nDefaults: settle 3s, sample 5s. Existing output files are never overwritten."
                );
                std::process::exit(0);
            }
            let mut options = Self {
                app: fs::canonicalize(app)?,
                output: args.next().ok_or(usage)?.into(),
                animate: false,
                animation: "off",
                logo: "triangle",
                settle: 3,
                sample: 5,
                label: None,
                workload: Workload::Idle,
                render_profile: false,
                services: false,
                fixture_cursor: None,
            };
            while let Some(arg) = args.next() {
                match arg.to_str().ok_or("non-UTF8 option")? {
                    "--animate" => {
                        options.animate = true;
                        options.animation = "both";
                    }
                    "--animation" => {
                        options.animation = match args.next().as_deref().and_then(|v| v.to_str()) {
                            Some("off") => "off",
                            Some("logo") => "logo",
                            Some("background") => "background",
                            Some("both") => "both",
                            _ => {
                                return Err(
                                    "animation must be off, logo, background, or both".into()
                                );
                            }
                        };
                        options.animate = options.animation != "off";
                    }
                    "--logo" => {
                        options.logo = match args.next().as_deref().and_then(|v| v.to_str()) {
                            Some("triangle") => "triangle",
                            Some("atom") => "atom",
                            _ => return Err("logo must be triangle or atom".into()),
                        };
                    }
                    "--render-profile" => options.render_profile = true,
                    "--services" => options.services = true,
                    "--fixture-cursor" => {
                        options.fixture_cursor = Some(FixtureCursor::parse(
                            args.next()
                                .ok_or("missing fixture cursor")?
                                .to_str()
                                .ok_or("non-UTF8 fixture cursor")?,
                        )?);
                    }
                    "--workload" => {
                        options.workload = Workload::parse(
                            args.next()
                                .ok_or("missing workload")?
                                .to_str()
                                .ok_or("non-UTF8 workload")?,
                        )?;
                    }
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
            if options.animate && options.render_profile {
                return Err("animation cannot be combined with --render-profile".into());
            }
            if options.fixture_cursor.is_some() && options.workload != Workload::TextFixture {
                return Err("--fixture-cursor applies only to --workload text-fixture".into());
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
    struct RenderFrame {
        conversion_ns: u64,
        cols: u64,
        rows: u64,
        atlas_ns: u64,
        instances_ns: u64,
        upload_ns: u64,
        surface_ns: u64,
        submit_present_ns: u64,
        instance_count: u64,
        upload_bytes: u64,
        atlas_changed: bool,
        presented: bool,
        raw_conversion_line: String,
        raw_pipeline_line: String,
    }

    fn profile_value(line: &str, key: &str) -> Option<u64> {
        line.split_whitespace().find_map(|field| {
            let (name, value) = field.split_once('=')?;
            if name == key { value.parse().ok() } else { None }
        })
    }

    fn profile_bool(line: &str, key: &str) -> Option<bool> {
        match profile_value(line, key)? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    fn render_frames(log: &str) -> Vec<RenderFrame> {
        let mut pending = None;
        let mut frames = Vec::new();
        for line in log.lines().filter(|line| line.contains("mechanic_render_profile")) {
            let Some((_, values)) = line.split_once("render-profile ") else {
                continue;
            };
            if values.starts_with("conversion_ns=") {
                // A new conversion also discards an incomplete preceding frame.
                pending = profile_value(values, "conversion_ns")
                    .zip(profile_value(values, "cols"))
                    .zip(profile_value(values, "rows"))
                    .map(|((conversion_ns, cols), rows)| {
                        (conversion_ns, cols, rows, line.to_owned())
                    });
            } else if values.starts_with("atlas_ns=") {
                let Some((conversion_ns, cols, rows, raw_conversion_line)) = pending.take() else {
                    continue;
                };
                let frame = (|| {
                    Some(RenderFrame {
                        conversion_ns,
                        cols,
                        rows,
                        atlas_ns: profile_value(values, "atlas_ns")?,
                        instances_ns: profile_value(values, "instances_ns")?,
                        upload_ns: profile_value(values, "upload_ns")?,
                        surface_ns: profile_value(values, "surface_ns")?,
                        submit_present_ns: profile_value(values, "submit_present_ns")?,
                        instance_count: profile_value(values, "instance_count")?,
                        upload_bytes: profile_value(values, "upload_bytes")?,
                        atlas_changed: profile_bool(values, "atlas_changed")?,
                        presented: profile_bool(values, "presented")?,
                        raw_conversion_line,
                        raw_pipeline_line: line.to_owned(),
                    })
                })();
                if let Some(frame) = frame {
                    frames.push(frame);
                }
            }
        }
        frames
    }

    fn complete_sample_lines<'a>(before: &str, after: &'a str) -> &'a str {
        let Some(mut sample) = after.get(before.len()..) else {
            return "";
        };
        if !before.is_empty() && !before.ends_with('\n') {
            let Some(first_end) = sample.find('\n') else {
                return "";
            };
            sample = &sample[first_end + 1..];
        }
        // Ignore the final partially written log line, if any.
        sample.rfind('\n').map(|end| &sample[..=end]).unwrap_or("")
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
        render_profile_enabled: bool,
        render_frames: Vec<RenderFrame>,
        render_profile_scope: &'static str,
        settings: serde_json::Value,
        scope: &'static str,
        app_stderr: String,
    }

    fn sample(options: &Options, report: &mut Report, log_path: &Path) -> Result<()> {
        // Unix socket paths must fit macOS sockaddr_un (103 bytes).
        let isolated = tempfile::tempdir_in("/tmp")?;
        let config_dir = isolated.path().join("mechanic");
        fs::create_dir(&config_dir)?;
        let runtime = isolated.path().join("runtime");
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(&runtime)?;
        let peer = env::current_exe()?;
        // JSON string escaping also gives a valid TOML basic string for this path.
        let peer_string = serde_json::to_string(peer.to_str().ok_or("non-UTF8 peer path")?)?;
        fs::write(
            config_dir.join("mechanic.toml"),
            format!(
                "[session]\nrestore = {}\n[control]\nenabled = {}\n[font]\nfamily = \"Menlo\"\nsize = 14.0\n[shell]\nprogram = {peer_string}\n[theme]\nlogo = \"{}\"\nlogo_size = 180\n[theme.animation]\nlogo = {}\nbackground = {}\n[theme.opacity]\ntitle_bar_opacity = 1.0\ncontent_active_opacity = 1.0\ncontent_idle_opacity = 1.0\ntext_idle_opacity = 1.0\n",
                options.services,
                options.services,
                options.logo,
                matches!(options.animation, "logo" | "both"),
                matches!(options.animation, "background" | "both"),
            ),
        )?;
        let mut command = Command::new(&options.app);
        command
            .env("XDG_CONFIG_HOME", isolated.path())
            .env("XDG_STATE_HOME", isolated.path().join("state"))
            .env("XDG_RUNTIME_DIR", runtime)
            .env(PEER_ENV, "1")
            .env(WORKLOAD_ENV, options.workload.name())
            .env(FIXTURE_CURSOR_ENV, options.fixture_cursor.unwrap_or(FixtureCursor::Block).name())
            .env(
                "RUST_LOG",
                if options.render_profile {
                    "warn,mechanic::app=debug,mechanic_app::app=debug,mechanic_render_profile=trace"
                } else {
                    "warn,mechanic::app=debug,mechanic_app::app=debug"
                },
            )
            .env("RUST_LOG_STYLE", "never")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(log_path)?);
        if options.animation == "both" {
            command.arg("--hot-cpu");
        }
        let mut app = OwnedApp(command.spawn()?);
        let pid = app.0.id();
        report.pid = Some(pid);
        wait_alive(&mut app, Duration::from_secs(options.settle))?;
        if options.services {
            use std::os::unix::fs::FileTypeExt;
            let saved = isolated.path().join("state/mechanic/session.json").is_file();
            let socket = fs::read_dir(isolated.path().join("runtime/mechanic"))?
                .filter_map(|entry| entry.ok())
                .any(|entry| entry.file_type().is_ok_and(|kind| kind.is_socket()));
            if !saved || !socket {
                return Err("session save or control socket did not initialize".into());
            }
        }
        let before_log = fs::read_to_string(log_path)?;
        let before_events = focus_events(&before_log);
        report.focused_at_start = before_events.last().copied();
        if options.render_profile
            && let Some(frame) = render_frames(&before_log).last()
        {
            report.settings["actual_terminal_columns"] = frame.cols.into();
            report.settings["actual_terminal_rows"] = frame.rows.into();
        }
        let start = cpu_counters(pid, report.cpu_timebase)?;
        let started = Instant::now();
        report.cpu_start = Some(start);
        wait_alive(&mut app, Duration::from_secs(options.sample))?;
        let end = cpu_counters(pid, report.cpu_timebase)?;
        let elapsed = started.elapsed().as_secs_f64();
        report.cpu_end = Some(end);
        report.elapsed_seconds = Some(elapsed);
        report.app_stderr = fs::read_to_string(log_path)?;
        if options.render_profile {
            report.render_frames =
                render_frames(complete_sample_lines(&before_log, &report.app_stderr));
            if let Some(frame) = report.render_frames.last() {
                report.settings["actual_terminal_columns"] = frame.cols.into();
                report.settings["actual_terminal_rows"] = frame.rows.into();
            }
        }
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
        if options.render_profile
            && options.workload != Workload::Idle
            && report.render_frames.is_empty()
        {
            return Err("no complete render profile frames observed; ensure the app has profiling instrumentation".into());
        }
        if report.render_frames.first().is_some_and(|first| {
            report
                .render_frames
                .iter()
                .any(|frame| frame.cols != first.cols || frame.rows != first.rows)
        }) {
            return Err(
                "terminal dimensions changed during sampling; keep the window unchanged and rerun"
                    .into(),
            );
        }
        Ok(())
    }

    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    const MULTILINGUAL_ROWS: &[&str] = &[
        "Русский: Съешь ещё этих мягких французских булок, да выпей чаю.",
        "Українська: Ґрунт, їжак, єдність — Ще трохи українського тексту.",
        "日本語: 東京の新聞を読みます。ひらがな・カタカナ・漢字。日本語１２３",
        "العربية: السَّلَامُ عَلَيْكُمْ | لا لأ لإ لآ | مُحَمَّدٌ يَقْرَأُ الصَّحِيفَةَ",
        "Arabic + Latin: الأخبار (Mechanic 123) اليوم ٢٠٢٦، السعر 45.50 USD.",
        "Français: Où est le café ? Ça va très bien, Noël à Strasbourg.",
        "Deutsch: Grüße aus Köln — Straße, größer, süß, ÄÖÜ äöü ß.",
        "Español: ¡Buenos días! El niño pidió café; pingüino, corazón, ¿qué tal?",
        "Português: São Paulo, ação, coração, amanhã; avó e avô estão aqui.",
        "Italiano: Città, perché, più, già; un caffè è pronto per l'ospite.",
        "Decomposed: Cafe\u{301} | Gru\u{308}ße | nin\u{303}o | ac\u{327}a\u{303}o | piu\u{300}",
    ];
    // Original fixture prose, not a quotation from a published article.
    const ARABIC_PARAGRAPH: &str = "أعلنت الجهاتُ المعنيّة، صباحَ اليوم، إطلاقَ مشروعٍ جديدٍ لتحسين خدمات النقل في المدينة. وقال المسؤولون إنّ المرحلة الأولى تبدأ في ١٥ أكتوبر ٢٠٢٦، وتشمل 24 محطةً ومركزًا للتدريب. وأضاف التقرير: «لا تكتملُ التنميةُ إلا بمشاركة المجتمع»، مع تخصيص ٣٫٥ ملايين دولار للبرنامج. وستُنشر النتائج عبر منصة Open Data؛ ويستطيع القرّاء متابعة التفاصيل، ومقارنة أرقام العام الماضي، وطرح الأسئلة على فريق العمل.";

    fn fixture_cursor_row(rows: usize) -> usize {
        (MULTILINGUAL_ROWS.len() + 5).min(rows)
    }

    fn atlas_rows(cols: usize) -> Vec<String> {
        let latin: Vec<char> = (0x00c0..=0x00ff).filter_map(char::from_u32).collect();
        let cyrillic: Vec<char> = (0x0410..=0x044f).filter_map(char::from_u32).collect();
        let japanese: Vec<char> =
            (0x3041..=0x307f).chain(0x4e00..=0x4e1f).filter_map(char::from_u32).collect();
        let mut lines =
            vec!["Atlas: >128 distinct Latin/Cyrillic/Japanese chars, four styles".into()];
        for (name, sgr) in
            [("regular", "0"), ("bold", "1"), ("italic", "3"), ("bold italic", "1;3")]
        {
            lines.push(format!("\x1b[0mStyle: {name}"));
            for (chars, width) in [(&latin, 1), (&cyrillic, 1), (&japanese, 2)] {
                for chunk in chars.chunks((cols / width).max(1)) {
                    lines.push(format!("\x1b[{sgr}m{}\x1b[0m", chunk.iter().collect::<String>()));
                }
            }
        }
        lines
    }

    fn multilingual_output(workload: Workload, cols: usize, rows: usize) -> io::Result<Vec<u8>> {
        let mut output = Vec::new();
        let lines = if workload == Workload::Atlas {
            atlas_rows(cols)
        } else {
            let mut lines = vec![
                "Mechanic multilingual fixture — fixed layout; status cell changes at 20Hz".into(),
            ];
            lines.extend(MULTILINGUAL_ROWS.iter().map(|line| (*line).to_owned()));
            lines.push("Styles: \x1b[1mbold\x1b[0m | \x1b[3mitalic\x1b[0m | \x1b[4munderline\x1b[0m | \x1b[7minverse\x1b[0m".into());
            lines.push(
                "Wide bg: \x1b[48;2;30;90;150m日本語　界界\x1b[0m | decomposed e\u{301}\u{308}"
                    .into(),
            );
            lines.push("Conceal: visible [\x1b[8mSECRET MUST STAY HIDDEN\x1b[0m] visible".into());
            lines.push("\x1b[48;2;100;40;100m界\x1b[0m  Cursor over a wide character; unfocus for hollow outline.".into());
            lines
        };
        for (row, line) in lines.iter().take(rows.saturating_sub(1)).enumerate() {
            write!(output, "\x1b[{};1H\x1b[0m{line}\x1b[0m", row + 1)?;
        }
        if workload != Workload::Atlas {
            let paragraph_row = MULTILINGUAL_ROWS.len() + 6;
            let estimated_rows = ARABIC_PARAGRAPH.chars().count().div_ceil(cols.max(1));
            if paragraph_row + estimated_rows < rows {
                // Exercise actual terminal wrapping and paragraph bidi, without
                // scrolling away the fixed multilingual samples above it.
                write!(output, "\x1b[{paragraph_row};1H\x1b[?7h{ARABIC_PARAGRAPH}\x1b[?7l")?;
            }
        }
        write!(output, "\x1b[{rows};1H\x1b[0mStatus (last cell changes):")?;
        Ok(output)
    }

    fn paint_row(output: &mut Vec<u8>, cols: usize, row: usize, phase: usize) -> io::Result<()> {
        write!(output, "\x1b[{};1H", row + 1)?;
        output.extend((0..cols).map(|col| ALPHABET[(col + row + phase) % ALPHABET.len()]));
        Ok(())
    }

    fn workload_output(
        workload: Workload,
        cols: usize,
        rows: usize,
        phase: usize,
    ) -> io::Result<Vec<u8>> {
        let mut output = Vec::new();
        match workload {
            Workload::Idle => {}
            Workload::Cell => {
                output.extend_from_slice(b"\x1b[1;1H");
                output.push(ALPHABET[phase % ALPHABET.len()]);
            }
            Workload::Row => paint_row(&mut output, cols, rows / 2, phase)?,
            Workload::Full => {
                for row in 0..rows {
                    paint_row(&mut output, cols, row, phase)?;
                }
            }
            Workload::Scroll => {
                write!(output, "\x1b[{rows};1H\r\n")?;
                paint_row(&mut output, cols, rows - 1, phase)?;
            }
            Workload::Unicode | Workload::Atlas | Workload::TextFixture => {
                write!(output, "\x1b[{rows};{cols}H\x1b[0m")?;
                output.push(ALPHABET[phase % ALPHABET.len()]);
                if workload == Workload::TextFixture {
                    write!(output, "\x1b[{};1H", fixture_cursor_row(rows))?;
                }
            }
        }
        Ok(output)
    }

    fn peer() -> Result<()> {
        let workload = Workload::parse(&env::var(WORKLOAD_ENV).unwrap_or_else(|_| "idle".into()))?;
        // Reset SIGHUP in case the parent inherited an ignored disposition.
        unsafe {
            libc::signal(libc::SIGHUP, libc::SIG_DFL);
            libc::signal(libc::SIGPIPE, libc::SIG_IGN);
            // Also bound a write blocked by PTY backpressure or a stalled app.
            libc::signal(libc::SIGALRM, libc::SIG_DFL);
            libc::alarm(60);
        }
        let mut stdout = io::stdout().lock();
        let mut cols = 0;
        let mut rows = 0;
        if workload != Workload::Idle {
            let mut termios: libc::termios = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut termios) } != 0 {
                return Err(io::Error::last_os_error().into());
            }
            unsafe {
                libc::cfmakeraw(&mut termios);
            }
            if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &termios) } != 0 {
                return Err(io::Error::last_os_error().into());
            }
            let mut size: libc::winsize = unsafe { std::mem::zeroed() };
            if unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut size) } != 0 {
                return Err(io::Error::last_os_error().into());
            }
            cols = usize::from(size.ws_col);
            rows = usize::from(size.ws_row);
            if cols == 0 || rows == 0 {
                return Err("PTY has zero dimensions".into());
            }
            // Hide the cursor and disable autowrap so writing the final column
            // never adds an accidental scroll. Warm the fixed ASCII alphabet.
            stdout.write_all(b"\x1b[?25l\x1b[?7l\x1b[2J\x1b[H")?;
            if matches!(workload, Workload::Unicode | Workload::Atlas | Workload::TextFixture) {
                stdout.write_all(&multilingual_output(workload, cols, rows)?)?;
                if workload == Workload::TextFixture {
                    let cursor = FixtureCursor::parse(
                        &env::var(FIXTURE_CURSOR_ENV).unwrap_or_else(|_| "block".into()),
                    )?;
                    stdout.write_all(cursor.sequence())?;
                    write!(stdout, "\x1b[{};1H", fixture_cursor_row(rows))?;
                }
            } else {
                stdout.write_all(&workload_output(Workload::Full, cols, rows, 0)?)?;
            }
            stdout.flush()?;
        }
        // Idle has no periodic wakeups. Output workloads change content at 20Hz.
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut next_update = Instant::now() + UPDATE_INTERVAL;
        let mut phase = 1;
        loop {
            let now = Instant::now();
            let remaining = deadline.saturating_duration_since(now);
            if remaining.is_zero() {
                return Ok(());
            }
            if workload != Workload::Idle && now >= next_update {
                stdout.write_all(&workload_output(workload, cols, rows, phase)?)?;
                stdout.flush()?;
                phase = (phase + 1) % ALPHABET.len();
                next_update += UPDATE_INTERVAL;
                // Avoid bursts of catch-up output if the app imposed backpressure.
                if next_update <= Instant::now() {
                    next_update = Instant::now() + UPDATE_INTERVAL;
                }
                continue;
            }
            let wake_at =
                if workload == Workload::Idle { deadline } else { deadline.min(next_update) };
            let timeout = wake_at.saturating_duration_since(Instant::now());
            // Round up sub-millisecond waits; poll(timeout=0) would busy-spin.
            let timeout_ms = timeout.as_nanos().div_ceil(1_000_000);
            let mut fd = libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 };
            let ready = unsafe { libc::poll(&mut fd, 1, timeout_ms.min(i32::MAX as u128) as i32) };
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
            return match peer() {
                Err(error)
                    if error
                        .downcast_ref::<io::Error>()
                        .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe) =>
                {
                    Ok(())
                }
                result => result,
            };
        }
        let options = Options::parse()?;
        let metadata = fs::metadata(&options.app)?;
        let timebase = cpu_timebase()?;
        // Reserve output before launching a window; never truncate earlier results.
        let mut output = OpenOptions::new().write(true).create_new(true).open(&options.output)?;
        let log = tempfile::NamedTempFile::new()?;
        let mut report = Report {
            schema_version: 3,
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
            mode: if options.animate { "focused_animation" } else { options.workload.name() },
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
            render_profile_enabled: options.render_profile,
            render_frames: Vec::new(),
            render_profile_scope: "Raw complete full-content frames during sampling. Stage timings measure host CPU/wall work only; upload_ns measures host writes/resizing, atlas_changed denotes atlas generation/bind-group change, and presented indicates host present issued. No GPU completion timing. Calculate stage distributions from these records.",
            settings: serde_json::json!({
                "font_family": "Menlo", "font_size_points": 14,
                "opacity": 1.0, "pty_peer": "controlled Rust peer",
                "workload": options.workload.name(),
                "animation": options.animation,
                "session_and_control_enabled": options.services,
                "logo": options.logo,
                "logo_size_physical_pixels": 180,
                "workload_update_interval_ms": if options.workload == Workload::Idle { None } else { Some(50) },
                "workload_alphabet": "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
                "fixture_cursor": if options.workload == Workload::TextFixture { Some(options.fixture_cursor.unwrap_or(FixtureCursor::Block).name()) } else { None },
                "unicode_samples": if matches!(options.workload, Workload::Unicode | Workload::TextFixture) { Some(MULTILINGUAL_ROWS) } else { None },
                "arabic_paragraph_original_fixture": if matches!(options.workload, Workload::Unicode | Workload::TextFixture) { Some(ARABIC_PARAGRAPH) } else { None },
                "atlas_fixture": if options.workload == Workload::Atlas { Some("64 Latin + 64 Cyrillic + 95 Japanese scalars, regular/bold/italic/bold-italic; clipped to viewport") } else { None },
                "actual_terminal_columns": null,
                "actual_terminal_rows": null,
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
        use super::{
            ARABIC_PARAGRAPH, CpuTimebase, Workload, atlas_rows, complete_sample_lines,
            multilingual_output, render_frames, ticks_to_ns, workload_output,
        };

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

        #[test]
        fn render_profile_keeps_only_complete_pairs_inside_sample() {
            let pipeline = "[TRACE mechanic_render_profile] render-profile atlas_ns=2 instances_ns=3 upload_ns=4 surface_ns=5 submit_present_ns=6 instance_count=70 upload_bytes=800 atlas_changed=0 presented=1\n";
            let conversion =
                "[TRACE mechanic_render_profile] render-profile conversion_ns=1 cols=120 rows=40\n";
            let log = format!("{pipeline}{conversion}{pipeline}{conversion}");
            let frames = render_frames(&log);
            assert_eq!(frames.len(), 1);
            assert_eq!(frames[0].cols, 120);
            assert_eq!(frames[0].upload_bytes, 800);
            assert!(!frames[0].atlas_changed);
            assert!(frames[0].presented);
            assert_eq!(frames[0].raw_conversion_line, conversion.trim_end());
            let before = "partial line";
            let after = format!("{before} remainder\n{conversion}{pipeline}last partial");
            let sampled = complete_sample_lines(before, &after);
            assert_eq!(sampled, format!("{conversion}{pipeline}"));
            assert_eq!(render_frames(sampled).len(), 1);
        }

        #[test]
        fn render_profile_drops_incomplete_or_invalid_pipeline() {
            let conversion =
                "[TRACE mechanic_render_profile] render-profile conversion_ns=1 cols=120 rows=40\n";
            let incomplete = "[TRACE mechanic_render_profile] render-profile atlas_ns=2 instances_ns=3 upload_ns=4\n";
            assert!(render_frames(&format!("{conversion}{incomplete}")).is_empty());
        }

        #[test]
        fn atlas_fixture_exceeds_old_distinct_character_limit_in_four_styles() {
            let rows = atlas_rows(120);
            let distinct: std::collections::BTreeSet<char> = rows
                .iter()
                .flat_map(|row| row.chars())
                .filter(|character| !character.is_ascii())
                .collect();
            assert_eq!(distinct.len(), 223);
            for style in ["\x1b[0m", "\x1b[1m", "\x1b[3m", "\x1b[1;3m"] {
                assert!(rows.iter().any(|row| row.starts_with(style) && row.contains('À')));
            }
            assert!(rows.len() < 40);
        }

        #[test]
        fn multilingual_fixture_keeps_original_arabic_paragraph_and_small_updates() {
            let initial =
                String::from_utf8(multilingual_output(Workload::TextFixture, 120, 40).unwrap())
                    .unwrap();
            assert!(ARABIC_PARAGRAPH.chars().count() > 120);
            assert!(initial.contains(ARABIC_PARAGRAPH));
            assert!(initial.contains("\x1b[?7h"));
            assert!(initial.contains("Cafe\u{301}"));
            assert!(initial.contains("SECRET MUST STAY HIDDEN"));
            assert!(initial.contains("\x1b[8m"));
            let update =
                String::from_utf8(workload_output(Workload::TextFixture, 120, 40, 1).unwrap())
                    .unwrap();
            assert_eq!(update, "\x1b[40;120H\x1b[0mB\x1b[16;1H");
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
