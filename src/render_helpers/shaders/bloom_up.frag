#version 100

precision highp float;

varying vec2 v_coords;

/// 更小一级的累加结果。
uniform sampler2D tex;
/// 当前级（更细）的 downsample 结果。
uniform sampler2D fine_tex;
uniform vec2 texel_size;
uniform float sample_scale;

vec4 four_tap(vec2 uv, vec2 offset) {
    return 0.25 * (
        texture2D(tex, uv + offset * vec2(-1.0, -1.0)) +
        texture2D(tex, uv + offset * vec2(1.0, -1.0)) +
        texture2D(tex, uv + offset * vec2(-1.0, 1.0)) +
        texture2D(tex, uv + offset * vec2(1.0, 1.0)));
}

void main() {
    // 上游 UpsamplePixel：coarse 四抽头 + currentFine，权重恒 1，不归一化。
    vec2 offset = texel_size * (sample_scale * 0.5);
    vec4 coarse = four_tap(v_coords, offset);
    vec4 fine = texture2D(fine_tex, v_coords);
    gl_FragColor = coarse + fine;
}