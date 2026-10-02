use mechanic_config::theme::Rgb;

/// Returns the wgpu clear color corresponding to an [`Rgb`] value.
pub fn clear_color(bg: Rgb) -> wgpu::Color {
    wgpu::Color {
        r: f64::from(bg.r) / 255.0,
        g: f64::from(bg.g) / 255.0,
        b: f64::from(bg.b) / 255.0,
        a: 1.0,
    }
}
