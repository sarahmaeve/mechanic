# Workspace features

macOS 26.6.2, arm64, Rust 1.99.0. Release benchmarks ran serially without other
builds or tests. No user workspace files were accessed.

Final application SHA-256:
`f8934cb40811f73b0eaecb5fab77a9c9b5222a69b6e94221400c16e4d2b3e7e5`.
The full suite passed 564 tests; opt-in native and Metal checks passed separately.

The paired renderer benchmark compares two panes totaling 3,840 cells with and
without titles/custom outlines. Order alternates within each pair; 20 warmups
precede 100 measured pairs per workload.

| Preparation workload | Plain mean | Styled mean | Geometry upload |
| --- | ---: | ---: | ---: |
| Idle | 11.781 µs | 12.246 µs | 0 bytes in either case |
| One pane changes | 55.393 µs | 56.524 µs | 14,080 bytes in either case |

These are host preparation measurements, excluding GPU completion and window
presentation. They do not measure whole-app idle CPU. Static terminal/header
shaping is retained. No background animation or polling timer was added.

`services.json` measures a 16-pane snapshot, a styled 16-pane named loadout, and
an authenticated local socket with an immediate empty-list reply. Each case has
10 warmups. Median explicit loadout save (100 samples, including atomic replace
and filesystem sync) was 9.183 ms; retrieval from memory (2,000 samples) was
5.375 µs. Window/shell creation is excluded. Explicit save/delete runs on the UI
thread; ordinary workspace persistence continues using its worker.

Median snapshot encoding was 4.208 µs; durable snapshot save/flush was 8.076 ms;
local socket transport was 55.375 µs. These are fresh reference measurements,
not evidence of a regression or improvement against earlier runs.

Reproduce using the commands in [the benchmark guide](../../../README.md).
Native checks cover live moves and command waiters, loadouts and pane styling,
Unicode palette input, zoom/focus, and real GPU destruction/recovery with a
surviving shell variable. All native fixtures use isolated state and local shells.
