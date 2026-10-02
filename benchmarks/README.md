# Benchmarks

Build once, then use the same release executable in each terminal:

```sh
cargo build --release -p mechanic-bench
mkdir -p benchmarks/results
```

Open a fresh, focused 120×40 window in Mechanic, iTerm2, or Ghostty, change
to this repository, and run the corresponding command:

```sh
./target/release/mechanic-bench terminal mechanic benchmarks/results/mechanic.json
./target/release/mechanic-bench terminal iterm2 benchmarks/results/iterm2.json
./target/release/mechanic-bench terminal ghostty benchmarks/results/ghostty.json
```

Use the same machine, font, size, opacity, display, and power settings.
Match scrollback capacity where possible: Mechanic and iTerm2 use lines,
while Ghostty uses bytes, so record both limits and treat history comparisons
as approximate. Keep the window visible and focused; disable Mechanic's
animations with `--no-animate-logo --no-animate-background`. Do not type or resize during a run. The scrollback case
clears the window's history. `--cols` and `--rows` select another shared
size. Record terminal versions and settings with `--notes "..."`; environment
version strings are also saved but may be absent or inherited from the shell.

The default is one warmup and five measured samples of at least 4 MiB per
case. Payloads are generated before timing and end on complete records.
Each sample waits for a cursor-position reply after output. This measures
PTY delivery and parsing, including any rendering that delays the reply;
it does **not** measure presentation completion, FPS, or keyboard-to-pixel
latency. Unsupported replies and timeouts fail the run. Run directly in
the terminal, without tmux, SSH, or redirected terminal I/O.

| Case | Work |
| --- | --- |
| ascii / wrap | Short lines / long wrapped lines |
| unicode | Multilingual text, combining marks, CJK, emoji |
| sgr | Truecolor foreground and indexed background changes |
| scrollback | Normal-screen output and history retention |
| scroll-region | Restricted scrolling region and scroll commands |
| repaint / sparse | Full-screen / cursor-addressed partial updates |
| query-roundtrip | 100 one-character updates acknowledged by cursor reports |

All cases except scrollback use the alternate screen. Unicode timing does
not establish rendering correctness; inspect output separately. Glyph
caches remain warm after warmup. Cold glyph loading, UI scrolling, selection,
clipboard transfer, idle CPU, and GPU presentation need separate profiling.

```sh
./target/release/mechanic-bench compare benchmarks/results/{mechanic,iterm2,ghostty}.json
./target/release/mechanic-bench core benchmarks/results/core.json
```

Core mode measures the same parser/grid workloads without a PTY or renderer,
plus paste sanitization and 200 resize/reflow operations per sample on a
filled 10,000-line history. It cannot be compared with terminal mode.
`--case ascii --mib 1 --samples 2` runs a short subset. `--timeout` applies
only to terminal I/O. Ctrl+C or Ctrl+Z cancels a terminal run and restores
terminal settings; failed runs remove their incomplete result file.

JSON contains raw sample times, build profile, workload version, dimensions, byte counts,
platform, and terminal metadata. `compare` rejects incompatible workloads
and reports median, nearest-rank p95, and elapsed-time ratios to the first
file. With five samples p95 is the maximum; increase `--samples` for useful
tail estimates. Repeat complete runs in rotated terminal order. Results are
never overwritten; choose a new filename for each run.

See [recorded results](RESULTS.md) for the initial cross-terminal baseline and
PTY transport comparison.

## Paste transport responsiveness

This mode creates a fresh `mechanic_core::Terminal` and an isolated raw/noecho
PTY peer for each sample. It calls `Terminal::paste()` on the thread that also
processes terminal output, matching the app's transport entry point. It does
not require a GUI window and does not measure GUI frames or keyboard-to-pixel
latency.

```sh
./target/release/mechanic-bench paste benchmarks/results/paste-before.json --label before
# Rebuild after the transport change, keeping workload settings identical.
./target/release/mechanic-bench paste benchmarks/results/paste-after.json --label after
./target/release/mechanic-bench compare benchmarks/results/{paste-before,paste-after}.json
```

The default uses a 1 MiB deterministic ASCII payload, one warmup and five
samples for both consuming and delayed-reader peers. The delayed reader waits
500 ms after announcing readiness. Payload construction, shell startup and
readiness are outside timing. `paste-call` measures the complete paste call,
including filtering, wrapping and transport submission; `verified-delivery`
measures time from call entry until the peer acknowledges exact bytes,
bracketed markers and a subsequent input probe in order. An asynchronous
transport can return quickly while delivery still waits for the reader.

