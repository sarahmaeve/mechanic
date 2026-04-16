# Mechanic: Architecture Plan

## Technology Decisions

### Recommended Stack

| Concern | Choice | Rationale |
|---------|--------|-----------|
| Terminal emulation | `alacritty_terminal` | Battle-tested VT parser, PTY management, grid state, scrollback. Most complete Rust terminal engine available. Internally uses `vte` for escape sequence parsing. |
| GPU rendering | `wgpu` | First-class Metal backend on macOS. Low-level enough for custom visual effects (transparency, animated gradients, 3D transforms). Used by Alacritty itself. |
| Windowing | `winit` | De facto Rust windowing library. Strong macOS support, per-window transparency via `with_transparent(true)`, used by both Alacritty and Bevy. |
| Text shaping | `cosmic-text` | Bundles `rustybuzz` (pure-Rust HarfBuzz port) for complex text shaping. Handles Arabic shaping, Cyrillic, Latin extensions. Integrates `unicode-bidi` for bidirectional text. |
| Font loading | `cosmic-text` + system fonts | cosmic-text handles font discovery and fallback chains. Berkeley Mono loaded as preferred font, with system fallbacks for coverage gaps. |

### Candidates Evaluated and Set Aside

| Candidate | Verdict | Why |
|-----------|---------|-----|
| **Bevy** | Deferred | ECS is a natural fit for entity-per-window and animation systems, and 3D transforms would be trivial. But: no complex text shaping built-in, pre-1.0 API churn, heavy binary/startup cost for a terminal app, very few non-game precedents. The 3D carousel effect can be achieved with raw wgpu at lower cost. |
| **egui / egui_term** | Reference only | egui_term uses `alacritty_terminal` under the hood -- useful to study its integration approach. But egui's immediate-mode rendering model limits control over the GPU pipeline we need for transparency, gradients, and 3D compositing. |
| **Raw `vte`** | Too low-level | `vte` is just a parser -- it dispatches escape sequence tokens but provides no grid, no scrollback, no cursor tracking. Building all of that ourselves would duplicate the work already done in `alacritty_terminal` with no clear benefit. |
| **Forking Alacritty wholesale** | Risky | Alacritty is a full application, not a library. Forking means inheriting its renderer, config system, and opinions about window management. Harder to reshape than building on `alacritty_terminal` (the library) with our own rendering layer. |

---

## Crate Structure

A Cargo workspace with focused crates. Start lean and extract further crates as boundaries become clear.

```
mechanic/
├── Cargo.toml                    # Workspace root
├── crates/
│   ├── mechanic-app/             # Binary crate: entry point, event loop, glue
│   │   └── src/
│   │       └── main.rs
│   ├── mechanic-core/            # Terminal state: wraps alacritty_terminal, PTY lifecycle
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── terminal.rs       # Terminal instance (grid + PTY handle)
│   │       ├── pty.rs            # PTY spawning and I/O
│   │       └── event.rs          # Terminal events (bell, title change, etc.)
│   ├── mechanic-renderer/        # GPU pipeline: wgpu setup, text atlas, shaders
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── pipeline.rs       # Render pipeline setup
│   │       ├── text.rs           # Glyph atlas, cosmic-text integration
│   │       ├── grid.rs           # Terminal grid -> vertex buffer
│   │       ├── background.rs     # Background rendering, animated gradient
│   │       └── shaders/
│   │           ├── cell.wgsl     # Cell/glyph rendering shader
│   │           └── background.wgsl
│   └── mechanic-config/          # Configuration, themes, font settings
│       └── src/
│           ├── lib.rs
│           ├── theme.rs          # Color palettes (Stark default)
│           └── font.rs           # Font configuration
```

### Why This Split

- **mechanic-core** owns terminal emulation and PTY I/O. It knows nothing about rendering. This lets us test terminal behavior without a GPU.
- **mechanic-renderer** owns the GPU pipeline and text rendering. It takes a grid snapshot and produces frames. It knows nothing about escape sequences or PTY.
- **mechanic-config** is shared by both -- themes inform rendering, font config informs both text shaping and rendering.
- **mechanic-app** wires everything together: event loop, input dispatch, and lifecycle management.

