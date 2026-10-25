//! BA 点击特效的曲线、颜色与参数层。
//!
//! 逐行移植自 `CialloKing/ba-click-fx-desktop` 的 `src/fx/src/simulation.cpp`
//! 匿名命名空间部分（Hermite 曲线求值/积分、颜色渐变、磁盘/环/碎片的大小与
//! 颜色曲线、环角速度积分、参数归一化）。
//!
//! 颜色空间：曲线关键帧是 gamma 域的 Unity 作者色，`disk_color` / `ring_color`
//! / `triangle_color` 出口处统一走 `unity_active_color_space()` 转进线性域
//! ——这正是上游 `applyUnityActiveColorSpace` 的那一次转换。之后在线性域做
//! `纹理色 × 顶点色 × 强度` 并相加，最终由着色器编码回 sRGB。

/// Hermite 曲线关键帧（时间、值、入切线、出切线）。时间取值域 `[0, 1]`。
#[derive(Debug, Clone, Copy)]
pub struct CurveKey {
    pub time: f32,
    pub value: f32,
    pub in_slope: f32,
    pub out_slope: f32,
}

impl CurveKey {
    const fn new(time: f32, value: f32, in_slope: f32, out_slope: f32) -> Self {
        Self {
            time,
            value,
            in_slope,
            out_slope,
        }
    }
}

/// 颜色关键帧（用于三角碎片的生命周期渐变）。
#[derive(Debug, Clone, Copy)]
struct ColorKey {
    time: f32,
    color: Color,
}

impl ColorKey {
    const fn new(time: f32, color: Color) -> Self {
        Self { time, color }
    }
}

/// RGBA 颜色，分量 `[0, 1]`。RGB 是作者色（sRGB 编码），A 是不透明度。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    pub const WHITE: Self = Self::new(1.0, 1.0, 1.0, 1.0);

    pub fn to_array(self) -> [f32; 3] {
        [self.r, self.g, self.b]
    }
}

#[inline]
fn clamp_unit(value: f32) -> f32 {
    value.clamp(0.0, 1.0)
}

#[inline]
fn lerp(from: f32, to: f32, amount: f32) -> f32 {
    from + (to - from) * amount
}

/// 分段 Hermite 求值（与 Unity AnimationCurve 同公式）。
pub fn evaluate_hermite(keys: &[CurveKey], time: f32) -> f32 {
    debug_assert!(!keys.is_empty());
    let first = keys[0];
    if time <= first.time {
        return first.value;
    }
    let last = keys[keys.len() - 1];
    if time >= last.time {
        return last.value;
    }

    for index in 1..keys.len() {
        let right = keys[index];
        if time > right.time {
            continue;
        }
        let left = keys[index - 1];
        let duration = right.time - left.time;
        let t = (time - left.time) / duration;
        let t2 = t * t;
        let t3 = t2 * t;
        let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
        let h10 = t3 - 2.0 * t2 + t;
        let h01 = -2.0 * t3 + 3.0 * t2;
        let h11 = t3 - t2;
        return h00 * left.value
            + h10 * duration * left.out_slope
            + h01 * right.value
            + h11 * duration * right.in_slope;
    }

    last.value
}

/// 分段 Hermite 的定积分（0 到 `time`）。
pub fn integrate_hermite(keys: &[CurveKey], time: f32) -> f32 {
    debug_assert!(!keys.is_empty());
    let t = clamp_unit(time);
    let first = keys[0];
    let mut integral = first.value * t.min(first.time);
    if t <= first.time {
        return integral;
    }

    for index in 1..keys.len() {
        let left = keys[index - 1];
        let right = keys[index];
        let duration = right.time - left.time;
        if duration <= 0.0 {
            continue;
        }

        let segment_end = t.min(right.time);
        let u = clamp_unit((segment_end - left.time) / duration);
        let u2 = u * u;
        let u3 = u2 * u;
        let u4 = u3 * u;
        let h00_integral = 0.5 * u4 - u3 + u;
        let h10_integral = 0.25 * u4 - (2.0 / 3.0) * u3 + 0.5 * u2;
        let h01_integral = -0.5 * u4 + u3;
        let h11_integral = 0.25 * u4 - (1.0 / 3.0) * u3;
        integral += duration
            * (h00_integral * left.value
                + h10_integral * duration * left.out_slope
                + h01_integral * right.value
                + h11_integral * duration * right.in_slope);
        if t <= right.time {
            return integral;
        }
    }

    integral + (t - keys[keys.len() - 1].time) * keys[keys.len() - 1].value
}

