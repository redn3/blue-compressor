// Develop on the GPU (wgpu: Metal, Vulkan or DX12). Mirrors the CPU path in develop.rs and
// denoise.rs: noise reduction on the quad planes, debayer and color per output pixel, then
// sharpening, the user LUT and the output format (RGBA 8-bit or P010).

struct Shade {
    gains: vec4f,
    black: f32,
    inv_range: f32,
    mode: u32,
    view: u32,
    m_out: array<vec4f, 3>,
    m_709: array<vec4f, 3>,
    highlights: f32,
    shadows: f32,
    saturation: f32,
    color_boost: f32,
    contrast: f32,
    shoulder: f32,
    slope: f32,
    pad: f32,
}

struct Geo {
    w: u32, h: u32, ow: u32, oh: u32,
    // Crop origin and step in mosaic pixels (twice the quad size).
    ox: f32, oy: f32, sx: f32, sy: f32,
    // 1: full resolution debayer, 0: quad sampling (output at or below quad size).
    debayer: u32,
    // Sharpen amount (0 off), output format (0 RGBA8, 1 P010), matrix (0 BT.709, 1 BT.2020).
    sharpen: f32,
    format: u32,
    bt2020: u32,
    // LUT: 0 none, 1 1D, 2 3D; size; domain.
    lut_kind: u32,
    lut_size: u32,
    p0: u32, p1: u32,
    lut_min: vec4f,
    lut_max: vec4f,
}

struct Nr {
    w: u32, h: u32, radius: u32, horizontal: u32,
    k: f32, sigma_scale: f32, black: f32, a: f32,
    keep: f32, luma_on: u32, chroma_on: u32, pad: u32,
}

//#SECTION nr

@group(0) @binding(0) var<storage, read_write> planes: array<f32>;
@group(0) @binding(1) var<storage, read_write> nr_g: array<f32>;
@group(0) @binding(2) var<storage, read_write> nr_s: array<f32>;
@group(0) @binding(3) var<storage, read_write> nr_dg: array<f32>;
@group(0) @binding(4) var<storage, read_write> nr_cr: array<f32>;
@group(0) @binding(5) var<storage, read_write> nr_cb: array<f32>;
@group(0) @binding(6) var<uniform> nr: Nr;

const FLOOR: f32 = 32.0;

@compute @workgroup_size(256)
fn nr_prep(@builtin(global_invocation_id) id: vec3u) {
    let n = nr.w * nr.h;
    let i = id.x + id.y * 65535u * 256u;
    if (i >= n) { return; }
    let g1 = planes[i];
    let r = planes[n + i];
    let g2 = planes[2u * n + i];
    let b = planes[3u * n + i];
    let g = 0.5 * (g1 + g2);
    nr_g[i] = g;
    nr_s[i] = sqrt(nr.a * max(g - nr.black, FLOOR));
    nr_dg[i] = g1 - g2;
    nr_cr[i] = r - g;
    nr_cb[i] = b - g;
}

@compute @workgroup_size(256)
fn nr_merge(@builtin(global_invocation_id) id: vec3u) {
    let n = nr.w * nr.h;
    let i = id.x + id.y * 65535u * 256u;
    if (i >= n) { return; }
    _ = nr_s[i];
    let g = nr_g[i];
    let dg = nr_dg[i] * nr.keep;
    planes[i] = g + 0.5 * dg;
    planes[n + i] = g + nr_cr[i];
    planes[2u * n + i] = g - 0.5 * dg;
    planes[3u * n + i] = g + nr_cb[i];
}

//#SECTION bilateral
@group(0) @binding(0) var<storage, read> bl_src: array<f32>;
@group(0) @binding(1) var<storage, read_write> bl_dst: array<f32>;
@group(0) @binding(2) var<storage, read> bl_sigma: array<f32>;
@group(0) @binding(3) var<uniform> bl: Nr;

