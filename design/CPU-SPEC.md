# Mechanic: CPU-Awareness Specification

Every new feature in Mechanic has two specs, not one: what the user *sees*,
and what the CPU *does*.  This document defines the second spec — the
questions every feature must answer before it lands, the architectural
rules those answers have to compose with, and worked examples showing
the patterns that have repeatedly bitten us in this codebase.

Use it as a checklist in PR descriptions and design docs.  Use it as a
review guide when reading someone else's feature branch.  Use it as a
thinking aid when an idea is still in your head and you want to catch
the CPU bug before it's committed.

---

## Context: Why this exists

Mechanic spends well over 99% of its wall time in a single state: a
focused window showing a shell prompt with no keystrokes arriving.
**The idle-at-prompt state must cost effectively zero CPU.**  Anything
else is a bug, even if the pixels on screen look correct.

Four distinct regressions in the first week of development all shared
one root cause: features specified by their visible behaviour, with the
CPU cost an afterthought that showed up in Activity Monitor only after
the feature landed.  These were the symptoms:

| Regression | Symptom | Fixed in |
|---|---|---|
| Continuous opacity fade on blur | ~15% CPU for 30 s after every blur | `2a0e0d2` (ripped it out; opacity snaps now) |
| Busy-polling PTY reader thread | 100% of one core at idle shell | `b2054b0` (cleared `O_NONBLOCK`) |
| Unconditional `request_redraw` per frame | ~10% CPU forever | `46de83b` (sleep-at-idle event loop) |
| Shader breath / gradient / pulses on by default | Permanent 30 FPS tax | `46de83b` (gated behind `--hot-cpu`) |

The pattern in every case: the feature worked, looked good, and
silently pegged the CPU in a state where the user's screen wasn't
changing.  The fix in every case was to flip the spec around —
describe **wake sources** and **sleep conditions** first, let the
visible behaviour follow.

---

## The five-question checklist

Every feature that touches rendering, the event loop, or any thread
that could wake the event loop must answer these five questions in
its design doc or PR description.  In that order.

### 1. What is the wake source?

Every redraw has an origin.  List yours explicitly.  Valid origins:

- User input (`WindowEvent::KeyboardInput`, `MouseInput`, etc.)
- PTY output (`UserEvent::PtyOutput(WindowId)` via `EventLoopProxy`)
- Scheduled deadline (`ControlFlow::WaitUntil(t)` driven by
  `classify_animation` returning `Active { next_frame }`)
- Window lifecycle (`Resized`, `Focused`, `CloseRequested`)
- Cross-thread signal (a new `UserEvent` variant, if you need one)

