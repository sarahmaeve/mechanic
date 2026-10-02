# Measurements

The queued PTY transport removes the large-paste stall and completes a duplex
transfer that previously deadlocked. Output batching also improves most tested
GUI workloads. These are initial observations from one machine, not stable
cross-terminal rankings.

## Workspace controls, styling, and loadouts — 2026-10-02

Two-pane renderer preparation with titles/custom outlines averaged **12.25 µs**
idle versus **11.78 µs** plain, and **56.52 µs** versus **55.39 µs** when one pane
changed. Each workload used 100 alternating-order pairs after 20 warmups. Idle
geometry uploads stayed at zero; busy upload bytes were identical. GPU completion,
presentation, and whole-app CPU are outside these measurements.

A styled 16-pane loadout took **9.18 ms** median to save durably and **5.38 µs**
to retrieve from memory, excluding shell/window creation. Saving is explicit;
ordinary automatic workspace persistence still uses its worker.

All **564 workspace tests** passed, plus native docking, palette, control, and
loadout checks. A real GPU device destruction/recovery test retained the original
shell variable and Find match. Strict Clippy, formatting, and release build pass.
[Raw results and scope](baselines/2026-10-02/workspaces/README.md).

## Pane rearrangement and detachment — 2026-10-02

A balanced 16-pane preview-and-move benchmark measured **2.27 µs per operation**
(five batches of 10,000; range 2.06–3.08 µs). It includes candidate geometry
validation and tree mutation; rendering, PTY reflow, and window creation are
excluded. Identical divider/grip/preview state uploads zero geometry bytes.
Hover and preview changes retain the terminal shaping and geometry caches.

All 543 workspace tests passed, plus native gesture/live-detach checks and
Metal divider/grip/preview checks. Tests verify the large-left/two-stacked-right
layout, preview agreement, mouse capture, cancellation, live shell/history
preservation, and stable control handles/completion waiters after the source
window closes. Clippy, formatting, and the release build pass.

Both GUI idle CPU samples lost focus during measurement and are excluded;
no CPU change is claimed. [Raw results and reproduction](baselines/2026-10-02/pane-drag/README.md).

## Session restoration and local control — 2026-10-02

Restoration writes layout/directory snapshots through a coalescing worker.
The control listener blocks on a private Unix socket; completion waits use
shell events and event-loop deadlines. Neither service polls while idle.

Serial GUI checks sampled eight seconds after three seconds of settling,
with animations and render profiling off. Both services were enabled in
temporary directories and verified initialized. All samples remained focused.

| CPU, percent of one core | Before services | Final build, services enabled |
| --- | ---: | ---: |
| Idle | 0.013% | 0.010% |
| Single-cell updates at 20 Hz | 4.335% | 4.103% |

An earlier enabled run measured 0.012% idle and 4.377% updating; the new build
with services disabled measured 0.012% idle. These short samples show no large
regression; they do not establish a speedup. CPU excludes the PTY peer,
WindowServer, and GPU work.

| Service operation | Median | p95 |
| --- | ---: | ---: |
| Encode a 16-pane snapshot | 3.67 µs | 5.96 µs |
| Atomic durable save + worker flush | 8.00 ms | 8.14 ms |
| Authenticated socket round trip | 77.96 µs | 113.04 µs |

The transport benchmark returns an empty list immediately; it excludes GUI
dispatch, output extraction, and shell execution. Save timings include disk
synchronization; ordinary UI changes queue saves asynchronously.

Native checks restore two windows/four fresh PTYs and verify split ratios,
font sizes, active panes, directories, control input/output, pending and retained
completion waits, stale identifiers, focus preservation, and silent-input
viewport redraw. Storage/transport tests cover malformed state, private files,
writer locks, bounded reads/writes, Unicode output, and saturated input queues.
All 531 workspace tests passed, along with both hidden native smoke checks,
strict workspace Clippy, formatting, and the release build.

[Raw measurements, build hashes, and reproduction commands](baselines/2026-10-02/sessions-control/README.md).

## Split panes, directory inheritance and completion notifications — 2026-10-02

Panes share one surface, device and glyph atlas per window. Each retains its
own shaped rows, geometry and converted grid. The same parser queue schedules
one bounded batch globally per turn; idle panes add no polling or animation
timers. Native completion notifications default off and run on shell events.

The offscreen Metal preparation benchmark holds 4,800 cells constant and
changes one cell per active pane. Medians use 100 observations after 20 warmups
per case, in one serial run. Preparation includes shaping/atlas work, geometry
and queued uploads; it excludes GPU synchronization and presentation.

| Panes | One pane changing | Every pane changing | Vertex bytes, one pane changing |
| --- | ---: | ---: | ---: |
| 1 | 81.71 µs | 83.33 µs | 21,120 |
| 2 | 57.58 µs | 84.46 µs | 10,560 |
| 4 | 46.73 µs | 92.54 µs | 5,280 |

Unchanged panes retained their row identities and required no vertex uploads.
With smaller panes, a single changed row costs less to rebuild. These timings
do not measure simultaneous shell throughput or input latency. A separate
16-pane layout plus 16 hit tests measured 721 ns median per iteration across
five batches of 20,000 (batch range 552–1,205 ns).

Mechanic-only GUI CPU checks compare `7745f09` with this change, one three-second
sample after two seconds of settling. Animations were off; all samples stayed
focused. The single-cell workload updates at 20 Hz in a 121×42 grid, with
render profiling enabled in both builds and 60 complete frames each.

| CPU, percent of one core | Before | After |
| --- | ---: | ---: |
| Idle | 0.007% | 0.008% |
| Single-cell updates | 4.56% | 4.77% |

