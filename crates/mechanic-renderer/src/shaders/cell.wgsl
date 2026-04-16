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

    out.clip_pos  = vec4<f32>(ndc, 0.0, 1.0);

    // Map local [0,1]^2 UV → atlas sub-rect UV.
    out.uv = vec2<f32>(
        mix(inst.atlas_uv.x, inst.atlas_uv.z, lv.x),
        mix(inst.atlas_uv.y, inst.atlas_uv.w, lv.y),
    );

    out.fg_color  = inst.fg_color;
    out.bg_color  = inst.bg_color;
    out.use_atlas = inst.use_atlas;

    return out;
}

// ── Fragment stage ────────────────────────────────────────────────────────────

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    if in.use_atlas == 1u {
        // Sample the glyph mask (alpha-only).  The atlas stores pre-rendered
        // alpha coverage in the R channel.
        let alpha = textureSample(atlas_texture, atlas_sampler, in.uv).r;
        // Composite glyph over background.
        return mix(in.bg_color, in.fg_color, alpha);
    } else {
        // Solid background — no glyph.
        return in.bg_color;
    }
}
