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
    pub async fn new<W>(
        window: W,
        size: (u32, u32),
        theme: &Theme,
        font_config: FontConfig,
    ) -> Result<Self, Box<dyn std::error::Error>>
    where
        W: HasWindowHandle + HasDisplayHandle + Send + Sync + 'static,
    {
        let state = RenderState::new(window, size, font_config.size, theme.background).await?;

        let text = TextRenderer::new(&state.device, &state.queue, &font_config);

        Ok(Self { state, text, font_config })
    }

    /// Notify the renderer that the window has been resized.
    pub fn resize(&mut self, size: (u32, u32)) {
        self.state.resize(size);
    }

    /// Render one frame from the given terminal grid.
    pub fn render(&mut self, grid: &RenderGrid) {
        self.state.render(grid, &mut self.text, &self.font_config);
    }
}
