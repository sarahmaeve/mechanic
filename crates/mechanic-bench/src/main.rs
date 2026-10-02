mod paste;
mod tty;
mod workload;

use alacritty_terminal::{Term, grid::Dimensions, term::Config, vte::ansi::Processor};
use mechanic_core::EventProxy;
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    hint::black_box,
    io,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Deserialize, Serialize)]
struct Measurement {
    case: String,
    bytes: usize,
    operations: usize,
    seconds: Vec<f64>,
}

impl Measurement {
    fn percentile(&self, p: f64) -> f64 {
        let mut values = self.seconds.clone();
        values.sort_by(f64::total_cmp);
        values[(values.len() as f64 * p).ceil().max(1.0) as usize - 1]
    }
}

#[derive(Deserialize, Serialize)]
struct Report {
    schema: u32,
    workload_version: u32,
    mode: String,
    build_profile: String,
    label: String,
    cols: usize,
    rows: usize,
    requested_bytes: usize,
    warmups: usize,
    samples: usize,
    os: String,
    arch: String,
    unix_seconds: u64,
    term: String,
    term_program: String,
    term_program_version: String,
    notes: String,
    measurements: Vec<Measurement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    paste: Option<paste::Metadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    paste_samples: Option<Vec<paste::Sample>>,
}

struct Options {
    mode: String,
    label: String,
    output: String,
    cols: usize,
    rows: usize,
    bytes: usize,
    samples: usize,
    case: Option<String>,
    timeout: Duration,
    notes: String,
    reader_delay: Duration,
    duplex_bytes: usize,
}

struct ResultFile {
    file: fs::File,
    path: String,
    complete: bool,
}

