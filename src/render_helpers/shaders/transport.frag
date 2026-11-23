#version 100

precision highp float;

varying vec2 v_coords;

uniform sampler2D direct_tex;
uniform sampler2D bloom_tex;
uniform vec2 bloom_texel_size;
uniform float sample_scale;
uniform float exposure_gain;
uniform float alpha_limit;

float linear_to_srgb_ch(float value) {
    float clamped = clamp(value, 0.0, 1.0);
    if (clamped <= 0.0031308) {
        return clamped * 12.92;
    }
    return 1.055 * pow(clamped, 1.0 / 2.4) - 0.055;
}

vec3 linear_to_srgb(vec3 value) {
    return vec3(
        linear_to_srgb_ch(value.r),
        linear_to_srgb_ch(value.g),
        linear_to_srgb_ch(value.b));
}

vec4 four_tap(vec2 uv, vec2 offset) {
    return 0.25 * (
        texture2D(bloom_tex, uv + offset * vec2(-1.0, -1.0)) +
        texture2D(bloom_tex, uv + offset * vec2(1.0, -1.0)) +
        texture2D(bloom_tex, uv + offset * vec2(-1.0, 1.0)) +
        texture2D(bloom_tex, uv + offset * vec2(1.0, 1.0)));
}

void main() {
    vec4 direct = texture2D(direct_tex, v_coords);
    // resize 阶段：4-tap 读取最终 bloom 层（上游 `SampleBloomForResolve`）。
    vec2 offset = bloom_texel_size * (sample_scale * 0.5);
    vec4 bloom = four_tap(v_coords, offset);

    // 上游默认 BackgroundAware 预设的 UnknownBackground 分支：
    //   linear = direct + bloom * exposureGain
    //   srgb   = LinearToSrgb(linear)
    //   alpha  = min(max(direct.a, LinearToSrgbChannel(bloom.a * gain)), 250/255)
    // 雾色相：上游默认远场 R:G:B = 0.24:0.39:1.0。盘纹理的 G 比作者蓝高，
    // Bloom 卷积后远场 G 会到 0.45；按上游默认比例把 Bloom 的 G 乘 0.86。
    // 只改 Bloom（雾），direct 盘心/环/碎片不受影响。
    const float BLOOM_GREEN_SCALE = 0.86;
    vec3 bloom_rgb = max(bloom.rgb, 0.0) * exposure_gain;
    bloom_rgb.g *= BLOOM_GREEN_SCALE;
    vec3 linear_rgb = max(direct.rgb, 0.0) + bloom_rgb;
    vec3 srgb = linear_to_srgb(linear_rgb);
    float scene_coverage = clamp(direct.a, 0.0, 1.0);
    float bloom_transport = linear_to_srgb_ch(
        max(bloom.a, 0.0) * exposure_gain);
    float alpha = min(max(scene_coverage, bloom_transport), alpha_limit);
    if (alpha <= 0.000001) {
        gl_FragColor = vec4(0.0, 0.0, 0.0, 0.0);
        return;
    }
    // 不压缩、不混白、不补偿：预乘 sRGB 直接交给 smithay 的
    // ONE / ONE_MINUS_SRC_ALPHA。发射项允许超过 alpha（上游 extended premultiplied）。
    gl_FragColor = vec4(srgb, alpha);
}