Use `--delay-ms`, `--mib`, `--samples`, `--timeout`, `--case consuming`,
`--case delayed-reader`, `--label`, and `--notes` to configure a run. The timeout
also arms an alarm in the isolated peer, so a blocked baseline write eventually
fails when the peer exits. It never changes the invoking shell's termios.

```sh
./target/release/mechanic-bench paste benchmarks/results/paste-duplex.json \
  --case duplex --duplex-mib 8 --timeout 3 --samples 1
```

The optional duplex peer writes output before reading input to expose a
bidirectional transport deadlock. After the paste call returns, the benchmark
continues draining terminal output. Timeout and verification failures are
saved in `paste_samples`; the command exits unsuccessfully, and `compare`
rejects incomplete delivery samples. Keep delay, duplex size and timeout
identical for before/after comparisons. JSON includes these settings and raw
warmup/measured observations. Five samples give a rough median; p95 is their
maximum. This mode benchmarks Mechanic's own transport and cannot compare
paste handling inside iTerm2 or Ghostty.

## Parser-call responsiveness under a flood

```sh
cargo build --release -p mechanic-bench --example parse_responsiveness
./target/release/examples/parse_responsiveness benchmarks/results/parse-before.json
# Build the same example against the updated core, then choose a new output path.
./target/release/examples/parse_responsiveness benchmarks/results/parse-after.json
```

The example streams at least 32 MiB of ASCII and escape-heavy SGR output from
an isolated raw/noecho PTY child. It lets output reach backpressure for 100 ms,
then records every `Terminal::process_input()` call while production continues.
Each case has one warmup and three measured samples. Calls yield for 1 ms;
this measures parser-call durations and drain throughput, not GUI input or
frame latency. JSON includes raw durations, maximum call time, nearest-rank
p95 of calls that parsed output, call counts, total drain time, and workload
settings. A final visible marker and title must be processed before the
producer's distinct exit status, checking final-output ordering across yields.
This checks marker-before-exit ordering, not every byte of the flood stream.

Use identical release builds, machine conditions and arguments for before/after
runs. `--mib`, `--samples`, `--timeout`, and `--case ascii|sgr` select a subset.
The default 20-second peer watchdog bounds failed/blocking runs; finite output
also ensures an old unbounded parser eventually finishes. Incomplete setup
removes its result file; completed observations retain verification failures.

## App CPU and animations (macOS)

```sh
cargo build --release -p mechanic-app
cargo build --release -p mechanic-bench --example app_cpu
./target/release/examples/app_cpu ./target/release/mechanic benchmarks/results/cpu-idle.json
./target/release/examples/app_cpu ./target/release/mechanic benchmarks/results/cpu-animated.json --animate
./target/release/examples/app_cpu ./target/release/mechanic benchmarks/results/cpu-logo.json --animation logo --logo atom
./target/release/examples/app_cpu ./target/release/mechanic benchmarks/results/cpu-background.json --animation background --logo atom
```

`--animation off|logo|background|both` selects effects independently; the benchmark
defaults to `off` and writes both settings explicitly to isolate the run. `--animate`
remains an alias for `both`. `--logo triangle|atom` chooses the logo (180 pixels).

The example launches an isolated window with Menlo 14 pt, opaque content and
an idle Rust PTY peer. After three seconds it samples five seconds of native
process user/system CPU counters, converted with the native Mach timebase.
Keep the window size and focus unchanged. Animation runs require confirmed
focus; idle runs accept either stable focus state. Unknown focus or any focus
transition marks the report inconclusive.
`--settle-secs`, `--sample-secs`, and `--label` customize a run. Existing reports
are never overwritten; only the launched app is terminated afterward.

Percentages use one CPU core as 100%. They exclude the peer, WindowServer,
GPU activity and energy use. Actual window dimensions are not instrumented.
Compare identical window/display conditions and release builds. The example
passes the legacy `--hot-cpu` alias for `both`, so it can also measure older app
binaries. Independent `logo` and `background` modes require the updated app.

## Render stages and multilingual text

```sh
./target/release/examples/app_cpu ./target/release/mechanic benchmarks/results/cell.json --workload cell --render-profile
./target/release/examples/app_cpu ./target/release/mechanic benchmarks/results/unicode.json --workload unicode --render-profile
./target/release/examples/app_cpu ./target/release/mechanic benchmarks/results/atlas.json --workload atlas --render-profile
./target/release/examples/app_cpu ./target/release/mechanic benchmarks/results/cursor.json --workload text-fixture --fixture-cursor bar
```

