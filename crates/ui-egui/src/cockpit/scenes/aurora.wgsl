// Aurora: slow ribbons of the theme's accent over a night sky; calm enough to read on.
fn scene(p: vec2<f32>) -> vec4<f32> {
    let uv = p / u.size;
    let t = u.time * 0.05;
    var col = mix(u.background.rgb * 0.6, vec3<f32>(0.01, 0.015, 0.04), uv.y);
    for (var i = 0; i < 3; i++) {
        let k = f32(i);
        let wave = fbm2(vec2<f32>(uv.x * 2.5 + t * (1.0 + k * 0.3) + k * 5.0, t + k));
        let band = exp(-pow((uv.y - 0.25 - 0.25 * wave - k * 0.08) * 9.0, 2.0));
        let tint = mix(u.accent.rgb, vec3<f32>(0.3, 0.9, 0.6), k * 0.4);
        col += tint * band * (0.18 + 0.12 * u.busy);
    }
    col += vec3<f32>(step(0.997, hash21(floor(p * 0.5)))) * (1.0 - uv.y) * 0.6;
    return vec4<f32>(col, 1.0);
}
