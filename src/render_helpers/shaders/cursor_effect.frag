precision highp float;

#if defined(DEBUG_FLAGS)
uniform float niri_tint;
#endif

uniform float niri_alpha;
uniform float niri_scale;
uniform vec2 niri_size;
varying vec2 niri_v_coords;

// ─────────────────────────────────────────────────────────────────────────────
// BA 点击特效材质（路线图 P2，逐行对齐上游 `fxMaterialsShaderSource`）
//
// 这里只输出**线性域的 emission 与 coverage**，不编码、不压缩、不加解析辉光：
//
//   CrossPixel     → 圆盘
//   DissolvePixel  → 溶解环（硬 clip）
//   AdditivePixel  → 三角碎片
//   TrailPixel     → 拖尾（emission 不乘 coverage）
//
// Bloom 由 `bloom.rs` 对整帧 direct 目标做真正的 BrightPass + mip 金字塔，
// Transport 由 `transport.frag` 完成 `LinearToSrgb` 与 BackgroundAware 的
// Alpha 规则。原先 1509 行里的 SDF/解析高斯/容量压缩/补偿常量全部删除 —— 那些
// 正是“用一整套拟合常量替代卷积机器”的来源（N1–N5）。
// ─────────────────────────────────────────────────────────────────────────────

uniform mat3 input_to_geo;

uniform float u_mode;      // 0=圆盘 1=溶解环 2=三角碎片 3=拖尾
uniform vec3 u_color;      // 主色（线性域作者色）
uniform vec3 u_color_b;    // 拖尾：段尾颜色（线性域）
uniform float u_color_a;   // 顶点色 alpha
uniform float u_gop;       // 全局不透明度
uniform float u_intensity; // Unity HDR 强度
uniform float u_threshold; // 溶解裁剪阈值
uniform vec2 u_center;     // 精灵中心（相对四边形左上角的像素）
uniform float u_size;      // 精灵边长（像素）
uniform float u_rot;       // 精灵/环旋转（弧度）
uniform vec2 u_uv_off;     // 图集 u：起点与跨度
uniform vec2 u_cov;        // 拖尾：两端纵向覆盖
uniform vec2 u_progress;   // 拖尾：纹理 u 起点与增量
uniform vec4 u_axis;       // 拖尾：切线 xy、法线 zw（单位向量）
uniform vec2 u_origin;     // 拖尾：段起点（相对四边形左上角的像素）
uniform vec2 u_seg_end;    // 拖尾：段末端（相对四边形左上角的像素）
uniform vec2 u_extent;     // 拖尾：段长与带宽（像素）
uniform vec2 u_cap;        // 拖尾：首/尾段标记（保留签名，P2 不再用）
uniform float u_bloom;     // 兼容旧 uniform（P2 不再使用）
uniform float u_glow_radius; // 兼容旧 uniform
uniform float u_pass;      // 兼容旧 uniform
uniform float u_ring_inner; // 溶解环内径比（外径的倍数）
uniform float u_ref_scale; // 输出高度相对 1080p 的倍率

uniform sampler2D tex_disk;
uniform sampler2D tex_ring;
uniform sampler2D tex_triangle;
uniform sampler2D tex_trail;

const float PI = 3.141592653589793;
/// `makeUnityRingMesh()` 的 `firstAngleRadians = 95.625°`。
const float RING_FIRST_ANGLE = 1.6689711;
/// 网格 uv 留出的半像素边距（`firstU/lastU/innerV/outerV` 取 0.0005..0.9995）。
const float RING_UV_INSET = 0.0005;
const float RING_UV_SPAN = 0.999;
/// 拖尾纹理是 512×512，半纹素用于 clamp 首尾接缝。
const float TRAIL_HALF_TEXEL = 0.5 / 512.0;

float srgb_to_linear_ch(float value) {
    float clamped = clamp(value, 0.0, 1.0);
    return clamped <= 0.04045
        ? clamped / 12.92
        : pow((clamped + 0.055) / 1.055, 2.4);
}

