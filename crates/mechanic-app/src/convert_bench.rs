//! Opt-in host stage timings; no parser, instance construction, or presentation timing.

use std::{
    fs::File,
    hint::black_box,
    io::Write,
    time::{Duration, Instant},
};

use alacritty_terminal::{
    event::VoidListener,
    grid::Scroll,
    index::{Column, Line},
    term::{Term, test::TermSize},
    vte::ansi::Processor,
};
use mechanic_config::FontConfig;
use mechanic_renderer::text::TextRenderer;

use super::*;

const COLS: usize = 121;
const ROWS: usize = 42;
const SAMPLES: usize = 200;
const WARMUPS: usize = 20;
const ARABIC: &str = "تحتفظ اللغة العربية بسياق الفقرة عند التفاف السطور، وتعرض الأخبار والأسئلة بوضوح. القاهرة ٢٠٢٦؛ تقرير 42. ";

#[derive(Clone, Copy)]
enum Workload {
    Cell,
    Row,
    Full,
    Scroll,
    ArabicWrapped,
    ChineseWrapped,
    JapaneseWrapped,
    KoreanWrapped,
    UnicodeRows,
}

impl Workload {
    fn wrapped(self) -> bool {
        matches!(
            self,
            Self::ArabicWrapped
                | Self::ChineseWrapped
                | Self::JapaneseWrapped
                | Self::KoreanWrapped
        )
    }

