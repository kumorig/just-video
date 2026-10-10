// Just Video: per-pixel ray casting from each eye into the video's projection.
// One fullscreen triangle per eye; no meshes, so projections are exact.

const PI: f32 = 3.14159265358979;

struct Eye {
    // View-to-world rotation (columns).
    rot0: vec4<f32>,
    rot1: vec4<f32>,
    rot2: vec4<f32>,
    // tan(angle) of the eye's field of view: left, right, down, up.
    tan: vec4<f32>,
    // xyz: eye position in the reference space; w: eye index (0 left, 1 right).
    origin: vec4<f32>,
    // x: projection (0 flat, 1 equirect 180, 2 equirect 360, 3 fisheye, 4 curved screen),
    // y: stereo (0 mono, 1 side-by-side, 2 top-bottom), z: swap eyes, w: fisheye FOV (rad).
    mode: vec4<f32>,
    // Flat screen: x width, y height, z distance, w center height (metres).
    // Spherical video: w is the viewpoint's offset along -Z inside the
    // sphere, as a fraction of its radius (0 = centre).
    screen: vec4<f32>,
    // xy: luma texture size in texels; z: debug view (0 off, 1 projection UV, 2 raw luma);
    // w: zoom (magnification for spherical and fisheye video).
    tex: vec4<f32>,
}

// YUV -> R'G'B' as affine rows (xyz: coefficients, w: offset), then
// params: x sample scale, y semi-planar chroma, z BT.2020 primaries, w transfer (0 SDR, 1 PQ, 2 HLG).
// adjust: x brightness, y contrast, z saturation, w quarter turns clockwise.
struct Color {
    m0: vec4<f32>,
    m1: vec4<f32>,
    m2: vec4<f32>,
    params: vec4<f32>,
    adjust: vec4<f32>,
}

var<immediate> eye: Eye;
@group(0) @binding(0) var samp: sampler;
@group(0) @binding(1) var tex_y: texture_2d<f32>;
@group(0) @binding(2) var tex_u: texture_2d<f32>; // U plane, or interleaved UV
@group(0) @binding(3) var tex_v: texture_2d<f32>;
@group(0) @binding(4) var<uniform> color: Color;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) ndc: vec2<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u)) * 2.0 - 1.0;
    var out: VsOut;
    out.pos = vec4<f32>(p, 0.0, 1.0);
    out.ndc = p;
    return out;
}

// Maps a direction/ray to video UV in [0,1]^2 for one eye's image. Returns
// (uv, inside): `inside` is a signed distance in UV units (positive inside
// the video), continuous across the edge so it can be anti-aliased. No
// per-pixel early returns: the caller takes screen-space derivatives.
fn project(origin: vec3<f32>, d: vec3<f32>) -> vec3<f32> {
    let kind = u32(eye.mode.x);
    if kind == 0u {
        // Flat screen facing +Z at distance `screen.z`; rays start at the eye.
        let dz = min(d.z, -1e-4);
        let t = (-eye.screen.z - origin.z) / dz;
        let p = origin + t * d;
        let uv = vec2<f32>(p.x / eye.screen.x + 0.5, 0.5 - (p.y - eye.screen.w) / eye.screen.y);
        return vec3<f32>(uv, select(-1.0, rect_inside(uv), d.z < -1e-4));
    }
    if kind == 4u {
        // Curved screen: a cylinder of radius `screen.z` around the viewer,
        // `screen.x` metres of arc wide. Rays start inside it: take the far hit.
        let r = eye.screen.z;
        let a = max(d.x * d.x + d.z * d.z, 1e-6);
        let b = 2.0 * (origin.x * d.x + origin.z * d.z);
        let c = origin.x * origin.x + origin.z * origin.z - r * r;
        let disc = max(b * b - 4.0 * a * c, 0.0);
        let t = (-b + sqrt(disc)) / (2.0 * a);
        let p = origin + t * d;
        let arc = atan2(p.x, -p.z) * r;
        let uv = vec2<f32>(arc / eye.screen.x + 0.5, 0.5 - (p.y - eye.screen.w) / eye.screen.y);
        return vec3<f32>(uv, select(-1.0, rect_inside(uv), p.z < 0.0));
    }
    // Spherical video is a unit sphere around the viewer, locked to head
    // rotation (the eye's position is ignored). Moving the viewpoint along
    // -Z inside it (screen.w) makes the picture ahead grow and its edge
    // slide behind, like leaning towards a screen.
    let k = eye.screen.w;
    let o = vec3<f32>(0.0, 0.0, -k);
    let od = dot(o, d);
    // The far hit; |k| < 1 keeps the root real and t positive.
    let t = -od + sqrt(max(od * od - k * k + 1.0, 0.0));
    let n = normalize(o + t * d);
    // Zooming a spherical video magnifies it around the view centre.
    let zoom = select(eye.tex.w, 1.0, eye.tex.w <= 0.0);
    if kind == 3u {
        // Equidistant fisheye centred on -Z.
        let theta = acos(clamp(-n.z, -1.0, 1.0)) / zoom;
        let r = theta / (eye.mode.w * 0.5) * 0.5;
        let phi = atan2(n.y, n.x);
        return vec3<f32>(0.5 + r * cos(phi), 0.5 - r * sin(phi), 0.5 - r);
    }
    let lon = atan2(n.x, -n.z) / zoom;
    let lat = asin(clamp(n.y, -1.0, 1.0)) / zoom;
    let v = 0.5 - lat / PI;
    if kind == 1u {
        let uv = vec2<f32>(lon / PI + 0.5, v);
        return vec3<f32>(uv, rect_inside(uv));
    }
    return vec3<f32>(lon / (2.0 * PI) + 0.5, v, 1.0);
}

