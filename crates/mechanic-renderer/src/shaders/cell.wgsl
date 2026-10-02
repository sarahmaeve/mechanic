// Layout must match pipeline.rs::Globals (64 bytes).
struct Globals {
    viewport_size: vec2<f32>,
    cell_size: vec2<f32>,
    time: f32,
    content_opacity: f32,
    animation_flags: u32,
    text_opacity: f32,
    bloom_progress: f32,
    bloom_peak_multiplier: f32,
    logo_size: f32,
    logo_style: u32,
    pane_origin: vec2<f32>,
    padding: vec2<f32>,
}

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var atlas_texture: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;
@group(0) @binding(3) var logo_texture: texture_2d<f32>;

// use_atlas: 0 background, 1 glyph, 2/3 cursor, 4–7 decorations.
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
    @location(5) quad_local: vec2<f32>,
    @location(6) @interpolate(flat) quad_size: vec2<f32>,
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

    if inst.use_atlas != 0u {
        quad_origin = cell_origin + inst.glyph_offset;
        quad_size = inst.glyph_size;
    } else {
        quad_origin = cell_origin;
        quad_size = globals.cell_size;
    }

    let px = globals.pane_origin + quad_origin + lv * quad_size;

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
    out.quad_local = lv * quad_size;
    out.quad_size = quad_size;

    return out;
}

const HOLLOW_CURSOR_BORDER_PX: f32 = 1.5;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    if in.use_atlas >= 4u {
        var coverage = 1.0;
        let thickness = in.quad_size.y;
        if in.use_atlas == 5u {
            let x = (in.pixel_pos.x % (thickness * 3.0)) - thickness * 1.5;
            let y = in.quad_local.y - thickness * 0.5;
            coverage = 1.0 - smoothstep(thickness * 0.3, thickness * 0.7, length(vec2<f32>(x, y)));
        } else if in.use_atlas == 6u {
            coverage = 1.0 - smoothstep(thickness * 3.5, thickness * 4.5, in.pixel_pos.x % (thickness * 6.0));
        } else if in.use_atlas == 7u {
            let stroke = thickness / 3.0;
            let wave = thickness * 0.5 + sin(in.pixel_pos.x * 0.785398 / stroke) * stroke;
            coverage = 1.0 - smoothstep(stroke * 0.3, stroke * 0.7, abs(in.quad_local.y - wave));
        }
        return vec4<f32>(in.fg_color.rgb, coverage * globals.text_opacity);
    }
    if in.use_atlas == 2u {
        let cell_local = in.quad_local;
        let d_left = cell_local.x;
        let d_right = in.quad_size.x - cell_local.x;
        let d_top = cell_local.y;
        let d_bot = in.quad_size.y - cell_local.y;
        let min_edge = min(min(d_left, d_right), min(d_top, d_bot));
        if min_edge > HOLLOW_CURSOR_BORDER_PX {
            discard;
        }
        return vec4<f32>(in.bg_color.rgb, 1.0);
    }
    if in.use_atlas == 3u {
        return vec4<f32>(in.bg_color.rgb, 1.0);
    }
    if in.use_atlas == 1u {
        let glyph_coverage = textureSample(atlas_texture, atlas_sampler, in.uv).r;
        let effective_coverage = glyph_coverage * globals.text_opacity;
        return vec4<f32>(in.fg_color.rgb, effective_coverage);
    } else {
        var bg_rgb = in.bg_color.rgb;

        let uv_pos = in.pixel_pos / globals.viewport_size;
        let dist = length(uv_pos - vec2<f32>(1.0, 1.0));

        var breath = 1.0;
        var t = 0.5;
        if (globals.animation_flags & 1u) != 0u {
            breath += sin(globals.time * 2.1) * 0.18;
            t = sin(globals.time * 0.5) * 0.5 + 0.5;
        }
        let gradient_strength = exp(-dist * dist / 0.08) * 0.10 * breath;
        let gradient_r: f32 = mix(0.322, 0.0, t) * gradient_strength;
        let gradient_g: f32 = mix(0.910, 0.498, t) * gradient_strength;
        let gradient_b: f32 = gradient_strength;

        bg_rgb = bg_rgb + vec3<f32>(gradient_r, gradient_g, gradient_b);

        let logo_size = globals.logo_size;
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

            if globals.logo_style == 1u {
                var time = 0.0;
                if (globals.animation_flags & 2u) != 0u {
                    time = globals.time;
                }
                bg_rgb += vec3<f32>(0.85, 1.0, 1.0) * atom_effects(logo_uv * 256.0, time);
            } else if (globals.animation_flags & 2u) != 0u {
                let pulse_glow = triangle_effects(logo_px, logo_size, globals.time);
                bg_rgb += vec3<f32>(0.85, 1.0, 1.0) * pulse_glow;
            }
        }

        return vec4<f32>(bg_rgb, globals.content_opacity);
    }
}

