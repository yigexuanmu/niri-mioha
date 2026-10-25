precision highp float;

#if defined(DEBUG_FLAGS)
uniform float niri_tint;
#endif

uniform float niri_alpha;
uniform float niri_scale;
uniform vec2 niri_size;
varying vec2 niri_v_coords;

// ─────────────────────────────────────────────────────────────────────────────
// BA 点击特效材质（移植自 ba-click-fx-desktop 的 `fxMaterialsShaderSource`）
//
// 逐个材质对齐上游：
// · `CrossPixel`     → 圆盘（`u_mode == 0`）
// · `DissolvePixel`  → 溶解环（`u_mode == 1`）
// · `AdditivePixel`  → 三角碎片（`u_mode == 2`）
// · `TrailPixel`     → 拖尾（`u_mode == 3`）
//
// 色彩：上游把四张材质纹理按 sRGB SRV 采样（`R8G8B8A8_UNORM_SRGB`），所以
// `srgb_to_linear(texel.rgb)` 才是它们进入线性域的值；而精灵/拖尾的顶点色在
// Unity 里本身就是线性域作者色，**不能再转一次**。最终上游把 direct 与 bloom
// 在线性域相加后用 `LinearToSrgb` 编码，再用 `capacityScale` 把预乘色压到不
// 超过 coverage（见 `ResolveUnknownBackgroundDesktopTransport`）。niri 直接输
// 出到 sRGB 编码的合成目标，所以这里复刻「编码 + 容量压缩」，把结果当作预乘
// sRGB 交给 smithay 默认的 `ONE, ONE_MINUS_SRC_ALPHA` 混合。
//
// 几何：四边形永远是轴对齐的，`input_to_geo` 只是把 `niri_v_coords ∈ [0,1]²`
// 放大成「相对四边形左上角的像素」。精灵靠 `u_center` / `u_rot` / `u_size` 在
// 着色器里反旋，拖尾段靠 `u_origin` / `u_axis` / `u_extent` 投影到条带坐标系。
// 因此四边形被刻意画得比图元本身大一圈（见 `render.rs` 的 padding 常量），好让
// 辉光能溢出形状边缘。
//
// 辉光（bloom）：上游是独立的 mip 链后处理（`unity_bloom.cpp`，diffusion 7、
// intensity 1.7 ⇒ `exposureGain ≈ 0.125`，再叠加各级上采样），没法在 niri 的
// 「一个元素一次 draw」管线里做同构实现，这里改成解析近似：把图元的形状当作解
// 析几何，用「高斯模糊后的实心区域」函数求它在自己形状外的扩散，再乘 `u_bloom`
// 增益。增益与扩散半径按上游官方演示图（`docs/images/native-click-trail.png`）
// 的径向剖面标定：形状内部 ≈ 直射色的 50%，形状外以 σ ≈ 0.45 半尺寸衰减。
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
uniform vec2 u_extent;     // 拖尾：段长与带宽（像素）
uniform vec2 u_cap;        // 拖尾：首/尾段的纵向渐隐长度（像素，0 = 硬切）
uniform float u_bloom;     // 辉光增益（0 = 关闭）
/// 输出高度相对 1080p 的倍率。
///
/// **不能**用 `niri_size`：smithay 交给着色器的 `niri_size` 是四边形自身的尺寸，
/// 而辉光半径是屏幕空间量（上游是整帧图像上的 mip 链）。用四边形尺寸做归一化，
/// 光晕会缩到原来的几十分之一 —— 在离线 harness 里看不出来（那里 `niri_size`
/// 恰好等于输出尺寸），一到实机就完全没有辉光。
uniform float u_ref_scale;

uniform sampler2D tex_disk;
uniform sampler2D tex_ring;
uniform sampler2D tex_triangle;
uniform sampler2D tex_trail;

const float PI = 3.141592653589793;
/// `makeUnityRingMesh()` 的 `firstAngleRadians = 95.625°`。
const float RING_FIRST_ANGLE = 1.6689711;
/// `unityRingInnerRadius / unityRingOuterRadius`。
const float RING_INNER_RATIO = 0.9401275;
/// 网格 uv 留出的半像素边距（`firstU/lastU/innerV/outerV` 取 0.0005..0.9995）。
const float RING_UV_INSET = 0.0005;
const float RING_UV_SPAN = 0.999;

