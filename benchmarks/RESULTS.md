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
redraw, so throughput can pay for extra presentation turns. Decoupling parsing
continuations from presentation is the next scheduling improvement; the
4 ms budget favors shorter UI parsing stalls. No keyboard-to-pixel result is
claimed.

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