vec3 srgb_to_linear(vec3 value) {
    return vec3(
        srgb_to_linear_ch(value.r),
        srgb_to_linear_ch(value.g),
        srgb_to_linear_ch(value.b));
}

/// 精灵局部坐标（单位 = 图元半尺寸，已反旋）。
vec2 sprite_local(vec2 pixel) {
    vec2 delta = pixel - u_center;
    float cosine = cos(u_rot);
    float sine = sin(u_rot);
    vec2 local = vec2(
        cosine * delta.x + sine * delta.y,
        -sine * delta.x + cosine * delta.y);
    return local / max(u_size * 0.5, 0.0001);
}

/// 精灵（圆盘 / 三角）的 uv：反旋到精灵局部，归一化后套上图集窗口。
vec2 sprite_uv(vec2 pixel) {
    vec2 normalized = sprite_local(pixel);
    return vec2(
        u_uv_off.x + (normalized.x * 0.5 + 0.5) * u_uv_off.y,
        normalized.y * 0.5 + 0.5);
}

/// 溶解环的 uv：按 `makeUnityRingMesh()` 的展开方式从极坐标重建。
/// P2 删除了 `RING_THICKEN`：内径严格等于 `outer × inner_ratio`，与上游一致。
vec2 ring_uv(vec2 pixel, float radius) {
    float outer = u_size * 0.5;
    float inner = outer * max(u_ring_inner, 0.0);
    float radial = (radius - inner) / max(outer - inner, 0.0001);
    // 网格顶点是 `center + R(rot) * mesh`，且网格 y 轴在屏幕上翻转过，
    // 所以局部角 = atan2(-dy, dx) - rot。
    float angle = atan(-(pixel.y - u_center.y), pixel.x - u_center.x) - u_rot;
    float progress = fract((angle - RING_FIRST_ANGLE) / (2.0 * PI));
    return vec2(
        RING_UV_INSET + RING_UV_SPAN - RING_UV_SPAN * progress,
        RING_UV_INSET + RING_UV_SPAN * radial);
}

/// 点到线段的距离。
float trail_seg_distance(vec2 p, vec2 a, vec2 b) {
    vec2 ab = b - a;
    float length_sq = dot(ab, ab);
    if (length_sq < 1.0e-6) {
        return length(p - a);
    }
    float t = clamp(dot(p - a, ab) / length_sq, 0.0, 1.0);
    return length(p - (a + ab * t));
}

