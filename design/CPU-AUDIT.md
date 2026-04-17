# Mechanic: `request_redraw()` Audit

Snapshot date: 2026-04-17  
Audited commit: `2a0e0d2` (tip of `main`)  
Audited file: `crates/mechanic-app/src/app.rs`

Every `request_redraw()` call in the event-loop layer is inventoried
here and classified against the rules in `design/CPU-SPEC.md`
(Architectural Rule 2: "`classify_animation` is the scheduler"; Rule
5: "`content_dirty` and `request_redraw()` travel together").

**Bottom line: the file is clean.**  Nineteen sites are event-driven,
one is animation-driven and correctly routed through
`classify_animation`, and no latent spinners were found.  Two minor
refinements are flagged as suggestions, not bugs.

Regenerate this audit after any PR that adds a `request_redraw()`
call, modifies the event loop, or changes `classify_animation`.

---

## Classification rubric

Each site falls into exactly one bucket:

- **Event-driven** — called in response to a winit event, a user
  action, or a cross-thread `UserEvent`.  Wakes the loop exactly
  once per external stimulus.  Permitted; this is what
  `request_redraw()` is for.
- **Animation-driven via `classify_animation`** — called inside
  `about_to_wait` after the classifier returns `Active`.  Permitted;
  this is the single sanctioned path for periodic redraws.
- **Animation-driven bypassing `classify_animation`** — any other
  periodic or time-based redraw scheduling.  **Forbidden.**  This is
  the shape that produced the four regressions catalogued in
  `design/CPU-SPEC.md`.

---

## Inventory

Twenty call sites total, in source order.

### Event-driven (19)

| # | Line | Context | Trigger |
|---|------|---------|---------|
| 1 | 242 | `apply_font_size` | Cmd+= / Cmd+− / Cmd+0 keypress |
| 2 | 358 | `spawn_window` | Startup and Cmd+N |
| 3 | 488 | `WindowEvent::Resized` | OS resize event |
| 4 | 516 | `WindowEvent::Focused` | OS focus change (also seeds `focus_redraw_frames`) |
| 5 | 629 | `CmdShortcut::Paste` | Cmd+V |
| 6 | 636 | `CmdShortcut::ClearScrollback` | Cmd+K |
| 7 | 644 | `CmdShortcut::SelectAll` | Cmd+A |
| 8 | 676 | `CmdShortcut::ReadlineUndo` | Cmd+Z |
| 9 | 709 | `WindowEvent::KeyboardInput` default tail | Every keypress forwarded to the PTY |
| 10 | 748 | `WindowEvent::Ime` | IME `Commit` or `Preedit` |
| 11 | 798 | `MouseInput` forwarded path | Forwarded mouse button (PTY write) |
| 12 | 887 | `MouseInput` local left-button | Selection start/end, click-to-move-cursor |
| 13 | 897 | `MouseInput` local middle-button | Middle-click paste from primary selection |
| 14 | 982 | `CursorMoved` local drag path | Drag-select extension |
| 15 | 1035 | `MouseWheel` forwarded path | Forwarded wheel event (PTY write) |
| 16 | 1048 | `MouseWheel` local path | Local scrollback scroll |
| 17 | 1099 | `RedrawRequested` child-exit branch | One-shot banner inject after shell exit |
| 18 | 1177 | `user_event(UserEvent::PtyOutput)` | Cross-thread wake from PTY reader |
| 19 | 1592 | `respawn_shell` | Cmd+R inside a frozen window |

All 19 fire exactly once per external event.  None encode any
periodic or deadline-driven wake.  Compliant with Rule 2.

### Animation-driven via `classify_animation` (1)

| # | Line | Context | Details |
|---|------|---------|---------|
| 20 | 1147 | `about_to_wait` | Fires only when `classify_animation` returns `Active { next_frame }`; partnered with `merge_deadline` to set `ControlFlow::WaitUntil`. |

This is the **only** place in the file that can cause a periodic
redraw.  That's by design, and it's the invariant that the CPU-spec
doc exists to protect.

### Animation-driven bypassing `classify_animation` (0)

None found.  Good.

---

## `content_dirty` pairing check

