uniform vec3 tint;

void main() {
    float edge = abs(v_uv.y - 0.5) * 2.0;
    float alpha = (1.0 - edge) * u_progress;
    fragColor = vec4(tint * alpha, alpha); // premultiplied
}