fn evaluate_color_gradient(keys: &[ColorKey], time: f32) -> Color {
    let first = keys[0];
    if time <= first.time {
        return first.color;
    }
    for index in 1..keys.len() {
        let right = keys[index];
        if time <= right.time {
            let left = keys[index - 1];
            let amount = (time - left.time) / (right.time - left.time);
            return lerp_color(left.color, right.color, amount);
        }
    }
    keys[keys.len() - 1].color
}

pub fn lerp_color(from: Color, to: Color, amount: f32) -> Color {
    let t = clamp_unit(amount);
    Color::new(
        lerp(from.r, to.r, t),
        lerp(from.g, to.g, t),
        lerp(from.b, to.b, t),
        lerp(from.a, to.a, t),
    )
}

/// Unity 在 Linear 色彩空间下把粒子颜色**一次性**转进线性域再上传
/// （上游 `applyUnityActiveColorSpace`）。Alpha 不参与转换。
pub fn unity_active_color_space(color: Color) -> Color {
    let linear = srgb_to_linear([color.r, color.g, color.b]);
    Color::new(linear[0], linear[1], linear[2], color.a)
}

// ─── sRGB 传输函数（着色器里用同一公式，这里给测试与 CPU 侧换算用） ───

pub fn srgb_to_linear_channel(value: f32) -> f32 {
    let srgb = value.clamp(0.0, 1.0);
    if srgb <= 0.04045 {
        srgb / 12.92
    } else {
        ((srgb + 0.055) / 1.055).powf(2.4)
    }
}

pub fn linear_to_srgb_channel(value: f32) -> f32 {
    let linear = value.clamp(0.0, 1.0);
    if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    }
}

pub fn srgb_to_linear(color: [f32; 3]) -> [f32; 3] {
    [
        srgb_to_linear_channel(color[0]),
        srgb_to_linear_channel(color[1]),
        srgb_to_linear_channel(color[2]),
    ]
}

pub fn linear_to_srgb(color: [f32; 3]) -> [f32; 3] {
    [
        linear_to_srgb_channel(color[0]),
        linear_to_srgb_channel(color[1]),
        linear_to_srgb_channel(color[2]),
    ]
}

// ─── 各图层的曲线 ───

pub fn disk_size_curve(normalized_age: f32) -> f32 {
    const KEYS: [CurveKey; 3] = [
        CurveKey::new(0.0, 0.325_835_82, 2.400_473_4, 2.400_473_4),
        CurveKey::new(0.213_928_22, 0.715_977_3, 0.911_574_5, 0.911_574_5),
        CurveKey::new(1.0, 1.0, 0.0, 0.0),
    ];
    2.0 * evaluate_hermite(&KEYS, normalized_age)
}

pub fn ring_size_curve(normalized_age: f32) -> f32 {
    const KEYS: [CurveKey; 3] = [
        CurveKey::new(0.007_209_778, 0.420_508_98, 2.400_473_4, 2.400_473_4),
        CurveKey::new(0.213_928_22, 0.715_977_3, 0.911_574_5, 0.911_574_5),
        CurveKey::new(1.0, 1.0, 0.0, 0.0),
    ];
    evaluate_hermite(&KEYS, normalized_age)
}

pub fn triangle_size_curve(normalized_age: f32) -> f32 {
    const KEYS: [CurveKey; 3] = [
        CurveKey::new(0.0, 0.0, 0.0, 0.0),
        CurveKey::new(0.154_450_95, 1.0, 0.0, 0.0),
        CurveKey::new(1.0, 0.0, -2.162_150_1, -2.162_150_1),
    ];
    evaluate_hermite(&KEYS, normalized_age)
}

pub fn triangle_alpha_curve(normalized_age: f32) -> f32 {
    const TIMES: [f32; 8] = [
        0.288_242_92,
        0.364_705_88,
        0.470_588_24,
        0.573_525_6,
        0.667_643_24,
        0.755_886_17,
        0.852_948_8,
        1.0,
    ];
    const VALUES: [f32; 8] = [1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 1.0];

    if normalized_age <= TIMES[0] {
        return VALUES[0];
    }
    for index in 1..TIMES.len() {
        if normalized_age <= TIMES[index] {
            let t = (normalized_age - TIMES[index - 1]) / (TIMES[index] - TIMES[index - 1]);
            return VALUES[index - 1] + (VALUES[index] - VALUES[index - 1]) * t;
        }
    }
    VALUES[VALUES.len() - 1]
}