Rule 5 says `content_dirty = true` and `request_redraw()` travel
together whenever grid state changes.  Every site that marks dirty
also requests a redraw in the same arm:

| Line | Pair |
|------|------|
| 241 / 242 | `apply_font_size` |
| 487 / 488 | Resized |
| 514 / 516 | Focused (with intervening `focus_redraw_frames` seed) |
| 628 / 629 | Paste |
| 635 / 636 | ClearScrollback |
| 643 / 644 | SelectAll |
| 675 / 676 | ReadlineUndo |
| 708 / 709 | KeyboardInput tail |
| 747 / 748 | IME |
| 797 / 798 | Forwarded mouse button |
| 886 / 887 | Local left-button |
| 896 / 897 | Middle-click paste |
| 981 / 982 | Drag motion |
| 1034 / 1035 | Forwarded wheel |
| 1047 / 1048 | Local wheel |
| 1098 / 1099 | Exit banner |
| 1591 / 1592 | respawn_shell |

One site marks dirty without a paired `request_redraw()`: line 1061,
inside the `RedrawRequested` handler itself:

```rust
if outcome.grid_maybe_changed {
    state.content_dirty = true;
}
```

This is correct.  We're already inside a redraw; `render_frame` at
line 1103 picks up the flag and takes the full-render branch.
Requesting another redraw from inside a redraw would be a noise
wake.

---

## Observations and minor refinements

Not bugs, but patterns worth noting for future work.

### Obs. 1 — `content_dirty` invariant is maintained by discipline

Rule 5 holds today, but nothing in the type system prevents a
future PR from setting `content_dirty = true` without a paired
redraw request (leaves a stale frame) or calling `request_redraw()`
after a state change without dirtying (renders a cache-hit frame
that shows the old content).

**Suggestion:** extract a helper method

```rust
impl AppState {
    fn invalidate(&mut self) {
        self.content_dirty = true;
        self.window.request_redraw();
    }
}
```

and replace the 17 paired sites with single `state.invalidate()`
calls.  The invariant becomes mechanically enforced rather than
hand-maintained.  Leaves the four exceptional sites untouched:
line 358 (spawn, no AppState yet), 1061 (inside redraw), 1099
(banner path with `self.close_window` borrow), 1147
(animation-driven; no content change).

Not urgent; every current site is correctly paired.

### Obs. 2 — Forwarded mouse motion has no self-redraw

`CursorMoved` forwarded path (lines 920–961) writes mouse-motion
bytes to the PTY but does **not** mark dirty or request a redraw.
The reasoning: if the TUI program echoes anything in response, the
PTY reader thread posts `UserEvent::PtyOutput`, which wakes the
loop at site #18 (line 1177).  If the program doesn't echo
(common — mouse-motion reports are often consumed silently for
position tracking), there's nothing new to show and we correctly
stay asleep.

This is the right behaviour, but it's **subtly different** from the
other forwarded paths (mouse button, wheel) which *do* mark dirty
and request a redraw unconditionally.  The asymmetry is
deliberate: mouse motion at full drag speed would thrash the
redraw scheduler if handled the same way, and the expected
response from the program is implicitly opt-in via echo.

Document this in a comment on the motion path?  Left as a minor
readability improvement — behaviour is correct, just non-obvious.

### Obs. 3 — Late `UserEvent::PtyOutput` after respawn is harmless but noisy

When `respawn_shell` replaces `state.terminal`, the old `Terminal`'s
`Drop` closes its PTY and the reader thread sees EOF and exits.
Any `UserEvent::PtyOutput(window_id)` events already in the winit
event queue at that moment will be delivered to the *new* state,
which is safe (the handler just requests a redraw on the window,
and `process_input` on the new terminal is a no-op) but wastes a
render cycle.

Fix would require either (a) tagging `PtyOutput` events with a
terminal generation counter, or (b) draining the winit queue
before respawn — both feel heavier than the cost.  Leave alone;
flag if it ever shows up in a profile.

### Obs. 4 — `classify_animation` has four inputs and one output arm

Current shape is minimal and clean.  When Phase 5 (spawn
animation) and Phase 6 (3D carousel) land, the checklist in
`CPU-SPEC.md` prescribes adding new inputs (`spawn_deadline`,
`carousel_state`) and new rules in precedence order.