    fn name(self) -> &'static str {
        match self {
            Self::Cell => "one_cell",
            Self::Row => "one_row",
            Self::Full => "full_ascii",
            Self::Scroll => "scroll",
            Self::ArabicWrapped => "arabic_wrapped_cell",
            Self::ChineseWrapped => "chinese_wrapped_cell",
            Self::JapaneseWrapped => "japanese_wrapped_cell",
            Self::KoreanWrapped => "korean_wrapped_cell",
            Self::UnicodeRows => "unicode_hard_rows_cell",
        }
    }

    fn fixture(self) -> Term<VoidListener> {
        let mut term = Term::new(Default::default(), &TermSize::new(COLS, ROWS), VoidListener);
        let mut processor: Processor = Processor::new();
        processor.advance(&mut term, b"\x1b[?25l");
        match self {
            Self::ArabicWrapped => {
                let paragraph = ARABIC.repeat(100);
                processor.advance(&mut term, paragraph.as_bytes());
                assert!(term.grid()[Line(0)][Column(COLS - 1)].flags.contains(Flags::WRAPLINE));
            }
            Self::ChineseWrapped | Self::JapaneseWrapped | Self::KoreanWrapped => {
                let text = match self {
                    Self::ChineseWrapped => {
                        "新闻报道展示中文段落在终端窗口中自动换行时的文字显示与更新性能"
                    }
                    Self::JapaneseWrapped => {
                        "日本語の新聞記事を端末で表示して自動折り返しと文字更新の性能を確認します"
                    }
                    Self::KoreanWrapped => {
                        "신문기사를터미널에서표시하고자동줄바꿈과글자변경의성능을확인합니다"
                    }
                    _ => unreachable!(),
                };
                assert!(text.chars().all(|c| unicode_width::UnicodeWidthChar::width(c) == Some(2)));
                // Match source display width; odd-width rows add CJK wrap padding.
                let characters = unicode_width::UnicodeWidthStr::width(ARABIC) * 100 / 2;
                let paragraph: String = text.chars().cycle().take(characters).collect();
                processor.advance(&mut term, paragraph.as_bytes());
                assert!(term.grid()[Line(0)][Column(COLS - 1)].flags.contains(Flags::WRAPLINE));
                assert!(term.grid()[Line(21)][Column(0)].flags.contains(Flags::WIDE_CHAR));
                assert!(term.grid()[Line(21)][Column(1)].flags.contains(Flags::WIDE_CHAR_SPACER));
            }
            _ => {
                let count = if matches!(self, Self::Scroll) { ROWS * 3 } else { ROWS };
                for row in 0..count {
                    if row != 0 {
                        processor.advance(&mut term, b"\r\n");
                    }
                    let text = if matches!(self, Self::UnicodeRows) {
                        let prefix = "س Новости Україна 日本語 e\u{301} café Straße ";
                        let width = unicode_width::UnicodeWidthStr::width(prefix);
                        format!("{prefix}{}", " ".repeat(COLS - width))
                    } else {
                        (0..COLS).map(|col| char::from(b'a' + ((row + col) % 26) as u8)).collect()
                    };
                    processor.advance(&mut term, text.as_bytes());
                }
            }
        }
        term
    }

    /// Return the viewport coordinate and independently expected updated text.
    fn mutate(
        self,
        term: &mut Term<VoidListener>,
        step: usize,
        novel: bool,
    ) -> (usize, usize, char) {
        if novel {
            assert!(self.wrapped());
            let wide = !matches!(self, Self::ArabicWrapped);
            let base = if wide { '０' } else { '0' } as u32;
            let stride = if wide { 2 } else { 1 };
            for index in 0..3 {
                let digit = (step / 10usize.pow(index as u32)) % 10;
                term.grid_mut()[Line(21)][Column(index * stride)].c =
                    char::from_u32(base + digit as u32).unwrap();
            }
            return (0, 21, char::from_u32(base + (step % 10) as u32).unwrap());
        }
        let alternate = step.is_multiple_of(2);
        let character = if alternate { 'X' } else { 'Y' };
        match self {
            Self::Cell => term.grid_mut()[Line(21)][Column(60)].c = character,
            Self::Row => {
                for col in 0..COLS {
                    term.grid_mut()[Line(21)][Column(col)].c = character;
                }
            }
            Self::Full => {
                for row in 0..ROWS {
                    for col in 0..COLS {
                        term.grid_mut()[Line(row as i32)][Column(col)].c = character;
                    }
                }
            }
            Self::Scroll => {
                term.scroll_display(Scroll::Delta(if alternate { 1 } else { -1 }));
                let top = -(term.grid().display_offset() as i32);
                return (0, 0, term.grid()[Line(top)][Column(0)].c);
            }
            Self::ArabicWrapped | Self::UnicodeRows => {
                let character = if alternate { 'س' } else { 'ش' };
                let cell = &mut term.grid_mut()[Line(21)][Column(0)];
                cell.c = character;
                return (0, 21, character);
            }
            Self::ChineseWrapped | Self::JapaneseWrapped | Self::KoreanWrapped => {
                let (first, second) = match self {
                    Self::ChineseWrapped => ('中', '文'),
                    Self::JapaneseWrapped => ('あ', 'い'),
                    Self::KoreanWrapped => ('한', '글'),
                    _ => unreachable!(),
                };
                let character = if alternate { first } else { second };
                term.grid_mut()[Line(21)][Column(0)].c = character;
                return (0, 21, character);
            }
        }
        (60, 21, character)
    }
}

fn converted(term: &Term<VoidListener>, theme: &Theme) -> RenderGrid {
    let mut grid = convert_content(term.renderable_content(), COLS, ROWS, theme, true);
    append_bidi_context(&mut grid, term.grid());
    grid
}

#[expect(
    clippy::too_many_arguments,
    reason = "benchmark passes stage resources explicitly without timing their construction"
)]
fn step(
    workload: Workload,
    novel: bool,
    term: &mut Term<VoidListener>,
    iteration: usize,
    theme: &Theme,
    text: &mut TextRenderer,
    config: &FontConfig,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    previous: &mut Option<char>,
) -> (u128, u128, u128) {
    let (col, row, expected) = workload.mutate(term, iteration, novel);
    let started = Instant::now();
    let grid = converted(term, theme);
    let conversion_ns = started.elapsed().as_nanos();
    let started = Instant::now();
    let shaped = text.shape_grid(black_box(&grid), config);
    let shaping_ns = started.elapsed().as_nanos();
    let started = Instant::now();
    text.prepare_frame(black_box(&shaped), device, queue).unwrap();
    let prepare_ns = started.elapsed().as_nanos();
    let observed = grid.get(col, row).unwrap().character;
    assert_eq!(observed, expected, "{} update missing from viewport", workload.name());
    assert_ne!(*previous, Some(observed), "{} update was a no-op", workload.name());
    *previous = Some(observed);
    assert_eq!(shaped.len(), ROWS);
    black_box((&grid, &shaped));
    (conversion_ns, shaping_ns, prepare_ns)
}

