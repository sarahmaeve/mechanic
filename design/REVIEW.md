# Code review — 2026-10-02

Two Sol 6.1/medium agents reviewed correctness and performance. Findings were
checked against source; measurements are in [results](../benchmarks/RESULTS.md).

Fixed: ordered nonblocking PTY transport, bounded parser turns, parsing independent
of presentation, fixed frame deadlines, and hidden-window presentation suspension.
PTY notifications now coalesce until the next parser turn. Failed cached
presentations return failure, allowing the existing paced redraw retry.
Lost native surfaces are recreated with the existing device and render resources;
failed recreation attempts back off for 250 ms.
OSC 8 links now have underlined labels, native hover previews and context menus,
Cmd-click browser opening, and exact-address copying. Activation checks the
release target and cancels on dragging. HTTP/HTTPS are the supported open schemes;
other destinations are copyable. Plain URLs are not detected automatically.

Text rendering now preserves combining marks, wide-cell backgrounds and concealed
text. Arabic uses contextual shaping and paragraph bidi across soft-wrapped rows,
with shared maps for backgrounds, cursor, selection and mouse coordinates. Copying
retains logical source order. Atlas uploads are preflighted before instance
construction, bounded by device limits, and include sampling gutters. Cursor
visibility and bar/underline/wide-outline geometry are tested.
Soft wraps preserve Arabic joining forms without cross-row ligatures. Font-style
boundaries retain joining, and glyph origins are relative to their geometry group
so offscreen context does not change subpixel placement.

Content rendering caches row geometry and uploads changed ranges. Cache keys
include shaping identity, colors, decorations and resolved cursor geometry;
atlas or cell-size changes invalidate all rows. Full conversion and full-surface
presentation remain. Sparse updates improve substantially; dense geometry work
has a measured 9–10% cache overhead (about 16–18 µs in the offscreen benchmark).

Fixed: hover/held-button mouse reporting, fractional trackpad scrolling,
underline styles and strikeout, OSC palette overrides and color query replies,
viewport size replies, and visible IME preedit text. IME composition remains a
display overlay until commit and clips to the current row.

Remaining issues:

| Priority | Finding | Location |
| --- | --- | --- |
| P2 | GPU device loss is detected but requires device/resource recreation. | `renderer/pipeline.rs` |

Text limits: terminal wrapping is by cells, not words. Ligatures stay within
physical rows. Offscreen bidi context is bounded to 64 KiB per side;
truncated or discarded scrollback cannot supply full paragraph context. Color
bitmaps use their alpha as monochrome coverage; emoji typography is not validated.
IME candidate positioning is mapped; joined emoji composition uses conservative
per-character widths.

Profiling is opt-in and measures host work, not GPU completion. No new idle timers
were added for text shaping or composition. Saturated PTY input queues can reject
an entire protocol-reply batch with a warning; replies are not partially enqueued.