As the project grows, we anticipate extracting:
- **mechanic-window** -- subwindow management, layout, chrome rendering (Phase 5)
- **mechanic-animation** -- tweening, transition system, timeline (Phase 4-6)

---

## Architecture Overview

```
┌─────────────────────────────────────────────────────────┐
│                    mechanic-app                         │
│  ┌──────────┐  ┌──────────────┐  ┌──────────────────┐  │
│  │  winit   │  │  Event Loop  │  │  Window Manager  │  │
│  │  Window  │──│  & Dispatch  │──│  (future: multi) │  │
│  └──────────┘  └──────┬───────┘  └──────────────────┘  │
│                       │                                 │
│         ┌─────────────┼─────────────┐                   │
│         ▼             ▼             ▼                   │
│  ┌─────────────┐ ┌─────────┐ ┌───────────────┐         │
│  │  Input      │ │  Timer  │ │  Config       │         │
│  │  Handler    │ │  System │ │  (hot reload) │         │
│  └──────┬──────┘ └────┬────┘ └───────────────┘         │
│         │             │                                 │
└─────────┼─────────────┼────────────────────────────────-┘
          │             │
          ▼             ▼
┌─────────────────┐   ┌──────────────────────────────────┐
│  mechanic-core  │   │       mechanic-renderer          │
│                 │   │                                  │
│  ┌───────────┐  │   │  ┌──────────┐  ┌─────────────┐  │
│  │ Terminal  │──┼──▶│  │ Grid     │  │ Text Atlas  │  │
│  │ (alacrty) │  │   │  │ Renderer │  │ (cosmic-txt)│  │
│  └───────────┘  │   │  └──────────┘  └─────────────┘  │
│  ┌───────────┐  │   │  ┌──────────┐  ┌─────────────┐  │
│  │ PTY I/O   │  │   │  │ BG Grad  │  │ wgpu Pipeln │  │
│  └───────────┘  │   │  │ Renderer │  │ & Surface   │  │
│                 │   │  └──────────┘  └─────────────┘  │
└─────────────────┘   └──────────────────────────────────┘
```

**Data flow per frame:**
1. `winit` delivers input events (keyboard, mouse, resize) to the event loop
2. Keyboard input is forwarded to the PTY via `mechanic-core`
3. PTY output is fed to `alacritty_terminal`, which updates grid state
4. The event loop requests a render
5. `mechanic-renderer` reads the grid snapshot, renders glyphs via the text atlas, composites the background gradient, and presents the frame via wgpu

---

## Implementation Phases

### Phase 1: Foundation (Skeleton)

**Goal:** A window opens, a shell spawns, you can type commands and see output. Ugly but functional.

