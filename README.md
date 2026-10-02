# Mechanic

macOS terminal emulator using alacritty_terminal, winit, wgpu/Metal, and
cosmic-text. Each pane has its own shell and terminal state; panes share a window renderer.

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

[notifications]
enabled = false
min_command_seconds = 10.0

[session]
restore = true

[control]
enabled = true
```

Restart Mechanic after changing configuration.

Cmd+Shift+P opens the command palette. Type to filter, use Up/Down to select,
Return to run, and Escape to close. It includes pane actions, titles/colors,
loadouts, Find, and animation controls.

Zsh shell integration is automatic and leaves your startup files unchanged.
The title shows the working directory and running/completed command status.
Cmd+Shift+Up/Down navigates recorded prompts; Cmd+Shift+C copies the last
completed command's retained output. Set `[shell] integration = false` to
disable automatic hooks. Other shells can supply compatible OSC 7/133 markers.
Markers are limited to 1,024 commands. Reflow, clearing, edits and history
eviction can make old navigation/output ranges unavailable; copied output is
the current grid text, not an immutable command transcript. It can include zsh's
end-of-line marker (`%`) when command output has no final newline. Automatic
hooks apply to the configured zsh session; nested shells need their own hooks.

New windows and panes inherit the active pane's reported local directory.
Missing, inaccessible or remote paths fall back to Mechanic's startup directory.
This requires OSC 7 directory reports, supplied automatically by the zsh hooks.

Optional completion notifications use shell command boundaries. When enabled,
commands lasting at least `min_command_seconds` notify only while their window
is unfocused. macOS app bundles request alert permission on the first eligible
completion; unbundled Cargo/CLI builds request Dock attention instead. Dock
attention has no effect while Mechanic is the active application. Notifications
contain completion status and duration, without command text or output.

Cmd+F opens a native scrollback Find panel. Return / Shift+Return and
Cmd+G / Cmd+Shift+G move between matches; Escape closes it. Search highlights
logical text across soft wraps, including combining marks and wide characters.
Hard line breaks separate matches. Search ignores case by default using Unicode
case folding and canonical normalization: `Straße`, `STRASSE` and `STRAẞE` match,
as do composed/decomposed umlauts. Accents remain meaningful: `Müller` does not
match `Muller` or `Mueller`, and `s` does not match half of `ß`. Enable **Match
case** for exact stored-text matching. Highlighting and copying retain the
original spelling. Searches are bounded to two million cells and 10,000 matches;
the panel reports partial results. New output invalidates highlights; press
Return to refresh. Search does not continually rescan streaming output.

Cmd+N opens a window. Cmd+D splits side by side; Cmd+Shift+D stacks panes.
Click a pane to focus it, use Cmd+[/] to cycle, or Cmd+Option+arrow to move focus
by direction. Drag the visible divider to resize; it brightens and shows a resize
cursor on hover. Cmd+W closes the active pane (or its
window when it is the last pane); Cmd+Shift+W closes the window. Up to 16 panes
share one renderer per window, with independent shells, scrollback, selections
and Find queries. Scrolling targets the hovered pane; keyboard input targets
the focused pane. Font-size changes apply to all panes in the window.

Drag the small grip above a pane onto another pane's left, right, top, or bottom
edge in the same or another Mechanic window to rearrange them. The outline previews the resulting
pane bounds. Release outside the window to detach the pane into its own window;
its running shell, scrollback, and local-control session handle survive the move.
Escape cancels a drag. Text below the grip retains normal selection and TUI mouse
behavior.

Cmd+Shift+Return zooms the active pane to fill its window; press it again to
restore the layout. Focus shortcuts work while zoomed. Other shells keep running.
Splitting or docking into a window reveals its full layout.

Use the palette to set an optional pane title, text color, or outline color.
Colors use `#RRGGBB`; submit an empty value to reset that field. Text color changes
the default foreground; explicit ANSI/OSC colors remain available to programs.
Titles and colors follow live panes and survive workspace restoration.

Save a named loadout from the palette, with or without starting directories.
Loadouts contain all current windows, pane layouts, titles/colors, and font sizes.
Opening one adds fresh shells in new windows; existing shells keep running.
Loadouts do not save commands or scrollback. Saving an existing name replaces it.
They live in `loadouts.json` beside `session.json` and remain available when
automatic session restoration is disabled. Temporary pane zoom is not saved.

Mechanic saves window sizes/positions, pane layouts, active panes, font sizes,
and reported local directories. Relaunching starts fresh configured shells;
commands and terminal output are not restored. `--no-restore` starts a fresh
workspace and saves it normally. `[session] restore = false` disables both
loading and saving. State lives in `$XDG_STATE_HOME/mechanic/session.json`, or
`~/.local/state/mechanic/session.json`. Only one app instance owns the saved
workspace at a time. Cmd+Q saves the whole workspace; closing the last window
retains its layout. Missing directories fall back to the startup directory.

