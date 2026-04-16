// cell.wgsl — Instanced rendering of terminal cells.
//
// Each instance represents one terminal cell.  The vertex shader positions a
// unit quad and the fragment shader either:
//   • Samples the glyph atlas texture (when `use_atlas` == 1u), or
//   • Draws a solid background color (when `use_atlas` == 0u).

// ── Uniforms ──────────────────────────────────────────────────────────────────

struct Globals {
    // Viewport size in pixels, used to convert pixel coordinates → NDC.
    viewport_size: vec2<f32>,
    // Cell size in pixels.
    cell_size: vec2<f32>,
    // Elapsed seconds since app start (for animation).
    time: f32,
    // Content opacity (activity-based fade).
    content_opacity: f32,
    // 1.0 when the window has keyboard focus, 0.0 when blurred.  Used to
    // freeze the corner-gradient color pulse on unfocused windows.
    focused: f32,
    _pad: f32,
}

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var atlas_texture: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;

// ── Per-instance data ─────────────────────────────────────────────────────────
//
// Laid out to match `GpuInstance` in pipeline.rs.

struct Instance {
    // Grid position (col, row) in cell coordinates.
    @location(0) cell_pos: vec2<u32>,
    // Atlas UV rectangle covering the actual glyph bitmap: (u_min, v_min, u_max, v_max).
    @location(1) atlas_uv: vec4<f32>,
    // Foreground color (linear, premultiplied optional — we keep sRGB for now).
    @location(2) fg_color: vec4<f32>,
    // Background color.
    @location(3) bg_color: vec4<f32>,
    // Pixel offset from cell origin to glyph quad origin.
    @location(4) glyph_offset: vec2<f32>,
    // Pixel size of the glyph quad (0, 0 for background instances).
    @location(5) glyph_size: vec2<f32>,
    // 1u → sample atlas (text glyph); 0u → draw background solid.
    @location(6) use_atlas: u32,
}

// ── Vertex stage ──────────────────────────────────────────────────────────────

struct VertexOutput {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) fg_color: vec4<f32>,
    @location(2) bg_color: vec4<f32>,
    @location(3) use_atlas: u32,
    @location(4) pixel_pos: vec2<f32>,  // pixel position for gradient
}

// Quad corners in local [0,1]^2 space (two triangles, 6 vertices).
var<private> QUAD_VERTS: array<vec2<f32>, 6> = array<vec2<f32>, 6>(
    vec2<f32>(0.0, 0.0),
    vec2<f32>(1.0, 0.0),
    vec2<f32>(0.0, 1.0),
    vec2<f32>(1.0, 0.0),
    vec2<f32>(1.0, 1.0),
    vec2<f32>(0.0, 1.0),
);

@vertex
fn vs_main(
    inst: Instance,
    @builtin(vertex_index) vid: u32,
) -> VertexOutput {
    var out: VertexOutput;

    let lv = QUAD_VERTS[vid];

    // Top-left corner of this cell in pixel space (Y increases downward).
    let cell_origin = vec2<f32>(
        f32(inst.cell_pos.x) * globals.cell_size.x,
        f32(inst.cell_pos.y) * globals.cell_size.y,
    );

    // Determine quad origin and size based on whether this is a glyph or background.
    var quad_origin: vec2<f32>;
    var quad_size: vec2<f32>;

    if inst.use_atlas == 1u {
        // Glyph: position at the bearing offset within the cell, sized to the
        // actual glyph bitmap dimensions.
        quad_origin = cell_origin + inst.glyph_offset;
        quad_size = inst.glyph_size;
    } else {
        // Background: fill the entire cell.
        quad_origin = cell_origin;
        quad_size = globals.cell_size;
    }

    // Pixel position of this vertex.
    let px = quad_origin + lv * quad_size;

    // Convert pixel coords to NDC.  wgpu uses Y-up NDC but pixel Y is Y-down,
    // so we flip Y: NDC_y = 1 - 2 * (px_y / viewport_h).
    let ndc = vec2<f32>(
         2.0 * px.x / globals.viewport_size.x - 1.0,
        -2.0 * px.y / globals.viewport_size.y + 1.0,
    );

    out.clip_pos = vec4<f32>(ndc, 0.0, 1.0);

    // Map local [0,1]^2 UV → atlas sub-rect UV.
    out.uv = vec2<f32>(
        mix(inst.atlas_uv.x, inst.atlas_uv.z, lv.x),
        mix(inst.atlas_uv.y, inst.atlas_uv.w, lv.y),
    );

    out.fg_color  = inst.fg_color;
    out.bg_color  = inst.bg_color;
    out.use_atlas = inst.use_atlas;
    out.pixel_pos = px;

    return out;
}

// ── Fragment stage ────────────────────────────────────────────────────────────
//
// macOS Metal only exposes `CompositeAlphaMode::PostMultiplied` for
// transparent surfaces, which expects non-premultiplied colors in the
// surface texture.  The compositor then does:
//
//     screen = surface.rgb * surface.a + desktop * (1 - surface.a)
//
// So we output `(final_rgb, content_opacity)` directly — RGB is the final
// color we want to appear, alpha is the window opacity that tells the
// compositor how much of our pixel vs the desktop to show.

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    if in.use_atlas == 1u {
        let alpha = textureSample(atlas_texture, atlas_sampler, in.uv).r;
        // Mix fg and bg based on glyph coverage — this gives us the final
        // on-screen color for this pixel, before compositing with the desktop.
        let color_rgb = mix(in.bg_color.rgb, in.fg_color.rgb, alpha);
        return vec4<f32>(color_rgb, globals.content_opacity);
    } else {
        // Background cell — apply gradient.
        var bg_rgb = in.bg_color.rgb;

        // Animated gradient glow in the lower-right corner.
        let uv_pos = in.pixel_pos / globals.viewport_size;
        let dist = length(uv_pos - vec2<f32>(1.0, 1.0));
        let gradient_strength = exp(-dist * dist / 0.08) * 0.15;

        // Animated color: slowly rotating between electric cyan (#52E8FF)
        // and azure (#007FFF) over time.  Phase is multiplied by `focused`
        // so unfocused windows stay at phase = 0 (static color) — avoids
        // distracting pulses on background / fading windows.
        let phase: f32 = globals.time * 0.3 * globals.focused;
        let t: f32 = sin(phase) * 0.5 + 0.5;
        let gradient_r: f32 = mix(0.322, 0.0, t) * gradient_strength;
        let gradient_g: f32 = mix(0.910, 0.498, t) * gradient_strength;
        let gradient_b: f32 = gradient_strength;

        bg_rgb = bg_rgb + vec3<f32>(gradient_r, gradient_g, gradient_b);

        return vec4<f32>(bg_rgb, globals.content_opacity);
    }
}