pub fn disk_color(normalized_age: f32) -> Color {
    const WHITE: Color = Color::new(1.0, 1.0, 1.0, 1.0);
    const BLUE: Color = Color::new(0.240_566_02, 0.390_618_15, 1.0, 1.0);
    let color_t = normalized_age / 0.120_592_05;
    let mut color = lerp_color(WHITE, BLUE, color_t);
    // 圆盘在参考图里要「实心」地存在大半个生命周期，最后才淡出。上游把它写在
    // 0.109 就开始衰减，配合 250ms 寿命等于 27ms 之后盘面就半透明了 —— 桌面
    // 背景从盘面透上来，蓝通道永远打不满，看起来就是「颜色发灰、不对」。
    // 推到 0.78 之后，盘心在参考图对应的那几格里稳定落在 (110,165,255)。
    const FADE_START: f32 = 0.78;
    color.a = if normalized_age <= FADE_START {
        1.0
    } else {
        1.0 - (normalized_age - FADE_START) / (1.0 - FADE_START)
    };
    color.a = clamp_unit(color.a);
    unity_active_color_space(color)
}

pub fn ring_color(normalized_age: f32) -> Color {
    const WHITE: Color = Color::new(1.0, 1.0, 1.0, 1.0);
    const BLUE: Color = Color::new(0.297_169_8, 0.653_286_5, 1.0, 1.0);
    const WHITE_UNTIL: f32 = 0.111_772_335;
    if normalized_age <= WHITE_UNTIL {
        return unity_active_color_space(WHITE);
    }
    unity_active_color_space(lerp_color(
        WHITE,
        BLUE,
        (normalized_age - WHITE_UNTIL) / (0.500_007_63 - WHITE_UNTIL),
    ))
}

pub fn triangle_color(normalized_age: f32) -> Color {
    const START: Color = Color::new(0.537_735_8, 0.537_735_8, 0.537_735_8, 1.0);
    const KEYS: [ColorKey; 5] = [
        ColorKey::new(0.182_360_57, Color::new(1.0, 1.0, 1.0, 1.0)),
        ColorKey::new(0.282_352_94, Color::new(0.372_641_5, 0.773_187_3, 1.0, 1.0)),
        ColorKey::new(0.461_768_52, Color::new(0.372_549_03, 0.772_549_1, 1.0, 1.0)),
        ColorKey::new(0.661_768_52, Color::new(0.352_941_2, 0.729_411_8, 0.945_098_1, 1.0)),
        ColorKey::new(0.826_474_4, Color::new(0.372_549_03, 0.772_549_1, 1.0, 1.0)),
    ];
    let lifetime = evaluate_color_gradient(&KEYS, normalized_age);
    let mut color = Color::new(
        START.r * lifetime.r,
        START.g * lifetime.g,
        START.b * lifetime.b,
        START.a * lifetime.a,
    );
    color.a *= triangle_alpha_curve(normalized_age);
    unity_active_color_space(color)
}

pub fn dissolve_threshold(normalized_age: f32) -> f32 {
    const KEYS: [CurveKey; 3] = [
        CurveKey::new(0.0, 1.0, 0.0, 0.0),
        CurveKey::new(0.2, 0.0, 0.0, 2.424_936_8),
        CurveKey::new(1.0, 1.0, 0.277_356_36, 0.277_356_36),
    ];
    evaluate_hermite(&KEYS, normalized_age)
}

/// 环在 `[from_age, to_age]` 区间内的旋转增量（弧度）。
///
/// Unity 把角速度存在「归一化寿命」域，故在曲线空间积分再乘寿命；
/// 公共方向是屏幕顺时针，而网格旋转是屏幕逆时针，所以这里取反。
pub fn ring_rotation_delta(
    angular_blend: f32,
    from_age_seconds: f32,
    to_age_seconds: f32,
    settings: &ClickParticleSettings,
) -> f32 {
    if to_age_seconds <= from_age_seconds {
        return 0.0;
    }

    const MINIMUM_KEYS: [CurveKey; 2] = [
        CurveKey::new(0.149_039_03, 1.0, 0.0, 0.0),
        CurveKey::new(1.0, 0.455_618_26, 0.0, 0.0),
    ];
    const MAXIMUM_KEYS: [CurveKey; 2] = [
        CurveKey::new(0.158_653_84, 0.798_816_56, 0.0, 0.0),
        CurveKey::new(1.0, -0.065_091_34, 0.0, 0.0),
    ];
    let lifetime_seconds = settings.rings_lifetime_ms * 0.001;
    let from_normalized = from_age_seconds / lifetime_seconds;
    let to_normalized = to_age_seconds / lifetime_seconds;
    let minimum_from = integrate_hermite(&MINIMUM_KEYS, from_normalized);
    let minimum_to = integrate_hermite(&MINIMUM_KEYS, to_normalized);
    let maximum_from = integrate_hermite(&MAXIMUM_KEYS, from_normalized);
    let maximum_to = integrate_hermite(&MAXIMUM_KEYS, to_normalized);
    let blend = clamp_unit(angular_blend);
    let blended_from = minimum_from + (maximum_from - minimum_from) * blend;
    let blended_to = minimum_to + (maximum_to - minimum_to) * blend;

    (blended_to - blended_from)
        * settings.rings_angular_velocity_multiplier
        * lifetime_seconds
        * -settings.rings_rotation_direction
}