These short samples are sanity checks, not precise regression thresholds.
CPU excludes the controlled PTY peer, WindowServer and GPU work. A serial
2,000-command marker benchmark including completion-event tracking measured
0.657 ms plain and 1.511 ms with markers, about 427 ns additional per lifecycle;
shell execution and native notification delivery are excluded.

Validation: workspace tests and final affected-package tests total 484 passed;
12 Metal functional checks, the hidden app smoke, strict workspace Clippy,
formatting and release build passed. Checks include directory fallback and
independent PTYs, tiny/nested layouts, clipping, atlas growth, idle cache reuse,
bidi mapping, mouse capture across focus changes, logical shortcuts on non-US
layouts, IME cancellation, and callback rejection after close/restart or reused
window IDs. Native notification content/delegate checks sent no alerts and
requested no authorization; actual OS banner delivery was not tested.

[Raw samples, commands and build fingerprints](baselines/2026-10-02/panes/metadata.txt).

## Shell integration and scrollback search — 2026-10-02

Shell integration adds bounded OSC 7/133 metadata to the existing parser.
It does not add a second parser or an idle timer. Automatic zsh startup hooks
run in child-only configuration; user startup files remain unchanged.

Matched release PTY flood runs compare commit `6e5998d` with shell integration.
Each case sends 32 MiB, with one warmup and three measured samples per build;
before ran first, then after, without concurrent builds or benchmarks.

| Metric | Before | After |
| --- | ---: | ---: |
| ASCII total drain, median | 221.80 ms | 217.85 ms |
| SGR total drain, median | 220.94 ms | 218.28 ms |
| Busy parser call p95, both cases | 4.134 ms | 4.163 ms |
| Maximum parser call | 4.264 ms | 4.294 ms |

All samples verified final-output-before-exit ordering. These short runs show
similar responsiveness under the existing soft 4 ms parser budget; they do not
measure GUI latency or idle CPU. The flood emits no shell markers.

A separate 2,000-command parser test compares identical visible output with
and without four OSC 133 markers per command. Ten batches per variant measured
upper medians of 0.961 ms plain and 2.078 ms with markers: about 559 ns additional
work per command in this run. Metadata limits, exit status and final copied
output were checked. Shell hook execution, startup and rendering are excluded.

Final shell compatibility checks added `ERR_RETURN` coverage and verified
Unicode output, status 42, Ctrl+C status 130 and subsequent prompt navigation
through a real zsh PTY. The wrapper preserves status in user prompt hooks and
`%?`; native window titles update only when their text changes. All 442 current
workspace tests, strict Clippy and the release build passed. A final serial
marker benchmark measured 1.100 ms plain versus 2.149 ms with markers for 2,000
commands, about 524 ns added per lifecycle. This is another short parser-only
observation, not a measured improvement over the earlier run.

Search timings use 10,000 populated 120-column rows plus the trailing blank row,
nine samples per fixture and budget. Terminal population is outside timing.
Full scans use the default two-million-cell limit; restricted scans examine
the newest 120,000 cells and explicitly report partial results.

| Search text | Full-history median | Restricted median |
| --- | ---: | ---: |
| ASCII | 7.36 ms | 0.71 ms |
| Accented Latin | 7.64 ms | 0.73 ms |
| Chinese/Japanese | 7.02 ms | 0.70 ms |
| Arabic | 9.10 ms | 0.90 ms |

Each full scan returned exactly 10,000 matches; each restricted scan returned
999. Queries run on user input. Streaming output invalidates results without
rescanning; Return refreshes them. UI rendering and window-system work are
outside search timings. No other terminal was launched for these measurements.

[Raw observations and build metadata](baselines/2026-10-02/shell-search/metadata.txt).

Validation: 446 workspace tests, 187 vendored parser/terminal unit tests, nine
offscreen Metal checks, hidden native Find-panel smoke, strict Clippy and a
release build passed. Real zsh/PTY tests cover startup hooks, user exit status,
Unicode cwd, copied output and prompt navigation. Search tests cover Russian,
Ukrainian, Japanese, Chinese, Arabic, French, German, Spanish, Portuguese and
Italian at four wrap widths, hard breaks, case matching and original-text
extraction; application tests cover highlighting, selection, cursor and stale
results. These initial timings predate canonical accent normalization and full
Unicode case folding; the German-search follow-up below measures that change.

## German and canonical Unicode search — 2026-10-02

Find now uses canonical caseless matching (NFD → full case fold → NFD).
`Straße`, `STRASSE` and `STRAẞE` match; composed/decomposed umlauts match too.
Accents remain significant, so `Müller` differs from `Muller` and `Mueller`.
Matches cover complete source cells: `ss` matches `ß`, but `s` does not match
half of it. The native Match case control selects exact stored-text matching.
Original spelling, selection coordinates and copied text remain unchanged.

