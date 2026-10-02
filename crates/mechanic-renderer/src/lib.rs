pub mod background;
pub mod grid;
pub mod logo;
pub mod pipeline;
pub mod text;

pub use grid::{CellFlags, CursorStyle, RenderCell, RenderGrid};
pub use pipeline::FrameUniforms;
pub use pipeline::{PaneRect, RenderDivider, RenderPane, RenderPaneHandle, RenderPaneHeader};
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
    theme: Theme,
}

impl Renderer {
    /// Refresh one pane's bidi layout before processing input or IME geometry.
    pub fn prepare_pane_layout(&mut self, id: u64, grid: &RenderGrid) {
        if self.needs_device_recovery() {
            return;
        }
        self.state.prepare_pane_layout(id, grid, &mut self.text, &self.font_config);
    }

    pub fn pane_visual_column(&self, id: u64, col: usize, row: usize) -> usize {
        self.state.pane_visual_column(id, col, row)
    }

    pub fn pane_logical_column(&self, id: u64, col: usize, row: usize) -> (usize, bool) {
        self.state.pane_logical_column(id, col, row)
    }

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
        let mut state = RenderState::new_with_atlas(
            surface_init,
            &text.atlas_view,
            atlas_gen,
            cell_metrics,
            theme.background,
            theme.logo,
        )?;
        state.set_pane_colors(theme.cursor, theme.ansi.bright_black);
        state.set_divider_colors(theme.foreground, theme.background, theme.cursor);
        state.set_pane_header_color(theme.foreground);

        Ok(Self { state, text, font_config, scale_factor, theme: theme.clone() })
    }

    /// Return the real cell metrics extracted from the font.
    pub fn cell_metrics(&self) -> CellMetrics {
        self.text.cell_metrics()
    }

    pub fn needs_device_recovery(&self) -> bool {
        self.state.needs_device_recovery()
    }

    /// Driver-thread wake hook, typically sending an event through a window proxy.
    pub fn set_device_lost_waker(&mut self, waker: std::sync::Arc<dyn Fn() + Send + Sync>) {
        self.state.set_device_lost_waker(waker);
    }

    /// Exercise real device destruction for diagnostic recovery harnesses.
    #[doc(hidden)]
    pub fn simulate_device_loss(&self) {
        self.state.device.destroy();
        // wgpu delivers the Destroyed callback only after queued work drains.
        let _ = self.state.device.poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(std::time::Duration::from_secs(2)),
        });
    }

    /// Retry deadline after a failed device recovery; `None` permits an immediate attempt.
    pub fn device_recovery_retry_at(&self) -> Option<std::time::Instant> {
        self.state.device_recovery_retry_at()
    }

    /// Recover a lost GPU device on the window event-loop thread. Rebuilds every
    /// GPU resource while preserving font, theme and pane decoration settings.
    /// Returns true when replaced; false if healthy or waiting for the retry deadline.
    /// The next frame must use `render` or `render_panes` to populate fresh buffers.
    pub async fn recover_device(&mut self) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(init) = self.state.replacement_surface_init().await? else {
            return Ok(false);
        };
        let device = init.device.clone();
        // A replacement can fail while resources are being allocated. Capture
        // driver/validation/OOM errors so the event loop can retry instead of
        // allowing wgpu's uncaptured-error handler to panic.
        let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let internal = device.push_error_scope(wgpu::ErrorFilter::Internal);
        let memory = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let text =
            TextRenderer::new(&init.device, &init.queue, &self.font_config, self.scale_factor);
        let state = RenderState::new_with_atlas(
            init,
            &text.atlas_view,
            text.atlas_generation(),
            text.cell_metrics(),
            self.theme.background,
            self.theme.logo,
        );
        let errors = [memory.pop().await, internal.pop().await, validation.pop().await];
        if let Some(error) = errors.into_iter().flatten().next() {
            self.state.device_recovery_failed();
            return Err(format!("GPU resource recovery failed: {error}").into());
        }
        let mut state = match state {
            Ok(state) if !state.needs_device_recovery() => state,
            Ok(_) => {
                self.state.device_recovery_failed();
                return Err("replacement GPU device was lost during initialization".into());
            }
            Err(error) => {
                self.state.device_recovery_failed();
                return Err(error);
            }
        };
        state.copy_pane_settings_from(&self.state);
        self.text = text;
        self.state = state;
        log::info!("GPU device and rendering resources recovered");
        Ok(true)
    }

    /// Notify the renderer that the window has been resized.
    pub fn resize(&mut self, size: (u32, u32)) {
        self.state.resize(size);
    }

    /// Render one frame from the given terminal grid.
    pub fn render(&mut self, grid: &RenderGrid, uniforms: FrameUniforms) -> bool {
        self.state.render(grid, &mut self.text, &self.font_config, uniforms)
    }

    /// Render all panes with one window surface acquisition, submission and presentation.
    pub fn render_panes(&mut self, panes: &[RenderPane<'_>], uniforms: FrameUniforms) -> bool {
        self.state.render_panes(panes, &mut self.text, &self.font_config, uniforms)
    }

    /// Update visible divider bounds and hover/drag state. An empty slice clears them.
    pub fn set_pane_dividers(&mut self, dividers: &[RenderDivider]) {
        self.state.set_pane_dividers(dividers);
    }

    /// Update pane drag grips within their reserved header bounds.
    pub fn set_pane_handles(&mut self, handles: &[RenderPaneHandle]) {
        self.state.set_pane_handles(handles);
    }

    /// Override an outline by stable pane ID. An empty slice clears overrides.
    pub fn set_pane_outline_colors(
        &mut self,
        colors: &[(u64, Option<mechanic_config::theme::Rgb>)],
    ) {
        self.state.set_pane_outline_colors(colors);
    }

    /// Render header labels inside their reserved physical pixel bounds.
    pub fn set_pane_headers(&mut self, headers: &[RenderPaneHeader]) {
        self.state.set_pane_headers(headers);
    }

    /// Show the proposed pane bounds during a drag, or clear the preview.
    pub fn set_pane_drop_preview(&mut self, preview: Option<PaneRect>) {
        self.state.set_pane_drop_preview(preview);
    }

    /// Draw cached instances with new uniforms; true only after presentation.
    pub fn render_animation(&mut self, uniforms: FrameUniforms) -> bool {
        self.state.render_animation(uniforms)
    }

    /// Change the font size and rebuild text rendering state.
    pub fn set_font_size(&mut self, new_size: f32) -> CellMetrics {
        let clamped = new_size.clamp(6.0, 72.0);
        self.font_config.size = clamped;
        if self.needs_device_recovery() {
            return self.cell_metrics();
        }

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

    /// Rebuild shared text rendering at a new window DPI scale.
    pub fn set_scale_factor(&mut self, scale_factor: f32) -> CellMetrics {
        if !scale_factor.is_finite() || scale_factor <= 0.0 || scale_factor == self.scale_factor {
            return self.cell_metrics();
        }
        self.scale_factor = scale_factor;
        self.set_font_size(self.font_config.size)
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
