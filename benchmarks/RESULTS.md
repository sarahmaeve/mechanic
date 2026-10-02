# Measurements — 2026-10-01

The queued PTY transport removes the large-paste stall and completes a duplex
transfer that previously deadlocked. Output batching also improves most tested
GUI workloads. These are initial observations from one machine, not stable
cross-terminal rankings.

## Paste responsiveness

Release build, 1 MiB payload, one warmup and five measured samples per case.
The delayed reader waits 500 ms. Median milliseconds:

| Measurement | Before | After |
| --- | ---: | ---: |
| consuming-paste-call | 49.230 | 0.288 |
| consuming-verified-delivery | 50.417 | 54.124 |
| delayed-reader-paste-call | 552.657 | 0.334 |
| delayed-reader-verified-delivery | 553.829 | 587.800 |

All ordinary samples, including warmups, verified exact payload bytes, paste
markers and the following input probe in order. Paste calls return promptly;
verified delivery was 6–7% slower in this batch. The improvement is removal of
caller blocking, not faster paste delivery.

The duplex peer writes 8 MiB before reading the paste. With a three-second
watchdog, both baseline observations failed (warmup and measured sample);
the measured paste call took 3603.820 ms before failure.
Both final observations passed: the measured paste call took 0.336 ms and
verified delivery took 645.781 ms. One measured duplex sample demonstrates
completion, not a latency distribution. These tests exercise Mechanic's core
transport, not iTerm2 or Ghostty's clipboard handling.

## GUI output and replies

Nearest-rank median milliseconds, as reported by the runner; lower is faster.
Output cases send at least 4 MiB and
wait for a cursor-position reply. Each has one warmup and five samples;
query-roundtrip has 100 measured one-character updates.

| Case | Mechanic before | Mechanic after | iTerm2 | Ghostty |
| --- | ---: | ---: | ---: | ---: |
| ascii | 99.963 | 35.775 | 61.741 | 33.228 |
| wrap | 101.605 | 41.782 | 49.825 | 32.007 |
| unicode | 118.417 | 42.975 | 1086.097 | 33.290 |
| sgr | 117.105 | 36.101 | 146.903 | 35.910 |
| scrollback | 66.508 | 35.754 | 45.201 | 33.847 |
| scroll-region | 29.936 | 32.001 | 2264.064 | 87.936 |
| repaint | 101.670 | 35.794 | 52.604 | 32.144 |
| sparse | 33.757 | 32.612 | 277.122 | 37.268 |
| query-roundtrip | 16.683 | 8.309 | 20.167 | 0.013 |

Same macOS 26.6.2 arm64 machine, 121×42 cells, Menlo 14 pt, opacity 1,
release runner. Mechanic runs in quiet mode. iTerm2 is 3.6.11; Ghostty is 1.3.1.
Mechanic 0.1.0 before/after refers to the PTY transport change, after the
dependency migration and terminal query-reply fix. Both Mechanic runs use the
same preserved terminal-workload executable. Benchmarks ran sequentially.

Mechanic and iTerm2 retain 10,000 history lines; Ghostty's limit is 10,000,000
bytes. These are different capacities, so the scrollback row is approximate.
Environment version fields may be inherited from the launching shell; the
labels and notes identify the actual terminal.

DSR timing measures delivery, parsing and reply scheduling, not presentation
completion or keyboard-to-pixel latency. Mechanic's Unicode and rendering
support remain incomplete, so timings do not establish equivalent visual
results. This is one batch per terminal without rotated repeat runs; display
scheduling and background activity can affect results. With five samples,
nearest-rank p95 is the maximum. Scroll-region was about 7% slower after the
change; sparse output was similar.

## Reproduce and inspect

[Raw JSON](baselines/2026-10-01/) contains the eight before/after and comparison
reports plus the initial parser-only core baseline. Each report retains raw
samples and workload settings. See [runner instructions](README.md) for new
runs; output files are never overwritten.

```sh
./target/release/mechanic-bench compare \
  benchmarks/baselines/2026-10-01/mechanic-before-pty.json \
  benchmarks/baselines/2026-10-01/mechanic-after-pty.json \
  benchmarks/baselines/2026-10-01/iterm2-before-pty.json \
  benchmarks/baselines/2026-10-01/ghostty-before-pty.json
./target/release/mechanic-bench compare \
  benchmarks/baselines/2026-10-01/paste-before.json \
  benchmarks/baselines/2026-10-01/paste-after.json
```

The comparator rejects the failed duplex baseline; inspect its raw
`paste_samples` for the timeout/error observations.

Transport change validation: 303 workspace tests, strict Clippy, formatting,
and release builds passed.

## Bounded parsing follow-up

Parsing now yields after a soft 4 ms budget, checked between at most 64 KiB
chunks, with a hard limit of 4 MiB / 64 chunks per call. At least one chunk is
processed. A budget yield requests another redraw, including when the
producer has stopped sending wakes. Final output drains before exit/error
delivery. This limits parsing work, not total frame time: one chunk, protocol
replies, rendering and OS scheduling can exceed the time budget.

