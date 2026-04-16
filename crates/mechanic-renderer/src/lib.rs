// mechanic-renderer: GPU rendering pipeline for Mechanic.
//
// Composes wgpu surface management (`pipeline`), cosmic-text glyph rasterization
// (`text`), and terminal grid conversion (`grid`) into a single `Renderer` struct.

pub mod background;
pub mod grid;
pub mod pipeline;
pub mod text;

// Re-export the public surface of the renderer.
pub use grid::{CellFlags, CursorStyle, RenderCell, RenderGrid};
pub use text::CellMetrics;

use mechanic_config::{font::FontConfig, theme::Theme};
use pipeline::RenderState;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use text::TextRenderer;

/// Top-level renderer.  Composes the wgpu pipeline and the text renderer.
pub struct Renderer {
    state: RenderState,
    text: TextRenderer,
    font_config: FontConfig,
    /// The window's DPI scale factor, stored so `set_font_size` can
    /// rebuild the text renderer at the same physical resolution.
    scale_factor: f32,
}

impl Renderer {
    /// Construct the renderer for the given window.
    ///
    /// `size` is the initial surface size in physical pixels.
    /// `scale_factor` is the window's DPI scale (e.g. 2.0 on Retina Macs).
    pub async fn new<W>(
        window: W,
        size: (u32, u32),
        scale_factor: f32,
        theme: &Theme,
        font_config: FontConfig,
    ) -> Result<Self, Box<dyn std::error::Error>>
    where
        W: HasWindowHandle + HasDisplayHandle + Send + Sync + 'static,
    {
        // We need a temporary wgpu device to create the TextRenderer (for
        // atlas texture creation) before we can create the full RenderState.
        // Instead, create a temporary device just for font metric extraction,
        // then create the full RenderState with real CellMetrics.
        //
        // Since RenderState creates its own device, we create TextRenderer
        // after RenderState using its device/queue.
        let state = RenderState::new(
            window,
            size,
            Self::bootstrap_metrics(&font_config, scale_factor),
            theme.background,
        )
        .await?;

        let text = TextRenderer::new(&state.device, &state.queue, &font_config, scale_factor);

        Ok(Self { state, text, font_config, scale_factor })
    }

    /// Compute a bootstrap `CellMetrics` without a GPU device.
    ///
    /// This runs cosmic-text font shaping on the CPU to get real metrics,
    /// then those metrics are passed to `RenderState::new` so the globals
    /// uniform is correct from the first frame.
    fn bootstrap_metrics(config: &FontConfig, scale_factor: f32) -> CellMetrics {
        use cosmic_text::{Attrs, Buffer, FontSystem, Metrics, Shaping};

        let mut font_system = FontSystem::new();
        let px_size = config.size * scale_factor;
        let line_height = px_size * 1.3;
        let metrics = Metrics::new(px_size, line_height);

        let mut buffer = Buffer::new(&mut font_system, metrics);
        let mut borrow = buffer.borrow_with(&mut font_system);
        let attrs = Attrs::new().family(cosmic_text::Family::Name(&config.family));
        borrow.set_text(" ", &attrs, Shaping::Advanced, None);
        borrow.shape_until_scroll(false);

        let mut cell_width = px_size * 0.6;
        let mut cell_height = line_height;
        let mut ascent = px_size * 0.8;

        if let Some(run) = borrow.layout_runs().next() {
            cell_height = run.line_height;
            ascent = run.line_y - run.line_top;
            if let Some(glyph) = run.glyphs.first() {
                cell_width = glyph.w;
            }
        }

        CellMetrics { cell_width, cell_height, ascent }
    }

    /// Return the real cell metrics extracted from the font.
    pub fn cell_metrics(&self) -> CellMetrics {
        self.text.cell_metrics()
    }

    /// Notify the renderer that the window has been resized.
    pub fn resize(&mut self, size: (u32, u32)) {
        self.state.resize(size);
    }

    /// Render one frame from the given terminal grid.
    pub fn render(&mut self, grid: &RenderGrid, content_opacity: f32, time: f32) {
        self.state.render(grid, &mut self.text, &self.font_config, content_opacity, time);
    }

    /// Change the font size and rebuild text rendering state.
    ///
    /// The new size is clamped to `[6.0, 72.0]` points.  The `TextRenderer`
    /// is reconstructed (new atlas, fresh ASCII pre-rasterization at the
    /// new size) and the pipeline's cell size is updated so the next frame
    /// uses the new metrics.
    ///
    /// Returns the new [`CellMetrics`] so the caller can resize the terminal
    /// grid to match.
    pub fn set_font_size(&mut self, new_size: f32) -> CellMetrics {
        let clamped = new_size.clamp(6.0, 72.0);
        self.font_config.size = clamped;

        // Rebuild the text renderer: new atlas, re-extracted metrics.
        self.text = TextRenderer::new(
            &self.state.device,
            &self.state.queue,
            &self.font_config,
            self.scale_factor,
        );

        let metrics = self.text.cell_metrics();
        self.state.set_cell_size((metrics.cell_width, metrics.cell_height));
        metrics
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    /// Validate every bundled WGSL shader at test time.
    ///
    /// wgpu uses naga internally to validate shaders when creating a
    /// `ShaderModule`.  If a shader has a type error or other validation
    /// failure, it won't be caught until runtime — long after `cargo test`
    /// has declared success.  This test runs the same parse + validate
    /// pipeline naga uses, so shader bugs fail the test suite immediately.
    #[test]
    fn cell_shader_is_valid_wgsl() {
        let source = include_str!("shaders/cell.wgsl");
        validate_wgsl("cell.wgsl", source);
    }

    /// Parse `source` as WGSL and run the full validator.
    ///
    /// Panics with a readable error if parsing or validation fails.
    fn validate_wgsl(name: &str, source: &str) {
        let module = match naga::front::wgsl::parse_str(source) {
            Ok(m) => m,
            Err(e) => panic!("{name}: WGSL parse error:\n{}", e.emit_to_string(source)),
        };

        let mut validator = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        );

        if let Err(e) = validator.validate(&module) {
            panic!("{name}: WGSL validation error:\n{e:?}");
        }
    }
}