/// 圆盘纹理 `center_disk` 里那个圆片的归一化半径：r ≤ 225 恒为 255，到 r = 233
/// 衰减完，按 512 宽折算是 0.879。辉光的形状边界必须用这个值，而**不是 1.0**
/// —— 否则光晕会浮在实体外面一圈，盘和雾之间空出一道暗环。
const float DISK_SHAPE_RADIUS = 0.879;

/// 上游 `ResolveRecordingCompatibleDesktopTransport` 的预设参数：源覆盖 Alpha
/// 上限 0.90（给「网页透明叠加」预留背景透视），亮核补偿强度 0.35。
///
/// niri 这边两处都做了调整，理由见各自使用处：
/// · Alpha 上限随实体覆盖度放开（实体盘实心、光晕仍守 0.90）；
/// · 亮核补偿 0.35 → 0.10，否则圆盘会被抬成灰蓝色（见下方合成段注释）。
const float TRANSPORT_ALPHA_LIMIT = 0.90;
const float TRANSPORT_COMPENSATION_MIX = 0.10;

/// 抗锯齿半宽（物理像素）。niri 以 1:1 物理像素绘制本元素，`pixel` 的片元步长
/// 恒为 1 像素，所以可以直接用像素单位的常量做 SDF 羽化，不必依赖 `fwidth`
/// （GLES2 里需要 `GL_OES_standard_derivatives` 扩展）。
const float AA_HALF_WIDTH = 0.5;
/// 溶解阈值的软化半宽（Alpha 单位）。只影响阈值处的过渡，溶解形状基本不变。
const float DISSOLVE_SOFTNESS = 0.02;

/// 辉光扩散半径（单位：图元半尺寸）。圆盘 / 环 / 碎片用这一套。
const float BLOOM_SIGMA = 0.4;
/// 拖尾光晕的三段指数衰减长度（1080p 参考像素）。
///
/// 上游 `unity_bloom.cpp` 的 mip 链把一条只有 2~3px 宽的 HDR 带子抹成一大片
/// 蓝雾：近场 3~4px 内陡降，中程二十几像素，远场一直拖到 100px 才见底。三段
/// 指数叠起来才贴合官方图（`docs/images/native-click-trail.png`）的横向扫描
/// 实测剖面 —— 单段高斯要么太窄（没有雾），要么太宽（变成一团糊）。
///
/// 注意单位是**像素**而不是半带宽：bloom 是屏幕空间后处理，半径只跟图像尺寸
/// 有关。按半带宽归一化会让光晕缩成几像素 —— 那正是「拖尾没有辉光」的成因。
const float BLOOM_TRAIL_FALLOFF_NEAR = 3.5;
const float BLOOM_TRAIL_FALLOFF_MID = 22.0;
const float BLOOM_TRAIL_FALLOFF_FAR = 90.0;
const float BLOOM_TRAIL_NEAR_WEIGHT = 0.769;
const float BLOOM_TRAIL_MID_WEIGHT = 0.225;
const float BLOOM_TRAIL_FAR_WEIGHT = 0.006;
/// 光晕的平滑截断位置（参考像素）。四边形留白按这个值算。
const float BLOOM_TRAIL_HALO_END = 110.0;
/// 拖尾的 HDR 强度是 23.97（`trailArtisticIntensity`），这是**直射色**的增益；
/// 辉光不能直接套这个数。mip 链把一条 2~3px 宽的带子面积平均掉大半，真正漏进
/// 光晕的只剩一小部分。按官方图反解：取拖尾亮段的作者色 (0, 0.39, 1.0)，要让
/// 带子边缘处的线性蓝通道落在实测的 ≈1.3，系数就是 ≈0.25 —— 也就是说有效 HDR
/// 强度 23.97 × 0.25 ≈ 6，正好和圆盘/碎片用的 `UNITY_HDR_INTENSITY`(5.99) 同级。
///
/// 这个系数必须对整条拖尾**统一**：上游的雾是逐像素累加出来的能量，颜色暗的
/// 尾段本来就该比亮段暗。按最亮段标定、再用同一个系数乘下去，尾段的雾自然变
/// 弱；反过来按暗段标定，亮段的雾会糊成一整块过曝的方块。
const float BLOOM_TRAIL_SOURCE_SCALE = 0.25;
/// 三角碎片在 128² 图集格里大致占半径 0.45 的圆；碎片本身很小，官方图里它的
/// 光晕也贴得很紧，所以用更小的半径与更窄的扩散。
const float TRIANGLE_GLOW_RADIUS = 0.45;
const float TRIANGLE_GLOW_SIGMA_SCALE = 0.7;