// One pass of the separable bilateral filter (see denoise.rs).
@compute @workgroup_size(16, 16)
fn bilateral(@builtin(global_invocation_id) id: vec3u) {
    if (id.x >= bl.w || id.y >= bl.h) { return; }
    let i = id.y * bl.w + id.x;
    let c = bl_src[i];
    let t = max(bl.k * bl_sigma[i] * bl.sigma_scale, 1e-3);
    let inv = 1.0 / (t * t);
    var sum = c;
    var wsum = 1.0;
    let rs = f32(bl.radius) * 0.6 + 0.5;
    for (var d = 1u; d <= bl.radius; d++) {
        let xs = f32(d) / rs;
        let sp = exp(-0.5 * xs * xs);
        for (var s = 0; s < 2; s++) {
            let dir = select(1, -1, s == 0);
            var j: u32;
            if (bl.horizontal != 0u) {
                let xx = clamp(i32(id.x) + dir * i32(d), 0, i32(bl.w) - 1);
                j = id.y * bl.w + u32(xx);
            } else {
                let yy = clamp(i32(id.y) + dir * i32(d), 0, i32(bl.h) - 1);
                j = u32(yy) * bl.w + id.x;
            }
            let v = bl_src[j];
            let dv = v - c;
            let q = 1.0 + dv * dv * inv;
            let wt = sp / (q * q);
            sum += wt * v;
            wsum += wt;
        }
    }
    bl_dst[i] = sum / wsum;
}

//#SECTION develop

@group(0) @binding(0) var<storage, read> dv_planes: array<f32>;
@group(0) @binding(1) var<storage, read_write> img: array<u32>;
@group(0) @binding(2) var<uniform> sh: Shade;
@group(0) @binding(3) var<uniform> geo: Geo;

fn sample_plane(c: u32, u0: f32, v0: f32) -> f32 {
    let w = geo.w;
    let h = geo.h;
    let u = clamp(u0, 0.0, f32(w - 1u));
    let v = clamp(v0, 0.0, f32(h - 1u));
    let x0 = u32(u);
    let y0 = u32(v);
    let x1 = min(x0 + 1u, w - 1u);
    let y1 = min(y0 + 1u, h - 1u);
    let fx = u - f32(x0);
    let fy = v - f32(y0);
    let o = c * w * h;
    let a = dv_planes[o + y0 * w + x0] + (dv_planes[o + y0 * w + x1] - dv_planes[o + y0 * w + x0]) * fx;
    let b = dv_planes[o + y1 * w + x0] + (dv_planes[o + y1 * w + x1] - dv_planes[o + y1 * w + x0]) * fx;
    return a + (b - a) * fy;
}

fn norm(x: f32) -> f32 {
    return max((x - sh.black) * sh.inv_range, 0.0);
}