If your feature's wake source is *"a timer that runs always"* or
*"the shader clock"*, stop — you are about to add Regression
Pattern #4.  Continuous wake sources must be gated behind an
explicit opt-in flag (see Architectural Rule #3 below).

### 2. What is the sleep condition?

For every wake source, state in one sentence what makes it stop.

- **Bounded animations** have a deadline: *"wakes every 33 ms until
  `Instant::now() >= animation_end`, then goes idle"*.  The window
  spawn animation (Phase 5) and the focus-redraw burst are of this
  shape.
- **Event-driven wakes** have no deadline; they fire once per event
  and fall back to `ControlFlow::Wait`.  PTY output, keystrokes, and
  mouse events are all of this shape.
- **Unbounded animations** have no sleep condition at all.  These are
  only acceptable when gated behind `--hot-cpu` (or a successor
  flag), because the user has opted in to paying for them.  The
  shader light show is of this shape.

If you cannot state the sleep condition in one sentence, the feature
is under-specified.  Do not start coding yet.

### 3. Can it snap instead of interpolate?

This is the lesson of commit `2a0e0d2`.  An interpolation that runs
for N × `FRAME_INTERVAL` keeps the event loop awake for N frames.
A snap is one frame.  Before adding any `ease`, `fade`, `smoothstep`,
or `lerp`, challenge the assumption:

- Does the human eye actually need the interpolation to read the
  transition as intentional?  Opacity snaps between 0.65 and 0.85 on
  focus change were unanimously read as "correct" by users; the 30 s
  fade they replaced was read as "sluggish".  Snaps can feel *better*,
  not worse.
- If a visual cue is genuinely needed to smooth the discontinuity,
  can it be a **short forced-redraw burst** (the `focus_redraw_frames`
  pattern — 5 frames ≈ 165 ms) rather than a full interpolation?
- Reserve interpolation for features where the motion itself is the
  point: the Phase 5 spawn animation (the window *flying in* is the
  feature), the Phase 6 carousel rotation.  Everything else should
  default to snap.

### 4. How does `classify_animation` classify this?

`classify_animation` in `crates/mechanic-app/src/app.rs` is the
single source of truth for what the event loop does at idle.  If
your feature wants a frame drawn at time T without a user event
triggering it, the feature has to extend this function.

Concretely, answer:

- Which `AnimationState` arm does the feature return?
  `Active { next_frame }` for continuous or periodic redraws,
  `Idle` for fully-static.
- What new input to the classifier does the feature require?
  (Add a field to `AnimationInputs`; do **not** read from `AppState`
  outside `classify_animation`'s signature.)
- What's the precedence relative to existing rules?  Rule 1 (frozen
  window → Idle) currently outranks everything; Rule 2 (focus burst)
  outranks `hot_cpu`; Rule 3 (focused + `hot_cpu`) falls through to
  Rule 4 (Idle) otherwise.  Where does yours fit?
- What's the unit test that pins the new arm, **including** a test
  that proves an unfocused or frozen window still classifies as
  `Idle`?

If you're adding a `request_redraw()` call outside an event handler,
or introducing a new way for the loop to wake itself periodically,
and it doesn't go through `classify_animation` — that is the bug.

### 5. What's the idle-at-prompt CPU cost?

Measurement is part of the feature.  Every PR that could affect CPU
use must include a number.  The measurement protocol:

1. Build release: `cargo build --release`.
2. Launch: `./target/release/mechanic` (no flags — test the default).
3. Let it sit at the shell prompt.  Don't touch it.
4. Read CPU% from Activity Monitor or `top -pid $(pgrep mechanic)`
   averaged over 30 seconds.  Or run the integration test:
   `cargo test --test idle_cpu --release -- --ignored`.

Target: **< 1%** on a modern Mac with no keystrokes arriving.  The
regression test asserts < 5% (headroom for transient blips on a
busy system), but your PR should hit < 1% in practice.  If it
doesn't, something is still waking the loop, and the other four
questions haven't been answered honestly.

Also measure under `--hot-cpu` for a second data point: the shader
light show legitimately costs a few percent, but a regression that
takes it to 40% is still a regression.

---

## Architectural rules

The checklist is the per-feature test.  These are the system-wide
invariants that make the checklist workable.  Don't break them
without a design discussion.

### Rule 1 — The event loop sleeps by default

`ControlFlow::Wait` is the baseline.  `about_to_wait` transitions to
`ControlFlow::WaitUntil(t)` only when a window classifies as
`Active { next_frame: t }`.  Any feature that requires continuous
redraws must compose with this rule by returning `Active` from
`classify_animation`, never by adding a side-channel timer.

### Rule 2 — `classify_animation` is the scheduler

There is exactly one place that decides when the next frame should
happen: the per-window loop inside `about_to_wait`, driven by
`classify_animation`.  `request_redraw()` calls elsewhere in the
code are permitted **only** as responses to external events (user
input, PTY output, winit lifecycle).  They must not encode periodic
or time-based scheduling.

The current state of this invariant is good — see `design/CPU-AUDIT.md`
for the full inventory.

### Rule 3 — Unbounded animation is opt-in

Any animation driven by the shader clock (`time` uniform) or any
other continuous source without a bounded end time must be gated
behind a user-facing flag, currently `--hot-cpu`.  The flag's
contract is: *"off by default, costs nothing to hold a focused
window open; on if the user explicitly asks for the light show"*.

When adding a new unbounded animation:
- Gate it at both the shader side (pass `hot_cpu` into the relevant
  uniform) **and** the scheduler side (`classify_animation` returns
  `Idle` when `!hot_cpu`).  Either gate alone is insufficient: the
  shader gate without the scheduler gate keeps the loop spinning
  rendering static frames; the scheduler gate without the shader
  gate means the animation appears frozen mid-state the moment focus
  shifts.
- Per-window: unbounded animations stop for unfocused windows
  regardless of the flag.  The user is not looking at that window.

### Rule 4 — Background threads never poll

Any thread that produces data for the main loop (PTY reader, future
file-watcher, future network poller) must block in the kernel when
idle.  Specifically:

- No `while let Ok(_) = ... { }` loops that spin on `WouldBlock`.
- No `thread::sleep(SHORT_INTERVAL)` polling patterns.
- Use blocking syscalls (`read(2)`, `poll(2)`) and wake the main
  loop via `EventLoopProxy::send_event` when data arrives.

This is the lesson of commit `b2054b0` — the PTY reader thread
inherited `O_NONBLOCK` from `alacritty_terminal`'s setup and burned
100% of a core at idle.  The fix was to block in the kernel
(`fcntl` to clear the flag); the invariant is that no thread in
Mechanic ever runs without work to do.

### Rule 5 — `content_dirty` and `request_redraw()` travel together

Every event handler that changes grid state both sets
`state.content_dirty = true` and calls `state.window.request_redraw()`
in the same arm.  The dirty flag tells the renderer to rebuild its
instance buffer; the redraw request tells winit to deliver a
`RedrawRequested` event.  One without the other either leaves a
stale frame on screen (dirty without wake) or wastes the wake on
a cache-hit render (wake without dirty).

Until a helper extracts this invariant, respect it by hand.  See the
audit doc for the full inventory of paired sites.

---

## Worked examples: the four regressions

Each of these shipped, broke the idle-CPU target, and was reverted.
Running them through the checklist shows where each spec failed.

### Example 1 — The 30 s opacity fade (reverted in `2a0e0d2`)

Original spec: *"Ramp to 95% over 30 seconds of continuous interaction;
fade to 80% starting at 30 s of inactivity, reaching 80% at 60 s.
Smooth interpolation (ease-in-out)."* (`design/ARCHITECTURE.md` Phase 4.)

Checklist review:

- **Wake source**: "a timer that ticks at 30 FPS for 30 seconds after
  every focus change" → fails Question 1 as written.  A 900-frame
  wake source chasing a moving target is the most expensive possible
  shape.
- **Sleep condition**: *"when `elapsed >= 30 s`"* — bounded, technically
  OK for Question 2, but in practice the user triggers focus changes
  constantly (Cmd+Tab to the browser and back), keeping the fade
  window almost always open.
- **Snap alternative (Q3)**: *not considered*.  This is the question
  that wasn't asked at design time.  When asked after the fact, the
  answer was *"snaps look great, ship it."*
- **Classifier integration (Q4)**: required a new `AnimationState`
  arm or continuous `Active` returns during the fade — which is
  exactly what made it expensive.
- **Idle cost (Q5)**: ~15% CPU for 30 s after every blur.  Not measured
  before shipping.

Resolution: snap.  One frame, zero ongoing cost.

### Example 2 — PTY reader thread spin (fixed in `b2054b0`)

Not a feature, but a cross-cutting architectural mistake.  The
library we vendored (`alacritty_terminal`) set `O_NONBLOCK` on the
PTY master because *its* event loop uses mio to multiplex.  We
inherited the flag without inheriting the loop.

Checklist would have caught it at Rule 4: *"Any thread that produces
data for the main loop must block in the kernel when idle."*  A PTY
reader whose `read(2)` returns `WouldBlock` and `continue`s with no
backoff is, by definition, polling.  The fix was one `fcntl` call.

Lesson: rules apply to infrastructure, not just user-facing features.
Anyone touching the PTY / background-thread plumbing should re-read
Rule 4.

### Example 3 — Unconditional per-frame redraw (fixed in `46de83b`)

Original `about_to_wait`: `for window in windows { window.request_redraw(); }`.

Checklist:

- **Wake source (Q1)**: "the event loop itself, every frame" →
  fails immediately.  The loop was waking *itself* to redraw
  unchanged pixels.
- **Sleep condition (Q2)**: "none" → fails.
- **Classifier (Q4)**: there wasn't one.  The refactor in `46de83b`
  *added* `classify_animation`.

The fix doubled as the establishment of Rule 2: from this commit
onward, `classify_animation` is the one place that can schedule a
redraw without an external event.

### Example 4 — Shader light show on by default

Pre-`46de83b`, the corner gradient breath, color pulse, and electron
pulses ran unconditionally.  Any focused window cost ~30 FPS forever.

Checklist:

- **Sleep condition (Q2)**: none — unbounded by definition.
- **Opt-in (Rule 3)**: missing.  An unbounded animation was on by
  default.

Resolution: `--hot-cpu` flag, gated at both the shader side
(`shader_focused = state.focused && hot_cpu`) and the scheduler side
(`classify_animation` returns `Idle` when `!hot_cpu`).

---

## Phase 4-6 feature walkthroughs

Applying the template to the remaining roadmap features.  These
aren't final specs — they're sketches showing the shape of the
spec the checklist forces you into.

### Phase 4 revisited: activity-based opacity

Original design: 30 s interpolation in both directions.  Current
implementation (shipped): snap between active and idle opacities on
focus change, with a 5-frame forced-redraw burst to beat macOS
AppKit's coalescing of `setNeedsDisplay:` across focus edges.

- Q1 Wake: `WindowEvent::Focused` (one-shot) + the 5-frame burst.
- Q2 Sleep: burst drains via `focus_redraw_frames.saturating_sub(1)`
  per frame; hits zero and returns to `Idle`.
- Q3 Snap: yes, already.
- Q4 Classifier: Rule 2 (`focus_redraw_frames > 0 → Active`).
- Q5 Cost: ~165 ms of animation per focus change, zero steady-state.

No further work needed; the template is a retroactive fit.

### Phase 4: animated corner gradient, electron pulses, shader breath

Current implementation: gated behind `--hot-cpu`.

- Q1 Wake: `classify_animation` returning `Active { next_frame: now + FRAME_INTERVAL }`.
- Q2 Sleep: focus loss (per-window) or user exits `--hot-cpu` mode.
- Q3 Snap: N/A — the animation *is* the feature.
- Q4 Classifier: Rule 3 (`focused + hot_cpu → Active`).
- Q5 Cost: measured in practice to be a few percent of one core;
  the user opted in.

Spec passes.  Existing.

### Phase 5: window spawn animation

Proposed: new window flies in from top at 25% size/opacity over
1000 ms, ease-out to final position.

- Q1 Wake: `spawn_window` fires at time T; classifier returns
  `Active { next_frame: now + FRAME_INTERVAL }` while
  `now < T + 1000 ms`.
- Q2 Sleep: `now >= T + 1000 ms`; window returns to normal
  classification (`Idle` at default, `Active` if focused +
  `--hot-cpu`).
- Q3 Snap: no — this is one of the few features where the motion
  is the feature.  1000 ms may be too long; consider 400 ms.  The
  ease-out means the perceived duration is shorter than the literal
  duration.
- Q4 Classifier: new field `AnimationInputs::spawn_deadline: Option<Instant>`,
  new rule between current Rule 2 and Rule 3:
  ```
  Rule 2.5: spawn_deadline.is_some_and(|t| now < t) → Active.
  ```
  Unit test must cover: (a) active during window, (b) drops to Idle
  after deadline, (c) drops to Idle if window loses focus mid-animation
  (do we interrupt?  design decision — probably yes, skip to final
  state on blur).
- Q5 Cost: a few percent for < 1 s per window spawn.  Steady state
  unchanged.

### Phase 6: 3D window carousel

Proposed: keyboard shortcut tilts all subwindows 45° toward centre,
user navigates L/R to cycle, selected window un-tilts to full-screen.

This is the feature that stresses every question.

- Q1 Wake: user triggers carousel shortcut; classifier returns
  `Active` for the duration the user is in carousel mode.  Navigation
  keys (L/R) are the wake source *during* carousel mode; no periodic
  wakes when the transform is at rest.
- Q2 Sleep: user selects a window or cancels; carousel mode exits,
  classifier returns to normal.  Transition animations on entry and
  exit are bounded (say, 300 ms each).
- Q3 Snap: the tilt transform itself probably wants interpolation
  (the motion is part of the effect).  But consider: once the
  carousel is at rest between navigation keypresses, the transform
  is static.  `Active` during transitions, `Idle` at rest?  Worth
  testing — the carousel at rest on the same tilt for several
  seconds is the idle-at-prompt analogue here.
- Q4 Classifier: new field
  `AnimationInputs::carousel_state: Option<CarouselPhase>` where
  `CarouselPhase` is `{ Entering { deadline }, AtRest, Transitioning { deadline }, Exiting { deadline } }`.
  `AtRest` → `Idle`.  Others → `Active { next_frame }`.
- Q5 Cost: this is where it matters.  Compositing N terminal
  textures with a 3D transform every frame is the expensive path.
  The architecture doc already flags the right mitigation:
  **snapshot the grid textures on carousel entry**.  During carousel
  mode, the terminal grids are frozen — PTY output sets a dirty flag
  but doesn't render until carousel exit.  Idle-at-rest carousel
  should cost zero (no rendering).  Transitioning carousel costs
  only the compositing, not the grid rebuild.

The carousel is the feature where the spec template earns its keep.
Without it you'd casually set up a "render 8 terminals at 30 FPS
while the user decides" loop and discover the 50% CPU cost at the
demo.

---

## When to consult this document

- Before writing a design doc for any new rendering or animation
  feature.  Copy the five questions into the doc and answer them
  before prose.
- Before approving a PR that adds a `request_redraw()` call, modifies
  `classify_animation`, or introduces a background thread.  Reviewer
  should be able to cite the Q/A answers.
- When Activity Monitor shows an unexpected CPU number.  Walk the
  event loop — wake source, sleep condition — and find which rule
  was violated.
- Annually, or when any rule seems wrong.  The rules are inferred
  from Mechanic's current shape.  If we migrate off winit, swap the
  shader pipeline, or add true multi-threaded rendering, this
  document should be revisited.