// Signed distance (UV units) inside the unit square.
fn rect_inside(uv: vec2<f32>) -> f32 {
    let e = vec2<f32>(0.5) - abs(uv - vec2<f32>(0.5));
    return min(e.x, e.y);
}

// One bilinear YUV sample.
fn sample_yuv(uv: vec2<f32>) -> vec3<f32> {
    let scale = color.params.x;
    let y = textureSampleLevel(tex_y, samp, uv, 0.0).r * scale;
    if color.params.y > 0.5 {
        let c = textureSampleLevel(tex_u, samp, uv, 0.0).rg * scale;
        return vec3<f32>(y, c.x, c.y);
    }
    return vec3<f32>(
        y,
        textureSampleLevel(tex_u, samp, uv, 0.0).r * scale,
        textureSampleLevel(tex_v, samp, uv, 0.0).r * scale,
    );
}

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + 0.055) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

fn pq_to_linear(c: vec3<f32>) -> vec3<f32> {
    let m1 = 0.1593017578125;
    let m2 = 78.84375;
    let c1 = 0.8359375;
    let c2 = 18.8515625;
    let c3 = 18.6875;
    let p = pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0 / m2));
    let nits = 10000.0 * pow(max(p - c1, vec3<f32>(0.0)) / (c2 - c3 * p), vec3<f32>(1.0 / m1));
    // Reference white 203 nits -> 1.0, then a soft shoulder for highlights.
    let x = nits / 203.0;
    return x * (1.0 + x / 16.0) / (1.0 + x);
}

