void main() {
    vec2 p = v_uv * u_size;
    float d = mantle_sdf(p);
    vec2 toward = normalize(u_size * 0.5 - p + 1e-4);
    vec2 grain = (texture(noise, fract(v_uv * 2.0)).rg - 0.5) * 4.0 / u_size;
    vec2 bent = v_uv + toward * smoothstep(-12.0, 0.0, d) * (4.0 + 8.0 * u_progress) / u_size + grain;
    float inside = 1.0 - smoothstep(-0.5, 0.5, d);
    fragColor = mix(mantle_input(v_uv), mantle_input_blurred(bent), inside);
}
