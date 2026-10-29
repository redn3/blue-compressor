// Develop pass: one thread per output pixel. Mirrors shade_pixel in develop.rs.
#include <metal_stdlib>
using namespace metal;

struct Shade {
    float4 gains;
    float black;
    float inv_range;
    uint mode;
    uint view;
    float4 m_out[3];
    float4 m_709[3];
    float highlights;
    float shadows;
    float saturation;
    float color_boost;
    float contrast;
    float shoulder;
    float slope;
    float pad;
};

struct Geo {
    uint w, h, ow, oh;
    float ox, oy, sx, sy;
};

static float sample_plane(device const float* p, uint w, uint h, float u, float v) {
    u = clamp(u, 0.0f, float(w - 1));
    v = clamp(v, 0.0f, float(h - 1));
    uint x0 = uint(u), y0 = uint(v);
    uint x1 = min(x0 + 1, w - 1), y1 = min(y0 + 1, h - 1);
    float fx = u - float(x0), fy = v - float(y0);
    float a = p[y0 * w + x0] + (p[y0 * w + x1] - p[y0 * w + x0]) * fx;
    float b = p[y1 * w + x0] + (p[y1 * w + x1] - p[y1 * w + x0]) * fx;
    return a + (b - a) * fy;
}

static float sstep(float e0, float e1, float x) {
    float t = clamp((x - e0) / (e1 - e0), 0.0f, 1.0f);
    return t * t * (3.0f - 2.0f * t);
}

static float3 look(constant Shade& s, float3 v) {
    float l = max(0.2627f * v.x + 0.678f * v.y + 0.0593f * v.z, 1e-7f);
    float ev = log2(l / 0.18f);
    float ev2 = ev + s.highlights * 2.0f * sstep(0.0f, 4.0f, ev) + s.shadows * 2.0f * sstep(0.0f, -6.0f, ev);
    ev2 *= 1.0f + 0.5f * s.contrast;
    float k = exp2(ev2 - ev);
    float l2 = l * k;
    float chroma = max(max(fabs(v.x - l), fabs(v.y - l)), fabs(v.z - l)) / l;
    float sat = (1.0f + s.saturation) * (1.0f + s.color_boost * (1.0f - min(chroma * 2.0f, 1.0f)));
    return l2 + (v * k - l2) * sat;
}

static float log3g10(float x) {
    x += 0.01f;
    return x < 0.0f ? x * 15.1927f : 0.224282f * log10(x * 155.97533f + 1.0f);
}

static float nlog(float x) {
    return x < 0.328f ? 650.0f * pow(max(x + 0.0075f, 0.0f), 1.0f / 3.0f) / 1023.0f : (150.0f * log(x) + 619.0f) / 1023.0f;
}

static float rec709(constant Shade& s, float x) {
    x = 0.18f * pow(max(x, 0.0f) / 0.18f, s.slope) * 1.1f;
    float y = s.shoulder > 0.0f ? x * (1.0f + x / (s.shoulder * s.shoulder)) / (1.0f + x) * min(1.0f + 1.0f / s.shoulder, 2.0f) : x;
    return pow(clamp(y, 0.0f, 1.0f), 1.0f / 2.4f);
}

kernel void develop(device const float* planes [[buffer(0)]],
                    device ushort* out [[buffer(1)]],
                    constant Shade& s [[buffer(2)]],
                    constant Geo& g [[buffer(3)]],
                    uint2 id [[thread_position_in_grid]]) {
    if (id.x >= g.ow || id.y >= g.oh) return;
    uint n = g.w * g.h;
    float u = g.ox + (float(id.x) + 0.5f) * g.sx - 0.5f;
    float v = g.oy + (float(id.y) + 0.5f) * g.sy - 0.5f;
    float c0 = max((sample_plane(planes, g.w, g.h, u, v) - s.black) * s.inv_range, 0.0f);
    float c1 = max((sample_plane(planes + n, g.w, g.h, u, v) - s.black) * s.inv_range, 0.0f);
    float c2 = max((sample_plane(planes + 2 * n, g.w, g.h, u, v) - s.black) * s.inv_range, 0.0f);
    float c3 = max((sample_plane(planes + 3 * n, g.w, g.h, u, v) - s.black) * s.inv_range, 0.0f);
    float3 w = clamp(float3(c1, 0.5f * (c0 + c2), c3) * s.gains.xyz, 0.0f, 1.0f);
    float3 px;
    if (s.mode == 2 || s.view != 0) {
        float3 lin = float3(dot(s.m_709[0].xyz, w), dot(s.m_709[1].xyz, w), dot(s.m_709[2].xyz, w));
        float3 t = look(s, lin);
        px = float3(rec709(s, t.x), rec709(s, t.y), rec709(s, t.z));
    } else {
        float3 lin = float3(dot(s.m_out[0].xyz, w), dot(s.m_out[1].xyz, w), dot(s.m_out[2].xyz, w));
        float3 t = look(s, lin);
        px = s.mode == 0 ? float3(log3g10(t.x), log3g10(t.y), log3g10(t.z)) : float3(nlog(t.x), nlog(t.y), nlog(t.z));
    }
    uint o = (id.y * g.ow + id.x) * 3;
    px = clamp(px, 0.0f, 1.0f) * 65535.0f + 0.5f;
    out[o] = ushort(px.x);
    out[o + 1] = ushort(px.y);
    out[o + 2] = ushort(px.z);
}
