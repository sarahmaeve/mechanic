//! Run inside an isolated terminal window; samples its parent process at 20 Hz.

#[cfg(target_os = "macos")]
#[path = "../src/tty.rs"]
mod tty;

#[cfg(target_os = "macos")]
mod macos {
    use super::tty::Tty;
    use objc2_app_kit::NSWorkspace;
    use serde_json::json;
    use std::{
        env,
        fs::File,
        io::{self, Write},
        thread,
        time::{Duration, Instant},
    };

    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
    const INTERVAL: Duration = Duration::from_millis(50);
    const SAMPLES: usize = 60;
    const WARMUPS: usize = 20;
    const TIMEOUT: Duration = Duration::from_secs(5);
    const ARABIC: &str = "تحتفظ اللغة العربية بسياق الفقرة عند التفاف السطور، وتعرض الأخبار والأسئلة بوضوح. القاهرة ٢٠٢٦؛ تقرير 42. ";
    const CHINESE: &str = "新闻报道展示中文段落在终端窗口中自动换行时的文字显示与更新性能";
    const JAPANESE: &str =
        "日本語の新聞記事を端末で表示して自動折り返しと文字更新の性能を確認します";
    const KOREAN: &str = "신문기사를터미널에서표시하고자동줄바꿈과글자변경의성능을확인합니다";

    fn record(output: &mut File, value: serde_json::Value) -> Result<()> {
        serde_json::to_writer(&mut *output, &value)?;
        writeln!(output)?;
        output.flush()?;
        Ok(())
    }

    fn foreground(pid: i32) -> bool {
        NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .is_some_and(|app| app.processIdentifier() == pid && !app.isHidden())
    }

    fn process_path(pid: i32) -> Result<String> {
        let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: buf is writable for the supplied size.
        let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
        if len <= 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(String::from_utf8_lossy(&buf[..len as usize]).trim_end_matches('\0').to_owned())
    }

