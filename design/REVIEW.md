# Code review — 2026-10-01

Two Sol 6.1/medium agents reviewed correctness and performance. Findings were
checked against source; measurements are in [results](../benchmarks/RESULTS.md).

Fixed: ordered nonblocking PTY transport, bounded parser turns, parsing independent
of presentation, fixed frame deadlines, and hidden-window presentation suspension.

Text rendering now preserves combining marks, wide-cell backgrounds and concealed
text. Arabic uses contextual shaping and paragraph bidi across soft-wrapped rows,
with shared maps for backgrounds, cursor, selection and mouse coordinates. Copying
retains logical source order. Atlas uploads are preflighted before instance
construction, bounded by device limits, and include sampling gutters. Cursor
visibility and bar/underline/wide-outline geometry are tested.

Remaining issues:

| Priority | Finding | Location |
| --- | --- | --- |
| P2 | Mode 1003 hover is encoded as left-button drag; other held buttons are untracked. | `app/app.rs` |
| P2 | SGR underline and other text decorations are not drawn. | `renderer/pipeline.rs` |
| P2 | Fractional trackpad scroll deltas are discarded per event. | `app/app.rs` |
| P2 | PTY output posts wake events even when one is pending. | `core/pty.rs` |
| P2 | Terminal color/size query callbacks remain unimplemented. | `core/terminal.rs` |

Text limits: terminal wrapping is by cells, not words. Shaping breaks joining at
physical row boundaries. Offscreen bidi context is bounded to 64 KiB per side;
truncated or discarded scrollback cannot supply full paragraph context. Color
bitmaps use their alpha as monochrome coverage; emoji typography is not validated.
IME candidate positioning is mapped, but preedit text is not drawn in the grid.

Full conversion, instance construction and uploads still occur on content frames.
Profiling is opt-in and measures host work, not GPU completion. No new idle timers
were added for text shaping.