The same Rust `parse_responsiveness` example was built against the preserved
pre-budget core and updated core. Each case streams at least 32 MiB, with a
100 ms prefill and 1 ms yields between calls; one warmup and three measured
samples per case. All eight observations per build verified the final marker
and title before exit, including warmups. This does not verify every byte or
measure GUI input latency.

| Flood measurement (ms) | Before | After |
| --- | ---: | ---: |
| Longest parsing call, both cases | 16.970 | 4.288 |
| P95 call that parsed output, both cases | 1.360 | 4.139 |
| ASCII median total drain | 220.237 | 210.613 |
| SGR median total drain | 221.194 | 208.035 |

The maximum call fell; p95 increased as work was redistributed into bounded
calls. Calls and batch sizes differ, so these are scheduling observations,
not a uniform speedup. Total drain timings include the example's yields.

Fresh GUI before/after runs used the same preserved terminal-workload runner
and 121×42 settings as above. Median milliseconds:

| Case | Before budget | After budget |
| --- | ---: | ---: |
| ascii | 35.558 | 36.015 |
| wrap | 35.636 | 36.630 |
| unicode | 41.685 | 36.045 |
| sgr | 36.305 | 36.488 |
| scrollback | 35.468 | 43.425 |
| scroll-region | 32.243 | 59.655 |
| repaint | 35.720 | 36.504 |
| sparse | 33.651 | 62.095 |
| query-roundtrip | 8.320 | 8.320 |

Most cases changed little. Scrollback was 22% slower; scroll-region and sparse
updates were about 85% slower. Parser continuation currently waits for another
redraw, so throughput can pay for extra presentation turns. At this stage the
4 ms budget favors shorter UI parsing stalls; the next section measures the
subsequent scheduling change. No keyboard-to-pixel result is claimed.

Raw reports: [call durations before](baselines/2026-10-01/parse-calls-before.json),
[after](baselines/2026-10-01/parse-calls-after.json),
[GUI before](baselines/2026-10-01/mechanic-parse-before.json), and
[after](baselines/2026-10-01/mechanic-parse-after.json).
The ordinary `compare` command accepts the GUI pair; flood reports use a
separate schema with raw call observations.

Validation after this follow-up: 309 workspace tests, strict Clippy, formatting,
and release builds pass. Tests cover continuation without a producer wake,
split UTF-8/escape sequences, expired budgets, the byte cap, and final-output /
error / exit ordering.

## Parsing independent of presentation

One pending window now parses per event-loop turn, rotating fairly through a
deduplicated queue. Redraws only render the current grid. The existing soft
4 ms / hard 4 MiB parser limits are unchanged. Content presentation uses a
16 ms deadline, animations a 33 ms deadline; hidden windows keep parsing but
suspend rendering. Focus glow receives its final frame before becoming idle.

A fresh GUI pair used the same runner and settings as the preceding tests.
Median milliseconds:

| Case | Parsing on redraw | Independent parsing |
| --- | ---: | ---: |
| ascii | 35.988 | 32.405 |
| wrap | 36.753 | 32.217 |
| unicode | 36.575 | 32.506 |
| sgr | 36.514 | 32.739 |
| scrollback | 42.299 | 33.756 |
| scroll-region | 60.129 | 31.700 |
| repaint | 36.254 | 32.218 |
| sparse | 61.845 | 31.448 |
| query-roundtrip | 8.344 | 0.018 |

The two cases slowed by the parsing budget recover their earlier throughput.
Query replies no longer wait for presentation. These remain DSR completion
measurements, not visible frame latency. Explicit input/resize redraws remain
immediate; paced PTY updates can wait for their 16 ms deadline plus rendering.

### Animation CPU

`--animate` enables the existing gradient and logo effects while focused;
`--hot-cpu` remains an alias. Continuous effects are opt-in. Animation redraws
now honor their deadline, use cached terminal instances, and stop while hidden.

The Rust `app_cpu` example sampled app-process CPU for five seconds after a
three-second settle, with an idle PTY peer, Menlo 14 pt and opaque content.
All four accepted samples remained focused. Percent of one CPU core:

| Mode | Before scheduling change | After |
| --- | ---: | ---: |
| Focused continuous effects | 13.746% | 3.966% |
| Focused idle | 2.815% | 2.984% |

Continuous effects used about 71% less app CPU in this small comparison.
Idle CPU was similar, around 3%; the change does not eliminate native/runtime
background work. These are single samples on this machine, excluding the
peer, WindowServer, GPU use and energy. Actual window dimensions were not
instrumented. No general battery-life or GPU-cost claim is made.

The animated reports are corrected versions of initially valid focused
samples: native `proc_pid_rusage` counters had been mislabeled as nanoseconds.
Raw counter values and elapsed times are preserved. The host's Mach timebase
is 125/3; a 250 ms CPU probe matched POSIX `getrusage` after conversion
(249.9085 ms versus 249.909 ms). Conversion tests now cover fractional scaling
and overflow. Later focus-interrupted attempts were excluded.

