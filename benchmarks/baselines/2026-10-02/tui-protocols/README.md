# TUI protocols

macOS 26.6.2, arm64, Rust 1.99.0. Release benchmarks ran serially without
concurrent builds or tests. Five invocations per protocol benchmark are retained;
the table reports their medians. Variation is visible in `summary.csv` and raw logs.
These are reference measurements in the new build, not before/after speedups.

| Parser payload | Ordinary | Sync timeout | Explicit sync end |
| --- | ---: | ---: | ---: |
| ASCII | 217.20 MiB/s | 234.31 MiB/s | 239.38 MiB/s |
| Dense OSC titles | 178.22 MiB/s | 168.36 MiB/s | 166.01 MiB/s |
| OSC color queries/changes | 73.05 MiB/s | 69.82 MiB/s | 71.10 MiB/s |

Each case has 16 warmup batches and 128 measured batches. Timeout expiry is
invoked directly, without waiting 150 ms. Query replies use a buffered transport.
The synchronized title/query cases include callback and buffering costs.
Rendering, real PTY delivery and presentation are excluded.

Ctrl+A encoding took 12.5 ns/event for legacy input, 26.4 ns with Kitty
disambiguation and 31.0 ns with all Kitty flags. Each case has 10,000 warmups
and 500,000 measured encodings. This measures allocation/encoding for one key,
not native event routing or input-to-screen latency. Tests cover the broader
key and language matrix independently.

`core.json` records the standard parser/grid workloads at 120×40 with one warmup
and five samples per case. ASCII measured 279.56 MiB/s and multilingual output
297.30 MiB/s. This harness calls VTE directly and differs from the protocol
wrapper benchmark above. Its inherited iTerm environment fields identify the
launching shell; no iTerm2 or Ghostty benchmark was run.

Validation passed: workspace tests with all targets, strict Clippy, release
workspace build, 55 VTE tests, and the hidden native protocol smoke check.
Both vendor keyboard regression tests also passed in an isolated copy with a
temporary workspace manifest; the repository's lockfile was unchanged by that check.
Workspace tests include malformed/oversized OSC 52 recovery, keyboard stack
overflow, all 32 keyboard flag combinations, IME/Option composition, shortcut
release routing, synchronized EOF flushing, and source-ordered color replies.
Some example targets repeat application tests; do not sum their counts as
independent coverage.

The native check verifies real PTY negotiation and encoded input, pane-local
selection replies, native approval callback teardown, and event-loop timeout
recovery in an occluded, zoom-hidden pane. It asserts the timeout leaves no
idle deadline. Clipboard tests use a fake backend or pane-local selection;
they do not access the system clipboard or present approval dialogs.

Reproduction commands are in [the benchmark guide](../../../README.md#tui-protocols).
