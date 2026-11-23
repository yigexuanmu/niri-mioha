#version 100

precision highp float;

varying vec2 v_coords;

uniform sampler2D tex;
uniform vec2 texel_size;

vec4 four_tap(vec2 uv, vec2 offset) {
    return 0.25 * (
        texture2D(tex, uv + offset * vec2(-1.0, -1.0)) +
        texture2D(tex, uv + offset * vec2(1.0, -1.0)) +
        texture2D(tex, uv + offset * vec2(-1.0, 1.0)) +
        texture2D(tex, uv + offset * vec2(1.0, 1.0)));
}

void main() {
    gl_FragColor = four_tap(v_coords, texel_size);
}