// ─── 参数与归一化 ───

const MINIMUM_PARTICLE_LIFETIME_MS: f32 = 1.0;
const MAXIMUM_PARTICLE_LIFETIME_MS: f32 = 10_000.0;
const MAXIMUM_RING_COUNT: u32 = 64;
const MAXIMUM_RING_RADIUS: f32 = 2000.0;
const MAXIMUM_RING_ANGULAR_VELOCITY_MULTIPLIER: f32 = 100.0;
const MAXIMUM_CLICK_SHARD_COUNT: u32 = 1000;
const MAXIMUM_SHARD_RADIUS_OR_SPEED: f32 = 5000.0;
const MAXIMUM_SHARD_SIZE: f32 = 2000.0;
const MINIMUM_TRAIL_LENGTH_MULTIPLIER: f32 = 0.0;
const MAXIMUM_TRAIL_LENGTH_MULTIPLIER: f32 = 10000.0 / 300.0;
const MINIMUM_TIME_SCALE: f32 = 0.01;
const MAXIMUM_TIME_SCALE: f32 = 4.0;

/// 点击爆裂（中心圆盘 + 溶解环）参数。数值是 1080p 参考像素约定。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClickParticleSettings {
    pub disk_lifetime_ms: f32,
    pub rings_count: u32,
    pub rings_lifetime_ms: f32,
    pub rings_radius_min: f32,
    pub rings_radius_max: f32,
    pub rings_angular_velocity_multiplier: f32,
    pub rings_rotation_direction: f32,
}

impl Default for ClickParticleSettings {
    fn default() -> Self {
        Self {
            disk_lifetime_ms: 200.0,
            rings_count: 2,
            rings_lifetime_ms: 600.0,
            rings_radius_min: 68.925_712_32,
            rings_radius_max: 80.413_331_04,
            rings_angular_velocity_multiplier: 11.170_107,
            rings_rotation_direction: -1.0,
        }
    }
}

/// 碎片（点击爆裂三角 + 拖拽三角）参数，1080p 参考像素。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShardParticleSettings {
    pub click_count: u32,
    pub click_lifetime_min_ms: f32,
    pub click_lifetime_max_ms: f32,
    pub click_radius: f32,
    pub click_speed_min: f32,
    pub click_speed_max: f32,
    pub size_min: f32,
    pub size_max: f32,
}

impl Default for ShardParticleSettings {
    fn default() -> Self {
        Self {
            click_count: 4,
            click_lifetime_min_ms: 600.0,
            click_lifetime_max_ms: 700.0,
            click_radius: 49.876_948_8,
            click_speed_min: 49.876_948_8,
            click_speed_max: 66.502_598_4,
            size_min: 16.625_649_6,
            size_max: 33.251_299_2,
        }
    }
}

fn normalize_finite_range(value: f32, fallback: f32, minimum: f32, maximum: f32) -> f32 {
    if !value.is_finite() {
        return fallback;
    }
    value.clamp(minimum, maximum)
}

pub fn normalize_click_particle_settings(
    mut settings: ClickParticleSettings,
) -> ClickParticleSettings {
    let defaults = ClickParticleSettings::default();
    settings.disk_lifetime_ms = normalize_finite_range(
        settings.disk_lifetime_ms,
        defaults.disk_lifetime_ms,
        MINIMUM_PARTICLE_LIFETIME_MS,
        MAXIMUM_PARTICLE_LIFETIME_MS,
    );
    settings.rings_count = settings.rings_count.min(MAXIMUM_RING_COUNT);
    settings.rings_lifetime_ms = normalize_finite_range(
        settings.rings_lifetime_ms,
        defaults.rings_lifetime_ms,
        MINIMUM_PARTICLE_LIFETIME_MS,
        MAXIMUM_PARTICLE_LIFETIME_MS,
    );
    settings.rings_radius_min = normalize_finite_range(
        settings.rings_radius_min,
        defaults.rings_radius_min,
        0.0,
        MAXIMUM_RING_RADIUS,
    );
    settings.rings_radius_max = normalize_finite_range(
        settings.rings_radius_max,
        defaults.rings_radius_max,
        0.0,
        MAXIMUM_RING_RADIUS,
    );
    settings.rings_angular_velocity_multiplier = normalize_finite_range(
        settings.rings_angular_velocity_multiplier,
        defaults.rings_angular_velocity_multiplier,
        0.0,
        MAXIMUM_RING_ANGULAR_VELOCITY_MULTIPLIER,
    );
    settings.rings_rotation_direction = normalize_finite_range(
        settings.rings_rotation_direction,
        defaults.rings_rotation_direction,
        -1.0,
        1.0,
    );
    settings
}

