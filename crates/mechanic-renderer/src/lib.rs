pub mod background;
pub mod grid;
pub mod logo;
pub mod pipeline;
pub mod text;

pub use grid::{CellFlags, CursorStyle, RenderCell, RenderGrid};
pub use pipeline::FrameUniforms;
pub use text::CellMetrics;

use mechanic_config::{font::FontConfig, theme::Theme};
use pipeline::{RenderState, init_surface};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use text::TextRenderer;

/// Top-level renderer.  Composes the wgpu pipeline and the text renderer.
pub struct Renderer {
    state: RenderState,
    text: TextRenderer,
    font_config: FontConfig,
    /// The window's DPI scale factor, stored so `set_font_size` can rebuild the text renderer at the same physical resolution.
    scale_factor: f32,
}

impl Renderer {
    /// Refresh bidi hit testing when input precedes a pending redraw.
    pub fn prepare_layout(&mut self, grid: &RenderGrid) {
        self.state.prepare_layout(grid, &mut self.text, &self.font_config);
    }
    /// Map terminal columns to the displayed bidi layout.
    pub fn visual_column(&self, col: usize, row: usize) -> usize {
        self.state.visual_column(col, row)
    }

    /// Return the logical column and direction under a displayed cell.
    pub fn logical_column(&self, col: usize, row: usize) -> (usize, bool) {
        self.state.logical_column(col, row)
    }
    /// Construct the renderer for the given window.
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
        let surface_init = init_surface(window, size).await?;

        let text = TextRenderer::new(
            &surface_init.device,
            &surface_init.queue,
            &font_config,
            scale_factor,
        );

        let cell_metrics = text.cell_metrics();
        let atlas_gen = text.atlas_generation();
        let state = RenderState::new_with_atlas(
            surface_init,
            &text.atlas_view,
            atlas_gen,
            cell_metrics,
            theme.background,
            theme.logo,
        )?;

        Ok(Self { state, text, font_config, scale_factor })
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
    pub fn render(&mut self, grid: &RenderGrid, uniforms: FrameUniforms) -> bool {
        self.state.render(grid, &mut self.text, &self.font_config, uniforms)
    }

    /// Draw cached instances with new uniforms; true only after presentation.
    pub fn render_animation(&mut self, uniforms: FrameUniforms) -> bool {
        self.state.render_animation(uniforms)
    }

    /// Change the font size and rebuild text rendering state.
    pub fn set_font_size(&mut self, new_size: f32) -> CellMetrics {
        let clamped = new_size.clamp(6.0, 72.0);
        self.font_config.size = clamped;

        self.text = TextRenderer::new(
            &self.state.device,
            &self.state.queue,
            &self.font_config,
            self.scale_factor,
        );

        self.state.update_atlas_bind_group(&self.text.atlas_view);
        self.state.sync_atlas_generation(self.text.atlas_generation());

        let metrics = self.text.cell_metrics();
        self.state.set_cell_size((metrics.cell_width, metrics.cell_height));
        metrics
    }
}

#[cfg(test)]
mod tests {
    /// Validate every bundled WGSL shader at test time.
    #[test]
    fn cell_shader_is_valid_wgsl() {
        let source = include_str!("shaders/cell.wgsl");
        validate_wgsl("cell.wgsl", source);
    }

    /// Parse `source` as WGSL and run the full validator.
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