**Deliverables:**
- Cargo workspace with `mechanic-app`, `mechanic-core`, `mechanic-renderer`, `mechanic-config`
- `winit` window on macOS with wgpu Metal surface
- PTY spawning (`/bin/bash` or user's default shell)
- Basic monospace text rendering -- ASCII only, fixed-size glyph atlas, no shaping
- Keyboard input forwarding to PTY
- Raw terminal grid rendering (fixed palette, no themes yet)
- Resize handling (terminal grid + window)

**Key risks:** Getting wgpu surface creation right on macOS; PTY I/O threading model (PTY reads on a background thread, grid updates on main thread).

**Definition of done:** You can open Mechanic, type `ls`, see colored output, and `ctrl-c` a process.

---

### Phase 2: Real Terminal Emulation

**Goal:** Mechanic is a usable daily terminal for ASCII/Latin workflows.

**Deliverables:**
- Full `alacritty_terminal` integration -- CSI sequences, OSC, DEC modes
- ANSI 256-color and truecolor support
- Stark color palette as default theme (`mechanic-config`)
- Berkeley Mono font loading (with fallback to system monospace)
- Scrollback buffer with scroll-up/down
- Selection and copy/paste (macOS pasteboard)
- Cursor rendering (block, beam, underline)
- Basic window chrome -- title bar with terminal title
- Bell notification (visual flash or system sound)

**Definition of done:** You can run `vim`, `htop`, `git log --oneline --graph`, and SSH sessions without visual glitches.

---

### Phase 3: Text Shaping & Multilanguage

**Goal:** Full multilanguage support, including bidirectional text.

**Deliverables:**
- `cosmic-text` integration replacing the basic glyph atlas
- Font fallback chain: Berkeley Mono -> system fonts per script
- Latin-1 extended characters (French, Spanish accents)
- Cyrillic rendering (Russian)
- Arabic text shaping (ligatures, contextual forms)
- Bidirectional text support in the terminal grid
  - This is the hard part: terminal grids are inherently LTR. We need a bidi layer between `alacritty_terminal`'s grid and the renderer that reorders glyphs for display while preserving logical cursor position.
- IME support for multilingual keyboard input on macOS

**Key risks:** Terminal bidi is notoriously underspecified. We should study how other terminals (e.g., Windows Terminal, mlterm) handle it and adopt a pragmatic approach. Perfect bidi in a terminal may not exist -- but good-enough bidi does.

**Definition of done:** You can `cat` a file containing mixed English/Arabic text and see correctly shaped, correctly ordered output. You can type French accented characters via macOS IME.

---

### Phase 4: Transparency & Visual Polish

**Goal:** Mechanic looks like Mechanic -- the Stark aesthetic comes alive.

**Deliverables:**
- Window transparency via `winit` + wgpu alpha compositing
  - Title bar: 95% opacity
  - Content area: 80% base, 95% during activity
- Activity-based opacity animation system
  - Ramp to 95% over 30 seconds of continuous interaction
  - Fade to 80% starting at 30 seconds of inactivity, reaching 80% at 60 seconds
  - Smooth interpolation (ease-in-out curve)
- Animated gradient in lower-right corner
  - Subtle, slow-moving color wash on the background
  - Implemented as a fragment shader in `background.wgsl`
- Custom window decorations (optional -- depends on whether the title bar design requires it)
- Refined color palette:
  - Electric `#52E8FF`, Celeste `#ADFFFF`, Azure `#007FFF`, Blue `#0015FF`
  - Orange/yellow highlight colors for file listings (via LS_COLORS integration)
  - Orange-red for alerts and errors

**Definition of done:** A screenshot of Mechanic looks like a Stark Industries terminal. The opacity fading is smooth and feels natural.

---

### Phase 5: Floating Subwindows

**Goal:** Multiple terminal sessions in floating, draggable panes within the Mechanic window.

**Deliverables:**
- Subwindow manager (likely extracted into `mechanic-window` crate)
  - Each subwindow owns an independent `Terminal` instance + PTY
  - Z-ordering, focus management
  - Drag to reposition, resize handles
- Subwindow chrome: title bar, close/minimize controls
- Keyboard shortcut for new subwindow, switching focus, closing
- Per-subwindow opacity (all subwindows follow the same activity rules independently)
- Animated window spawning (dream feature):
  - New window appears at top of screen at 25% size, 25% opacity
  - Animates to final position at 100% size, standard opacity over 1000ms
  - Ease-out curve for natural deceleration

**Key risks:** Subwindow rendering requires compositing multiple terminal grids into a single wgpu frame. Each subwindow is essentially a render-to-texture target that gets composited onto the main surface. This is the architectural inflection point where we might extract the animation system into its own crate.

**Definition of done:** You can open 3+ floating terminal subwindows, drag them around, and each runs an independent shell session.

---

### Phase 6: 3D Window Carousel

**Goal:** The dream feature -- perspective-based window switching.

**Deliverables:**
- 3D perspective transform for subwindows
  - Each subwindow rendered to a texture
  - On activation (keyboard shortcut), all windows "tilt" 45 degrees toward screen center
  - Windows appear to shrink slightly, creating depth
  - Implemented as a vertex transform in the compositing shader
- Carousel navigation: left/right to cycle through tilted windows
- Text overlay showing window titles during carousel mode
- Smooth transition animations (enter carousel, navigate, exit carousel)
- Exit carousel: selected window "un-tilts" back to full-screen, others fade

**Key risks:** Getting the 3D math right (projection matrix, rotation, perspective divide) and making it feel responsive. Frame budget is tight -- we're rendering N terminal textures plus compositing them with 3D transforms. May need to freeze terminal rendering during carousel and only show static snapshots.

**Definition of done:** The window switching animation looks fluid and cinematic. It's a feature you'd show off to friends.

---

## Cross-Cutting Concerns

### Threading Model

```
Main Thread (winit event loop)
├── Input handling
├── Timer ticks (opacity animation, gradient)
├── Render dispatch
└── Window management

PTY I/O Thread (one per terminal instance)
├── Read PTY output -> send to channel
└── Write keyboard input from channel -> PTY

Render Thread (optional, can start on main)
├── wgpu command encoding
├── Glyph atlas updates
└── Frame presentation
```

`alacritty_terminal`'s grid is updated on the main thread from PTY output received via a channel. This keeps the grid access single-threaded and avoids locks on the hot path.

### Configuration System

- TOML-based config file: `~/.config/mechanic/mechanic.toml`
- Hot-reloadable for theme and font changes
- Sensible defaults baked into the binary (Stark palette, Berkeley Mono with fallback)
- Config schema:
  ```toml
  [font]
  family = "Berkeley Mono"
  size = 14.0
  
  [theme]
  name = "stark"
  
  [window]
  opacity_active = 0.95
  opacity_idle = 0.80
  fade_begin_secs = 30
  fade_end_secs = 60
  
  [shell]
  program = "/bin/zsh"
  ```

### Error Handling Strategy

- PTY failures (shell exit, spawn failure) -> display inline error in the terminal grid, don't crash
- GPU errors (surface lost, device lost) -> attempt recovery, fall back to software rasterization if available
- Font loading failures -> fall back through the chain, always have a last-resort built-in font
- Config parse errors -> warn in stderr, use defaults

### Testing Strategy

- **mechanic-core**: Unit tests for terminal state, PTY mocking, grid assertions
- **mechanic-renderer**: Snapshot tests (render a known grid, compare output pixels) -- initially manual, automated later
- **mechanic-config**: Unit tests for parsing, defaults, validation
- **Integration tests**: Script-driven tests that spawn Mechanic, send input via PTY, and assert terminal state

---

## Dependencies (Expected Cargo.toml)

```toml
# mechanic-core
alacritty_terminal = "0.24"  # terminal emulation
# Note: version should be verified against crates.io at project start

# mechanic-renderer  
wgpu = "24"                  # GPU rendering
cosmic-text = "0.12"         # text shaping and layout
image = "0.25"               # image loading (if needed for textures)

# mechanic-app
winit = "0.30"               # windowing
raw-window-handle = "0.6"    # window handle interop

# mechanic-config
serde = { version = "1", features = ["derive"] }
toml = "0.8"

# Shared
log = "0.4"
env_logger = "0.11"
crossbeam-channel = "0.5"    # PTY <-> main thread communication
```

*Versions are approximate and should be pinned to current releases at project start.*

---

## Open Questions

1. **Custom window decorations vs. native:** macOS title bars fight against custom opacity. We may need `with_decorations(false)` and render our own title bar. This affects Phase 2 and 4.

2. **PTY library:** `alacritty_terminal` bundles PTY support, but we could also use the `portable-pty` crate for more control. Worth evaluating.

3. **Scrollback storage:** In-memory (Alacritty's default) vs. disk-backed for very long sessions. Start with in-memory, revisit if users hit memory pressure.

4. **Config format:** TOML (simple, Rust-native) vs. a more expressive format. TOML is the recommendation unless there's a preference otherwise.

5. **macOS-specific APIs:** For full transparency and vibrancy effects, we may need to call into `NSWindow`/`NSVisualEffectView` via `objc2` crate. This is a Phase 4 decision.
