// Layout must match pipeline.rs::Globals (48 bytes).
struct Globals {
    viewport_size: vec2<f32>,
    cell_size: vec2<f32>,
    time: f32,
    content_opacity: f32,
    shader_focused: f32,
    text_opacity: f32,
    bloom_progress: f32,
    bloom_peak_multiplier: f32,
    _pad0: f32,
    _pad1: f32,
}

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var atlas_texture: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;
@group(0) @binding(3) var logo_texture: texture_2d<f32>;

// Attribute locations match GpuInstance; use_atlas: 0 solid, 1 glyph, 2 hollow cursor.
struct Instance {
    @location(0) cell_pos: vec2<u32>,
    @location(1) atlas_uv: vec4<f32>,
    @location(2) fg_color: vec4<f32>,
    @location(3) bg_color: vec4<f32>,
    @location(4) glyph_offset: vec2<f32>,
    @location(5) glyph_size: vec2<f32>,
    @location(6) use_atlas: u32,
}

struct VertexOutput {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) fg_color: vec4<f32>,
    @location(2) bg_color: vec4<f32>,
    @location(3) @interpolate(flat) use_atlas: u32,
    @location(4) pixel_pos: vec2<f32>,  // pixel position for gradient
}

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

    let cell_origin = vec2<f32>(
        f32(inst.cell_pos.x) * globals.cell_size.x,
        f32(inst.cell_pos.y) * globals.cell_size.y,
    );

    var quad_origin: vec2<f32>;
    var quad_size: vec2<f32>;

    if inst.use_atlas == 1u {
        quad_origin = cell_origin + inst.glyph_offset;
        quad_size = inst.glyph_size;
    } else {
        quad_origin = cell_origin;
        quad_size = globals.cell_size;
    }

    let px = quad_origin + lv * quad_size;

    let ndc = vec2<f32>(
         2.0 * px.x / globals.viewport_size.x - 1.0,
        -2.0 * px.y / globals.viewport_size.y + 1.0,
    );

    out.clip_pos = vec4<f32>(ndc, 0.0, 1.0);

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

const HOLLOW_CURSOR_BORDER_PX: f32 = 1.5;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    if in.use_atlas == 2u {
        let cell_local = fract(in.pixel_pos / globals.cell_size) * globals.cell_size;
        let d_left = cell_local.x;
        let d_right = globals.cell_size.x - cell_local.x;
        let d_top = cell_local.y;
        let d_bot = globals.cell_size.y - cell_local.y;
        let min_edge = min(min(d_left, d_right), min(d_top, d_bot));
        if min_edge > HOLLOW_CURSOR_BORDER_PX {
            discard;
        }
        return vec4<f32>(in.bg_color.rgb, globals.content_opacity);
    }
    if in.use_atlas == 1u {
        let glyph_coverage = textureSample(atlas_texture, atlas_sampler, in.uv).r;
        let effective_coverage = glyph_coverage * globals.text_opacity;
        let color_rgb = mix(in.bg_color.rgb, in.fg_color.rgb, effective_coverage);
        return vec4<f32>(color_rgb, globals.content_opacity);
    } else {
        var bg_rgb = in.bg_color.rgb;

        let uv_pos = in.pixel_pos / globals.viewport_size;
        let dist = length(uv_pos - vec2<f32>(1.0, 1.0));

        let breath: f32 = 1.0 + sin(globals.time * 2.1) * 0.18 * globals.shader_focused;
        let gradient_strength = exp(-dist * dist / 0.08) * 0.10 * breath;

        let phase: f32 = globals.time * 0.5 * globals.shader_focused;
        let t: f32 = sin(phase) * 0.5 + 0.5;
        let gradient_r: f32 = mix(0.322, 0.0, t) * gradient_strength;
        let gradient_g: f32 = mix(0.910, 0.498, t) * gradient_strength;
        let gradient_b: f32 = gradient_strength;

        bg_rgb = bg_rgb + vec3<f32>(gradient_r, gradient_g, gradient_b);

        let logo_size: f32 = 270.0;   // display size in physical pixels
        let logo_margin: f32 = 16.0;  // inset from the corner
        let logo_opacity_base: f32 = 0.40;
        let bloom_env: f32 = sin(globals.bloom_progress * 3.14159265);
        let bloom_lift: f32 = mix(1.0, globals.bloom_peak_multiplier, bloom_env);
        let logo_opacity: f32 = logo_opacity_base * bloom_lift;

        let logo_br = globals.viewport_size - vec2<f32>(logo_margin, logo_margin);
        let logo_tl = logo_br - vec2<f32>(logo_size, logo_size);
        let logo_px = in.pixel_pos - logo_tl;

        if logo_px.x >= 0.0 && logo_px.x < logo_size
            && logo_px.y >= 0.0 && logo_px.y < logo_size {
            let logo_uv = logo_px / logo_size;
            let logo = textureSample(logo_texture, atlas_sampler, logo_uv);
            let a = logo.a * logo_opacity;
            bg_rgb = logo.rgb * logo_opacity + bg_rgb * (1.0 - a);

            let pulse_glow = electron_pulses(logo_px, logo_size, globals.time)
                * globals.shader_focused;
            let electron_color = vec3<f32>(0.85, 1.0, 1.0);
            bg_rgb = bg_rgb + electron_color * pulse_glow;
        }

        return vec4<f32>(bg_rgb, globals.content_opacity);
    }
}

