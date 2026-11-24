#version 450
// One triangle that covers the viewport; `at` runs 0..1 across the picture.
layout(location = 0) out vec2 at;

void main() {
    at = vec2((gl_VertexIndex << 1) & 2, gl_VertexIndex & 2);
    gl_Position = vec4(at * 2.0 - 1.0, 0.0, 1.0);
}
