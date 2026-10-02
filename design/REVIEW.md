# Code review — 2026-10-01

Two independent Sol 6.1/medium reviews covered correctness and performance.
Findings below were checked against source; GUI effects and throughput were
not measured by the reviewers. This is a prioritized backlog, not a clean
audit. Paths name the owning implementation; line numbers change during
comment cleanup.

Fixed here: `Terminal::process_input` now forwards `PtyWrite` replies to the
child, enabling DSR/DA queries and benchmark completion fences. A PTY test
checks an actual cursor report. Color/size callbacks remain unimplemented.

PTY reads and writes now share a cancellable nonblocking worker with bounded
queues. Large pastes return without waiting for the child; closing a window
wakes the worker and asynchronously terminates/reaps its child. Tests cover
input ordering, queue limits, output backpressure, exit delivery, and shutdown.
See [measured results](../benchmarks/RESULTS.md) for before/after timings.

Parsing now yields between chunks at a soft 4 ms budget with a 4 MiB hard
limit. One pending window parses per event-loop turn; continuations rotate
through the queue independently of presentation. Exits and transport errors
wait for preceding output to drain. Presentation uses fixed deadlines,
suspends while hidden, and renders the final frame of a focus glow.

| Priority | Finding and trigger | Location / direction |
| --- | --- | --- |
| P1 | Atlas growth clears earlier entries during prepopulation; later instance emission can grow again and invalidate UVs. | `renderer/text.rs`: preserve entries or prepare until stable before emitting instances. |
| P1 | Atlas height doubles without checking device texture limits. | `renderer/text.rs`: cap allocation and add eviction/pages. |
| P1 | Bar/underline cursor sizes are ignored by the solid shader path, producing full blocks. | `renderer/pipeline.rs`, `shaders/cell.wgsl`: honor cursor geometry. |
| P1 | Spacer backgrounds are lost and the next cell's background can cover the right half of wide glyphs. | `app/convert.rs`, `renderer/pipeline.rs`: retain spacer style and draw backgrounds before glyphs. |
| P2 | Hidden cursors still render; offscreen live cursors clamp onto scrollback. | `app/convert.rs`: propagate visibility and reject offscreen positions. |
| P2 | Mouse hover in mode 1003 is encoded as left-button drag; other held buttons are untracked. | `app/app.rs`: encode actual button state, including no button. |
| P2 | Combining marks/ZWJ data is dropped; glyph shaping receives one character. | `app/convert.rs`, `renderer/grid.rs`, `renderer/text.rs`: preserve clusters. |
| P2 | Color glyph data is uploaded as one-byte coverage without checking Swash content. | `renderer/text.rs`: handle RGBA/color glyphs and bitmap stride explicitly. |
| P2 | SGR underline is unused and concealed text remains visible. | `app/convert.rs`, `renderer/pipeline.rs`: implement decorations and concealment. |
| P2 | Fractional scroll deltas are truncated separately, losing slow trackpad scrolling. | `app/app.rs`: accumulate remainders per window. |
| P2 | Selection highlighting scans all selected history rows before rejecting invisible ones. | `app/convert.rs`: intersect selection with viewport first. |
| P2 | PTY output batches post wake events even if one is pending. | `core/pty.rs`: coalesce notifications with a clear/recheck protocol. |

Paths above are under `crates/mechanic-*`. Full-grid allocation and upload on
every content change are additional profiling targets, not measured
bottlenecks. The recorded benchmarks establish a transport baseline; profile
scheduling and atlas allocation before changing them.
