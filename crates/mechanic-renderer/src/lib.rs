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

        Ok(Self { state, text, font_config })
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
}
