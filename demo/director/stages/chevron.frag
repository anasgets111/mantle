// The next wallpaper pours in from the top behind a V edge, the logo's chevron, with a soft seam.
const float SLOPE = 0.35;
const float SOFT = 0.05;

void main() {
    float x = (v_uv.x - 0.5) * u_size.x / u_size.y;
    float shape = v_uv.y + abs(x) * SLOPE;
    float far = 1.0 + 0.5 * (u_size.x / u_size.y) * SLOPE;
    float edge = mix(-SOFT * 2.0, far + SOFT * 2.0, u_progress);
    float behind = smoothstep(edge - SOFT, edge + SOFT, shape);
    vec4 color = mix(mantle_to(v_uv), mantle_from(v_uv), behind);
    float seam = exp(-pow((shape - edge) / (SOFT * 0.6), 2.0));
    fragColor = vec4(color.rgb + vec3(0.18) * seam * color.a, color.a);
}
