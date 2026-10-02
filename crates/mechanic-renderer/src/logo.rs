//! Rasterize the bundled corner logo and upload it once at startup.

use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg;

/// SVG source bundled into the binary at compile time.
const LOGO_SVG: &str = include_str!("../assets/logo.svg");

/// Texture resolution for the rasterized logo, in pixels.
pub const LOGO_SIZE: u32 = 256;

/// The rasterized logo, living on the GPU as a texture.
pub struct Logo {
    /// Owning handle to the texture.  Kept alive alongside the view.
    #[allow(dead_code)]
    pub texture: wgpu::Texture,
    /// View used when binding the texture to the render pipeline.
    pub view: wgpu::TextureView,
}

impl Logo {
    /// Rasterize the bundled SVG and upload the result to a new GPU texture.
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let rgba = rasterize_svg();

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("logo_texture"),
            size: wgpu::Extent3d { width: LOGO_SIZE, height: LOGO_SIZE, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x: 0, y: 0, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            &rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(LOGO_SIZE * 4),
                rows_per_image: None,
            },
            wgpu::Extent3d { width: LOGO_SIZE, height: LOGO_SIZE, depth_or_array_layers: 1 },
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self { texture, view }
    }
}

/// Parse `LOGO_SVG` and rasterize it into an RGBA byte buffer at `LOGO_SIZE × LOGO_SIZE`.  The SVG's viewBox is mapped uniformly onto the square output.
fn rasterize_svg() -> Vec<u8> {
    let opts = usvg::Options::default();
    let tree =
        usvg::Tree::from_str(LOGO_SVG, &opts).expect("bundled logo SVG must parse at startup");

    let mut pixmap =
        Pixmap::new(LOGO_SIZE, LOGO_SIZE).expect("LOGO_SIZE must be a valid Pixmap dimension");

    let svg_size = tree.size();
    let scale_x = LOGO_SIZE as f32 / svg_size.width();
    let scale_y = LOGO_SIZE as f32 / svg_size.height();
    let transform = Transform::from_scale(scale_x, scale_y);

    resvg::render(&tree, transform, &mut pixmap.as_mut());

    pixmap.take()
}
