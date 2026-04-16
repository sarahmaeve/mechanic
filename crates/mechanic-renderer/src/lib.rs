// mechanic-renderer: GPU rendering pipeline for Mechanic.
//
// Composes wgpu surface management (`pipeline`), cosmic-text glyph rasterization
// (`text`), and terminal grid conversion (`grid`) into a single `Renderer` struct.

pub mod background;
pub mod grid;
pub mod logo;
pub mod pipeline;
pub mod text;

// Re-export the public surface of the renderer.
pub use grid::{CellFlags, CursorStyle, RenderCell, RenderGrid};
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
        // Phase 1: initialise the wgpu device/queue/surface without building
        // any pipelines — we need the device to create the TextRenderer first.
        let surface_init = init_surface(window, size).await?;

        // Phase 2: build the TextRenderer (and its atlas texture) using the
        // device/queue from phase 1.
        let text = TextRenderer::new(
            &surface_init.device,
            &surface_init.queue,
            &font_config,
            scale_factor,
        );

        // Phase 3: build the full pipeline using the real atlas view.
        // No dummy texture needed — the bind group is correct from frame 0.
        let cell_metrics = text.cell_metrics();
        let atlas_gen = text.atlas_generation();
        let state = RenderState::new_with_atlas(
            surface_init,
            &text.atlas_view,
            atlas_gen,
            cell_metrics,
            theme.background,
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
    ///
    /// `focused` gates the corner-gradient color animation — `false` freezes
    /// it on unfocused windows so fading / background windows don't
    /// visibly pulse.
    pub fn render(&mut self, grid: &RenderGrid, content_opacity: f32, time: f32, focused: bool) {
        self.state.render(grid, &mut self.text, &self.font_config, content_opacity, time, focused);
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

        // The new TextRenderer has a fresh atlas texture — unconditionally
        // rebuild the bind group so the pipeline points at it, and sync the
        // stored generation so the per-frame check won't trigger again
        // immediately.
        self.state.update_atlas_bind_group(&self.text.atlas_view);
        self.state.sync_atlas_generation(self.text.atlas_generation());

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
