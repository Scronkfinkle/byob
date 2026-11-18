// Cockpit: a dark hull, stars streaming past the canopy at the top, a cabin light that
// swings slowly and casts the panels' shadows, and instrument-amber edges that come alive
// while the model works.
const AMBER = vec3<f32>(1.0, 0.62, 0.22);

fn stars(p: vec2<f32>, t: f32) -> f32 {
    var s = 0.0;
    for (var layer = 1; layer <= 3; layer++) {
        let k = f32(layer);
        let q = p * (0.025 * k) + vec2<f32>(t * 0.5 * k, 0.0);
        let cell = floor(q);
        let f = fract(q) - 0.5;
        let h = hash21(cell + k * 13.0);
        let off = vec2<f32>(hash21(cell + 3.1), hash21(cell + 7.7)) - 0.5;
        let d = length(f - off * 0.6);
        let twinkle = 0.6 + 0.4 * sin(t * 3.0 + h * 40.0);
        s += step(0.9, h) * (1.0 - smoothstep(0.0, 0.08 / k, d)) * twinkle;
    }
    return s;
}

fn scene(p: vec2<f32>) -> vec4<f32> {
    let uv = p / u.size;
    // The hull: dark brushed metal.
    var col = vec3<f32>(0.035, 0.04, 0.05) + 0.03 * fbm2(vec2<f32>(p.x * 0.004, p.y * 0.08));
    // The canopy: space through the top of the window.
    let canopy = 1.0 - smoothstep(0.12, 0.4, uv.y);
    col += canopy * stars(p, u.time) * vec3<f32>(0.8, 0.9, 1.0);
    col += canopy * 0.06 * vec3<f32>(0.25, 0.35, 0.7) * fbm2(p * 0.003 + vec2<f32>(u.time * 0.02, 0.0));
    // The cabin light, swinging, and the shadows the panels cast from it.
    let light = vec3<f32>(u.size.x * (0.5 + 0.3 * sin(u.time * 0.2)), -40.0, 260.0);
    col *= 0.4 + 0.6 * soft_shadow(p, light, 18.0);
    // Instruments: the panels' edges glow amber, pulsing while the model works.
    let outside = step(0.0, panel_sdf(p));
    let pulse = 0.5 + 0.5 * sin(u.time * 4.0);
    col += AMBER * panel_glow(p, 10.0) * outside * (0.22 + u.busy * (0.3 + 0.4 * pulse));
    // A warm hand light at the pointer.
    col += AMBER * 0.08 * exp(-length(p - u.mouse) / 140.0) * step(0.0, u.mouse.x);
    // Scanlines and a vignette.
    col *= 0.93 + 0.07 * sin(p.y * 1.6);
    col *= 1.0 - 0.5 * pow(length(uv - 0.5) * 1.25, 2.0);
    return vec4<f32>(col, 1.0);
}

// Over the interface: faint scanlines, and the control under the pointer catching the
// instruments' amber light (brighter while pressed).
fn overlay(p: vec2<f32>) -> vec4<f32> {
    let lines = 0.035 * step(0.5, fract(p.y * 0.5));
    let d = max(hot_sdf(p), 0.0);
    let pressed = step(1.5, u.hot_state.x);
    let rise = min(u.hot_state.y * 6.0, 1.0);
    let glow = exp(-d / 6.0) * step(0.5, u.hot_state.x) * rise * (0.3 + 0.3 * pressed);
    return vec4<f32>(AMBER * glow, lines);
}