// Vertices match assets/logo.svg. Travel is measured along all three edges.
fn triangle_path(t: f32, a: vec2<f32>, b: vec2<f32>, c: vec2<f32>) -> vec2<f32> {
    let ab = distance(a, b);
    let bc = distance(b, c);
    let ca = distance(c, a);
    let d = fract(t) * (ab + bc + ca);
    if d < ab {
        return mix(a, b, d / ab);
    } else if d < ab + bc {
        return mix(b, c, (d - ab) / bc);
    } else {
        return mix(c, a, (d - ab - bc) / ca);
    }
}

fn triangle_effects(logo_px: vec2<f32>, logo_size: f32, time: f32) -> f32 {
    let p = logo_px * (256.0 / logo_size);
    let outer = triangle_path(time / 4.0,
        vec2<f32>(24.0, 36.0), vec2<f32>(232.0, 36.0), vec2<f32>(128.0, 216.0));
    let inner = triangle_path(0.5 - time / 3.0,
        vec2<f32>(76.0, 66.0), vec2<f32>(180.0, 66.0), vec2<f32>(128.0, 156.0));
    let d_outer = p - outer;
    let d_inner = p - inner;
    let center = p - vec2<f32>(128.0, 96.0);
    let edge = min(p.y - 66.0, min(
        (90.0 * (p.x - 76.0) - 52.0 * (p.y - 66.0)) / 104.0,
        (90.0 * (180.0 - p.x) - 52.0 * (p.y - 66.0)) / 104.0));
    let glow = exp(-dot(center, center) / 700.0) * smoothstep(0.0, 10.0, edge)
        * (0.04 + 0.01 * sin(time * 1.2));
    return exp(-dot(d_outer, d_outer) / 12.0) + exp(-dot(d_inner, d_inner) / 12.0)
        + glow;
}

// Ellipse radii and rotations match assets/atom.svg.
fn electron_position(phase: f32, rotation: vec2<f32>) -> vec2<f32> {
    let p = vec2<f32>(104.0 * cos(phase), 38.0 * sin(phase));
    return vec2<f32>(rotation.x * p.x - rotation.y * p.y,
        rotation.y * p.x + rotation.x * p.y);
}

fn atom_effects(p: vec2<f32>, time: f32) -> f32 {
    let center = p - vec2<f32>(128.0);
    let electrons = electron_glow(center, time * 1.2, vec2<f32>(1.0, 0.0))
        + electron_glow(center, 2.1 - time, vec2<f32>(0.5, 0.8660254))
        + electron_glow(center, 4.2 + time * 0.85, vec2<f32>(0.5, -0.8660254));
    let nucleus = exp(-dot(center, center) / 160.0) * (0.06 + 0.015 * sin(time * 1.2));
    return electrons + nucleus;
}

fn electron_glow(center: vec2<f32>, phase: f32, rotation: vec2<f32>) -> f32 {
    let delta = center - electron_position(phase, rotation);
    let depth = sin(phase) * 0.5 + 0.5;
    let radius = mix(2.4, 4.0, depth);
    let distance_sq = dot(delta, delta);
    let halo = 0.22 * exp(-distance_sq / (radius * radius * 3.0));
    var sphere = 0.0;
    if distance_sq < (radius + 0.7) * (radius + 0.7) {
        let xy = delta / radius;
        let normal = vec3<f32>(xy, sqrt(max(0.0, 1.0 - dot(xy, xy))));
        let lighting = 0.25 + 0.75 * max(0.0, dot(normal, vec3<f32>(-0.4, -0.5, 0.768)));
        let coverage = 1.0 - smoothstep((radius - 0.7) * (radius - 0.7),
            (radius + 0.7) * (radius + 0.7), distance_sq);
        sphere = lighting * coverage;
    }
    return (sphere + halo) * mix(0.6, 1.0, depth);
}