#[test]
#[ignore = "offscreen Metal host-stage benchmark; run explicitly with --release"]
fn frame_preparation_stages() {
    let path = std::env::var_os("MECHANIC_FRAME_PREPARATION_CSV")
        .expect("set MECHANIC_FRAME_PREPARATION_CSV to a new output path");
    let mut output = File::options().write(true).create_new(true).open(path).unwrap();
    let novel = std::env::var_os("MECHANIC_FRAME_PREPARATION_NOVEL").is_some();
    writeln!(output, "workload,sample,conversion_ns,shaping_ns,prepare_ns,cols,rows,font,font_size,scale,profile,warmups,cpu_warmup_ms,update_mode").unwrap();
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::METAL,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = pollster::block_on(instance.request_adapter(&Default::default())).unwrap();
    let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).unwrap();
    let config = FontConfig { family: "Menlo".into(), size: 16.0, ..Default::default() };
    let theme = Theme::default();
    for workload in [
        Workload::Cell,
        Workload::Row,
        Workload::Full,
        Workload::Scroll,
        Workload::ArabicWrapped,
        Workload::ChineseWrapped,
        Workload::JapaneseWrapped,
        Workload::KoreanWrapped,
        Workload::UnicodeRows,
    ] {
        if novel && !workload.wrapped() {
            continue;
        }
        let mut term = workload.fixture();
        let mut text = TextRenderer::new(&device, &queue, &config, 1.0);
        let mut previous = None;
        let mut iteration = 0;
        for _ in 0..WARMUPS {
            black_box(step(
                workload,
                novel,
                &mut term,
                iteration,
                &theme,
                &mut text,
                &config,
                &device,
                &queue,
                &mut previous,
            ));
            iteration += 1;
        }
        let warmup_until = Instant::now() + Duration::from_millis(100);
        while Instant::now() < warmup_until {
            black_box(step(
                workload,
                novel,
                &mut term,
                iteration,
                &theme,
                &mut text,
                &config,
                &device,
                &queue,
                &mut previous,
            ));
            iteration += 1;
        }
        for sample in 0..SAMPLES {
            let (conversion, shaping, prepare) = step(
                workload,
                novel,
                &mut term,
                iteration,
                &theme,
                &mut text,
                &config,
                &device,
                &queue,
                &mut previous,
            );
            iteration += 1;
            writeln!(
                output,
                "{},{sample},{conversion},{shaping},{prepare},{COLS},{ROWS},Menlo,16,1,{},20,100,{}",
                workload.name(),
                if cfg!(debug_assertions) { "debug" } else { "release" },
                if novel { "novel_counter" } else { "alternating" },
            )
            .unwrap();
        }
    }
    output.flush().unwrap();
}

#[derive(Clone, Copy, Debug)]
enum MatrixScript {
    Latin,
    Cyrillic,
    Cjk,
    Arabic,
    HiddenRtl,
}