Workloads `cell`, `row`, `full`, and `scroll` update an ASCII grid at 20 Hz.
`unicode` and `text-fixture` show the requested languages, combining marks and an
original Arabic news-style paragraph. `atlas` displays 223 distinct characters
in four styles. `--fixture-cursor block|bar|underline|hidden` controls the fixed
cursor; unfocus the fixture to inspect its hollow outline.

`--render-profile` records complete frame pairs during the CPU sample, including
actual terminal dimensions. Fields are conversion, shaping/atlas preparation,
instance construction, host buffer upload, surface acquisition and submit/present
nanoseconds. Upload time excludes GPU completion; tracing affects CPU totals.
Animation and render profiling cannot be combined; output workloads support either.
Keep focus and window
size stable; failed runs remain marked inconclusive.

These GUI CPU/stage measurements are Mechanic-specific. Use the terminal workloads
above for iTerm2/Ghostty comparisons; parser replies do not validate rendered text.

Metal pixel and atlas checks run explicitly on macOS:

```sh
cargo test -p mechanic-renderer -- --ignored --nocapture
```

The multilingual test saves `mechanic-text-fixture.png` in the system temporary
directory; `MECHANIC_TEXT_FIXTURE_PNG` chooses another path. It uses the production
instance builder and shaders and reads the GPU target directly. It does not capture
the screen. Tests cover cursor geometry, overlapping glyph coverage, surface opacity
and atlas growth. Inspect the image for language typography as well as running tests.

## Row geometry cache

```sh
MECHANIC_ROW_CACHE_BENCH_CSV=/tmp/row-cache.csv \
  cargo test --release -p mechanic-renderer row_cache_geometry_benchmark -- --ignored --nocapture
```

This offscreen Metal test compares full geometry construction with row caching
on the same shaped 121×42 grid. It alternates measurement order, warms up for
20 iterations and records 200 iterations per cell/row/full/scroll workload.
CSV records host geometry time and planned upload byte counts; shaping, actual
GPU uploads, presentation and whole-app CPU are outside timing. An absolute
output path is required; the selected CSV is replaced. Every iteration also
checks that cached geometry exactly matches a full rebuild.

## Native surface recovery

```sh
cargo run --release -p mechanic-renderer --example surface_recovery -- /tmp/surface-recovery.csv
```

This opens a temporary 320×160 window with Menlo and animations off. After
three warmup frames it measures 12 pairs: cached presentation, explicit surface
replacement, and cached presentation on the replacement. Every pair must present
successfully and preserve the atlas generation. The event-loop deadline is eight
seconds, with a ten-second startup watchdog. Existing CSV files are not replaced.

These are host-call timings in the same build, including driver waits. They
measure recovery cost, not completed GPU execution or a before/after speedup.
Replacement uses the production recovery method; the driver is not forced to
emit a spontaneous surface-loss error. GPU device loss is a separate failure.

## Wrapped paragraph shaping

```sh
MECHANIC_SHAPING_CSV=/tmp/wrapped-shaping.csv \
  cargo test --release -p mechanic-renderer wrapped_shaping_benchmark -- --ignored --nocapture
```

This CPU benchmark reshapes changing 80×24 paragraphs: Arabic, mixed scripts,
Arabic with offscreen context, and ASCII through the contextual shaper. It uses
20 warmup iterations followed by seven samples of 20 iterations. CSV records
each sample's mean milliseconds per shape and glyph count. It excludes the
paragraph cache, atlas rasterization, uploads and presentation. The ASCII case
does not use the application's faster ASCII path. Compare identical harnesses
before and after shaping changes; glyph counts may differ when shaping is fixed.

## Hyperlinks

```sh
MECHANIC_HYPERLINK_CSV=/tmp/hyperlink-lookup.csv \
  cargo test --release -p mechanic-app hyperlink_lookup_benchmark -- --ignored --nocapture
```

The lookup benchmark compares linked and unlinked 80×24 and 160×100 grids.
Each case warms for 100 ms, then runs seven samples of one million lookups,
reversing case order on alternate samples. A 619-byte URI is verified before
timing. It measures grid lookup and shared metadata cloning; bidi hit mapping,
URL validation, tooltip updates, and rendering are outside timing.

The native AppKit smoke check runs explicitly on the main thread:

```sh
cargo rustc -p mechanic-app --example link_native_smoke --locked -- --cfg test
./target/debug/examples/link_native_smoke
```

It uses a hidden temporary window to verify tooltip copying/clearing and native
Open/Copy menu action delivery, including disabled Open. An eight-second watchdog
bounds startup failures. It does not display an interactive menu, launch a browser,
or modify the clipboard. Ordinary workspace tests do not open this window.
