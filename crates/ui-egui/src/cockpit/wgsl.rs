//! Backdrop shaders as WGSL: what the model writes (a `scene` function), what byob puts
//! around it ([`PRELUDE`]: the inputs, helpers and the entry points), and the check that
//! runs before a GPU ever sees it ([`check`]).
//!
//! The model's code comes first in the module, so a line number in an error is a line of
//! its own code. (WGSL declarations can be used before they're written.)

/// Panels the shader can see (`u.panels`), at most.
pub const MAX_PANELS: usize = 16;

/// The longest backdrop source kept.
pub const MAX_SOURCE: usize = 16 * 1024;

/// What byob adds after the model's code. `u` holds the frame's inputs; the helpers are
/// documented for the model in [`GUIDE`].
pub const PRELUDE: &str = r"
// ---- byob ----
struct Byob {
    size: vec2<f32>,
    mouse: vec2<f32>,
    time: f32,
    busy: f32,
    panel_count: u32,
    scale: f32,
    accent: vec4<f32>,
    background: vec4<f32>,
    text: vec4<f32>,
    hot: vec4<f32>,
    hot_state: vec4<f32>,
    panels: array<vec4<f32>, 16>,
}

@group(0) @binding(0) var<uniform> u: Byob;

fn hash21(p: vec2<f32>) -> f32 {
    var q = fract(p * vec2<f32>(123.34, 456.21));
    q += dot(q, q + 45.32);
    return fract(q.x * q.y);
}

fn noise2(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let w = f * f * (3.0 - 2.0 * f);
    let a = hash21(i);
    let b = hash21(i + vec2<f32>(1.0, 0.0));
    let c = hash21(i + vec2<f32>(0.0, 1.0));
    let d = hash21(i + vec2<f32>(1.0, 1.0));
    return mix(mix(a, b, w.x), mix(c, d, w.x), w.y);
}

fn fbm2(p: vec2<f32>) -> f32 {
    var v = 0.0;
    var a = 0.5;
    var q = p;
    for (var i = 0; i < 5; i++) {
        v += a * noise2(q);
        q = q * 2.03 + vec2<f32>(17.0, 9.0);
        a *= 0.5;
    }
    return v;
}

fn sd_box(p: vec2<f32>, center: vec2<f32>, half_size: vec2<f32>, radius: f32) -> f32 {
    let d = abs(p - center) - half_size + vec2<f32>(radius);
    return length(max(d, vec2<f32>(0.0))) + min(max(d.x, d.y), 0.0) - radius;
}

fn panel_sdf(p: vec2<f32>) -> f32 {
    var d = 1e6;
    let n = min(u.panel_count, 16u);
    for (var i = 0u; i < n; i++) {
        let r = u.panels[i];
        d = min(d, sd_box(p, (r.xy + r.zw) * 0.5, (r.zw - r.xy) * 0.5, 8.0));
    }
    return d;
}

fn panel_glow(p: vec2<f32>, radius: f32) -> f32 {
    return exp(-max(panel_sdf(p), 0.0) / max(radius, 0.001));
}

fn hot_sdf(p: vec2<f32>) -> f32 {
    if (u.hot_state.x <= 0.0) {
        return 1e6;
    }
    return sd_box(p, (u.hot.xy + u.hot.zw) * 0.5, (u.hot.zw - u.hot.xy) * 0.5, 4.0);
}

fn soft_shadow(p: vec2<f32>, light: vec3<f32>, height: f32) -> f32 {
    let to = light.xy - p;
    let dist = length(to);
    if (dist < 0.001) {
        return 1.0;
    }
    let dir = to / dist;
    var res = 1.0;
    var t = 1.0;
    for (var i = 0; i < 32; i++) {
        if (t >= dist || light.z * t / dist > height) {
            break;
        }
        let d = panel_sdf(p + dir * t);
        res = min(res, 6.0 * d / t);
        if (res <= 0.0) {
            return 0.0;
        }
        t += max(d, 2.0);
    }
    return clamp(res, 0.0, 1.0);
}

@vertex
fn byob_vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let x = f32(i / 2u) * 4.0 - 1.0;
    let y = f32(i % 2u) * 4.0 - 1.0;
    return vec4<f32>(x, y, 0.0, 1.0);
}

@fragment
fn byob_fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let c = scene(pos.xy / u.scale);
    var rgb = clamp(c.rgb, vec3<f32>(0.0), vec3<f32>(1.0));
    if (BYOB_LINEAR_TARGET) {
        rgb = pow(rgb, vec3<f32>(2.2));
    }
    return vec4<f32>(rgb, 1.0);
}
";

/// The overlay's entry point, added when the scene defines `fn overlay`: its colour over
/// everything, premultiplied (rgb is added; alpha darkens what's under it).
const OVERLAY_ENTRY: &str = r"
@fragment
fn byob_overlay_fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let c = overlay(pos.xy / u.scale);
    let a = clamp(c.a, 0.0, 0.6);
    var rgb = clamp(c.rgb, vec3<f32>(0.0), vec3<f32>(1.0));
    if (BYOB_LINEAR_TARGET) {
        rgb = pow(rgb, vec3<f32>(2.2));
    }
    return vec4<f32>(rgb, a);
}
";

