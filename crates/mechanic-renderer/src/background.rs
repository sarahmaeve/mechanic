// Background rendering.
//
// Phase 1: Clear to a solid color provided by the theme.
// Phase 4 hook: replace `render_background` with the animated gradient shader.

use mechanic_config::theme::Rgb;

/// Returns the wgpu clear color corresponding to an [`Rgb`] value.
///
/// In Phase 4 this function will be replaced / augmented with animated
/// gradient logic without changing the call sites.
pub fn clear_color(bg: Rgb) -> wgpu::Color {
    wgpu::Color {
        r: f64::from(bg.r) / 255.0,
        g: f64::from(bg.g) / 255.0,
        b: f64::from(bg.b) / 255.0,
        a: 1.0,
    }
}
