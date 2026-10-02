# Session restoration and local control

macOS 26.6.2 (25G83), arm64, Rust 1.99.0. Release builds, serial runs,
animations off. All GUI samples remained focused. State and sockets used
temporary directories; the harness verified that both services initialized.

The baseline includes the preceding pane work. Its SHA-256 is
`8b7ca7f2e5371f46229c9ac53ea3cbbc46a2d24c06bfabc06c27b7c8359bed52`.
The initial services build (`enabled-*`, `disabled-idle`) is
`680acedf89f9189d6b77c52db732f228e81f2b1572e1551ddfd1805c7b9d991c`.
The final build (`final-*`), including silent-input viewport redraw, is
`16e8d7c7fec9f2a7c6c5c88b2b1e377634dc43ee6639844b3071c6bf4a299fff`.

`app_cpu` sampled eight seconds after three seconds of settling. The cell
workload changes one cell at 20 Hz. CPU is percent of one core for Mechanic
only; it excludes the PTY peer, WindowServer, and GPU. Render profiling was off.
These short runs check for large regressions, not small percentage differences.

`services.json` contains warmed per-operation distributions: 2,000 snapshot
encodes, 100 durable atomic save/flushes, and 500 socket requests. The snapshot
has one window and 16 panes. Socket requests use the real authenticated
transport with an immediate empty-list callback; they exclude GUI dispatch,
terminal extraction, and shell latency. Save/flush includes worker handoff and
filesystem synchronization; the UI ordinarily queues saves without waiting.

Reproduce after building the two examples and application in release mode:

```sh
target/release/examples/services_bench
target/release/examples/app_cpu target/release/mechanic /tmp/idle.json --services --settle-secs 3 --sample-secs 8
target/release/examples/app_cpu target/release/mechanic /tmp/cell.json --services --workload cell --settle-secs 3 --sample-secs 8
```

Native smoke logs cover real hidden AppKit/Metal windows and PTYs. The
restoration smoke recreates two windows and four panes, then exercises socket
input/output, waits, stale identifiers, and focus preservation.
