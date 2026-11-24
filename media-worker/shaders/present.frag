#version 450
// Turns the two planes of a P010 picture into R'G'B'. The transfer function
// is left as it is: the swapchain's colour space names it.
layout(set = 0, binding = 0) uniform sampler2D luma;
layout(set = 0, binding = 1) uniform sampler2D chroma;
layout(push_constant) uniform Colour { mat4 to_rgb; } colour;
layout(location = 0) in vec2 at;
layout(location = 0) out vec4 colour_out;

void main() {
    // A chroma sample sits on the left one of its two luma samples, not
    // between them: a quarter of a chroma texel from where `at` points.
    vec2 chroma_at = at + vec2(0.25 / textureSize(chroma, 0).x, 0.0);
    vec4 sampled = vec4(texture(luma, at).r, texture(chroma, chroma_at).rg, 1.0);
    colour_out = vec4((colour.to_rgb * sampled).rgb, 1.0);
}