/// Whether the scene draws over the interface too (`fn overlay`).
pub fn has_overlay(source: &str) -> bool {
    source.contains("fn overlay")
}

/// For the model: what a backdrop is and what it can use (sent with the window's look).
pub const GUIDE: &str = "A backdrop is a WGSL shader drawn behind byob's panels, for every pixel, up to 30 times a second. Write module-scope WGSL that defines

    fn scene(p: vec2<f32>) -> vec4<f32>

p is the pixel's position from the window's top left in points (y grows downwards). Return its colour: rgb from 0 to 1 as on a web page (sRGB); alpha is ignored. Your code may also define consts and other functions.

It can read, from the uniform u:
- u.size: the window's size in points; u.mouse: the pointer (-1, -1 when it's outside); u.time: seconds; u.scale: pixels per point
- u.busy: 0, easing to 1 while the model is working on a reply (make the scene come alive while it thinks)
- u.accent, u.background, u.text: the theme's colours (vec4<f32>, use .rgb)
- u.panel_count and u.panels[i] (x0, y0, x1, y1 in points): where byob's panels are (the header, chat list, chat, message box, panes)
- u.hot (x0, y0, x1, y1): the control under the pointer (a button, a field, a chat in the list); u.hot_state.x is 0 for none, 1 hovered, 2 pressed, and u.hot_state.y the seconds since it was first hovered

Helpers (don't redefine them): hash21(p) -> f32 in 0..1; noise2(p), fbm2(p): smooth noise in 0..1; sd_box(p, center, half_size, radius): a rounded box's signed distance; panel_sdf(p): distance to the nearest panel, negative inside one; panel_glow(p, radius): 1 at a panel's edge fading to 0 over about radius points; hot_sdf(p): distance to the control under the pointer (huge when there's none); soft_shadow(p, light: vec3<f32>, height): 0 in the shadow the panels cast from a light at light.xy, light.z points above the window, as if the panels stood height points tall, else 1 (soft at the edges).

Optionally also define fn overlay(p: vec2<f32>) -> vec4<f32>, drawn over the whole interface, text included: its rgb is added (glows, light catching a control: use hot_sdf) and its alpha (at most 0.6) darkens what's below (scanlines, a vignette). Keep it faint: the text has to stay readable. Return vec4<f32>(0.0) where it should do nothing.