impl Drop for ResultFile {
    fn drop(&mut self) {
        if !self.complete {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn usage() -> &'static str {
    "Usage:\n  mechanic-bench terminal LABEL OUTPUT.json [OPTIONS]\n  mechanic-bench core OUTPUT.json [OPTIONS]\n  mechanic-bench paste OUTPUT.json [OPTIONS]\n  mechanic-bench compare RESULTS.json RESULTS.json [...]\n\nOptions: --cols N --rows N --mib N --samples N --case NAME --timeout SECONDS --notes TEXT\nDefaults: 120 columns, 40 rows, 4 MiB, 5 samples, 30-second timeout.\nCases: ascii, wrap, unicode, sgr, scrollback, scroll-region, repaint, sparse.\nCore mode also measures paste filtering and resize/reflow. Run a release build.\nTerminal mode requires a fresh, focused terminal window; scrollback is cleared.\nPaste defaults: 1 MiB, 500 ms delayed reader; --delay-ms N --duplex-mib N --label TEXT.\nPaste measures core transport entry and verified delivery, without a GUI."
}

fn options(args: &[String]) -> Result<Options, String> {
    let mode = args.first().ok_or_else(|| usage().to_owned())?.clone();
    let (label, output_index) = match mode.as_str() {
        "terminal" => (args.get(1).ok_or("missing terminal label")?.clone(), 2),
        "core" => ("mechanic-core".into(), 1),
        "paste" => ("mechanic-paste".into(), 1),
        _ => return Err(usage().into()),
    };
    let output = args.get(output_index).ok_or("missing JSON output path")?.clone();
    let mut o = Options {
        mode: mode.clone(),
        label,
        output,
        cols: 120,
        rows: 40,
        bytes: if mode == "paste" { 1024 * 1024 } else { 4 * 1024 * 1024 },
        samples: 5,
        case: None,
        timeout: Duration::from_secs(30),
        notes: String::new(),
        reader_delay: Duration::from_millis(500),
        duplex_bytes: 0,
    };
    let mut flags = args[output_index + 1..].iter();
    while let Some(flag) = flags.next() {
        let value = flags.next().ok_or_else(|| format!("missing value for {flag}"))?;
        let number =
            || value.parse::<usize>().map_err(|_| format!("invalid number for {flag}: {value}"));
        match flag.as_str() {
            "--cols" => o.cols = number()?,
            "--rows" => o.rows = number()?,
            "--mib" => o.bytes = number()?.checked_mul(1024 * 1024).ok_or("size overflow")?,
            "--samples" => o.samples = number()?,
            "--case" => o.case = Some(value.clone()),
            "--timeout" => o.timeout = Duration::from_secs(number()? as u64),
            "--notes" => o.notes = value.clone(),
            "--label" if mode == "paste" => o.label = value.clone(),
            "--delay-ms" if mode == "paste" => {
                o.reader_delay = Duration::from_millis(number()? as u64)
            }
            "--duplex-mib" if mode == "paste" => {
                o.duplex_bytes = number()?.checked_mul(1024 * 1024).ok_or("size overflow")?
            }
            _ => return Err(format!("unknown option: {flag}")),
        }
    }
    if !(16..=500).contains(&o.cols)
        || !(4..=200).contains(&o.rows)
        || !(1..=1000).contains(&o.samples)
        || !(1..=256 * 1024 * 1024).contains(&o.bytes)
        || o.timeout.is_zero()
        || o.timeout.as_secs() > 3600
    {
        return Err(
            "expected cols 16..500, rows 4..200, samples 1..1000, MiB 1..256, timeout 1..3600"
                .into(),
        );
    }
    let cases = if mode == "paste" { paste::CASES } else { workload::CASES };
    if o.case.as_ref().is_some_and(|case| !cases.contains(&case.as_str())) {
        return Err("unknown workload case".into());
    }
    if mode == "paste"
        && (o.reader_delay.is_zero()
            || o.reader_delay > Duration::from_secs(5)
            || o.reader_delay >= o.timeout
            || o.duplex_bytes > 64 * 1024 * 1024
            || (o.case.as_deref() == Some("duplex") && o.duplex_bytes == 0))
    {
        return Err("paste requires delay 1..5000 ms below timeout, duplex 0..64 MiB (nonzero for duplex case)".into());
    }
    Ok(o)
}

struct Size(usize, usize);
impl Dimensions for Size {
    fn columns(&self) -> usize {
        self.0
    }
    fn screen_lines(&self) -> usize {
        self.1
    }
    fn total_lines(&self) -> usize {
        self.1
    }
}

fn new_term(o: &Options) -> Term<EventProxy> {
    Term::new(
        Config { scrolling_history: 10_000, ..Config::default() },
        &Size(o.cols, o.rows),
        EventProxy::new(),
    )
}

fn measure(
    o: &Options,
    case: &str,
    data: &[u8],
    tty: &mut Option<tty::Tty>,
) -> io::Result<Measurement> {
    let mut result =
        Measurement { case: case.into(), bytes: data.len(), operations: 1, seconds: Vec::new() };
    for sample in 0..=o.samples {
        let elapsed = if let Some(tty) = tty {
            if tty.size()? != (o.cols, o.rows) {
                return Err(io::Error::other(
                    "window size changed or does not match --cols/--rows",
                ));
            }
            tty.reset(case == "scrollback", o.timeout)?;
            let start = Instant::now();
            let deadline = start + o.timeout;
            tty.write(data, deadline)?;
            tty.fence(deadline)?;
            let elapsed = start.elapsed();
            if tty.size()? != (o.cols, o.rows) {
                return Err(io::Error::other("window resized during sample"));
            }
            elapsed
        } else {
            let mut term = new_term(o);
            let mut parser: Processor = Processor::new();
            if case != "scrollback" {
                parser.advance(&mut term, b"\x1b[?1049h");
            }
            let start = Instant::now();
            for chunk in data.chunks(65536) {
                parser.advance(&mut term, black_box(chunk));
            }
            let elapsed = start.elapsed();
            black_box(term.grid());
            elapsed
        };
        if sample > 0 {
            result.seconds.push(elapsed.as_secs_f64());
        }
    }
    Ok(result)
}

fn core_extras(o: &Options) -> Vec<Measurement> {
    let paste = "paste\r\n中文\x1b[201~".repeat(o.bytes / 20 + 1);
    let mut filtering = Measurement {
        case: "paste-filter".into(),
        bytes: paste.len(),
        operations: 1,
        seconds: Vec::new(),
    };
    let mut resize = Measurement {
        case: "resize-reflow".into(),
        bytes: 0,
        operations: 200,
        seconds: Vec::new(),
    };
    let fill = workload::payload("wrap", o.bytes, o.cols, o.rows);
    for sample in 0..=o.samples {
        let start = Instant::now();
        black_box(mechanic_core::paste::filter(black_box(&paste), true));
        if sample > 0 {
            filtering.seconds.push(start.elapsed().as_secs_f64());
        }
        let mut term = new_term(o);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, &fill);
        let start = Instant::now();
        for _ in 0..100 {
            term.resize(Size(o.cols / 2, o.rows));
            term.resize(Size(o.cols, o.rows));
        }
        let elapsed = start.elapsed();
        black_box(term.grid());
        if sample > 0 {
            resize.seconds.push(elapsed.as_secs_f64());
        }
    }
    vec![filtering, resize]
}

fn new_report(o: &Options) -> Result<Report, Box<dyn std::error::Error>> {
    Ok(Report {
        schema: 1,
        workload_version: workload::VERSION,
        mode: o.mode.clone(),
        build_profile: if cfg!(debug_assertions) { "debug" } else { "release" }.into(),
        label: o.label.clone(),
        cols: o.cols,
        rows: o.rows,
        requested_bytes: o.bytes,
        warmups: 1,
        samples: o.samples,
        os: env::consts::OS.into(),
        arch: env::consts::ARCH.into(),
        unix_seconds: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        term: env::var("TERM").unwrap_or_default(),
        term_program: env::var("TERM_PROGRAM").unwrap_or_default(),
        term_program_version: env::var("TERM_PROGRAM_VERSION").unwrap_or_default(),
        notes: o.notes.clone(),
        measurements: Vec::new(),
        paste: None,
        paste_samples: None,
    })
}

fn run(o: Options) -> Result<(), Box<dyn std::error::Error>> {
    let mut report = new_report(&o)?;
    // Validate the destination before changing terminal modes. Never overwrite results.
    let mut output = ResultFile {
        file: fs::OpenOptions::new().write(true).create_new(true).open(&o.output)?,
        path: o.output.clone(),
        complete: false,
    };
    let mut terminal = if o.mode == "terminal" { Some(tty::Tty::open()?) } else { None };
    for case in
        workload::CASES.iter().filter(|case| o.case.as_ref().is_none_or(|chosen| chosen == **case))
    {
        let data = workload::payload(case, o.bytes, o.cols, o.rows);
        report.measurements.push(measure(&o, case, &data, &mut terminal)?);
    }
    if let Some(tty) = &mut terminal {
        tty.reset(false, o.timeout)?;
        let mut latency = Measurement {
            case: "query-roundtrip".into(),
            bytes: 0,
            operations: 1,
            seconds: Vec::new(),
        };
        for i in 0..=100 {
            let start = Instant::now();
            tty.write(b"\x1b[H.\x1b[K", start + o.timeout)?;
            tty.fence(start + o.timeout)?;
            if i > 0 {
                latency.seconds.push(start.elapsed().as_secs_f64());
            }
        }
        report.measurements.push(latency);
    } else if o.case.is_none() {
        report.measurements.extend(core_extras(&o));
    }
    drop(terminal);
    serde_json::to_writer_pretty(&mut output.file, &report)?;
    use std::io::Write;
    writeln!(output.file)?;
    output.file.sync_all()?;
    output.complete = true;
    println!("{} ({}):", report.label, report.mode);
    for m in &report.measurements {
        println!(
            "{:<18} median {:9.3} ms  p95 {:9.3} ms{}",
            m.case,
            m.percentile(0.5) * 1000.0,
            m.percentile(0.95) * 1000.0,
            if m.bytes > 0 {
                format!("  {:9.2} MiB/s", m.bytes as f64 / 1048576.0 / m.percentile(0.5))
            } else {
                format!("  {} ops/sample", m.operations)
            }
        );
    }
    println!("Saved {}", o.output);
    Ok(())
}

fn compare(paths: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if paths.len() < 2 {
        return Err("compare requires at least two result files".into());
    }
    let reports = paths
        .iter()
        .map(|p| Ok(serde_json::from_slice::<Report>(&fs::read(p)?)?))
        .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
    let first = &reports[0];
    for r in &reports {
        if r.schema != 1
            || r.workload_version != first.workload_version
            || r.mode != first.mode
            || r.paste != first.paste
            || r.build_profile != first.build_profile
            || (r.cols, r.rows, r.requested_bytes, r.warmups, r.samples)
                != (first.cols, first.rows, first.requested_bytes, first.warmups, first.samples)
            || r.os != first.os
            || r.arch != first.arch
            || r.measurements.len() != first.measurements.len()
        {
            return Err("results have incompatible mode, workload, dimensions, sample count, platform, or size".into());
        }
        for (a, b) in first.measurements.iter().zip(&r.measurements) {
            if a.case != b.case
                || a.bytes != b.bytes
                || a.operations != b.operations
                || b.seconds.is_empty()
                || b.seconds.len() != if b.case == "query-roundtrip" { 100 } else { r.samples }
                || b.seconds.iter().any(|v| !v.is_finite() || *v <= 0.0)
            {
                return Err("incompatible or invalid measurements".into());
            }
        }
    }
    println!("case\tterminal\tmedian_ms\tp95_ms\trelative_to_first (lower is faster)");
    for (i, base) in first.measurements.iter().enumerate() {
        for r in &reports {
            let m = &r.measurements[i];
            println!(
                "{}\t{}\t{:.3}\t{:.3}\t{:.3}",
                m.case,
                r.label,
                m.percentile(0.5) * 1000.0,
                m.percentile(0.95) * 1000.0,
                m.percentile(0.5) / base.percentile(0.5)
            );
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--help" || a == "-h") {
        println!("{}", usage());
        return Ok(());
    }
    if args.first().is_some_and(|a| a == "compare") {
        return compare(&args[1..]);
    }
    if args.first().is_some_and(|a| a == "_paste-peer") {
        return paste::peer(&args[1..]);
    }
    let o = options(&args)?;
    if o.mode == "paste" { paste::run(o) } else { run(o) }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn samples_use_nearest_rank_percentiles() {
        let m = Measurement {
            case: String::new(),
            bytes: 0,
            operations: 1,
            seconds: vec![5.0, 1.0, 4.0, 2.0, 3.0],
        };
        assert_eq!(m.percentile(0.5), 3.0);
        assert_eq!(m.percentile(0.95), 5.0);
    }
    #[test]
    fn invalid_parameters_are_rejected() {
        for flags in [
            ["--samples", "0"],
            ["--mib", "0"],
            ["--rows", "3"],
            ["--case", "nope"],
            ["--unknown", "1"],
        ] {
            let args: Vec<_> =
                ["core", "out.json"].into_iter().chain(flags).map(str::to_string).collect();
            assert!(options(&args).is_err());
        }
    }
}