This follows [Unicode canonical caseless matching, D145](https://www.unicode.org/versions/Unicode17.0.0/core-spec/chapter-3/#G53523)
and the full mappings for `ß`/`ẞ` in [CaseFolding.txt](https://www.unicode.org/Public/17.0.0/ucd/CaseFolding.txt).
[ICU German phonebook tailoring](https://unicode-org.github.io/icu/userguide/collation/concepts.html#expansions)
can additionally equate umlauts with two-letter spellings; Find does not apply
that locale-specific behavior. The libraries are `caseless` 0.2.2 (Unicode 16
folding tables) and `unicode-normalization` 0.1.25 (Unicode 17 normalization).
This does not claim Unicode 17 case-fold coverage for newly added characters.

The unchanged 10,000-row harness ran before/after, then after/before, with no
concurrent builds or benchmarks. Each cell below is the median of 18 samples.
Full scans retained 10,000 matches; restricted scans retained 999 and reported
partial results. Population, rendering and native UI work are outside timing.

| Search text | Full before → after | Restricted before → after |
| --- | ---: | ---: |
| ASCII | 7.28 → 8.17 ms | 0.74 → 0.79 ms |
| Accented Latin | 7.65 → 8.22 ms | 0.77 → 0.83 ms |
| Chinese/Japanese | 6.99 → 8.71 ms | 0.69 → 0.87 ms |
| Arabic | 9.21 → 11.55 ms | 0.92 → 1.20 ms |

Correct matching adds measurable cost, including source-boundary checks for
ASCII and normalization for other scripts. These searches run on user input;
the change adds no idle work or continuous rescanning. Short local runs are
not a GUI latency guarantee. [Raw samples and metadata](baselines/2026-10-02/german-search/metadata.txt).

Validation passed: workspace tests, 16 core search tests including overlapping
fold expansions and rejected partial matches, strict Clippy, formatting and
release build. German checks cover key translation, copy after reflow/scroll,
search highlighting, exact mode, styled glyphs, cursor mapping and equivalent
composed/decomposed rendering. Hidden AppKit checks cover Match case toggles
without resetting the query or editor selection. No physical German keyboard
layout or interactive IME session was tested.

## Font fallback and mixed-direction correctness — 2026-10-02

Font fallback now removes absent family names and exact duplicates from each
configured/platform list at renderer creation. All installed family aliases
and preference order are preserved. Full-paragraph shaping and bidi remain.

Stronger benchmark assertions exposed a separate Cosmic Text 0.19 bug:
numeric prefixes before Arabic/Hebrew could disappear in its no-wrap layout.
Mechanic now uses word layout with unbounded dimensions, retaining every span
without inserting line breaks. Both final benchmark builds include this
correctness repair; the comparison below isolates fallback pruning.

Fresh counter edits, 121×42, median shaping time:

| Paragraph | Before | After | Change |
| --- | ---: | ---: | ---: |
| Arabic | 8.77 ms | 7.22 ms | −18% |
| Chinese | 38.40 ms | 26.78 ms | −30% |
| Japanese | 25.75 ms | 21.13 ms | −18% |
| Korean | 2.79 ms | 2.75 ms | −2% |

One final batch per build, 200 distinct edits per case after 20 warmups and
100 ms CPU warmup. An earlier independent pair also showed gains for Arabic,
Chinese and Japanese, with Korean essentially unchanged. The small Korean
difference does not establish a useful improvement.

The expanded matrix covers 90 combinations: three viewport sizes, five text
fixtures, three update modes, and hard rows versus soft wrapping. Each case has
eight warmups and 24 samples. Four serial batches ran before/after/after/before,
giving 48 observations per build/case. At 121×42, wrapped Arabic shaping improved
18% and mixed CJK 21–22% across sparse edits, dense edits and scrolling. Latin,
Cyrillic and hidden-RTL-context workloads were mostly unchanged. Improvements
are not universal: the 40×12 accented-Latin sparse wrapped case rose from
305 to 331 µs (+8.5%), and several small cached cases rose about 0.2–1.3 µs.
The initial large hidden-RTL slowdown did not recur in either revised batch.

Release, Menlo 16 pt, scale 1, Rust 1.99.0, Apple M5 Pro/Metal,
macOS 26.6.2 arm64, unchanged host font database. Timings cover conversion,
shaping and atlas CPU preparation/enqueue; parsing, instance construction,
GPU execution, presentation and application CPU are excluded. These results
do not establish interactive latency or a comparison with another terminal.

The revised scroll fixtures use an eight-step full-viewport cycle and distinct
phrase IDs. Assertions require changed visible content, converted counters,
sparse counter glyphs and retained offscreen context. Initial fixtures had
unequal scroll cycles, repeated viewports, and missing numeric glyphs; their
retained observations are exploratory and excluded from the final matrix.

[Fresh-edit summary](baselines/2026-10-02/font-fallback/scripts-final-summary.csv),
[matrix medians and p95](baselines/2026-10-02/font-fallback/matrix-summary.csv),
and [build/run metadata](baselines/2026-10-02/font-fallback/metadata.txt).
Raw samples are adjacent. Percentiles use nearest rank; total times are
per-sample sums before aggregation.

Validation: 411 workspace/all-target/all-feature tests and nine explicit serial
offscreen Metal tests passed, along with strict Clippy, formatting and the
release workspace build. Differential tests compare exact font/glyph keys,
float geometry, clusters and bidi hit mappings using cloned font databases,
across primary/fallback fonts, absent/duplicate names, aliases, accents,
Cyrillic/CJK/Arabic/Hebrew/Indic/Thai text, controls, styles and wrap widths.
The numeric-prefix regression failed before the repair and now passes CPU and
pixel checks. Existing joining, viewport clipping, atlas-growth, cursor and
cached-versus-full geometry checks also pass. Independent review found no
remaining material issues in these changes.

## Frame preparation — 2026-10-02

Shaping-cache hits now borrow visible cells instead of allocating text keys.
The cache and eviction queue share each owned key; their existing entry and
payload limits remain unchanged. Single-row shaping also skips a temporary
row-slice allocation.

Pooled release medians in microseconds; totals are per-sample sums of conversion,
shaping and atlas preparation:

| Update | Shaping before | Shaping after | Total before | Total after |
| --- | ---: | ---: | ---: | ---: |
| One ASCII cell | 50.25 | 37.08 | 89.87 | 76.58 |
| One ASCII row | 50.00 | 37.33 | 89.54 | 76.96 |
| Full ASCII grid | 50.92 | 37.54 | 169.58 | 158.79 |
| Scroll | 50.25 | 37.58 | 172.87 | 166.17 |
| One multilingual cell, separate rows | 50.50 | 38.58 | 90.21 | 78.25 |
| One Arabic cell, 42 wrapped rows | 8922.29 | 8815.88 | 9089.00 | 8984.08 |

Cached shaping improved 24–26%; the three sparse workloads improved 13–15%
across these stages. Conversion remains about 37–39 µs. Dense atlas preparation
still scans changed glyphs: full ASCII was 81→83 µs and scrolling 85→91 µs;
the combined totals improved despite that variation. The approximately 1%
Arabic difference does not establish a speedup. Its two large paragraph states
do not remain cached together, so contextual shaping remains the main cost.

121×42 cells, Menlo 16 pt, scale 1, Rust 1.99.0, wgpu 30.0.1/Metal,
macOS 26.6.2 arm64. Three batches per build, 200 samples per workload per batch,
after 20 warmups and 100 ms additional warmup. Batch order was before/after,
after/before, before/after; no concurrent builds or benchmark runs. Baseline
production code is `33b795d`, using the same stage harness. The workloads
alternate two states, with mutations and visible-change assertions outside
timing. These results exclude parsing, geometry construction, GPU execution,
presentation and final snapshot destruction; they do not measure application
CPU or comparisons with other terminals.

[Raw batches and per-batch/pooled summary](baselines/2026-10-02/frame-preparation/summary.csv).
The six raw CSV files are adjacent to the summary. Summary percentiles use
nearest rank; each pooled group contains 600 samples.

Validation: 395 workspace tests, eight explicit serial offscreen Metal checks,
strict Clippy, formatting and release app build passed. Cache checks cover
multilingual text, combining marks, style/hidden flags, color overlays, paragraph
boundaries and context, fresh-shaping equivalence, and eviction limits.

### Wrapped CJK follow-up

Adding Chinese, Japanese and Korean exposes a broader paragraph-shaping cost.
All wrapped non-ASCII paragraphs use the same paragraph-level cache. Median
shaping milliseconds in the optimized build:

| Script | Alternating two states | Fresh counter edits |
| --- | ---: | ---: |
| Arabic | 8.844 | 8.935 |
| Chinese | 0.039 | 39.830 |
| Japanese | 0.043 | 26.980 |
| Korean | 0.040 | 2.738 |

The CJK alternating states remain cached; fresh edits expose full-paragraph
shaping. Arabic is therefore not the general performance bottleneck: this
fixture's Chinese and Japanese cache misses cost substantially more. The
earlier mixed-language hard-row test did not exercise this path.

Same grid, font, scale, warmups and 200 samples as above; one batch per mode.
The long paragraphs extend into history. CJK source display widths approximately
match Arabic's, with additional padding at odd-width wraps. Fresh edits advance
a three-digit counter in row 21 (fullwidth digits in CJK); usually one character
changes, with carries changing two or three. The text and selected fallback
fonts differ. These observations establish a shared performance problem, not
an inherent ranking of languages or typical keystroke-to-display latency.

[Alternating raw samples](baselines/2026-10-02/frame-preparation/scripts.csv) /
[fresh-edit raw samples](baselines/2026-10-02/frame-preparation/scripts-novel.csv).
Both offscreen benchmark runs passed their visible-update and row-count
assertions; strict app Clippy and formatting passed. Production code is unchanged
from the cache optimization above.

### Native UI investigation

Mechanic completed the 20 Hz GUI probe with stable focus and dimensions. CPU
percent of one core, one three-second sample per case after warmup:

| Script | Explicit rows | Long wrapped paragraph |
| --- | ---: | ---: |
| Arabic | 5.27% | 37.95% |
| Chinese | 8.00% | 71.56% |
| Japanese | 7.11% | 60.37% |
| Korean | 4.55% | 20.89% |

Idle was 0.03%; wrapped ASCII was 25.80%. Menlo 14 pt, 121×42, opaque window,
logo and animations disabled. Each case has 20 warmup counter edits and 60
measured edits at 50 ms intervals. Wrapped text extends into history; explicit
rows end with hard newlines. Process CPU includes renderer/driver work but
excludes the peer, WindowServer and GPU. This is a single exploratory run;
cursor replies do not establish frame completion. Raw samples:
[mechanic.jsonl](baselines/2026-10-02/paragraph-ui/mechanic.jsonl).

**No valid Ghostty CPU comparison was obtained.** Ghostty 1.3.1 attempts were
rejected for process identification failures or changing window dimensions.
The final attempt remained focused but changed from 121×39 to 121×36 during
sampling. Its cause is unresolved; it is not attributed to user interaction.
All attempts are retained beside the Mechanic result and excluded from rankings.

Version-matched source shows a useful architectural difference: Ghostty
[skips unchanged rows](https://github.com/ghostty-org/ghostty/blob/v1.3.1/src/renderer/generic.zig#L2417)
and [keeps shaping runs within a row](https://github.com/ghostty-org/ghostty/blob/v1.3.1/src/font/shaper/run.zig#L10).
Its macOS CoreText shaper also
[forces left-to-right embedding](https://github.com/ghostty-org/ghostty/blob/v1.3.1/src/font/shaper/coretext.zig#L175),
so its Arabic work is not equivalent to Mechanic's paragraph bidi support.
This supports investigating row reuse while preserving paragraph context;
it does not establish a measured performance ratio against Ghostty.

## Hyperlink lookup — 2026-10-02

Release-build median lookup times in nanoseconds:

| Grid | Linked cell | Unlinked cell |
| --- | ---: | ---: |
| 80×24 | 3.17 | 1.98 |
| 160×100 | 3.16 | 1.98 |

Seven samples of one million lookups after 100 ms warmup per case; case order
alternates. The 619-byte URI is verified before timing. This measures direct
grid lookup and shared metadata cloning, not complete hover handling. Bidi
coordinate mapping, validation, native tooltip work and rendering are excluded.
No whole-app CPU improvement is claimed.
[Raw CSV](baselines/2026-10-02/hyperlink-lookup/stabilized.csv).

Validation: 403 workspace tests, strict Clippy and release app build passed.
The explicit native AppKit check passed tooltip copying/clearing and Open/Copy
action delivery. Browser launch was stubbed; interactive popup placement was
coordinate-tested, not manually clicked. No browser or clipboard effects occurred.

## Surface recovery — 2026-10-02

All 12 native surface replacements resumed cached presentation and retained the
atlas generation. Median host-call times: ordinary cached render 0.968 ms,
surface recreation/configuration 0.873 ms, recovered cached render 0.591 ms.
These are paired observations in the same build, not a before/after speedup.
Driver waits affect both render timings; GPU completion is not measured.

Release build, Rust 1.99.0, wgpu 30.0.1/Metal, macOS 26.6.2 arm64. Temporary
320×160 window, Menlo 16 pt at scale 1, 16×4 ASCII grid, animations and logo
disabled; three full-frame warmups and 33 ms between pairs. The benchmark
explicitly replaces healthy surfaces using the production recovery method;
unit tests check that a `Lost` result dispatches that method. Device loss is
not covered. [Raw CSV](baselines/2026-10-02/surface-recovery/redraw-paired.csv).

## Arabic wrap repair — 2026-10-02

Median milliseconds per uncached 80×24 paragraph shape, before/after joining
and glyph-origin repairs:

| Workload | Before | After | Change |
| --- | ---: | ---: | ---: |
| Arabic | 1.660 | 1.713 | +3.2% |
| Arabic / Latin / Cyrillic | 1.280 | 1.335 | +4.3% |
| Arabic with offscreen context | 1.754 | 1.720 | −2.0% |
| ASCII through contextual shaper | 0.552 | 0.560 | +1.4% |

The first two cases cost about 53–55 µs more per full paragraph shape. This
benchmark bypasses the row/paragraph caches and normal ASCII fast path; it
does not measure application CPU, rasterization or presentation. These are
single sequential before/after batches, so small differences can include drift.

Same machine/toolchain as the recovery check. Menlo 16 px with configured
fallback, 24 px line height; fixed terminal metrics 10×24 px and 18 px ascent.
Each workload has 20 warmup shapes, then seven samples of 20 changing shapes.
Raw CSV glyph counts include invisible controls and are not correctness proof.
[Before](baselines/2026-10-02/wrapped-shaping/before.csv) /
[after](baselines/2026-10-02/wrapped-shaping/after.csv).

Correctness checks cover split lam–alef, joining/nonjoining controls, combining
marks, style boundaries and multi-cell wraps. Full and clipped viewport glyph
geometry match; the offscreen Metal check compares isolated rendered rows.
Logical copy order remains unchanged.

The release row-cache benchmark also passed all 800 geometry comparisons after
these repairs. Median sparse geometry time was 9.5–9.9 µs cached versus 193 µs
for full construction, with 21,296 versus 894,432 planned upload bytes. Dense
updates retained about 10% cache overhead (200–204 µs versus 181–185 µs).
This checks existing cache behavior, not a before/after shaping comparison.
[Raw geometry CSV](baselines/2026-10-02/row-cache/geometry.csv).

Validation: 384 workspace tests, nine explicit serial Metal checks, strict
Clippy, formatting and the release app build passed. The native recovery
example and CPU shaping benchmark were run separately.

## Paste responsiveness

Release build, 1 MiB payload, one warmup and five measured samples per case.
The delayed reader waits 500 ms. Median milliseconds:

| Measurement | Before | After |
| --- | ---: | ---: |
| consuming-paste-call | 49.230 | 0.288 |
| consuming-verified-delivery | 50.417 | 54.124 |
| delayed-reader-paste-call | 552.657 | 0.334 |
| delayed-reader-verified-delivery | 553.829 | 587.800 |

All ordinary samples, including warmups, verified exact payload bytes, paste
markers and the following input probe in order. Paste calls return promptly;
verified delivery was 6–7% slower in this batch. The improvement is removal of
caller blocking, not faster paste delivery.

The duplex peer writes 8 MiB before reading the paste. With a three-second
watchdog, both baseline observations failed (warmup and measured sample);
the measured paste call took 3603.820 ms before failure.
Both final observations passed: the measured paste call took 0.336 ms and
verified delivery took 645.781 ms. One measured duplex sample demonstrates
completion, not a latency distribution. These tests exercise Mechanic's core
transport, not iTerm2 or Ghostty's clipboard handling.

## GUI output and replies

Nearest-rank median milliseconds, as reported by the runner; lower is faster.
Output cases send at least 4 MiB and
wait for a cursor-position reply. Each has one warmup and five samples;
query-roundtrip has 100 measured one-character updates.

| Case | Mechanic before | Mechanic after | iTerm2 | Ghostty |
| --- | ---: | ---: | ---: | ---: |
| ascii | 99.963 | 35.775 | 61.741 | 33.228 |
| wrap | 101.605 | 41.782 | 49.825 | 32.007 |
| unicode | 118.417 | 42.975 | 1086.097 | 33.290 |
| sgr | 117.105 | 36.101 | 146.903 | 35.910 |
| scrollback | 66.508 | 35.754 | 45.201 | 33.847 |
| scroll-region | 29.936 | 32.001 | 2264.064 | 87.936 |
| repaint | 101.670 | 35.794 | 52.604 | 32.144 |
| sparse | 33.757 | 32.612 | 277.122 | 37.268 |
| query-roundtrip | 16.683 | 8.309 | 20.167 | 0.013 |

Same macOS 26.6.2 arm64 machine, 121×42 cells, Menlo 14 pt, opacity 1,
release runner. Mechanic runs in quiet mode. iTerm2 is 3.6.11; Ghostty is 1.3.1.
Mechanic 0.1.0 before/after refers to the PTY transport change, after the
dependency migration and terminal query-reply fix. Both Mechanic runs use the
same preserved terminal-workload executable. Benchmarks ran sequentially.

Mechanic and iTerm2 retain 10,000 history lines; Ghostty's limit is 10,000,000
bytes. These are different capacities, so the scrollback row is approximate.
Environment version fields may be inherited from the launching shell; the
labels and notes identify the actual terminal.

DSR timing measures delivery, parsing and reply scheduling, not presentation
completion or keyboard-to-pixel latency. Mechanic's Unicode and rendering
support remain incomplete, so timings do not establish equivalent visual
results. This is one batch per terminal without rotated repeat runs; display
scheduling and background activity can affect results. With five samples,
nearest-rank p95 is the maximum. Scroll-region was about 7% slower after the
change; sparse output was similar.

## Reproduce and inspect

[Raw JSON](baselines/2026-10-01/) contains the eight before/after and comparison
reports plus the initial parser-only core baseline. Each report retains raw
samples and workload settings. See [runner instructions](README.md) for new
runs; output files are never overwritten.

```sh
./target/release/mechanic-bench compare \
  benchmarks/baselines/2026-10-01/mechanic-before-pty.json \
  benchmarks/baselines/2026-10-01/mechanic-after-pty.json \
  benchmarks/baselines/2026-10-01/iterm2-before-pty.json \
  benchmarks/baselines/2026-10-01/ghostty-before-pty.json
./target/release/mechanic-bench compare \
  benchmarks/baselines/2026-10-01/paste-before.json \
  benchmarks/baselines/2026-10-01/paste-after.json
```

The comparator rejects the failed duplex baseline; inspect its raw
`paste_samples` for the timeout/error observations.

Transport change validation: 303 workspace tests, strict Clippy, formatting,
and release builds passed.

## Bounded parsing follow-up

Parsing now yields after a soft 4 ms budget, checked between at most 64 KiB
chunks, with a hard limit of 4 MiB / 64 chunks per call. At least one chunk is
processed. A budget yield requests another redraw, including when the
producer has stopped sending wakes. Final output drains before exit/error
delivery. This limits parsing work, not total frame time: one chunk, protocol
replies, rendering and OS scheduling can exceed the time budget.

The same Rust `parse_responsiveness` example was built against the preserved
pre-budget core and updated core. Each case streams at least 32 MiB, with a
100 ms prefill and 1 ms yields between calls; one warmup and three measured
samples per case. All eight observations per build verified the final marker
and title before exit, including warmups. This does not verify every byte or
measure GUI input latency.

| Flood measurement (ms) | Before | After |
| --- | ---: | ---: |
| Longest parsing call, both cases | 16.970 | 4.288 |
| P95 call that parsed output, both cases | 1.360 | 4.139 |
| ASCII median total drain | 220.237 | 210.613 |
| SGR median total drain | 221.194 | 208.035 |

The maximum call fell; p95 increased as work was redistributed into bounded
calls. Calls and batch sizes differ, so these are scheduling observations,
not a uniform speedup. Total drain timings include the example's yields.

Fresh GUI before/after runs used the same preserved terminal-workload runner
and 121×42 settings as above. Median milliseconds:

| Case | Before budget | After budget |
| --- | ---: | ---: |
| ascii | 35.558 | 36.015 |
| wrap | 35.636 | 36.630 |
| unicode | 41.685 | 36.045 |
| sgr | 36.305 | 36.488 |
| scrollback | 35.468 | 43.425 |
| scroll-region | 32.243 | 59.655 |
| repaint | 35.720 | 36.504 |
| sparse | 33.651 | 62.095 |
| query-roundtrip | 8.320 | 8.320 |

Most cases changed little. Scrollback was 22% slower; scroll-region and sparse
updates were about 85% slower. Parser continuation currently waits for another
redraw, so throughput can pay for extra presentation turns. At this stage the
4 ms budget favors shorter UI parsing stalls; the next section measures the
subsequent scheduling change. No keyboard-to-pixel result is claimed.

Raw reports: [call durations before](baselines/2026-10-01/parse-calls-before.json),
[after](baselines/2026-10-01/parse-calls-after.json),
[GUI before](baselines/2026-10-01/mechanic-parse-before.json), and
[after](baselines/2026-10-01/mechanic-parse-after.json).
The ordinary `compare` command accepts the GUI pair; flood reports use a
separate schema with raw call observations.

Validation after this follow-up: 309 workspace tests, strict Clippy, formatting,
and release builds pass. Tests cover continuation without a producer wake,
split UTF-8/escape sequences, expired budgets, the byte cap, and final-output /
error / exit ordering.

## Parsing independent of presentation

One pending window now parses per event-loop turn, rotating fairly through a
deduplicated queue. Redraws only render the current grid. The existing soft
4 ms / hard 4 MiB parser limits are unchanged. Content presentation uses a
16 ms deadline, animations a 33 ms deadline; hidden windows keep parsing but
suspend rendering. Focus glow receives its final frame before becoming idle.

A fresh GUI pair used the same runner and settings as the preceding tests.
Median milliseconds:

| Case | Parsing on redraw | Independent parsing |
| --- | ---: | ---: |
| ascii | 35.988 | 32.405 |
| wrap | 36.753 | 32.217 |
| unicode | 36.575 | 32.506 |
| sgr | 36.514 | 32.739 |
| scrollback | 42.299 | 33.756 |
| scroll-region | 60.129 | 31.700 |
| repaint | 36.254 | 32.218 |
| sparse | 61.845 | 31.448 |
| query-roundtrip | 8.344 | 0.018 |

The two cases slowed by the parsing budget recover their earlier throughput.
Query replies no longer wait for presentation. These remain DSR completion
measurements, not visible frame latency. Explicit input/resize redraws remain
immediate; paced PTY updates can wait for their 16 ms deadline plus rendering.

### Animation CPU

`--animate` enables the existing gradient and logo effects while focused;
`--hot-cpu` remains an alias. Continuous effects are opt-in. Animation redraws
now honor their deadline, use cached terminal instances, and stop while hidden.

The Rust `app_cpu` example sampled app-process CPU for five seconds after a
three-second settle, with an idle PTY peer, Menlo 14 pt and opaque content.
All four accepted samples remained focused. Percent of one CPU core:

| Mode | Before scheduling change | After |
| --- | ---: | ---: |
| Focused continuous effects | 13.746% | 3.966% |
| Focused idle | 2.815% | 2.984% |

Continuous effects used about 71% less app CPU in this small comparison.
Idle CPU was similar, around 3%; the change does not eliminate native/runtime
background work. These are single samples on this machine, excluding the
peer, WindowServer, GPU use and energy. Actual window dimensions were not
instrumented. No general battery-life or GPU-cost claim is made.

The animated reports are corrected versions of initially valid focused
samples: native `proc_pid_rusage` counters had been mislabeled as nanoseconds.
Raw counter values and elapsed times are preserved. The host's Mach timebase
is 125/3; a 250 ms CPU probe matched POSIX `getrusage` after conversion
(249.9085 ms versus 249.909 ms). Conversion tests now cover fractional scaling
and overflow. Later focus-interrupted attempts were excluded.

Raw [GUI before](baselines/2026-10-01/mechanic-decoupled-before.json) /
[after](baselines/2026-10-01/mechanic-decoupled-after.json),
[animated CPU before](baselines/2026-10-01/cpu-animated-before-corrected.json) /
[after](baselines/2026-10-01/cpu-animated-after-corrected.json), and
[idle CPU before](baselines/2026-10-01/cpu-idle-before.json) /
[after](baselines/2026-10-01/cpu-idle-after.json) retain the observations.

Validation: 313 workspace tests plus three CPU-conversion tests pass, with
strict Clippy, formatting and release builds. Scheduler tests cover fairness,
continuation without presentation, fixed deadlines, pending redraws,
occlusion/restoration, and the final bloom frame. Multi-window native input
latency and GPU frame timing were not measured.

## Unicode, cursor and atlas correctness

The renderer now keeps combining marks, shapes Arabic in context, resolves bidi
across soft-wrapped rows, and preserves logical copy order. Connected RTL words
retain their relative glyph positions; horizontal transforms rasterize from font
outlines. Japanese wide cells retain both backgrounds. Hidden cursors stay hidden,
bar/underline geometry is honored, and wide hollow cursors have one outline.
Atlas growth finishes before instance construction, checks device limits and
preserves all active glyphs. Configured font fallbacks now apply.

The [offscreen GPU fixture](baselines/2026-10-01/unicode/text-fixture.png) covers
Russian, Ukrainian, Japanese, Arabic, French, German, Spanish, Portuguese and
Italian, including decomposed accents and original Arabic news-style prose.
It uses Menlo 14 pt at 2× scale; Arabic resolves to installed Courier New.
The image comes from GPU readback, not screen capture. Word wrapping still follows
terminal cell boundaries, and joining stops at those boundaries.

Stage profiling used 121×42 cells, Menlo 14 pt, opaque content, a controlled Rust
PTY peer updating at 20 Hz, and release builds. Each sample lasted five seconds.
CPU percentages mean one process CPU core, excluding the peer, WindowServer and
GPU. Logs are enabled for both sides and affect CPU totals. These are single-run
observations, not latency or energy claims.

The initial correctness implementation increased unfocused ASCII output CPU from
about 5–6% to 7–8%. Profiling found repeated atlas residency checks on unchanged
rows. The resulting frame cache skips those rows, checks changed rows, and falls
back to full-frame preflight on a miss or atlas generation change.

Matched unfocused `cell` samples, with five-second settling and five-second sampling:

| Measurement | Correctness build before frame cache | With frame cache |
| --- | ---: | ---: |
| App CPU, one core | 7.675% | 6.590% |
| Shaping/atlas median | 0.761 ms | 0.235 ms |
| Shaping/atlas p95 | 0.831 ms | 0.303 ms |
| Complete presented frames | 100 | 100 |

The final focused samples below are a separate series. Do not compare their CPU
values directly with the unfocused baseline. Shaping/atlas and instance columns
are median host time; upload includes host writes/allocation, not GPU completion.

| Workload | App CPU | Shaping/atlas | Instances | Upload |
| --- | ---: | ---: | ---: | ---: |
| Cell | 6.242% | 0.236 ms | 0.792 ms | 0.497 ms |
| Row | 6.325% | 0.216 ms | 0.792 ms | 0.496 ms |
| Full repaint | 8.023% | 0.658 ms | 0.785 ms | 0.507 ms |
| Scroll | 6.861% | 0.644 ms | 0.791 ms | 0.461 ms |
| Multilingual fixture + changing status cell | 5.368% | 0.191 ms | 0.254 ms | 0.427 ms |

The multilingual sample measures steady display with a changing status cell; it
does not measure continuously reshaping new Arabic paragraphs. Unfocused idle
samples were 0.014% before these changes and 0.084% afterward, with no content
frames during the latter sample. They are too small and brief to infer a useful
idle regression. Text rendering adds no idle timer. Final atlas CPU attempts had
focus transitions and are inconclusive; the explicit atlas growth tests passed.

Full-grid instance construction remains the largest measured preparation stage
in ASCII workloads. Partial conversion/instance uploads are not implemented here.

[Raw observations and stage distributions](baselines/2026-10-01/unicode/summary.json)
include focus, dimensions, frame counts, medians and p95. The matched cache pair is
`unicode-final-cell-r2.json` / `unicode-cache-pair-after.json`; other files preserve
the original text path, the initial correctness build and the focused cached runs.
Rejected focus-transition attempts remain in the ignored results directory; one
final rejected atlas sample is retained beside the accepted observations.

Release binary SHA-256:

- Original text path plus instrumentation: `4e828c951b10779b47b08ca0422b994f76e98993028ed70dffc0b0a08f4ba71b`.
- Correctness build before frame cache: `b30d2c40244da000c051ef9b3503f6bb12697782fe843fd9206185240ff0a464`.
- With frame cache: `b813a46387ef8b9e8c59686395dbc47cdc0243bc9014f69006f1d704017a9e12`.

Validation: 339 workspace/all-target tests, three explicit Metal checks, strict
Clippy, formatting and release build. PTY tests need native terminal access;
sandbox-denied runs were rerun successfully without weakening assertions.

## Independent logo and background animation (2026-10-01)

Logo and background animation can be controlled independently; both now default off.
These measurements use the shaded atom logo at 180 physical pixels, Menlo 14,
opaque content, the default window size, and one release binary. Each sample
settles for three seconds and measures five seconds of app process CPU. Every
accepted sample stayed focused. Modes ran sequentially in rotated order.

| Animation | Idle CPU, median of 3 | CPU during 20 Hz cell updates | Update samples |
| --- | ---: | ---: | ---: |
| Off (default) | 0.015% | 6.46% | 1 |
| Logo only | 4.15% | 8.62% | 1 |
| Background only | 4.14% | Not measured | 0 |
| Both | 4.10% | 8.59% | 2 |

Percentages are fractions of one CPU core. Logo-only and combined animation
show no useful app CPU difference in these short samples. Disabling the
background controls appearance but does not remove the cost of presenting
animation frames. GPU time, WindowServer CPU, energy and achieved frame rate
were not measured, so these numbers cannot establish GPU savings.

The update measurements are preliminary: two logo runs lost focus, leaving
only one accepted logo sample. One idle background attempt also lost focus.
All three rejected runs remain marked inconclusive and excluded from the table.
Window dimensions were not instrumented; no resize was requested. Render-stage
tracing was off. Update samples measure app CPU while changing a cell at 20 Hz,
not presentation latency or frame-stage durations.

[Raw samples and summary](baselines/2026-10-01/animation-split/summary.json).
Measured release binary SHA-256:
`2ebb9a555cc4b3bb144d8b7adbeb3bbc0915647a8362d1b0f91c45b43d93312e`.

## Row geometry cache and terminal correctness fixes (2026-10-01)

Row caching reuses unchanged geometry and only uploads changed buffer ranges.
When a row's glyph count changes, shifted foreground ranges are uploaded too.
All backgrounds still draw before foregrounds, with the cursor last, preserving
glyph overhangs and cursor visibility. Full snapshot conversion and whole-surface
presentation remain.

Three release offscreen runs used a 121×42 grid, Menlo 14 at 2× scale, 20 warmup
iterations and 200 measured iterations per workload per run. Full and cached
construction alternated order on identical shaped input, with byte-for-byte
geometry verification each iteration. Medians below pool 600 observations.

| Workload | Full geometry | Cached geometry | Full upload bytes | Cached upload bytes |
| --- | ---: | ---: | ---: | ---: |
| One cell | 189.6 µs | 9.8 µs | 894,432 | 21,296 |
| One row | 189.8 µs | 9.5 µs | 894,432 | 21,296 |
| Full repaint | 176.6 µs | 192.5 µs | 894,432 | 894,432 |
| Scroll | 180.3 µs | 198.7 µs | 894,432 | 894,432 |

Sparse geometry is about 19–20× faster with 42× fewer upload bytes for these
fixed-glyph-count ASCII updates. Dense geometry has 9–10% overhead, approximately
16–18 µs, from maintaining and assembling cached rows. This is a deliberate
tradeoff for small updates; it is not an improvement in full-screen geometry.
Upload counts are planned byte ranges, not timed GPU transfers. Shaping, grid
conversion, presentation, GPU completion and app CPU are outside these timings.

The GUI baseline attempt was inconclusive: its window stopped presenting before
the CPU sample, leaving no complete profiled frames. It is retained as
`before-cell.json` and excluded from comparisons. No app CPU speedup is claimed.
`geometry-initial.csv` and `geometry-reuse.csv` retain the allocation-tuning runs;
the table uses only `geometry-final-1.csv` through `geometry-final-3.csv`.

[Raw timings and summary](baselines/2026-10-01/row-cache/summary.json).
Release app SHA-256:
`d942f61d10018b360d2a255cc1192b7acfd36f5ca4a1354af1f230e68954d5b8`.

Correctness coverage now includes mouse hover/held-button reporting, fractional
scrolling, underline styles/color and strikeout, OSC palette display/query/reset
ordering, viewport replies, IME composition, and partial-upload equivalence after
cursor, bidi, text, decoration and dimension changes. Independent review caught
and fixed cursor visual-position invalidation and wide IME selection coloring.
Validation: 368 workspace/all-target tests, eight explicit serial Metal checks,
strict Clippy, formatting and release build.