void main() {
    vec2 pixel = (input_to_geo * vec3(niri_v_coords, 1.0)).xy;
    float global_opacity = clamp(u_gop, 0.0, 1.0);
    float ref_scale = max(u_ref_scale, 0.0001);

    vec3 emission = vec3(0.0);
    float coverage = 0.0;

    if (u_mode < 0.5) {
        // CrossPixel：R 通道当形状遮罩。
        vec4 texel = texture2D(tex_disk, sprite_uv(pixel));
        float shape = srgb_to_linear_ch(texel.r);
        // P2 去掉了旧的 capacity 压缩后，盘面 emission 不再被 alpha 压过。
        // 盘色在生命周期末尾把 `color.a` 降到 0，但 emission 仍非零：direct
        // 预乘 over 会把它当成纯加法，盘心在 alpha=0 之后还留一个 Bloom 鬼影。
        // 上游 CrossPixel 的 emission 也不乘 coverage，但它的盘在 200ms 处
        // 已经不可见；这里显式乘盘色 alpha，让盘随 alpha 一起消失（与旧版
        // 实机行为一致，也避免 transport 用 Bloom 把鬼影重新抬起来）。
        emission = srgb_to_linear(texel.rgb)
            * u_color
            * u_intensity
            * shape
            * global_opacity
            * clamp(u_color_a, 0.0, 1.0);
        coverage = clamp(shape * u_color_a, 0.0, 1.0) * global_opacity;
    } else if (u_mode < 1.5) {
        // DissolvePixel：只保留 alpha 高于阈值的部分，硬 clip。
        float radius = length(pixel - u_center);
        float outer = u_size * 0.5;
        float inner = outer * max(u_ring_inner, 0.0);
        // 1px 几何抗锯齿；环带之外直接丢弃，避免极坐标 uv 在环外采到边缘纹素。
        float edge_distance = max(inner - radius, radius - outer);
        float geometry_coverage = clamp(0.5 - edge_distance, 0.0, 1.0);
        if (geometry_coverage <= 0.0) {
            discard;
        }
        vec4 texel = texture2D(tex_ring, ring_uv(pixel, radius));
        float material_coverage = texel.a * u_color_a;
        // 上游 `clip(materialCoverage - input.dissolveThreshold);`：二值，不留软边。
        if (material_coverage < u_threshold) {
            discard;
        }
        coverage = clamp(material_coverage * geometry_coverage, 0.0, 1.0)
            * global_opacity;
        emission = srgb_to_linear(texel.rgb)
            * u_color
            * u_intensity
            * material_coverage
            * geometry_coverage
            * global_opacity;
    } else if (u_mode < 2.5) {
        // AdditivePixel：纯发光，不做溶解裁剪。
        vec2 uv = sprite_uv(pixel);
        vec2 cell_min = vec2(u_uv_off.x, 0.0);
        vec2 cell_max = vec2(u_uv_off.x + u_uv_off.y, 1.0);
        vec4 texel = texture2D(tex_triangle, clamp(uv, cell_min, cell_max));
        texel *= step(cell_min.x, uv.x)
            * step(uv.x, cell_max.x)
            * step(cell_min.y, uv.y)
            * step(uv.y, cell_max.y);
        float material_coverage = clamp(texel.a * u_color_a, 0.0, 1.0);
        coverage = material_coverage * global_opacity;
        emission = srgb_to_linear(texel.rgb)
            * u_color
            * u_intensity
            * material_coverage
            * global_opacity;
    } else {
        // TrailPixel：Desktop Coverage 只是 transport 元数据，emission 不乘它。
        vec2 relative = pixel - u_origin;
        float along_raw = dot(relative, u_axis.xy);
        float across = dot(relative, u_axis.zw) / max(u_extent.y, 0.0001) + 0.5;
        float joint_px_len = 22.0 * ref_scale;
        float seg_len = max(u_extent.x - 2.0 * joint_px_len, 1.0);
        float along = along_raw / seg_len;
        float particle_opacity = clamp(u_color_a, 0.0, 1.0);
        vec3 segment_color = mix(u_color, u_color_b, clamp(along, 0.0, 1.0));
        float longitudinal = mix(u_cov.x, u_cov.y, clamp(along, 0.0, 1.0));

        float d_self = trail_seg_distance(pixel, u_origin, u_seg_end);
        float half_width = u_extent.y * 0.5;
        float body = clamp(0.5 - (d_self - half_width), 0.0, 1.0);
        if (body <= 0.0) {
            discard;
        }

        float uv_along = clamp(along, 0.0, 1.0);
        float texel_u = clamp(
            u_progress.x + uv_along * u_progress.y,
            TRAIL_HALF_TEXEL,
            1.0 - TRAIL_HALF_TEXEL);
        vec4 texel = texture2D(tex_trail, vec2(texel_u, across));
        float material_coverage = clamp(
            texel.a * particle_opacity * longitudinal,
            0.0,
            1.0);
        coverage = material_coverage * body * global_opacity;
        // 上游 TrailPixel 的 emission 只乘 particleOpacity / globalOpacity，
        // 不乘 materialCoverage 或几何覆盖度（否则纵向淡出会把发光也压暗）。
        emission = srgb_to_linear(texel.rgb)
            * segment_color
            * u_intensity
            * particle_opacity
            * global_opacity;
    }

    // 线性域、未压缩、预乘 over（smithay 默认 ONE / ONE_MINUS_SRC_ALPHA）。
    gl_FragColor = vec4(emission, coverage) * niri_alpha;

#if defined(DEBUG_FLAGS)
    if (niri_tint == 1.0)
        gl_FragColor = vec4(0.0, 0.3, 0.0, 0.3) + gl_FragColor * 0.7;
#endif
}