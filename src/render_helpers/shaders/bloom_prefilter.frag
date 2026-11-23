#version 100

precision highp float;

varying vec2 v_coords;

uniform sampler2D tex;
uniform vec2 texel_size;

const float Threshold = 1.0;
const float Knee = 1.0e-5;
const float ClampValue = 65472.0;

vec4 four_tap(vec2 uv, vec2 offset) {
    return 0.25 * (
        texture2D(tex, uv + offset * vec2(-1.0, -1.0)) +
        texture2D(tex, uv + offset * vec2(1.0, -1.0)) +
        texture2D(tex, uv + offset * vec2(-1.0, 1.0)) +
        texture2D(tex, uv + offset * vec2(1.0, 1.0)));
}

vec4 bright_pass(vec3 source) {
    vec3 color = min(max(source, 0.0), ClampValue);
    float brightness = max(color.r, max(color.g, color.b));
    float soft = clamp(brightness - (Threshold - Knee), 0.0, 2.0 * Knee);
    soft = soft * soft * (0.25 / Knee);
    float contribution = max(soft, brightness - Threshold)
        / max(brightness, 0.0001);
    float transport_energy = brightness * contribution;
    return vec4(color * contribution, transport_energy);
}

void main() {
    vec4 source = four_tap(v_coords, texel_size);
    gl_FragColor = bright_pass(source.rgb);
}