The precedence worth preserving: **frozen window always wins**
(Rule 1).  A future rule that fires `Active` for a carousel
animation on a window whose shell has exited would reintroduce
the "idle window pegs the CPU" class of bug.  Every new rule
should be tested against `is_alive: false` to confirm it yields
`Idle`.

### Obs. 5 — The "30 FPS cap" does not actually cap (confirmed 2026-04-17)

**Status: confirmed bug, documented for follow-up, not fixed.**

`FRAME_INTERVAL: Duration = Duration::from_millis(33)` in
`app.rs:36` and the `next_frame: now + FRAME_INTERVAL` return
from `classify_animation` state the *intent* that Active windows
render at 30 FPS.  The `46de83b` commit message claims:

> Animation frames target ~30 FPS via a 33 ms FRAME_INTERVAL
> constant — visually indistinguishable from 60 FPS given the
> ≥ 2-second animation periods we run, at half the cost.

The intent is not realised.  Empirical measurement during the
bloom feature's startup diagnostic (2026-04-17, ProMotion
display, default mode, no `--hot-cpu`) showed bloom progress
advancing by ~0.034 per frame over a 250 ms animation =
**~118 FPS**, not 30.  Frame intervals of ~8.5 ms rather than 33 ms.

**Root cause: `request_redraw()` creates a pending event that
bypasses `ControlFlow::WaitUntil`.**

The current `about_to_wait` body, for any window that classifies
as `Active { next_frame }`:

```rust
AnimationState::Active { next_frame } => {
    state.window.request_redraw();                        // (A)
    merge_deadline(&mut earliest_deadline, next_frame);   // (B)
}
// ... then at the bottom ...
event_loop.set_control_flow(ControlFlow::WaitUntil(earliest_deadline));  // (C)
```

The problem is the composition of (A) with (C):

1. `about_to_wait` at time `T` calls `request_redraw()` and
   sets `WaitUntil(T + 33 ms)`.
2. Control returns to winit.
3. winit's event loop checks for pending events before honoring
   `WaitUntil`.  The just-queued `RedrawRequested` *is* a pending
   event, so winit dispatches it immediately.
4. `RedrawRequested` → `render_frame` → back to `about_to_wait`
   at time `T + render_time` (few ms).
5. `about_to_wait` calls `request_redraw()` again.  Another
   pending event.  Loop continues at full speed.

The 33 ms deadline never arrives because we preempt it every
iteration.  The effective cap is whatever the render pipeline
can sustain — on a ProMotion display with `PresentMode::Fifo`
(vsync-locked presents), that's 120 FPS.  On 60 Hz displays it'd
be 60 FPS.  The only configuration this *is* 30 FPS on is a
hypothetical 30 Hz eInk display, which we don't support.

**CPU cost impact:**

Every `Active` window costs ~4× more CPU than the spec claims on
a modern Mac, ~2× on a 60 Hz display.  This affects:

- `--hot-cpu` mode (shader light show on focused window).  The
  intended cost profile was "a few percent, bounded" — the
  actual cost is ~4× that.
- The bloom feature's ~250 ms active window per focus-gain.
  Runs at 30 frames instead of the designed 8.  Bounded and
  transient, so not a showstopper, but the per-second CPU
  during rapid focus-cycling is higher than the design
  anticipated.
- The focus-redraw burst (5 frames nominal).  Ironically this
  one is *correct* by coincidence — the loop runs at ~120 FPS
  during the burst, draining the 5-frame counter in ~42 ms
  instead of 165 ms.  Less defensive coverage than intended
  for the AppKit focus-snap guarantee, but no one's complained
  because 42 ms is still past the ~50 ms coalescing window.

**Proposed fix (for a follow-up PR):**

Gate `request_redraw()` on "has `FRAME_INTERVAL` actually
elapsed since the last render?".  Track the last render
timestamp per window, and decline to queue a redraw if not
enough time has passed.  The `WaitUntil` then has a chance to
actually sleep.  Rough shape:

