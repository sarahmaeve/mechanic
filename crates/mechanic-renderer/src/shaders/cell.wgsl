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
// Corner logo — rasterized from assets/logo.svg once at startup.
// Sampled in the background fragment path to overlay the IC mark
// on top of the animated gradient in the lower-right corner.
@group(0) @binding(3) var logo_texture: texture_2d<f32>;

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

        // Breathing brightness oscillation — ±18% around the peak on a
        // ~3 second cycle.  Gated by `focused` so unfocused windows hold
        // a constant brightness rather than quietly pulsing in the
        // background.
        let breath: f32 = 1.0 + sin(globals.time * 2.1) * 0.18 * globals.focused;
        let gradient_strength = exp(-dist * dist / 0.08) * 0.22 * breath;

        // Animated color: rotating between electric cyan (#52E8FF) and
        // azure (#007FFF).  Phase multiplied by `focused` so unfocused
        // windows freeze at phase = 0 (static midpoint color).
        let phase: f32 = globals.time * 0.5 * globals.focused;
        let t: f32 = sin(phase) * 0.5 + 0.5;
        let gradient_r: f32 = mix(0.322, 0.0, t) * gradient_strength;
        let gradient_g: f32 = mix(0.910, 0.498, t) * gradient_strength;
        let gradient_b: f32 = gradient_strength;

        bg_rgb = bg_rgb + vec3<f32>(gradient_r, gradient_g, gradient_b);

        // ── Corner logo overlay ───────────────────────────────────────
        //
        // Sampled and composited on top of the gradient, anchored to the
        // lower-right of the viewport with a small margin.  The logo
        // texture has pre-multiplied alpha (tiny-skia native format), so
        // we composite with the standard "over" operator:
        //     out = src + dst * (1 - src.a)
        //
        // `logo_opacity` controls how prominent the logo reads against
        // the gradient — 1.0 is full strength, lower values blend in.
        let logo_size: f32 = 270.0;   // display size in physical pixels
        let logo_margin: f32 = 16.0;  // inset from the corner
        let logo_opacity: f32 = 0.60;

        let logo_br = globals.viewport_size - vec2<f32>(logo_margin, logo_margin);
        let logo_tl = logo_br - vec2<f32>(logo_size, logo_size);
        let logo_px = in.pixel_pos - logo_tl;

        if logo_px.x >= 0.0 && logo_px.x < logo_size
            && logo_px.y >= 0.0 && logo_px.y < logo_size {
            let logo_uv = logo_px / logo_size;
            let logo = textureSample(logo_texture, atlas_sampler, logo_uv);
            let a = logo.a * logo_opacity;
            bg_rgb = logo.rgb * logo_opacity + bg_rgb * (1.0 - a);

            // ── Electron pulses riding the circuit traces ────────────
            //
            // Hand-picked straight-line segments from the SVG traces.
            // Each electron is a small Gaussian blob that walks its
            // segment from start → end over `period` seconds, with a
            // per-segment phase offset so the pulses flow in a
            // staggered sequence rather than marching in lockstep.
            //
            // Coordinates are in SVG space (0–256) so you can read
            // them straight out of assets/logo.svg.  They're mapped to
            // the on-screen logo rect at the end.
            //
            // Only drawn when focused — keeps background windows quiet.
            let pulse_glow = electron_pulses(logo_px, logo_size, globals.time)
                * globals.focused;
            // Celeste-white core with a hint of cyan — brighter than
            // the trace it rides on, so the trace visibly lights up.
            let electron_color = vec3<f32>(0.85, 1.0, 1.0);
            bg_rgb = bg_rgb + electron_color * pulse_glow;
        }

        return vec4<f32>(bg_rgb, globals.content_opacity);
    }
}

// ── Electron pulse helpers ────────────────────────────────────────────────────

// Glow from a single electron moving along a SVG-space line segment.
//
// `logo_px` is the fragment's position inside the logo rect, in display pixels.
// `logo_size` is the on-screen logo edge length (same in x and y).
// `a_svg`/`b_svg` are the segment endpoints in SVG viewBox coords (0–256).
// `t` is the electron's progress along the segment, in [0, 1).
// `radius` is the glow radius in display pixels.
fn electron_glow(
    logo_px: vec2<f32>,
    logo_size: f32,
    a_svg: vec2<f32>,
    b_svg: vec2<f32>,
    t: f32,
    radius: f32,
) -> f32 {
    let e_svg = mix(a_svg, b_svg, t);
    // SVG → logo pixel space.  SVG viewBox is 256 units per edge.
    let e_px = e_svg * (logo_size / 256.0);
    let d = distance(logo_px, e_px);
    return exp(-d * d / (radius * radius));
}

// Sum of all electron glows for this fragment.
fn electron_pulses(logo_px: vec2<f32>, logo_size: f32, time: f32) -> f32 {
    let period: f32 = 3.0;   // seconds per full sweep of any given path
    let radius: f32 = 5.0;   // glow radius in display pixels

    // Phase offsets stagger the pulses through a common cycle so you
    // always see at least one mid-trace rather than all five starting
    // or ending together.
    let t0 = fract((time + 0.0) / period);
    let t1 = fract((time + 0.6) / period);
    let t2 = fract((time + 1.2) / period);
    let t3 = fract((time + 1.8) / period);
    let t4 = fract((time + 2.4) / period);

    var glow: f32 = 0.0;

    // 1. IC1 bottom-right pin down into IC2 — a prominent vertical trace
    //    in the middle-upper area.
    glow = glow + electron_glow(logo_px, logo_size,
        vec2<f32>(186.0, 118.0), vec2<f32>(186.0, 155.0), t0, radius);

    // 2. R2 → C1 horizontal feeder on the lower-left.
    glow = glow + electron_glow(logo_px, logo_size,
        vec2<f32>(44.0, 196.0), vec2<f32>(140.0, 196.0), t1, radius);

    // 3. Upper vertical trace on the right (travelling upward).
    glow = glow + electron_glow(logo_px, logo_size,
        vec2<f32>(228.0, 120.0), vec2<f32>(228.0, 78.0), t2, radius);

    // 4. Top meander from upper-left origin pad rightward to the
    //    capacitor C0.
    glow = glow + electron_glow(logo_px, logo_size,
        vec2<f32>(25.0, 28.0), vec2<f32>(100.0, 28.0), t3, radius);

    // 5. Right side of IC2, top-most pin out to its pad — short but
    //    close to the logo's visual anchor.
    glow = glow + electron_glow(logo_px, logo_size,
        vec2<f32>(222.0, 165.0), vec2<f32>(244.0, 165.0), t4, radius);

    return glow;
}