fn hlg_to_linear(c: vec3<f32>) -> vec3<f32> {
    let a = 0.17883277;
    let b = 0.28466892;
    let cc = 0.55991073;
    let lo = c * c / 3.0;
    let hi = (exp((c - cc) / a) + b) / 12.0;
    let scene = select(hi, lo, c <= vec3<f32>(0.5));
    return pow(scene, vec3<f32>(1.2)) * 3.0;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Vulkan NDC: y = -1 is the top of the image.
    let tx = mix(eye.tan.x, eye.tan.y, in.ndc.x * 0.5 + 0.5);
    let ty = mix(eye.tan.w, eye.tan.z, in.ndc.y * 0.5 + 0.5);
    let rot = mat3x3<f32>(eye.rot0.xyz, eye.rot1.xyz, eye.rot2.xyz);
    let d = normalize(rot * vec3<f32>(tx, ty, -1.0));
    let hit = project(eye.origin.xyz, d);
    // Derivatives come first, while control flow is still uniform: the
    // video's edge fades over one output pixel instead of stair-stepping.
    let coverage = clamp(hit.z / max(fwidth(hit.z), 1e-6) + 0.5, 0.0, 1.0);
    let debug = u32(eye.tex.z);
    if debug == 1u {
        // Red/green = projected UV, blue = inside the video, dim grey = view direction.
        let inside = step(0.0, hit.z);
        return vec4<f32>(hit.x * inside, hit.y * inside, inside, 1.0) * 0.8 + vec4<f32>(abs(d) * 0.2, 0.0);
    }

    // Select this eye's part of the frame; stay half a chroma texel inside it
    // so the other eye's image never bleeds in.
    var index = eye.origin.w;
    if eye.mode.z > 0.5 {
        index = 1.0 - index;
    }
    // Rotated video: turn the eye's image within its part of the frame.
    var st = clamp(hit.xy, vec2<f32>(0.0), vec2<f32>(1.0));
    let turns = u32(color.adjust.w + 0.5) % 4u;
    if turns == 1u {
        st = vec2<f32>(st.y, 1.0 - st.x);
    } else if turns == 2u {
        st = vec2<f32>(1.0 - st.x, 1.0 - st.y);
    } else if turns == 3u {
        st = vec2<f32>(1.0 - st.y, st.x);
    }
    let stereo = u32(eye.mode.y);
    var rect = vec4<f32>(0.0, 0.0, 1.0, 1.0);
    if stereo == 1u {
        rect = vec4<f32>(index * 0.5, 0.0, index * 0.5 + 0.5, 1.0);
    } else if stereo == 2u {
        rect = vec4<f32>(0.0, index * 0.5, 1.0, index * 0.5 + 0.5);
    }
    let margin = vec2<f32>(1.0) / eye.tex.xy;
    let lo = rect.xy + margin;
    let hi = rect.zw - margin;
    let uv = clamp(mix(rect.xy, rect.zw, st), lo, hi);
    // Texels covered by this output pixel (still uniform control flow).
    let duv = fwidth(uv);
    let footprint = max(duv.x * eye.tex.x, duv.y * eye.tex.y);
    if coverage <= 0.0 {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }

    // Minified video (8K on a 2K eye) shimmers with one bilinear sample per
    // pixel; four samples spread over the pixel's footprint average it out.
    var yuv: vec3<f32>;
    if footprint > 1.3 {
        let o = duv * 0.25;
        yuv = (sample_yuv(clamp(uv + vec2<f32>(-o.x, -o.y), lo, hi))
            + sample_yuv(clamp(uv + vec2<f32>(o.x, -o.y), lo, hi))
            + sample_yuv(clamp(uv + vec2<f32>(-o.x, o.y), lo, hi))
            + sample_yuv(clamp(uv + vec2<f32>(o.x, o.y), lo, hi))) * 0.25;
    } else {
        yuv = sample_yuv(uv);
    }
    let y = yuv.x;
    let cb = yuv.y;
    let cr = yuv.z;
    if debug == 2u {
        return vec4<f32>(y, cb, cr, 1.0);
    }
    var encoded = clamp(
        vec3<f32>(dot(color.m0.xyz, yuv) + color.m0.w, dot(color.m1.xyz, yuv) + color.m1.w, dot(color.m2.xyz, yuv) + color.m2.w),
        vec3<f32>(0.0),
        vec3<f32>(1.0),
    );
    // Picture corrections, on the encoded (perceptual) values.
    let luma = dot(encoded, vec3<f32>(0.2126, 0.7152, 0.0722));
    encoded = mix(vec3<f32>(luma), encoded, color.adjust.z);
    encoded = clamp((encoded - 0.5) * color.adjust.y + 0.5 + color.adjust.x, vec3<f32>(0.0), vec3<f32>(1.0));
    var linear: vec3<f32>;
    let transfer = u32(color.params.w);
    if transfer == 1u {
        linear = pq_to_linear(encoded);
    } else if transfer == 2u {
        linear = hlg_to_linear(encoded);
    } else {
        linear = srgb_to_linear(encoded);
    }
    if color.params.z > 0.5 {
        // BT.2020 -> BT.709 primaries (linear light).
        linear = mat3x3<f32>(
            vec3<f32>(1.6605, -0.1246, -0.0182),
            vec3<f32>(-0.5876, 1.1329, -0.1006),
            vec3<f32>(-0.0728, -0.0083, 1.1187),
        ) * linear;
    }
    // The swapchain is sRGB: the hardware encodes this linear value. The
    // edge fades to the black surround.
    return vec4<f32>(clamp(linear, vec3<f32>(0.0), vec3<f32>(1.0)) * coverage, 1.0);
}