    fn emulator_pid() -> Result<i32> {
        // Ghostty may insert /usr/bin/login between the emulator and command.
        let mut pid = unsafe { libc::getppid() };
        for _ in 0..8 {
            let path = process_path(pid)?;
            if ["/ghostty", "/mechanic", "/iTerm2"].iter().any(|name| path.ends_with(name)) {
                return Ok(pid);
            }
            let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of_val(&info) as i32;
            // SAFETY: proc_pidinfo writes at most size bytes to the output structure.
            let count = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDTBSDINFO,
                    0,
                    (&mut info as *mut libc::proc_bsdinfo).cast(),
                    size,
                )
            };
            if count != size || info.pbi_ppid <= 1 {
                break;
            }
            pid = info.pbi_ppid as i32;
        }
        Err("no supported terminal emulator found in parent chain".into())
    }

    fn cpu_ticks(pid: i32) -> Result<u128> {
        // SAFETY: zero initializes this C output structure; the call fills it.
        let mut usage: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::proc_pid_rusage(
                pid,
                libc::RUSAGE_INFO_V2,
                (&mut usage as *mut libc::rusage_info_v2).cast(),
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(u128::from(usage.ri_user_time) + u128::from(usage.ri_system_time))
    }

    fn fixture(text: &str, wide: bool, wrapped: bool, cols: usize, rows: usize) -> String {
        let stride = if wide { 2 } else { 1 };
        if wrapped {
            text.chars().cycle().take(cols * rows * 2 / stride).collect()
        } else {
            // Leave a spare cell to avoid autowrap; preserve explicit row breaks.
            let line: String = text.chars().cycle().take((cols - 1) / stride).collect();
            std::iter::repeat_n(line, rows).collect::<Vec<_>>().join("\r\n")
        }
    }

    fn update(index: usize, wide: bool, row: usize) -> Vec<u8> {
        let zero = if wide { '０' } else { '0' } as u32;
        let digits: String = (0..3)
            .map(|place| char::from_u32(zero + ((index / 10usize.pow(place)) % 10) as u32).unwrap())
            .collect();
        format!("\x1b[{};1H{digits}", row + 1).into_bytes()
    }

    #[allow(deprecated)]
    fn measure(output: &mut File, pid: i32, label: &str) -> Result<()> {
        let mut tty = Tty::open()?;
        thread::sleep(Duration::from_secs(3));
        let size = tty.size()?;
        if !(16..=500).contains(&size.0) || !(4..=200).contains(&size.1) {
            return Err(format!("expected 16..500 columns and 4..200 rows, got {size:?}").into());
        }
        let mut timebase = libc::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: the pointer names a writable Mach timebase structure.
        if unsafe { libc::mach_timebase_info(&mut timebase) } != 0 || timebase.denom == 0 {
            return Err("Mach timebase unavailable".into());
        }
        record(
            output,
            json!({"type":"metadata", "schema":1,
            "label": label, "pid":pid,
            "process_path":process_path(pid)?, "cols":size.0,"rows":size.1,
            "profile":if cfg!(debug_assertions) {"debug"} else {"release"},
            "term_program":env::var("TERM_PROGRAM").unwrap_or_default(),
            "term_program_version":env::var("TERM_PROGRAM_VERSION").unwrap_or_default(),
            "interval_ms":50,"samples":SAMPLES,"warmups":WARMUPS,
            "mach_numer":timebase.numer,"mach_denom":timebase.denom,
            "scope":"Emulator process CPU (one core = 100%) and parser-response latency; excludes GPU, WindowServer, PTY peer, and presentation completion."}),
        )?;
        if !foreground(pid) {
            return Err("benchmark window is not frontmost".into());
        }
        for (case_index, (name, text, wide, wrapped)) in [
            ("idle", "", false, false),
            ("ascii_wrapped", "the quick brown fox jumps over the lazy dog ", false, true),
            ("arabic_rows", ARABIC, false, false),
            ("arabic_wrapped", ARABIC, false, true),
            ("chinese_rows", CHINESE, true, false),
            ("chinese_wrapped", CHINESE, true, true),
            ("japanese_rows", JAPANESE, true, false),
            ("japanese_wrapped", JAPANESE, true, true),
            ("korean_rows", KOREAN, true, false),
            ("korean_wrapped", KOREAN, true, true),
        ]
        .into_iter()
        .enumerate()
        {
            tty.write(
                format!(
                    "\x1b]0;Paragraph benchmark {}/10: {name} - closes automatically\x07",
                    case_index + 1
                )
                .as_bytes(),
                Instant::now() + TIMEOUT,
            )?;
            tty.reset(true, TIMEOUT)?;
            tty.write(
                fixture(text, wide, wrapped, size.0, size.1).as_bytes(),
                Instant::now() + TIMEOUT,
            )?;
            tty.fence(Instant::now() + TIMEOUT)?;
            // Warm the font/atlas, then warm fresh edits at the measured cadence.
            thread::sleep(Duration::from_millis(500));
            let edits: Vec<_> =
                (0..WARMUPS + SAMPLES).map(|index| update(index, wide, size.1 / 2)).collect();
            for edit in edits.iter().take(WARMUPS) {
                let start = Instant::now();
                if name != "idle" {
                    tty.write(edit, start + TIMEOUT)?;
                    tty.fence(start + TIMEOUT)?;
                }
                thread::sleep(INTERVAL.saturating_sub(start.elapsed()));
            }
            let focused_start = foreground(pid);
            let before = cpu_ticks(pid)?;
            let start = Instant::now();
            let mut responses = Vec::new();
            let mut focus_lost = !focused_start;
            let mut changed_size = false;
            let mut last_size = size;
            let mut late = 0;
            for (sample, edit) in edits.iter().skip(WARMUPS).enumerate() {
                focus_lost |= !foreground(pid);
                last_size = tty.size()?;
                changed_size |= last_size != size;
                let tick = Instant::now();
                if name != "idle" {
                    tty.write(edit, tick + TIMEOUT)?;
                    tty.fence(tick + TIMEOUT)?;
                    responses.push(tick.elapsed().as_secs_f64() * 1000.0);
                }
                let deadline = start + INTERVAL * (sample + 1) as u32;
                late += usize::from(Instant::now() > deadline);
                thread::sleep(deadline.saturating_duration_since(Instant::now()));
            }
            let elapsed = start.elapsed().as_secs_f64();
            let ticks =
                cpu_ticks(pid)?.checked_sub(before).ok_or("CPU counters moved backwards")?;
            let cpu_ns = ticks * u128::from(timebase.numer) / u128::from(timebase.denom);
            let valid = !focus_lost && !changed_size;
            record(
                output,
                json!({"type":"sample","case":name,"valid":valid,
                "focus_lost":focus_lost,"size_changed":changed_size,"last_size":last_size,"seconds":elapsed,
                "cpu_ns":cpu_ns,"cpu_percent":cpu_ns as f64 / (elapsed * 1e9) * 100.0,
                "parser_response_ms":responses,"late_intervals":late}),
            )?;
            if !valid {
                return Err("focus or dimensions changed during sample".into());
            }
        }
        Ok(())
    }

    pub fn run() -> Result<()> {
        let args: Vec<_> = env::args().skip(1).collect();
        let label = args
            .first()
            .cloned()
            .or_else(|| env::var("MECHANIC_PARAGRAPH_LABEL").ok())
            .ok_or("usage: paragraph_ui LABEL OUTPUT.jsonl [EMULATOR_PID_FILE]")?;
        let path = args
            .get(1)
            .cloned()
            .or_else(|| env::var("MECHANIC_PARAGRAPH_REPORT").ok())
            .ok_or("missing output path")?;
        let mut output = File::options().write(true).create_new(true).open(path)?;
        let result = (|| {
            let pid = if let Some(path) = args.get(2) {
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    if let Ok(text) = std::fs::read_to_string(path)
                        && let Ok(pid) = text.trim().parse::<i32>()
                    {
                        break pid;
                    }
                    if Instant::now() >= deadline {
                        return Err("emulator PID file unavailable".into());
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            } else {
                emulator_pid()?
            };
            let process = process_path(pid)?;
            if !["/ghostty", "/mechanic", "/iTerm2"].iter().any(|name| process.ends_with(name)) {
                return Err(format!("PID {pid} is not a supported terminal: {process}").into());
            }
            measure(&mut output, pid, &label)
        })();
        record(
            &mut output,
            json!({"type":"complete","success":result.is_ok(),
            "error":result.as_ref().err().map(ToString::to_string)}),
        )?;
        result
    }
}

fn main() {
    #[cfg(target_os = "macos")]
    if let Err(error) = macos::run() {
        eprintln!("paragraph_ui: {error}");
        std::process::exit(1);
    }
    #[cfg(not(target_os = "macos"))]
    eprintln!("paragraph_ui requires macOS process counters and AppKit");
}
