# The cockpit (experimental, branch `exp/cockpit`)

Some users want byob to look like anything they can describe ("a bounty hunter's gunship
cockpit"), not just a recoloured chat app. This branch explores how far that goes on egui,
without switching UI frameworks, in three steps, each usable on its own:

1. **Backdrops**: a WGSL shader drawn behind byob, with the panels as glass over it.
2. **Free arrangement**: the window's parts as modules placed anywhere, in bezels.
3. **Lit controls**: button styles, and a scene that sees (and can light) the control under
   the pointer, with an optional overlay drawn over the whole interface.

All of it is reachable three ways: by asking the model (the `ui_look` tool, through the
bundled Appearance plugin), in Settings → Appearance → Cockpit, and from the command line.

## Trying it

```sh
scripts/headless.sh --backdrop cockpit --arrangement cockpit --look controls=hud \
    --look accent=#e8a33d --screenshot /tmp/cockpit.png     # or plain `byob …` for a window
byob --backdrop my-scene.wgsl          # your own scene (checked first)
byob --arrangement focus --arranging   # the chat alone; handles to drag parts around
BYOB_NO_BACKDROP=1 byob                # never draw backdrops (a scene that misbehaves)
```

In the app: **⋯ → Arrange panels…** shows handles to drag, resize (corner) and hide (×) each
part, and a bar with the bundled arrangements, hidden parts, **Dock everything** and **Done**.

Ask the model: "make byob look like a starship's bridge", "put the chat in the middle and hide
the chat list", "HUD buttons", "a calmer backdrop". It reads the look first (`ui_look` with no
arguments), which now includes the backdrop guide (`cockpit::wgsl::GUIDE`, with a worked
example) and the current custom shader's source.

## How it's built

```
┌ Debug layer ─ overlay pass: fn overlay(p) (premultiplied: glows add, alpha darkens) ─┐
│ egui: panels / free modules (glass = panel colours with alpha), text, controls       │
└ background layer ─ scene pass: fn scene(p), drawn first, full window ─────────────────┘
```

- `crates/ui-egui/src/cockpit/`
  - `wgsl.rs`: the contract. The model writes `fn scene(p: vec2<f32>) -> vec4<f32>` (and
    optionally `fn overlay`); byob appends a prelude: the uniform `u` (size, time, pointer,
    busy, theme colours, the hovered control, up to 16 panel rects) and helpers (`hash21`,
    `noise2`, `fbm2`, `sd_box`, `panel_sdf`, `panel_glow`, `soft_shadow`, `hot_sdf`). The
    model's code goes first, so error line numbers are its own. `check` parses and validates
    with naga (wgpu's compiler, same version) before any GPU sees it.
  - `backdrop.rs`: egui-wgpu paint callbacks. Pipelines are built in `prepare` when the
    scene changes; inputs are written each frame. `install` replaces wgpu's default error
    handler (which ends the process) with one that logs and reports.
  - `mod.rs`: the per-device settings (`Cockpit`: backdrop, glass, gap, trial,
    arrangement), the bundled scenes, and `Stage` (draws the scene, eases `busy`, finds the
    hovered control via `interaction_snapshot` + `read_response`, times the trial).
  - `arrange.rs`: modules, places (% of the window, so arrangements survive resizing),
    bezels (glass, instrument, bare), presets, least sizes (part + bezel).
  - `free.rs`: drawing a free arrangement (each module in a child `Ui` with its own clip),
    the chat making room for open panes, and arranging by hand.
- `look.rs`: `backdrop`, `glass`, `panel_gap`, `controls`, `arrangement`, `place.<part>`,
  `bezel.<part>`, `label.<part>`; the report carries the guide and the source as `notes`
  (a new optional field of `LookReport`, shown to the model by `ui_look`).
- Docked panels over a backdrop get a gap and rounded corners (`cockpit::framed`), so the
  scene shows between them; without a backdrop nothing changes.

### Safety

- naga validation catches syntax, type and binding mistakes; the model gets
  `line 12:5: …` back and can fix it.
- What compiles can still be slow or loop forever. A new backdrop is **on trial** until it
  has been drawn for 3 s; one still on trial at the next start is dropped with a note (it
  may have hung the GPU or crashed byob). `BYOB_NO_BACKDROP=1` is the manual escape.
- GPU errors switch the backdrop off with a notice instead of ending byob.
- The scene redraws at most 30 times a second, only while the window is focused, and only
  when it reads `u.time`/`u.busy` (or is easing); "reduce motion" freezes time.
- Phones (narrow windows), Settings and maximized artifacts keep their usual screens, so a
  bad arrangement can't hide the way out. Esc ends arranging.

## What the live test showed (silicon, the Qwen-style reasoning model)

- Asked to "re-theme byob as the cockpit of a bounty hunter's battered gunship", the model
  read the look and layout, then wrote an ambitious scene of its own: a planet's limb in the
  canopy, star layers, rivet rows, rust and scorch from fbm, a cracked pane, a lock reticle
  that tightens while it works (`u.busy`). With only colours and a bundled scene it applied
  an amber accent, the cockpit backdrop, glass and corners in one call.
- **It never got to apply its own shader**: it thinks for 60–80k characters first. With the
  server's default output limit (~8k tokens) the reply ended mid-thought; with 30k it ended
  mid-tool-call (the call left as text). The guide now has a worked example to adapt.
- **byob bug (on main too):** the turn loop ignores `finish_reason: "length"`. A reply cut
  off at the output limit ends silently, with no answer and no note. That is also why the
  bench tool's interpretation came back empty. Worth fixing on main: at least say "the
  model hit its output limit", maybe continue once.

## What egui can't do here (and Rune could)

- **Frosted glass** (blurring what's behind a panel) needs the frame rendered to a texture
  first; eframe draws egui straight to the window. Possible by taking over eframe's render
  loop, or with a custom egui-wgpu integration.
- **Relighting the finished UI** (text included) as one image, Rune-light style: same
  reason. The overlay pass gets close for glows and scanlines but can't sample what's below.
- **Per-widget shaders**: egui widgets paint with its own tessellated shapes. The scene sees
  the hovered control's rect, which covers most "lit button" effects.

## Ideas, roughly in order of value

1. Fix `finish_reason: length` on main (above).
2. **Skins**: one file with look + backdrop + arrangement (+ controls), shareable like
   theme files, installable from a plugin. Today they're separate settings.
3. A `ui_backdrop` tool (or `ui_look` mode) for iterating on a shader alone: write, see the
   error or a screenshot of the result, adjust, without resending every setting.
4. Show the model its work: a screenshot of the window after a look change (the window can
   render one; vision models could then judge readability themselves).
5. Animated transitions between arrangements (spring the module rects).
6. Module bezels as shaders too (`fn bezel(p, rect, kind)`), for fully drawn instrument
   panels.
7. Frosted glass via render-to-texture (see above), if the look is worth the plumbing.

Not done: testing in the browser (the wasm build compiles and the code paths are the same,
but no live run), integrated-GPU performance, and light themes (the bundled scenes are dark).