float srgb_to_linear_ch(float value) {
    float srgb = clamp(value, 0.0, 1.0);
    if (srgb <= 0.04045) {
        return srgb / 12.92;
    }
    return pow((srgb + 0.055) / 1.055, 2.4);
}

vec3 srgb_to_linear(vec3 color) {
    return vec3(
        srgb_to_linear_ch(color.r),
        srgb_to_linear_ch(color.g),
        srgb_to_linear_ch(color.b));
}

float linear_to_srgb_ch(float value) {
    float linear = clamp(value, 0.0, 1.0);
    if (linear <= 0.0031308) {
        return linear * 12.92;
    }
    return 1.055 * pow(linear, 1.0 / 2.4) - 0.055;
}

vec3 linear_to_srgb(vec3 color) {
    return vec3(
        linear_to_srgb_ch(color.r),
        linear_to_srgb_ch(color.g),
        linear_to_srgb_ch(color.b));
}

/// 高斯模糊后的「实心区域」指示函数：`d` 是到边界的有符号距离（单位：图元半
/// 尺寸）。d ≤ -2σ 得 1，d = 0 得 0.5，d ≥ 2σ 得 0。
///
/// 用 2σ 支持域的 smoothstep 近似高斯模糊：误差集中在尾部，但**支持域有限**，
/// 保证辉光在四边形内部就衰减到 0，不会在四边形边界上留下一圈台阶。
float blurred_inside(float d, float sigma) {
    return 1.0 - smoothstep(-2.0 * sigma, 2.0 * sigma, d);
}

/// 像素单位的 SDF → 覆盖度：`d < 0` 落在形状内，边界处一个像素宽的 smoothstep
/// 过渡给出亚像素级渐变，斜向边界不再有阶梯。
float aa_coverage(float signed_distance) {
    return 1.0 - smoothstep(-AA_HALF_WIDTH, AA_HALF_WIDTH, signed_distance);
}

/// 精灵（圆盘 / 三角）的 uv：反旋到精灵局部，归一化后套上图集窗口。
vec2 sprite_uv(vec2 pixel) {
    vec2 delta = pixel - u_center;
    float cosine = cos(u_rot);
    float sine = sin(u_rot);
    vec2 local = vec2(
        cosine * delta.x + sine * delta.y,
        -sine * delta.x + cosine * delta.y);
    vec2 normalized = local / max(u_size * 0.5, 0.0001);
    return vec2(
        u_uv_off.x + (normalized.x * 0.5 + 0.5) * u_uv_off.y,
        normalized.y * 0.5 + 0.5);
}

/// 溶解环的 uv：按 `makeUnityRingMesh()` 的展开方式从极坐标重建。
vec2 ring_uv(vec2 pixel, float radius) {
    float outer = u_size * 0.5;
    float inner = outer * RING_INNER_RATIO;
    float radial = (radius - inner) / max(outer - inner, 0.0001);
    // 网格顶点是 `center + R(rot) * mesh`，且网格 y 轴在屏幕上翻转过，
    // 所以局部角 = atan2(-dy, dx) - rot。
    float angle = atan(-(pixel.y - u_center.y), pixel.x - u_center.x) - u_rot;
    float progress = fract((angle - RING_FIRST_ANGLE) / (2.0 * PI));
    return vec2(
        RING_UV_INSET + RING_UV_SPAN - RING_UV_SPAN * progress,
        RING_UV_INSET + RING_UV_SPAN * radial);
}

