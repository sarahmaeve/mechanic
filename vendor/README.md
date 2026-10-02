These are the registry sources of `vte` 0.15.0 and `alacritty_terminal`
0.26.0, with their original licenses. Only build inputs are included.
Manifest entries for the omitted example and external reference-test fixtures
are removed; in-source unit tests remain available.

Mechanic's patches provide a default ANSI handler callback for OSC 7/133,
forward those markers with cursor and scroll coordinates, and expose a main
screen scroll counter and invalidation generation. This lets shell integration
use the existing parser, including its handling of fragmented sequences and
control strings. No second byte parser runs beside it.

Full main-screen scrolling advances the counter. Partial-region scrolling,
screen clearing and terminal reset advance the generation, invalidating saved
coordinates. Alternate-screen activity leaves main-screen coordinates alone.
Mechanic invalidates coordinates when resizing may reflow the grid.

Partial screen/line erases and insert/delete characters report modified cell
ranges, so routine shell prompt redraws preserve the current prompt while
previous command output overlapping edited cells becomes unavailable.

The raw VTE patch limits shell OSC payload storage to 8192 bytes and OSC 52 to
base64 for 1 MiB plus framing. These sequences reject parameter overflow and
CAN/SUB cancellation, and wait for the backslash in an ESC-backslash terminator
before dispatch. OSC 52 requires exactly three fields. Pending sequences reuse
the parser's existing storage; allocator-free builds remain supported.

Synchronized-update replay exposes callbacks at OSC query boundaries so replies
use state from the query's position. Mechanic schedules the existing timeout in
its event loop and flushes buffered output before reporting EOF.

Keyboard-mode queries report active flags. Set operations update the current
stack entry, and keyboard-stack overflow removes its oldest entry without
touching the title stack.
