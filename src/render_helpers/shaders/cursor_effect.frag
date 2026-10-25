precision highp float;

#if defined(DEBUG_FLAGS)
uniform float niri_tint;
#endif

uniform float niri_alpha;
uniform float niri_scale;
uniform vec2  niri_size;
varying vec2 niri_v_coords;

// Cursor-effect glyph rendering. Coordinate convention mirrors `border.frag`:
// `coords_geo = input_to_geo * vec3(niri_v_coords, 1.0)` → geometry-pixel
// space (Canvas-style top-left origin). `u_center` is in this space. Without
// this matrix, niri_v_coords sits in a normalized [k, k+1] space and glyph
// placement collapses → the whole quad lights up ((the "square" bug).
uniform mat3 input_to_geo;

// Glyph sampling: pango/cairo rasterized BGRA white-on-black bitmap (AA edges
// included). RGB luminance is the glyph mask (sampled via max(r,g,b), immune
// to platform byte order). Each glyph ink is centered in a 64x64 cell; we
// sample center ±GLYPH_HALF texels.
uniform vec3  u_color;    // glyph color (rgb 0..1)
uniform vec2  u_center;   // glyph center (geo px)
uniform float u_radius;   // glyph half-size (geo px)
uniform float u_glyph;    // glyph index (0..94 → ASCII 0x21..0x7E)
uniform float u_rot;      // glyph rotation (radians)
uniform sampler2D cursor_glyph_atlas;

const float GLYPH_COLS = 16.0;
const float GLYPH_ROWS = 8.0;
const float GLYPH_CELL_W = 64.0;
const float GLYPH_CELL_H = 64.0;
const float GLYPH_HALF = 24.0;
const float GLYPH_ATLAS_W = 1024.0;
const float GLYPH_ATLAS_H = 512.0;

void main() {
    vec3 coords_geo3 = input_to_geo * vec3(niri_v_coords, 1.0);
    vec2 p = coords_geo3.xy;

    // Glyph: u_center 为字形中心、u_radius 为字形半边长（geo px）、u_rot 旋转、
    // u_glyph 字形索引。几何 y 向下（Canvas 风格），atlas 数据行 0 = 字形顶部，
    // 故 uv.y 直接随 local.y 增大即可保持字形正向。轮廓加 1px 透明间隔 +
    // smoothstep 阈值把 LINEAR 软边收紧。
    vec2 r = p - u_center;
    float cr = cos(-u_rot);
    float sr = sin(-u_rot);
    vec2 pr = vec2(cr * r.x - sr * r.y, sr * r.x + cr * r.y);
    vec2 local = pr / max(u_radius, 0.01); // [-1,1]
    float a = 0.0;
    if (local.x >= -1.0 && local.x <= 1.0 && local.y >= -1.0 && local.y <= 1.0) {
        float gcol = mod(u_glyph, GLYPH_COLS);
        float grow = floor(u_glyph / GLYPH_COLS);
        float cx = gcol * GLYPH_CELL_W + GLYPH_CELL_W * 0.5;
        float cy = grow * GLYPH_CELL_H + GLYPH_CELL_H * 0.5;
        // 字形 ink 居中于格子：采样中心 ±GLYPH_HALF texel。
        vec2 uv = vec2(
            (cx + local.x * GLYPH_HALF) / GLYPH_ATLAS_W,
            (cy + local.y * GLYPH_HALF) / GLYPH_ATLAS_H
        );
        vec4 tc = texture2D(cursor_glyph_atlas, uv);
        a = max(max(tc.r, tc.g), tc.b);
    }

    float alpha = a * niri_alpha;
    gl_FragColor = vec4(u_color * alpha, alpha);

#if defined(DEBUG_FLAGS)
    if (niri_tint == 1.0)
        gl_FragColor = vec4(0.0, 0.3, 0.0, 0.3) + gl_FragColor * 0.7;
#endif
}