```rust
struct AppState {
    // ... existing fields ...
    /// When the most recent `RedrawRequested` finished rendering.
    /// Set inside the `RedrawRequested` handler after `render_frame`
    /// returns.  None before the first render.
    last_frame_at: Option<Instant>,
}

fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
    let now = Instant::now();
    // ... per-window loop ...
    AnimationState::Active { next_frame } => {
        let frame_ready = state.last_frame_at
            .map_or(true, |t| now.saturating_duration_since(t) >= FRAME_INTERVAL);
        if frame_ready {
            state.window.request_redraw();
            merge_deadline(&mut earliest_deadline, next_frame);
        } else {
            // Not yet — schedule the wake at the frame boundary.
            let boundary = state.last_frame_at.unwrap() + FRAME_INTERVAL;
            merge_deadline(&mut earliest_deadline, boundary);
        }
    }
    // ...
}
```

When `frame_ready` is false, we don't request a redraw — the
only pending event is the `WaitUntil` deadline at the frame
boundary.  winit sleeps until then.  At the deadline,
`about_to_wait` fires again, now sees `frame_ready = true`,
and requests the redraw.

**Testing the fix:**

The `idle_cpu.rs` integration test (`#[ignore]`-gated) should
be extended with a third case that measures CPU under a
continuously-active window (`--hot-cpu`) and asserts the mean
is consistent with a ~30 FPS cap, not 120.  Concretely: a
`cpu_pct_hot_cpu_is_bounded_by_fps_cap` test that passes iff
`pct < HOT_CPU_THIRTY_FPS_THRESHOLD`, where the threshold is
calibrated to "what `--hot-cpu` at a true 30 FPS costs" rather
than the current loose 40% ceiling.

**Alternative fix considered and rejected:**

Could just embrace the vsync rate and remove `FRAME_INTERVAL`
entirely — let `PresentMode::Fifo` drive the pacing.  Rejected
because:
1. It gives up the power of an explicit scheduler knob.  If we
   ever want to drop to 15 FPS on battery, or uncap under
   `--hot-cpu` for smoothness, we'd need the knob back.
2. It's display-dependent and therefore unpredictable in tests
   and code review.
3. It works against the spirit of `CPU-SPEC.md` rule 2
   ("`classify_animation` is the scheduler") — the scheduler
   should be in code, not in hardware.

**Priority: medium.**  Not blocking any shipping feature — the
bloom still animates correctly over 250 ms, the light show
still runs, the idle-at-prompt state is unaffected (Idle
classification still leads to `ControlFlow::Wait` which *does*
sleep properly).  But the 4× CPU discrepancy during Active
windows is the kind of miscalibration that compounds over
Phase 5 / Phase 6 features and turns into the same "shipped
something that burned CPU silently" pattern the spec document
exists to prevent.

---

## Test coverage

`classify_animation` has eight unit tests in `app.rs` covering:

- Frozen window → Idle (both focused and unfocused)
- Focused quiet default (no `--hot-cpu`) → Idle
- Focused with `--hot-cpu` → `Active` with correct `next_frame`
- Unfocused always Idle (both `hot_cpu` on and off)
- Focus-redraw burst forces Active regardless of focus state
- Burst drains to Idle when counter hits zero
- Frozen window overrides non-zero burst

This is the right shape and depth.  When adding a new
`AnimationInputs` field or classifier arm, add a test at the same
granularity.

The `idle_cpu.rs` integration test (`#[ignore]`-gated, run locally
with `cargo test --release --test idle_cpu -- --ignored`) guards
the empirical outcome.  If the classifier tests pass but the
integration test fails, something outside the classifier is
waking the loop — that's the signal to audit this document again.

---

## When to regenerate

- After any PR that adds, removes, or reorders `request_redraw()`
  calls in `app.rs`.  Run `rg 'request_redraw\(\)' crates/mechanic-app/src/app.rs`
  and diff against the inventory above.
- After any PR that changes `AnimationState`, `AnimationInputs`, or
  `classify_animation`'s rule set.
- Quarterly, as a drift check.  Even if nothing obvious has
  changed, new events and arms accumulate and the classification
  may no longer match reality.

The file itself is cheap to regenerate.  The expensive part is the
reasoning — if you find yourself classifying a site as "probably
event-driven" rather than being certain, that's a sign the
call site needs a comment explaining what wakes it.
