uniform vec3 tint_a;
uniform vec3 tint_b;

// Three drifting bands. Every term is periodic in u_progress, so a looping 0..1 run never jumps.
void main() {
    float t = u_progress * 6.2831853;
    float x = v_uv.x * u_size.x / u_size.y;
    vec3 color = vec3(0.0);
    float alpha = 0.0;
    for (int i = 0; i < 3; i++) {
        float fi = float(i);
        float wave = 0.30 + 0.06 * fi
            + 0.06 * sin(x * 1.3 + t + fi * 1.7)
            + 0.03 * sin(x * 3.1 - 2.0 * t + fi);
        float band = exp(-pow((v_uv.y - wave) * (8.0 + 3.0 * fi), 2.0));
        float shimmer = 0.6 + 0.4 * sin(x * 9.0 + 3.0 * t + fi * 2.0);
        vec3 tint = mix(tint_a, tint_b, 0.5 + 0.5 * sin(x * 0.8 + t + fi));
        float a = band * shimmer * (0.24 - 0.05 * fi);
        color += tint * a;
        alpha += a;
    }
    fragColor = vec4(color, min(alpha, 1.0));
}
