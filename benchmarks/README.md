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
`--hot-cpu` mode. Do not type or resize during a run. The scrollback case
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