impl MatrixScript {
    fn name(self) -> &'static str {
        match self {
            Self::Latin => "latin_accents",
            Self::Cyrillic => "cyrillic",
            Self::Cjk => "cjk",
            Self::Arabic => "arabic",
            Self::HiddenRtl => "rtl_context_ltr_viewport",
        }
    }

    fn text(self) -> &'static str {
        match self {
            Self::Latin => "Le café ouvre tôt; e\u{301}té, Straße et lumière. ",
            Self::Cyrillic => "Новости города: Україна, люди и новые книги. ",
            Self::Cjk => "雨后的街道 日本語の図書館 소식과이야기 ",
            Self::Arabic => ARABIC,
            Self::HiddenRtl => "An ordinary Latin viewport with café and fresh news. ",
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum MatrixChange {
    Sparse,
    Scroll,
    Dense,
}

impl MatrixChange {
    fn name(self) -> &'static str {
        match self {
            Self::Sparse => "sparse_novel_counter",
            Self::Scroll => "scroll_cycle",
            Self::Dense => "dense_novel_text",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct MatrixCase {
    cols: usize,
    rows: usize,
    script: MatrixScript,
    change: MatrixChange,
    softwrap: bool,
}

fn fit_matrix_row(text: &str, cols: usize) -> String {
    let mut width = 0;
    text.chars()
        .cycle()
        .take_while(|&character| {
            width += unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
            width < cols
        })
        .collect()
}

impl MatrixCase {
    /// Populate through the parser so wide cells and wrap flags remain valid.
    fn populate(self, term: &mut Term<VoidListener>, processor: &mut Processor, epoch: usize) {
        processor.advance(term, b"\x1bc\x1b[?25l");
        if self.softwrap {
            let prefix = if matches!(self.script, MatrixScript::HiddenRtl) {
                "سياق عربي خارج الشاشة: الأخبار 42. "
            } else {
                ""
            };
            let phrase = format!("{epoch:04}/0000 {}", self.script.text());
            let repeats = (self.cols * self.rows * 3)
                .div_ceil(unicode_width::UnicodeWidthStr::width(phrase.as_str()));
            let mut content = prefix.to_owned();
            for sequence in 0..repeats {
                use std::fmt::Write;
                write!(content, "{epoch:04}/{sequence:04} {}", self.script.text()).unwrap();
            }
            processor.advance(term, content.as_bytes());
        } else {
            for row in 0..self.rows * 3 {
                if row != 0 {
                    processor.advance(term, b"\r\n");
                }
                let text = if matches!(self.script, MatrixScript::HiddenRtl) && row < self.rows {
                    ARABIC
                } else {
                    self.script.text()
                };
                let line = fit_matrix_row(&format!("{epoch:04}/{row:04} {text}"), self.cols);
                processor.advance(term, line.as_bytes());
            }
        }
    }

    fn mutate(self, term: &mut Term<VoidListener>, processor: &mut Processor, iteration: usize) {
        match self.change {
            MatrixChange::Sparse => {
                let update = format!("\x1b[{};1H{iteration:04}", self.rows / 2 + 1);
                processor.advance(term, update.as_bytes());
                for (col, expected) in format!("{iteration:04}").chars().enumerate() {
                    assert_eq!(term.grid()[Line((self.rows / 2) as i32)][Column(col)].c, expected);
                }
            }
            MatrixChange::Scroll => {
                let phase = iteration % 8;
                let step = if phase <= 4 { phase } else { 8 - phase };
                let offset = step * self.rows / 4;
                term.scroll_display(Scroll::Delta(
                    offset as i32 - term.grid().display_offset() as i32,
                ));
                assert_eq!(term.grid().display_offset(), offset);
            }
            MatrixChange::Dense => self.populate(term, processor, iteration),
        }
    }
}

#[test]
#[ignore = "expanded offscreen Metal matrix; run explicitly with --release"]
fn frame_preparation_matrix() {
    const MATRIX_SAMPLES: usize = 24;
    const MATRIX_WARMUPS: usize = 8;
    let path = std::env::var_os("MECHANIC_FRAME_MATRIX_CSV")
        .expect("set MECHANIC_FRAME_MATRIX_CSV to a new output path");
    let mut output = File::options().write(true).create_new(true).open(path).unwrap();
    writeln!(output, "script,update_mode,wrapping,cols,rows,sample,conversion_ns,shaping_ns,prepare_ns,font,font_size,scale,profile,warmups,samples,source_screens,parser_inside_timing,bidi_prefix_bytes,bidi_suffix_bytes,bidi_context_truncated,backend,adapter,display_offset").unwrap();
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::METAL,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = pollster::block_on(instance.request_adapter(&Default::default())).unwrap();
    let adapter_name = adapter.get_info().name.replace([',', '\n', '\r'], " ");
    let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).unwrap();
    let config = FontConfig { family: "Menlo".into(), size: 16.0, ..Default::default() };
    let theme = Theme::default();
    for (cols, rows) in [(40, 12), (81, 25), (121, 42)] {
        for script in [
            MatrixScript::Latin,
            MatrixScript::Cyrillic,
            MatrixScript::Cjk,
            MatrixScript::Arabic,
            MatrixScript::HiddenRtl,
        ] {
            for change in [MatrixChange::Sparse, MatrixChange::Scroll, MatrixChange::Dense] {
                for softwrap in [false, true] {
                    let case = MatrixCase { cols, rows, script, change, softwrap };
                    let mut term =
                        Term::new(Default::default(), &TermSize::new(cols, rows), VoidListener);
                    let mut processor = Processor::new();
                    case.populate(&mut term, &mut processor, 0);
                    let mut text = TextRenderer::new(&device, &queue, &config, 1.0);
                    let mut previous_cells = Vec::new();
                    for iteration in 0..MATRIX_WARMUPS + MATRIX_SAMPLES {
                        case.mutate(&mut term, &mut processor, iteration + 1);
                        let started = Instant::now();
                        let mut grid =
                            convert_content(term.renderable_content(), cols, rows, &theme, true);
                        append_bidi_context(&mut grid, term.grid());
                        let conversion = started.elapsed().as_nanos();
                        let started = Instant::now();
                        let shaped = text.shape_grid(black_box(&grid), &config);
                        let shaping = started.elapsed().as_nanos();
                        let started = Instant::now();
                        text.prepare_frame(black_box(&shaped), &device, &queue).unwrap();
                        let prepare = started.elapsed().as_nanos();
                        assert_eq!(shaped.len(), rows);
                        assert!(grid.cells != previous_cells, "unchanged viewport: {case:?}");
                        previous_cells.clone_from(&grid.cells);
                        let counter = format!("{:04}", iteration + 1);
                        match change {
                            MatrixChange::Sparse => {
                                let start = (rows / 2) * cols;
                                let visible: String = grid.cells[start..start + 4]
                                    .iter()
                                    .map(|cell| cell.character)
                                    .collect();
                                assert_eq!(visible, counter, "lost sparse update: {case:?}");
                                for col in 0..4 {
                                    assert!(
                                        shaped[rows / 2]
                                            .glyphs
                                            .iter()
                                            .any(|glyph| glyph.source_col == col),
                                        "missing counter glyph: {case:?}, iteration={iteration}, col={col}, cells={:?}, shaped={:?}",
                                        &grid.cells[start..start + 4],
                                        shaped[rows / 2]
                                    );
                                }
                            }
                            MatrixChange::Dense => {
                                let visible: String =
                                    grid.cells.iter().map(|cell| cell.character).collect();
                                assert!(visible.contains(&counter), "lost dense update: {case:?}");
                            }
                            MatrixChange::Scroll => {}
                        }
                        if softwrap {
                            assert!(
                                !grid.bidi_prefix.is_empty(),
                                "missing offscreen context: {case:?}"
                            );
                            if matches!(script, MatrixScript::HiddenRtl) {
                                assert!(
                                    grid.bidi_prefix.contains('س'),
                                    "missing hidden RTL: {case:?}"
                                );
                                assert!(
                                    grid.cells.iter().all(|cell| cell.character.is_ascii()
                                        || matches!(cell.character, 'é')),
                                    "RTL leaked into LTR viewport: {case:?}"
                                );
                            }
                        } else {
                            assert!(grid.bidi_prefix.is_empty());
                            assert!(grid.bidi_suffix.is_empty());
                        }
                        if iteration >= MATRIX_WARMUPS {
                            writeln!(output, "{},{},{},{cols},{rows},{},{conversion},{shaping},{prepare},Menlo,16,1,{},8,24,3,false,{},{},{},Metal,{},{}", script.name(), change.name(), if softwrap { "softwrap" } else { "explicit_rows" }, iteration - MATRIX_WARMUPS, if cfg!(debug_assertions) { "debug" } else { "release" }, grid.bidi_prefix.len(), grid.bidi_suffix.len(), grid.bidi_context_truncated, adapter_name, term.grid().display_offset()).unwrap();
                        }
                        black_box((&grid, &shaped));
                    }
                }
            }
        }
    }
    output.flush().unwrap();
}