pub fn normalize_shard_particle_settings(
    mut settings: ShardParticleSettings,
) -> ShardParticleSettings {
    let defaults = ShardParticleSettings::default();
    settings.click_count = settings.click_count.min(MAXIMUM_CLICK_SHARD_COUNT);
    settings.click_lifetime_min_ms = normalize_finite_range(
        settings.click_lifetime_min_ms,
        defaults.click_lifetime_min_ms,
        MINIMUM_PARTICLE_LIFETIME_MS,
        MAXIMUM_PARTICLE_LIFETIME_MS,
    );
    settings.click_lifetime_max_ms = normalize_finite_range(
        settings.click_lifetime_max_ms,
        defaults.click_lifetime_max_ms,
        MINIMUM_PARTICLE_LIFETIME_MS,
        MAXIMUM_PARTICLE_LIFETIME_MS,
    );
    settings.click_radius = normalize_finite_range(
        settings.click_radius,
        defaults.click_radius,
        0.0,
        MAXIMUM_SHARD_RADIUS_OR_SPEED,
    );
    settings.click_speed_min = normalize_finite_range(
        settings.click_speed_min,
        defaults.click_speed_min,
        0.0,
        MAXIMUM_SHARD_RADIUS_OR_SPEED,
    );
    settings.click_speed_max = normalize_finite_range(
        settings.click_speed_max,
        defaults.click_speed_max,
        0.0,
        MAXIMUM_SHARD_RADIUS_OR_SPEED,
    );
    settings.size_min = normalize_finite_range(
        settings.size_min,
        defaults.size_min,
        0.0,
        MAXIMUM_SHARD_SIZE,
    );
    settings.size_max = normalize_finite_range(
        settings.size_max,
        defaults.size_max,
        0.0,
        MAXIMUM_SHARD_SIZE,
    );
    settings
}

pub fn normalize_trail_length_multiplier(multiplier: f32) -> f32 {
    if !multiplier.is_finite() {
        return 1.0;
    }
    multiplier.clamp(
        MINIMUM_TRAIL_LENGTH_MULTIPLIER,
        MAXIMUM_TRAIL_LENGTH_MULTIPLIER,
    )
}

pub fn normalize_time_scale(time_scale: f32) -> f32 {
    if !time_scale.is_finite() {
        return 1.0;
    }
    time_scale.clamp(MINIMUM_TIME_SCALE, MAXIMUM_TIME_SCALE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hermite_matches_endpoints() {
        let keys = [
            CurveKey::new(0.0, 0.5, 0.0, 1.0),
            CurveKey::new(1.0, 2.0, -1.0, 0.0),
        ];
        assert_eq!(evaluate_hermite(&keys, -1.0), 0.5);
        assert_eq!(evaluate_hermite(&keys, 2.0), 2.0);
        // 端点连续
        assert!((evaluate_hermite(&keys, 0.0) - 0.5).abs() < 1e-6);
        assert!((evaluate_hermite(&keys, 1.0) - 2.0).abs() < 1e-5);
    }

    #[test]
    fn integrate_matches_numeric() {
        let keys = [
            CurveKey::new(0.0, 0.0, 0.0, 1.0),
            CurveKey::new(0.5, 1.0, 0.0, -1.0),
            CurveKey::new(1.0, 0.0, 0.0, 0.0),
        ];
        let analytic = integrate_hermite(&keys, 1.0);
        let mut numeric = 0.0f64;
        let steps = 20_000;
        for index in 0..steps {
            let t = (index as f64 + 0.5) / steps as f64;
            numeric += evaluate_hermite(&keys, t as f32) as f64 / steps as f64;
        }
        assert!((analytic as f64 - numeric).abs() < 1e-3, "{analytic} vs {numeric}");
    }

    #[test]
    fn srgb_transfer_round_trips() {
        for value in [0.0f32, 0.05, 0.2, 0.5, 0.8, 1.0] {
            let round = linear_to_srgb_channel(srgb_to_linear_channel(value));
            assert!((round - value).abs() < 1e-5, "{value} -> {round}");
        }
    }
}