Local automation uses a private Unix socket restricted to the current user.
`mechanic ctl instances` discovers running instances; `--socket PATH` chooses
one explicitly. `mechanic ctl list` returns panes and their instance/session
identifiers as JSON. Use the combined `INSTANCE:SESSION` handle to target a
pane; handles expire when its shell restarts or the app exits.

```sh
mechanic ctl list
mechanic ctl read --pane INSTANCE:SESSION --lines 100
mechanic ctl send --pane INSTANCE:SESSION --text 'cargo test' --enter
mechanic ctl wait --pane INSTANCE:SESSION --after 0 --timeout 30
```

Pane management and loadouts are also available to local automation:

```sh
mechanic ctl create --directory /absolute/project/path
mechanic ctl split --pane INSTANCE:SESSION --axis vertical
mechanic ctl focus --pane INSTANCE:SESSION
mechanic ctl zoom --pane INSTANCE:SESSION
mechanic ctl move --pane INSTANCE:SESSION --target INSTANCE:OTHER_SESSION --edge bottom
mechanic ctl move --pane INSTANCE:SESSION --new-window
mechanic ctl set --pane INSTANCE:SESSION --title Tests --text-color '#71DABC' --outline-color '#C081E8'
mechanic ctl loadout save --name Development
mechanic ctl loadout list
mechanic ctl loadout open --name Development
mechanic ctl loadout delete --name Development
mechanic ctl close --pane INSTANCE:SESSION
```

`vertical` splits side by side; `horizontal` stacks panes. Create/split accept
`--directory`; otherwise they use the selected pane's directory when available.
Explicit directories must exist and be absolute; a directory becoming unavailable
during shell launch can still fall back to the normal shell directory.
Use `--no-directories` with loadout save/open to omit or ignore saved directories.
Use `--clear-title`, `--clear-text-color`, or `--clear-outline-color` to reset a
pane field. Mutation replies include the resulting pane's session handle;
moving a pane keeps that handle. `close` terminates the selected shell.

`send` uses terminal paste handling; `--enter` appends Enter and `--raw` sends
unfiltered input. Embedded newlines follow the shell's normal paste behavior.
`--stdin` reads input from a pipe. `read` returns bounded
recent grid text in logical order, retaining multilingual characters and joining
soft wraps. `wait` needs OSC 133 command boundaries and returns a completion
whose ID is greater than `--after`; capture `latest_command_id` from `list`
before sending a new command. Recent completions remain available if the command
finishes before `wait` arrives. Control requests and replies are JSON; errors
and timeouts return a nonzero CLI status. `[control] enabled = false` disables
the socket. No network listener is opened.

Cmd+C/V copies/pastes, Cmd+K clears history,
Cmd+A selects the buffer, and Cmd++/−/0 changes/resets font size. After a
failed shell exit, Cmd+R restarts the shell.
Cmd+Shift+A toggles logo and background animations together across all windows.
If either is on, both turn off. This session-only switch does not change configuration.

`mechanic-core` owns terminal state and PTY I/O; `mechanic-app` routes events
and converts the grid; `mechanic-renderer` shapes glyphs and draws cells;
`mechanic-config` loads TOML. Text shaping supports combining marks, Japanese
wide cells, Arabic paragraph direction and joining across soft wraps. Configured font fallbacks precede
platform fallback; macOS supplies Hiragino Sans and Geeza Pro. See
[review findings](design/REVIEW.md) for remaining rendering limits.

Run the reading demo inside Mechanic:

```sh
cargo run --release -p mechanic-bench --example paragraph_demo
```

It scrolls original English, French, Chinese, Arabic and Japanese paragraphs
using native wrapping. Space pauses/resumes, R replays, and Q/Esc quits.
The final text stays available for scrollback; `--plain` prints it immediately.

Underline styles, underline colors and strikeout are rendered. OSC palette
changes affect both display and color-query replies. IME composition appears
underlined with a selection and caret, clipped to the current visible row.
Mouse reporting distinguishes hover and held buttons; trackpad scrolling retains
fractional movement within each gesture.

OSC 8 hyperlink labels are underlined. Hover to preview the address, Cmd-click
to open HTTP/HTTPS links in the default browser, or right-click for Open Link
and Copy Link Address. When a TUI captures the mouse, use Shift-right-click or
Cmd-right-click for the terminal's menu. Ordinary clicks still select text.
Other URI schemes remain copyable; Open Link is disabled. Plain URL text is
not automatically detected.

```sh
printf '\033]8;;https://example.com\033\\Example link\033]8;;\033\\\n'
```

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
Lost window surfaces are recreated while retaining the atlas and cached frames.
GPU device loss recreates the device, atlas, and rendering resources while
retaining shells and pane state. Failed recovery attempts retry after 250 ms;
occluded windows defer recovery until visible.

See [benchmarks](benchmarks/README.md) for Rust microbenchmarks and identical
workloads for Mechanic, iTerm2, and Ghostty.
