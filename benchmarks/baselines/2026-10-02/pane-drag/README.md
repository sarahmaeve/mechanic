# Pane drag and detachment

macOS 26.6.2, arm64, Rust 1.99.0. All timings ran serially without builds or
tests in parallel. The release layout benchmark uses a balanced 16-pane tree;
each operation computes and validates a candidate layout, then commits a move.
Five batches of 10,000 operations measured 2,271 ns median per operation
(batch range 2,056–3,077 ns). This excludes rendering, PTY resize/reflow, and
native window creation. Correctness is checked outside the timed loop.

```sh
cargo test --release --locked -p mechanic-app --bin mechanic panes::tests::move_preview_commit_benchmark -- --exact --ignored --nocapture
```

The GUI idle CPU attempts (`before-idle.json`, `after-idle.json`) both lost
focus during their samples and are invalid for comparison. No idle CPU change
is claimed. They used animations off, temporary state, and enabled services,
with three seconds settling and eight seconds sampling.

Before SHA-256:
`0f4e9031b0444a02e9c1af9b649156e48281c722b15e5d2576dd2888434248f6`.
After SHA-256:
`a47f0e45418cc3c8e299f5323e9c01fc52f188fb2cc2908904abcaa61f7ca13d`.

Native gesture checks cover the three-pane rearrangement, preview agreement,
detaching on outside release, click thresholds, cancellation, and mouse capture.
The live-detach check preserves shell variables, history, selection, Find,
control handles, and a completion waiter after closing the original window.
Metal checks verify divider/grip/preview pixels, clipping, and clearing. An
unchanged decoration list uploads zero geometry bytes; hover and preview changes
retain terminal shaping and geometry caches. No animation timer was added.