fn sstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = clamp((x - e0) / (e1 - e0), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

fn look(v: vec3f) -> vec3f {
    let l = max(0.2627 * v.x + 0.678 * v.y + 0.0593 * v.z, 1e-7);
    let ev = log2(l / 0.18);
    var ev2 = ev + sh.highlights * 2.0 * sstep(0.0, 4.0, ev) + sh.shadows * 2.0 * sstep(0.0, -6.0, ev);
    ev2 *= 1.0 + 0.5 * sh.contrast;
    let k = exp2(ev2 - ev);
    let l2 = l * k;
    let chroma = max(max(abs(v.x - l), abs(v.y - l)), abs(v.z - l)) / l;
    let sat = (1.0 + sh.saturation) * (1.0 + sh.color_boost * (1.0 - min(chroma * 2.0, 1.0)));
    return vec3f(l2) + (v * k - vec3f(l2)) * sat;
}

fn log3g10(x0: f32) -> f32 {
    let x = x0 + 0.01;
    if (x < 0.0) { return x * 15.1927; }
    return 0.224282 * log2(x * 155.97533 + 1.0) * 0.30102999566;
}

fn nlog(x: f32) -> f32 {
    if (x < 0.328) { return 650.0 * pow(max(x + 0.0075, 0.0), 1.0 / 3.0) / 1023.0; }
    return (150.0 * log(x) + 619.0) / 1023.0;
}

fn rec709(x0: f32) -> f32 {
    let x = 0.18 * pow(max(x0, 0.0) / 0.18, sh.slope) * 1.1;
    var y = x;
    if (sh.shoulder > 0.0) {
        y = x * (1.0 + x / (sh.shoulder * sh.shoulder)) / (1.0 + x) * min(1.0 + 1.0 / sh.shoulder, 2.0);
    }
    return pow(clamp(y, 0.0, 1.0), 1.0 / 2.4);
}

fn shade_pixel(cam: vec3f) -> vec3f {
    let w = clamp(cam * sh.gains.xyz, vec3f(0.0), vec3f(1.0));
    if (sh.mode == 2u || sh.view != 0u) {
        let lin = vec3f(dot(sh.m_709[0].xyz, w), dot(sh.m_709[1].xyz, w), dot(sh.m_709[2].xyz, w));
        let t = look(lin);
        return vec3f(rec709(t.x), rec709(t.y), rec709(t.z));
    }
    let lin = vec3f(dot(sh.m_out[0].xyz, w), dot(sh.m_out[1].xyz, w), dot(sh.m_out[2].xyz, w));
    let t = look(lin);
    if (sh.mode == 0u) { return vec3f(log3g10(t.x), log3g10(t.y), log3g10(t.z)); }
    return vec3f(nlog(t.x), nlog(t.y), nlog(t.z));
}

// RGGB phases of the planes G1, R, G2, B inside the 2x2 quad.
const PX = array<f32, 4>(1.0, 0.0, 0.0, 1.0);
const PY = array<f32, 4>(0.0, 0.0, 1.0, 1.0);

// Camera RGB (0..1 of the sensor range) at mosaic position (mx, my).
fn camera(mx: f32, my: f32) -> vec3f {
    if (geo.debayer == 0u) {
        let u = mx * 0.5 - 0.5;
        let v = my * 0.5 - 0.5;
        return vec3f(norm(sample_plane(1u, u, v)), 0.5 * (norm(sample_plane(0u, u, v)) + norm(sample_plane(2u, u, v))), norm(sample_plane(3u, u, v)));
    }
    // Full resolution: green from both green sites, red and blue as color differences
    // against the green of their own quads, each at its own phase.
    var q: array<vec2f, 4>;
    for (var c = 0u; c < 4u; c++) { q[c] = vec2f((mx - 0.5 - PX[c]) * 0.5, (my - 0.5 - PY[c]) * 0.5); }
    let g = 0.5 * (sample_plane(0u, q[0].x, q[0].y) + sample_plane(2u, q[2].x, q[2].y));
    let gr = 0.5 * (sample_plane(0u, q[1].x, q[1].y) + sample_plane(2u, q[1].x, q[1].y));
    let gb = 0.5 * (sample_plane(0u, q[3].x, q[3].y) + sample_plane(2u, q[3].x, q[3].y));
    let r = g + sample_plane(1u, q[1].x, q[1].y) - gr;
    let b = g + sample_plane(3u, q[3].x, q[3].y) - gb;
    return vec3f(norm(r), norm(g), norm(b));
}

@compute @workgroup_size(16, 16)
fn develop(@builtin(global_invocation_id) id: vec3u) {
    if (id.x >= geo.ow || id.y >= geo.oh) { return; }
    let mx = geo.ox + (f32(id.x) + 0.5) * geo.sx;
    let my = geo.oy + (f32(id.y) + 0.5) * geo.sy;
    let px = clamp(shade_pixel(camera(mx, my)), vec3f(0.0), vec3f(1.0));
    let i = (id.y * geo.ow + id.x) * 2u;
    img[i] = pack2x16unorm(px.xy);
    img[i + 1u] = pack2x16unorm(vec2f(px.z, 0.0));
}

//#SECTION finish

@group(0) @binding(0) var<storage, read> fin_img: array<u32>;
@group(0) @binding(1) var<storage, read_write> out: array<u32>;
@group(0) @binding(2) var<storage, read> lut: array<vec4f>;
@group(0) @binding(3) var<uniform> fg: Geo;

fn px_at(x: u32, y: u32) -> vec3f {
    let i = (y * fg.ow + x) * 2u;
    return vec3f(unpack2x16unorm(fin_img[i]), unpack2x16unorm(fin_img[i + 1u]).x);
}

fn lut1(i: u32, k: u32) -> f32 { return lut[i][k]; }

fn apply_lut(v: vec3f) -> vec3f {
    if (fg.lut_kind == 0u) { return v; }
    let n = fg.lut_size;
    let p = clamp((v - fg.lut_min.xyz) / (fg.lut_max.xyz - fg.lut_min.xyz), vec3f(0.0), vec3f(1.0)) * f32(n - 1u);
    let i0 = min(vec3u(p), vec3u(n - 2u));
    let f = p - vec3f(i0);
    if (fg.lut_kind == 1u) {
        var o: vec3f;
        for (var k = 0u; k < 3u; k++) {
            o[k] = mix(lut1(i0[k], k), lut1(i0[k] + 1u, k), f[k]);
        }
        return o;
    }
    let s1 = n;
    let s2 = n * n;
    let b = i0.z * s2 + i0.y * s1 + i0.x;
    let c00 = mix(lut[b].xyz, lut[b + 1u].xyz, f.x);
    let c10 = mix(lut[b + s1].xyz, lut[b + s1 + 1u].xyz, f.x);
    let c01 = mix(lut[b + s2].xyz, lut[b + s2 + 1u].xyz, f.x);
    let c11 = mix(lut[b + s2 + s1].xyz, lut[b + s2 + s1 + 1u].xyz, f.x);
    return mix(mix(c00, c10, f.y), mix(c01, c11, f.y), f.z);
}

fn finished(x: u32, y: u32) -> vec3f {
    var c = px_at(x, y);
    if (fg.sharpen != 0.0 && fg.ow >= 3u && fg.oh >= 3u) {
        let x0 = select(x - 1u, 0u, x == 0u);
        let x1 = min(x + 1u, fg.ow - 1u);
        let y0 = select(y - 1u, 0u, y == 0u);
        let y1 = min(y + 1u, fg.oh - 1u);
        let blur = (px_at(x0, y) + px_at(x1, y) + px_at(x, y0) + px_at(x, y1) + 4.0 * c) / 8.0;
        c = clamp(c + (c - blur) * fg.sharpen * 2.0, vec3f(0.0), vec3f(1.0));
    }
    return clamp(apply_lut(c), vec3f(0.0), vec3f(1.0));
}

@compute @workgroup_size(16, 16)
fn finish_rgba(@builtin(global_invocation_id) id: vec3u) {
    if (id.x >= fg.ow || id.y >= fg.oh) { return; }
    let c = finished(id.x, id.y);
    out[id.y * fg.ow + id.x] = pack4x8unorm(vec4f(c, 1.0));
}

// One thread per 2x2 block: four Y samples and one Cb Cr pair, 10-bit limited range in
// the top bits of 16 (P010).
@compute @workgroup_size(16, 16)
fn finish_p010(@builtin(global_invocation_id) id: vec3u) {
    let bw = fg.ow / 2u;
    let bh = fg.oh / 2u;
    if (id.x >= bw || id.y >= bh) { return; }
    var kr = 0.2126;
    var kb = 0.0722;
    if (fg.bt2020 != 0u) { kr = 0.2627; kb = 0.0593; }
    let kg = 1.0 - kr - kb;
    var cb = 0.0;
    var cr = 0.0;
    for (var dy = 0u; dy < 2u; dy++) {
        var ys: array<u32, 2>;
        for (var dx = 0u; dx < 2u; dx++) {
            let c = finished(id.x * 2u + dx, id.y * 2u + dy);
            let l = kr * c.x + kg * c.y + kb * c.z;
            ys[dx] = u32(clamp(round(64.0 + 876.0 * l), 0.0, 1023.0)) << 6u;
            cb += (c.z - l) * 0.5 / (1.0 - kb);
            cr += (c.x - l) * 0.5 / (1.0 - kr);
        }
        out[(id.y * 2u + dy) * bw + id.x] = ys[0] | (ys[1] << 16u);
    }
    let ucb = u32(clamp(round(512.0 + 896.0 * cb / 4.0), 0.0, 1023.0)) << 6u;
    let ucr = u32(clamp(round(512.0 + 896.0 * cr / 4.0), 0.0, 1023.0)) << 6u;
    out[fg.oh * bw + id.y * bw + id.x] = ucb | (ucr << 16u);
}
