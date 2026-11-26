#version 450
// Draws the stream overlay, an sRGB picture, above the video. In an HDR10
// swapchain it becomes PQ at the brightness of SDR white.
layout(set = 0, binding = 0) uniform sampler2D picture;
layout(push_constant) uniform Target { float pq; } target;
layout(location = 0) in vec2 at;
layout(location = 0) out vec4 colour_out;

// SDR white shown in HDR, as a part of 10000 nits (BT.2408: 203 nits).
const float WHITE = 203.0 / 10000.0;

void main() {
    vec3 colour = texture(picture, at).rgb;
    if (target.pq != 0.0) {
        vec3 light = mix(colour / 12.92, pow((colour + 0.055) / 1.055, vec3(2.4)), step(0.04045, colour));
        // Rec. 709 primaries to Rec. 2020.
        light = mat3(0.6274, 0.0691, 0.0164, 0.3293, 0.9195, 0.0880, 0.0433, 0.0114, 0.8956) * light;
        vec3 part = pow(light * WHITE, vec3(0.1593017578125));
        colour = pow((0.8359375 + 18.8515625 * part) / (1.0 + 18.6875 * part), vec3(78.84375));
    }
    colour_out = vec4(colour, 1.0);
}
