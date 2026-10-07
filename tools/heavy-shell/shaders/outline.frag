uniform float width;
uniform vec4 tint;

void main() {
    vec4 body = mantle_input(v_uv);
    float d = mantle_sdf(v_uv * u_size);
    float w = width * (0.5 + u_progress);
    float rim = smoothstep(-w - 0.5, -w + 0.5, d) * (1.0 - smoothstep(-0.5, 0.5, d));
    fragColor = body * (1.0 - rim * tint.a) + vec4(tint.rgb * tint.a, tint.a) * rim;
}