Raw [GUI before](baselines/2026-10-01/mechanic-decoupled-before.json) /
[after](baselines/2026-10-01/mechanic-decoupled-after.json),
[animated CPU before](baselines/2026-10-01/cpu-animated-before-corrected.json) /
[after](baselines/2026-10-01/cpu-animated-after-corrected.json), and
[idle CPU before](baselines/2026-10-01/cpu-idle-before.json) /
[after](baselines/2026-10-01/cpu-idle-after.json) retain the observations.

Validation: 313 workspace tests plus three CPU-conversion tests pass, with
strict Clippy, formatting and release builds. Scheduler tests cover fairness,
continuation without presentation, fixed deadlines, pending redraws,
occlusion/restoration, and the final bloom frame. Multi-window native input
latency and GPU frame timing were not measured.

## Unicode, cursor and atlas correctness

The renderer now keeps combining marks, shapes Arabic in context, resolves bidi
across soft-wrapped rows, and preserves logical copy order. Connected RTL words
retain their relative glyph positions; horizontal transforms rasterize from font
outlines. Japanese wide cells retain both backgrounds. Hidden cursors stay hidden,
bar/underline geometry is honored, and wide hollow cursors have one outline.
Atlas growth finishes before instance construction, checks device limits and
preserves all active glyphs. Configured font fallbacks now apply.

The [offscreen GPU fixture](baselines/2026-10-01/unicode/text-fixture.png) covers
Russian, Ukrainian, Japanese, Arabic, French, German, Spanish, Portuguese and
Italian, including decomposed accents and original Arabic news-style prose.
It uses Menlo 14 pt at 2× scale; Arabic resolves to installed Courier New.
The image comes from GPU readback, not screen capture. Word wrapping still follows
terminal cell boundaries, and joining stops at those boundaries.

Stage profiling used 121×42 cells, Menlo 14 pt, opaque content, a controlled Rust
PTY peer updating at 20 Hz, and release builds. Each sample lasted five seconds.
CPU percentages mean one process CPU core, excluding the peer, WindowServer and
GPU. Logs are enabled for both sides and affect CPU totals. These are single-run
observations, not latency or energy claims.

The initial correctness implementation increased unfocused ASCII output CPU from
about 5–6% to 7–8%. Profiling found repeated atlas residency checks on unchanged
rows. The resulting frame cache skips those rows, checks changed rows, and falls
back to full-frame preflight on a miss or atlas generation change.

Matched unfocused `cell` samples, with five-second settling and five-second sampling:

| Measurement | Correctness build before frame cache | With frame cache |
| --- | ---: | ---: |
| App CPU, one core | 7.675% | 6.590% |
| Shaping/atlas median | 0.761 ms | 0.235 ms |
| Shaping/atlas p95 | 0.831 ms | 0.303 ms |
| Complete presented frames | 100 | 100 |

The final focused samples below are a separate series. Do not compare their CPU
values directly with the unfocused baseline. Shaping/atlas and instance columns
are median host time; upload includes host writes/allocation, not GPU completion.

| Workload | App CPU | Shaping/atlas | Instances | Upload |
| --- | ---: | ---: | ---: | ---: |
| Cell | 6.242% | 0.236 ms | 0.792 ms | 0.497 ms |
| Row | 6.325% | 0.216 ms | 0.792 ms | 0.496 ms |
| Full repaint | 8.023% | 0.658 ms | 0.785 ms | 0.507 ms |
| Scroll | 6.861% | 0.644 ms | 0.791 ms | 0.461 ms |
| Multilingual fixture + changing status cell | 5.368% | 0.191 ms | 0.254 ms | 0.427 ms |

The multilingual sample measures steady display with a changing status cell; it
does not measure continuously reshaping new Arabic paragraphs. Unfocused idle
samples were 0.014% before these changes and 0.084% afterward, with no content
frames during the latter sample. They are too small and brief to infer a useful
idle regression. Text rendering adds no idle timer. Final atlas CPU attempts had
focus transitions and are inconclusive; the explicit atlas growth tests passed.

Full-grid instance construction remains the largest measured preparation stage
in ASCII workloads. Partial conversion/instance uploads are not implemented here.

[Raw observations and stage distributions](baselines/2026-10-01/unicode/summary.json)
include focus, dimensions, frame counts, medians and p95. The matched cache pair is
`unicode-final-cell-r2.json` / `unicode-cache-pair-after.json`; other files preserve
the original text path, the initial correctness build and the focused cached runs.
Rejected focus-transition attempts remain in the ignored results directory; one
final rejected atlas sample is retained beside the accepted observations.

Release binary SHA-256:

- Original text path plus instrumentation: `4e828c951b10779b47b08ca0422b994f76e98993028ed70dffc0b0a08f4ba71b`.
- Correctness build before frame cache: `b30d2c40244da000c051ef9b3503f6bb12697782fe843fd9206185240ff0a464`.
- With frame cache: `b813a46387ef8b9e8c59686395dbc47cdc0243bc9014f69006f1d704017a9e12`.

Validation: 339 workspace/all-target tests, three explicit Metal checks, strict
Clippy, formatting and release build. PTY tests need native terminal access;
sandbox-denied runs were rerun successfully without weakening assertions.