void main() {
    vec2 pixel = (input_to_geo * vec3(niri_v_coords, 1.0)).xy;
    float global_opacity = clamp(u_gop, 0.0, 1.0);

    vec3 emission = vec3(0.0);
    float coverage = 0.0;
    // 辉光的「源色」：图元在被形状完整覆盖处的线性发光色。
    vec3 glow_source = vec3(0.0);
    // 辉光形状因子：1 在形状里、向外平滑衰减。
    float glow_shape = 0.0;
    // 原料自带的不透明度（顶点色 alpha）。辉光必须跟着它一起淡出，否则图元快
    // 消失时实体的覆盖度已经很小、外圈光晕却还是满的，中心会塌成一个黑洞。
    float element_opacity = clamp(u_color_a, 0.0, 1.0);

    if (u_mode < 0.5) {
        // CrossPixel：R 通道当形状遮罩，覆盖度 = shape * color.a。
        vec4 texel = texture2D(tex_disk, sprite_uv(pixel));
        float shape = srgb_to_linear_ch(texel.r);
        emission = srgb_to_linear(texel.rgb)
            * u_color
            * u_intensity
            * shape
            * global_opacity;
        coverage = clamp(shape * u_color_a, 0.0, 1.0) * global_opacity;

        float radius = length(pixel - u_center) / max(u_size * 0.5, 0.0001);
        // 圆盘的光晕要比盘面本身白得多：参考图里盘心是饱和蓝 (110,165,255)，
        // 外围一圈雾却是近白的淡蓝。上游的 bloom 是整帧后处理，盘心那点白色
        // 高光被 mip 链摊到四周，所以雾偏白；直接拿 `u_color`（饱和蓝）当光源，
        // 叠出来就是发紫的一圈，颜色一眼就不对。
        glow_source = mix(u_color, vec3(1.0), 0.62) * u_intensity;
        // 盘内不加辉光：盘面本身已经是不透明的饱和蓝，再叠一层线性域的亮光会
        // 直接把它抬成浅白蓝（参考图里盘心始终是 (110,165,255)，没有泛白）。
        // 雾只从盘边开始向外扩散，正好对上参考图「实心蓝盘 + 白色外晕」的样子。
        float outside_disk = smoothstep(
            DISK_SHAPE_RADIUS - 0.015,
            DISK_SHAPE_RADIUS + 0.015,
            radius);
        glow_shape =
            blurred_inside(radius - DISK_SHAPE_RADIUS, BLOOM_SIGMA) * outside_disk;
    } else if (u_mode < 1.5) {
        // DissolvePixel：只保留 alpha 高于阈值的部分。
        float radius = length(pixel - u_center);
        float outer = u_size * 0.5;
        float inner = outer * RING_INNER_RATIO;
        // 环带的内外两条边是纯几何边界，硬比较会在斜向处出现阶梯，改用 SDF
        // 的 smoothstep 羽化。采样照旧放在统一控制流里，避免 GLES2 对非一致
        // 分支内取纹理的未定义行为。
        float ring_coverage = aa_coverage(max(inner - radius, radius - outer));
        vec4 texel = texture2D(tex_ring, ring_uv(pixel, radius));
        // 溶解阈值保持上游的硬裁语义，但在阈值两侧留一条极窄的过渡带（Alpha
        // 单位），溶解前沿才不会有格子状跳变。
        float material_alpha = texel.a * u_color_a;
        float dissolve = smoothstep(
            u_threshold - DISSOLVE_SOFTNESS,
            u_threshold + DISSOLVE_SOFTNESS,
            material_alpha);
        float material_coverage = material_alpha * dissolve * ring_coverage;
        // 辉光的可见度只跟「这道环整体还剩多少」有关，不含几何环带因子。
        element_opacity = material_alpha > 0.0 ? material_alpha * dissolve : 0.0;
        emission = srgb_to_linear(texel.rgb)
            * u_color
            * u_intensity
            * material_coverage
            * global_opacity;
        coverage = clamp(material_coverage * global_opacity, 0.0, 1.0);

        float outer_d = (radius - outer) / max(outer, 0.0001);
        float inner_d = (radius - inner) / max(outer, 0.0001);
        glow_source = u_color * u_intensity;
        // 环带只有 6% 宽度，摊薄后天然比圆盘暗得多，和官方图一致。
        glow_shape = blurred_inside(outer_d, BLOOM_SIGMA)
            - blurred_inside(inner_d, BLOOM_SIGMA);
    } else if (u_mode < 2.5) {
        // AdditivePixel：纯发光，不做溶解裁剪。
        // 图集左右两帧各占半张纹理，四边形又比精灵大一圈，所以采样既要夹在本帧
        // 格子里（否则会采到隔壁那半张的三角形，也会在图集接缝上做双线性混色），
        // 又要对越界部分按透明处理。
        vec2 uv = sprite_uv(pixel);
        vec2 cell_min = vec2(u_uv_off.x, 0.0);
        vec2 cell_max = vec2(u_uv_off.x + u_uv_off.y, 1.0);
        vec4 texel = texture2D(tex_triangle, clamp(uv, cell_min, cell_max));
        texel *= step(cell_min.x, uv.x)
            * step(uv.x, cell_max.x)
            * step(cell_min.y, uv.y)
            * step(uv.y, cell_max.y);
        float material_coverage = texel.a * u_color_a;
        emission = srgb_to_linear(texel.rgb)
            * u_color
            * u_intensity
            * material_coverage
            * global_opacity;
        coverage = material_coverage * global_opacity;

        float radius = length(pixel - u_center) / max(u_size * 0.5, 0.0001);
        glow_source = u_color * u_intensity;
        glow_shape = blurred_inside(
            radius - TRIANGLE_GLOW_RADIUS,
            BLOOM_SIGMA * TRIANGLE_GLOW_SIGMA_SCALE);
    } else {
        // TrailPixel：发光不被覆盖度压暗，覆盖度只管 Alpha 与纵向淡出。
        vec2 relative = pixel - u_origin;
        float along = dot(relative, u_axis.xy) / max(u_extent.x, 0.0001);
        float across = dot(relative, u_axis.zw) / max(u_extent.y, 0.0001) + 0.5;
        float particle_opacity = clamp(u_color_a, 0.0, 1.0);
        vec3 segment_color = mix(u_color, u_color_b, clamp(along, 0.0, 1.0));
        // 纵向覆盖必须算在分支外面：辉光是在分支外求的，如果只在采样分支里算，
        // 已经淡出到 0 的尾段（插值色是黑的）就会在四边形留白处被光晕画成一道
        // 黑边。
        float longitudinal = mix(u_cov.x, u_cov.y, clamp(along, 0.0, 1.0));
        element_opacity = particle_opacity * clamp(longitudinal, 0.0, 1.0);

        if (along >= 0.0 && along <= 1.0 && across >= 0.0 && across <= 1.0) {
            vec4 texel = texture2D(
                tex_trail,
                vec2(u_progress.x + along * u_progress.y, across));
            // 横向羽化：上游用 `smoothstep(0, fwidth(uv.y) * 0.5, edge)`，这里
            // 换成同一套像素单位的 SDF 覆盖度。
            float edge_distance = min(across, 1.0 - across) * u_extent.y;
            float geometry_coverage = aa_coverage(-edge_distance);
            float material_coverage = clamp(
                texel.a * particle_opacity * longitudinal * geometry_coverage,
                0.0,
                1.0);
            emission = srgb_to_linear(texel.rgb)
                * segment_color
                * u_intensity
                * particle_opacity
                * global_opacity;
            coverage = material_coverage * global_opacity;
        }

        // 距离以「半带宽」为单位：条带边缘处为 0，向两侧变正。
        // （不要再乘 2：那样算出来的像素距离会正好翻倍，光晕半径缩一半。）
        float across_d = abs(across - 0.5) * 2.0 - 1.0;
        float along_px = along * u_extent.x;
        // 纵向权重。段内部恒为 1；接缝处两侧各占半个像素，加起来正好是 1，
        // 所以一条连续的拖尾不会被逐段叠亮。超出本段范围即 0 —— 每个像素只会
        // 被「它所在那一段」的光晕覆盖。
        //
        // 笔画的两端例外（`u_cap`）：那里没有「相邻段」可以接力，硬切会在拖尾
        // 头部留下一道垂直于线段的直角边，几个线段拼起来就露出一个多边形轮廓。
        // 所以两端放行纵向限制，改由下面的「到端点」径向距离来收缩 —— 光晕便
        // 收成一个圆头，和上游整帧卷积出来的雾一致。
        bool cap_start = u_cap.x > 0.5;
        bool cap_end = u_cap.y > 0.5;
        float rise = cap_start ? 1.0 : clamp(along_px + 0.5, 0.0, 1.0);
        float fall = cap_end ? 1.0 : clamp(u_extent.x + 0.5 - along_px, 0.0, 1.0);
        float along_mask = min(rise, fall);
        // 到条带边缘的距离（像素，段内为负）。
        float across_px = across_d * u_extent.y * 0.5;
        if (cap_start && along_px < 0.0) {
            across_px = length(vec2(along_px, across_px));
        }
        if (cap_end && along_px > u_extent.x) {
            across_px = length(vec2(along_px - u_extent.x, across_px));
        }
        // 换算成 1080p 参考像素。
        float ref_scale = max(u_ref_scale, 0.0001);
        float edge_px = max(across_px, 0.0) / ref_scale;
        float halo = BLOOM_TRAIL_NEAR_WEIGHT * exp(-edge_px / BLOOM_TRAIL_FALLOFF_NEAR)
            + BLOOM_TRAIL_MID_WEIGHT * exp(-edge_px / BLOOM_TRAIL_FALLOFF_MID)
            + BLOOM_TRAIL_FAR_WEIGHT * exp(-edge_px / BLOOM_TRAIL_FALLOFF_FAR);
        // 平滑截断，让光晕在四边形内部就落到 0（否则边界上会留一道直边）。
        halo *= 1.0 - smoothstep(
            BLOOM_TRAIL_HALO_END * 0.8,
            BLOOM_TRAIL_HALO_END,
            edge_px);
        glow_source = segment_color * u_intensity * BLOOM_TRAIL_SOURCE_SCALE;
        glow_shape = along_mask * halo;
    }

    // ── 合成：严格对齐上游 `ResolveUnknownBackgroundDesktopTransport` ──
    // 辉光必须**在线性域**与直射色相加后再统一编码。编码是幂函数，先编码、再
    // 相加会让亮部的通道比例失真 —— 表现就是整片泛起白雾、饱和度整体下滑。
    float bloom_weight =
        max(glow_shape, 0.0) * u_bloom * element_opacity * global_opacity;
    vec3 bloom_linear = max(glow_source * bloom_weight, vec3(0.0));
    vec3 linear_rgb = max(emission, vec3(0.0)) + bloom_linear;
    vec3 encoded = linear_to_srgb(linear_rgb);

    // Alpha 取「视觉最大值」：光晕区域没有实体覆盖，也必须带上 Alpha，否则在
    // 浅色桌面上会被背景冲淡、在深色桌面上又退化成纯加色。
    //
    // 上限随实体覆盖度放开：上游那个 0.90 是给「网页透明叠加」预留背景透视的
    // 保守值，而 BA 在真实桌面上的圆盘是实心的（官方参考图里盘心蓝通道打满）。
    // 让实体区接近不透明、光晕区仍守在 0.90，两头都照顾到。
    float scene_coverage = clamp(coverage, 0.0, 1.0);
    // 上游 bloom 的 Alpha 通道是 `transportEnergy`：BrightPass 逐像素把
    // `max(rgb)` 写进 a，mip 链里和 RGB 同步累加。于是 bloom 的 Alpha 就是
    // **辉光自身的能量**，而不是「形状 × 增益」这个无量纲系数。
    //
    // 这里必须照同一个语义：拿 `bloom_weight` 去编码，等于让 Alpha 只跟形状
    // 走，而 `capacity = alpha / peak` 又把颜色再压一次 —— 两层相乘后光晕会被
    // 平方级压暗，薄雾直接消失（拖尾「没有辉光」就是这么来的）。
    float bloom_coverage =
        linear_to_srgb_ch(max(bloom_linear.r, max(bloom_linear.g, bloom_linear.b)));
    float alpha_limit = mix(TRANSPORT_ALPHA_LIMIT, 1.0, scene_coverage);
    float alpha = min(max(scene_coverage, bloom_coverage), alpha_limit);

    // 容量压缩：预乘色不允许超过 Alpha。这条不变量让源覆盖混合既能提亮（亮于
    // 背景）也能压暗（暗于背景），观感与上游 DComp 输出一致。
    float peak = max(encoded.r, max(encoded.g, encoded.b));
    float capacity = min(1.0, alpha / max(peak, 0.000001));
    vec3 premultiplied = min(encoded * capacity, vec3(alpha));

    // bright-core 补偿：把亮核的弱通道抬向峰值，重演上游「核心发白、外圈留色」
    // 的观感。能量低的拖尾尾段被 gate 挡住，不会整条尾巴灰掉。
    //
    // 强度取 0.10 而不是上游的 0.35：上游是为「不知道背景 + 网页叠加」调的，
    // 会把圆盘从 (78,138,229) 抬成 (137,169,229) 的灰蓝；按官方参考图的盘心
    // 实测 (108,163,255) 反解，0.10 才对得上。
    float premultiplied_peak = max(
        premultiplied.r,
        max(premultiplied.g, premultiplied.b));
    float energy_ratio = premultiplied_peak / max(alpha, 0.000001);
    float gate = smoothstep(0.25, 0.75, energy_ratio)
        * smoothstep(0.03125, 0.25, premultiplied_peak);
    premultiplied = mix(
        premultiplied,
        vec3(premultiplied_peak),
        TRANSPORT_COMPENSATION_MIX * gate);

    gl_FragColor = vec4(premultiplied, alpha) * niri_alpha;

#if defined(DEBUG_FLAGS)
    if (niri_tint == 1.0)
        gl_FragColor = vec4(0.0, 0.3, 0.0, 0.3) + gl_FragColor * 0.7;
#endif
}
