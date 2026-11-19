# The cockpit (experimental)

Some users want byob to look like anything they can describe ("a bounty hunter's gunship
cockpit"), not just a recoloured chat app. This explores how far that goes on egui,
without switching UI frameworks, in four parts, each usable on its own. byob ships the
capabilities and the guide agents re-theme with, not themes of its own: every scene is
written by the model (or loaded from a file).

1. **Backdrops**: a WGSL shader drawn behind byob, with the panels as glass over it.
2. **Free arrangement**: the window's parts as modules placed anywhere, in bezels.
3. **Lit controls**: button styles, and a scene that sees (and can light) the control under
   the pointer, with an optional overlay drawn over the whole interface.
4. **Frosted glass** (the compositor): byob drawn into a texture and composited over the
   scene, blurred behind the panels and sharp between them.

All of it is reachable three ways: by asking the model (the `ui_look` tool, through the
bundled Appearance plugin), in Settings → Appearance → Backdrop and glass, and from the command line.

## Trying it

```sh
scripts/headless.sh --backdrop my-scene.wgsl --arrangement floating --look controls=hud \
    --look accent=#e8a33d --screenshot /tmp/cockpit.png     # or plain `byob …` for a window
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
    arrangement), and `Stage` (draws the scene, eases `busy`, finds the
    hovered control via `interaction_snapshot` + `read_response`, times the trial).
  - `arrange.rs`: modules, places (% of the window, so arrangements survive resizing),
    bezels (glass, metal, bare), presets, least sizes (part + bezel).
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
  an amber accent, a backdrop, glass and corners in one call (byob then still shipped two
  scenes of its own; it doesn't any more).
- **It never got to apply its own shader**: it thinks for 60–80k characters first. With the
  server's default output limit (~8k tokens) the reply ended mid-thought; with 30k it ended
  mid-tool-call (the call left as text). The guide now has a worked example to adapt.
- **byob bug (on main too):** the turn loop ignores `finish_reason: "length"`. A reply cut
  off at the output limit ends silently, with no answer and no note. That is also why the
  bench tool's interpretation came back empty. Worth fixing on main: at least say "the
  model hit its output limit", maybe continue once.

## The compositor (`cockpit/compositor.rs`)

eframe draws egui straight to the window, so the finished frame can't be blurred or relit.
Rather than replace eframe's loop, byob runs in a second egui context of its own when a
backdrop is on at startup (`BYOB_COMPOSITOR=1` always, `=0` never; the choice is made once,
since textures and fonts belong to a context). eframe's context passes the frame's raw input
through and draws a single paint callback, in which egui-wgpu's renderer draws byob's frame
into a texture; the scene is drawn and blurred at a quarter size; the composite is
`ui + (1 − ui.a) · mix(scene, blurred, frost · covered)`, then the overlay.

eframe keeps what it's good at: the window and surface, saved state, input, IME, clipboard,
drag and drop. byob's platform output (cursor, copied text, IME, its accessibility tree),
viewport commands (screenshots, close) and repaint requests go back through eframe's context;
its own discarded layout passes don't (eframe would otherwise draw every frame twice); zoom is
mirrored so input arrives in byob's points; texture changes wait for a frame that's painted.
The Servo view loads its pictures into byob's context. Without a backdrop at startup nothing of
this runs: the window is drawn exactly as before.

## What egui still can't do here (and Rune could)

- **Relighting the finished UI** (text included) with the scene's lights: the compositor
  now has the UI as a texture, so this is within reach (the overlay could sample it); not
  done yet.
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
7. Relighting and bloom over the composited UI (the overlay sampling the UI texture).

Tested in the browser with headless Chromium (software WebGL): composited and frosted, with
a backdrop saved in the browser's settings. Not done: integrated-GPU performance, HTML apps (the Servo view)
inside a composited window, and light themes (scenes tend to be dark; light glass over
them turns grey).
