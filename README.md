# Mechanic

macOS terminal emulator using alacritty_terminal, winit, wgpu/Metal, and
cosmic-text. Each window has its own shell, terminal state, and renderer.

```sh
cargo run --release -p mechanic-app
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Rust 1.99.0 is pinned in `rust-toolchain.toml`. The logo is visible but static by
default; background lighting animation is also off. `--animate` enables both while focused
(`--hot-cpu` remains an alias). `--animate-logo` / `--animate-background` and
`--no-animate-logo` / `--no-animate-background` override each setting separately.
For repeated flags, the last setting for each effect wins.
`--no-mouse-tracking` keeps mouse selection local.

Configuration: `$XDG_CONFIG_HOME/mechanic/mechanic.toml`, or
`~/.config/mechanic/mechanic.toml`. Missing keys use defaults. Invalid files
log a warning and use the full default configuration.

```toml
[theme]
logo = "triangle" # triangle or atom
logo_size = 180 # physical pixels; 0 hides it, 270 restores the original size

[theme.animation]
logo = false
background = false

[font]
family = "Berkeley Mono"
size = 14.0

[terminal]
scrollback_lines = 10000
close_on_exit = "success" # always, success, never
```

Restart Mechanic after changing configuration.

Cmd+N/W opens/closes windows, Cmd+C/V copies/pastes, Cmd+K clears history,
Cmd+A selects the buffer, and Cmd++/−/0 changes/resets font size. After a
failed shell exit, Cmd+R restarts the shell.

`mechanic-core` owns terminal state and PTY I/O; `mechanic-app` routes events
and converts the grid; `mechanic-renderer` shapes glyphs and draws cells;
`mechanic-config` loads TOML. Text shaping supports combining marks, Japanese
wide cells and Arabic paragraph direction. Configured font fallbacks precede
platform fallback; macOS supplies Hiragino Sans and Geeza Pro. See
[review findings](design/REVIEW.md) for remaining rendering limits.

Underline styles, underline colors and strikeout are rendered. OSC palette
changes affect both display and color-query replies. IME composition appears
underlined with a selection and caret, clipped to the current visible row.
Mouse reporting distinguishes hover and held buttons; trackpad scrolling retains
fractional movement within each gesture.

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
presentation. Logo animation includes focus glow, pulses travelling around both
triangles, and a faint breathing glow inside the inner triangle.
The optional atom logo has three shaded elliptical orbits and a glowing nucleus.
Enable `theme.animation.logo` to move its electrons, varying their size and brightness
with depth, or to animate the triangle pulses. Background animation
controls only the corner gradient's changing color and brightness.
The event loop waits when parsing and animations are idle.

Content frames reuse unchanged row geometry and upload changed buffer ranges.
Snapshots and surface presentation still cover the whole visible grid.

See [benchmarks](benchmarks/README.md) for Rust microbenchmarks and identical
workloads for Mechanic, iTerm2, and Ghostty.
