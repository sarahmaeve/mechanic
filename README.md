# Mechanic

macOS terminal emulator using alacritty_terminal, winit, wgpu/Metal, and
cosmic-text. Each window has its own shell, terminal state, and renderer.

```sh
cargo run --release -p mechanic-app
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Rust 1.99.0 is pinned in `rust-toolchain.toml`. `--animate` enables paced
gradient and logo animations while focused (`--hot-cpu` remains an alias).
`--no-mouse-tracking` keeps mouse selection local.

Configuration: `$XDG_CONFIG_HOME/mechanic/mechanic.toml`, or
`~/.config/mechanic/mechanic.toml`. Missing keys use defaults. Invalid files
log a warning and use the full default configuration.

```toml
[font]
family = "Berkeley Mono"
size = 14.0

[terminal]
scrollback_lines = 10000
close_on_exit = "success" # always, success, never
```

Cmd+N/W opens/closes windows, Cmd+C/V copies/pastes, Cmd+K clears history,
Cmd+A selects the buffer, and Cmd++/−/0 changes/resets font size. After a
failed shell exit, Cmd+R restarts the shell.

`mechanic-core` owns terminal state and PTY I/O; `mechanic-app` routes events
and converts the grid; `mechanic-renderer` shapes glyphs and draws cells;
`mechanic-config` loads TOML. Rendering and Unicode support are incomplete;
see [review findings](design/REVIEW.md).

PTY writes are queued without waiting for the child. Pending input is limited
to 8 MiB (including paste markers) and 1,024 queued messages; a full queue
rejects the entire new write with a logged error. Closing a window cancels
its I/O worker. Fatal transport errors leave a restartable window with an
error banner.

PTY parsing yields between chunks after roughly 4 ms, or at most 4 MiB per
call. One window parses per event-loop turn, rotating through pending windows
independently of redraws. Exit/error handling waits for buffered output. The
time budget is soft: one parser chunk can take additional time.

Content redraws are paced at 16 ms and animation frames at 33 ms. Input and
resize redraws remain immediate. Hidden windows keep parsing but suspend
presentation. Focus glow runs briefly by default; continuous effects require
`--animate`. The event loop waits when parsing and animations are idle.

See [benchmarks](benchmarks/README.md) for Rust microbenchmarks and identical
workloads for Mechanic, iTerm2, and Ghostty.