fn poly4_pos(
    t: f32,
    p0: vec2<f32>,
    p1: vec2<f32>,
    p2: vec2<f32>,
    p3: vec2<f32>,
) -> vec2<f32> {
    let l0 = distance(p0, p1);
    let l1 = distance(p1, p2);
    let l2 = distance(p2, p3);
    let total = l0 + l1 + l2;
    let target_len = t * total;
    if target_len < l0 {
        return mix(p0, p1, target_len / l0);
    } else if target_len < l0 + l1 {
        return mix(p1, p2, (target_len - l0) / l1);
    } else {
        return mix(p2, p3, (target_len - l0 - l1) / l2);
    }
}

fn poly5_pos(
    t: f32,
    p0: vec2<f32>,
    p1: vec2<f32>,
    p2: vec2<f32>,
    p3: vec2<f32>,
    p4: vec2<f32>,
) -> vec2<f32> {
    let l0 = distance(p0, p1);
    let l1 = distance(p1, p2);
    let l2 = distance(p2, p3);
    let l3 = distance(p3, p4);
    let total = l0 + l1 + l2 + l3;
    let target_len = t * total;
    if target_len < l0 {
        return mix(p0, p1, target_len / l0);
    } else if target_len < l0 + l1 {
        return mix(p1, p2, (target_len - l0) / l1);
    } else if target_len < l0 + l1 + l2 {
        return mix(p2, p3, (target_len - l0 - l1) / l2);
    } else {
        return mix(p3, p4, (target_len - l0 - l1 - l2) / l3);
    }
}

fn path_position(id: u32, t: f32) -> vec2<f32> {
    switch id {
        case 0u: {
            return poly4_pos(t,
                vec2<f32>( 25.0,  28.0),
                vec2<f32>( 78.0,  28.0),
                vec2<f32>( 78.0,  56.0),
                vec2<f32>(142.0,  56.0));
        }
        case 1u: {
            return mix(vec2<f32>(170.0, 34.0), vec2<f32>(170.0, 70.0), t);
        }
        case 2u: {
            return poly5_pos(t,
                vec2<f32>(158.0, 118.0),
                vec2<f32>(158.0, 142.0),
                vec2<f32>(130.0, 142.0),
                vec2<f32>(130.0, 170.0),
                vec2<f32>(140.0, 170.0));
        }
        case 3u: {
            return mix(vec2<f32>(186.0, 118.0), vec2<f32>(186.0, 155.0), t);
        }
        case 4u: {
            return mix(vec2<f32>(44.0, 124.0), vec2<f32>(44.0, 196.0), t);
        }
        case 5u: {
            return mix(vec2<f32>(44.0, 196.0), vec2<f32>(140.0, 196.0), t);
        }
        case 6u: {
            return poly5_pos(t,
                vec2<f32>(216.0,  78.0),
                vec2<f32>(228.0,  78.0),
                vec2<f32>(228.0, 120.0),
                vec2<f32>(210.0, 120.0),
                vec2<f32>(210.0, 130.0));
        }
        case 7u: {
            return mix(vec2<f32>(222.0, 165.0), vec2<f32>(244.0, 165.0), t);
        }
        default: {
            return mix(vec2<f32>(222.0, 192.0), vec2<f32>(244.0, 192.0), t);
        }
    }
}

fn electron_pulses(logo_px: vec2<f32>, logo_size: f32, time: f32) -> f32 {
    let radius: f32 = 5.0;
    let num_paths: u32 = 9u;

    var periods: array<f32, 5> = array<f32, 5>(2.3, 2.9, 3.4, 2.7, 3.1);
    var phases: array<f32, 5> = array<f32, 5>(0.0, 0.7, 1.3, 1.9, 2.5);

    var glow: f32 = 0.0;
    let scale = logo_size / 256.0;

    for (var i: u32 = 0u; i < 5u; i++) {
        let period = periods[i];
        let phase = phases[i];
        let t_raw = (time + phase) / period;
        let cycle = u32(t_raw);
        let t = fract(t_raw);

        let path_id = (cycle * 5u + i * 11u) % num_paths;

        let e_svg = path_position(path_id, t);
        let e_px = e_svg * scale;
        let d = distance(logo_px, e_px);
        glow = glow + exp(-d * d / (radius * radius));
    }

    return glow;
}