Keep it cheap: loops with a small fixed bound (a few dozen steps), no loops over pixels. The panels are see-through over the backdrop (the look's glass setting, 1 opaque); text sits on them, so keep it calm where panel_sdf(p) < 0 or dim the scene there. A backdrop that fails to compile is refused with the line and the reason.

Re-theming the whole window as a place or mood (\"a starship's bridge\", \"a rainy neon city\", \"a cosy library\"), in one ui_look call when you can:
1. Colours first: mode, accent and the colour.* roles (app_bg, panel_bg, sidebar_bg, text, borders) in the palette of the place.
2. The backdrop: a scene of the place behind the panels (keep it dark or calm where text sits), alive with u.busy and the pointer.
3. glass, frost and panel_gap: how much of the scene shows through and between the panels.
4. The arrangement, when the place has a shape: arrangement free (or floating to start from), place.<part> for where each part goes (leave open space where the scene should show), bezel.<part> (metal for consoles and dials, glass, bare) and label.<part> (short words stamped on a bezel, in the place's own vocabulary).
5. controls (hud, plate, pill), corners and fonts to match.
Evoke a look the user names from films, games or brands with original details of your own (colours, shapes, light, words); don't copy logos, insignia or text from them. Afterwards say what you changed and that \"undo that\" puts the last look back.

A small example to start from (a sky over a glowing horizon, the panels casting shadows from the pointer, the hovered control lit):

    const GLOW = vec3<f32>(1.0, 0.55, 0.2);
    fn scene(p: vec2<f32>) -> vec4<f32> {
        let uv = p / u.size;
        var col = mix(vec3<f32>(0.02, 0.03, 0.07), GLOW * 0.35, smoothstep(0.4, 1.0, uv.y));
        col += vec3<f32>(step(0.996, hash21(floor(p)))) * (1.0 - uv.y);
        col *= 0.5 + 0.5 * soft_shadow(p, vec3<f32>(u.mouse, 300.0), 16.0);
        col += GLOW * panel_glow(p, 12.0) * (0.15 + 0.3 * u.busy);
        return vec4<f32>(col, 1.0);
    }
    fn overlay(p: vec2<f32>) -> vec4<f32> {
        let lit = exp(-max(hot_sdf(p), 0.0) / 6.0) * min(u.hot_state.x, 1.0);
        return vec4<f32>(GLOW * lit * 0.25, 0.0);
    }";

/// The whole module for `source` (the model's code), `linear` when the window's surface
/// takes linear colour (the scene's sRGB colour is converted).
pub fn module(source: &str, linear: bool) -> String {
    let overlay = if has_overlay(source) { OVERLAY_ENTRY } else { "" };
    format!("{source}\n{PRELUDE}{overlay}").replace("BYOB_LINEAR_TARGET", if linear { "true" } else { "false" })
}

/// The line count of `source` as [`module`] places it, for errors past its end.
fn lines(source: &str) -> usize {
    source.lines().count()
}

/// Check `source` as WGSL with byob's prelude, as wgpu would. Errors are short, with the
/// line and column in `source` when the problem is there ("line 12:5: …").
pub fn check(source: &str) -> Result<(), String> {
    if source.len() > MAX_SOURCE {
        return Err(format!("it's {} bytes; a backdrop can be at most {MAX_SOURCE}", source.len()));
    }
    if !source.contains("fn scene") {
        return Err("it needs fn scene(p: vec2<f32>) -> vec4<f32>".into());
    }
    let full = module(source, false);
    let at = |loc: Option<naga::SourceLocation>| match loc {
        Some(l) if (l.line_number as usize) <= lines(source) => format!("line {}:{}: ", l.line_number, l.line_position),
        Some(_) => "in its use of byob's helpers: ".into(),
        None => String::new(),
    };
    let module = naga::front::wgsl::parse_str(&full).map_err(|e| format!("{}{}", at(e.location(&full)), e.message()))?;
    let mut validator = naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::empty());
    validator.validate(&module).map_err(|e| {
        let mut text = e.as_inner().to_string();
        let mut source: Option<&dyn std::error::Error> = std::error::Error::source(e.as_inner());
        while let Some(s) = source {
            text = format!("{text}: {s}");
            source = s.source();
        }
        format!("{}{text}", at(e.location(&full)))
    })?;
    Ok(())
}

/// Whether the scene moves by itself (reads `u.time` or `u.busy`): only then is the window
/// redrawn for it.
pub fn animated(source: &str) -> bool {
    source.contains("u.time") || source.contains("u.busy")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guides_example_compiles() {
        let start = GUIDE.find("    const GLOW").unwrap();
        let example: String = GUIDE[start..].lines().map(|l| l.strip_prefix("    ").unwrap_or(l)).collect::<Vec<_>>().join("\n");
        assert!(has_overlay(&example));
        assert_eq!(check(&example), Ok(()));
    }

    #[test]
    fn a_simple_scene_compiles_and_uses_the_helpers() {
        let src = "fn scene(p: vec2<f32>) -> vec4<f32> {\n    let s = soft_shadow(p, vec3<f32>(u.mouse, 200.0), 12.0);\n    return vec4<f32>(u.accent.rgb * (0.2 + 0.8 * s) * fbm2(p * 0.01), 1.0);\n}\n";
        assert_eq!(check(src), Ok(()));
        assert!(!animated(src));
        assert!(animated("fn scene(p: vec2<f32>) -> vec4<f32> { return vec4<f32>(sin(u.time)); }"));
    }

    #[test]
    fn mistakes_name_the_line_in_the_models_code() {
        let e = check("fn scene(p: vec2<f32>) -> vec4<f32> {\n    let a = 1.0;\n    return vec4<f32>(a, nope, 0.0, 1.0);\n}\n").unwrap_err();
        assert!(e.starts_with("line 3:"), "{e}");
        assert!(e.contains("nope"), "{e}");
        // A type mistake is found by validation, not parsing.
        let e = check("fn scene(p: vec2<f32>) -> vec4<f32> {\n    return p;\n}\n").unwrap_err();
        assert!(e.contains("line 2") || e.contains("scene"), "{e}");
        assert!(check("fn other() {}").unwrap_err().contains("fn scene"));
        assert!(check(&"x".repeat(MAX_SOURCE + 1)).unwrap_err().contains("at most"));
    }

    /// A scene with an overlay compiles with its second entry point, and the overlay's
    /// mistakes are found too.
    #[test]
    fn an_overlay_is_checked_with_the_scene() {
        let scene = "fn scene(p: vec2<f32>) -> vec4<f32> { return vec4<f32>(0.0, 0.0, 0.0, 1.0); }\n";
        let ok = format!(
            "{scene}fn overlay(p: vec2<f32>) -> vec4<f32> {{\n    let g = exp(-max(hot_sdf(p), 0.0) / 8.0) * u.hot_state.x;\n    return vec4<f32>(u.accent.rgb * g * 0.3, 0.05 * step(0.5, fract(p.y * 0.5)));\n}}\n"
        );
        assert!(has_overlay(&ok) && !has_overlay(scene));
        assert_eq!(check(&ok), Ok(()));
        assert!(module(&ok, false).contains("byob_overlay_fs") && !module(scene, false).contains("byob_overlay_fs"));
        let bad = format!("{scene}fn overlay(p: vec2<f32>) -> vec4<f32> {{\n    return 1.0;\n}}\n");
        assert!(check(&bad).is_err());
    }

    #[test]
    fn redefining_a_helper_is_refused() {
        let e = check("fn hash21(p: vec2<f32>) -> f32 { return 0.0; }\nfn scene(p: vec2<f32>) -> vec4<f32> { return vec4<f32>(0.0); }\n").unwrap_err();
        assert!(e.contains("hash21"), "{e}");
    }
}
