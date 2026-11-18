//! The compositor: byob's interface drawn into a texture, then put on the window over the
//! backdrop with frosted glass under the panels and the scene's overlay on top.
//!
//! eframe draws egui straight to the window, so nothing can be done with the finished frame.
//! Instead the app runs byob in a second egui context of its own ([`Compositor::run`]):
//! eframe's context gets the input and window as always and passes them through, and draws
//! just one paint callback, in which this context's frame is rendered to a texture by
//! egui-wgpu's own renderer and composited:
//!
//! ```text
//! scene ──► S ──► blur (¼ size, twice) ──► B
//! byob (egui, transparent background) ──► U
//! window = U + (1 − U.a) · mix(S, B, frost where U covers)  then the overlay
//! ```
//!
//! What eframe does for the window (its surface, saved state, input, IME, the clipboard,
//! screen readers) stays eframe's: this context's platform output (cursor, copied text, IME,
//! the accessibility tree), viewport commands and repaint requests are handed back to it.

use std::sync::{Arc, Mutex, PoisonError};

use egui_wgpu::wgpu;

use super::backdrop::{self, Inputs, Pass};

/// The scene this frame, as [`super::Stage`] leaves it for the compositor (in the context's
/// temporary data) instead of drawing it with paint callbacks.
#[derive(Clone)]
pub struct SceneFrame {
    pub id: u64,
    pub source: Arc<str>,
    pub inputs: Arc<Mutex<Inputs>>,
    /// How frosted the glass is, 0 (clear) to 1.
    pub frost: f32,
}

fn scene_key() -> egui::Id {
    egui::Id::unique("byob-compositor-scene")
}

fn compositing_key() -> egui::Id {
    egui::Id::unique("byob-compositing")
}

/// Whether `ctx` is drawn by a compositor (the scene is left to it).
pub fn compositing(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<bool>(compositing_key())).unwrap_or(false)
}

/// Leave this frame's scene for the compositor (`None`: no scene).
pub(crate) fn leave_scene(ctx: &egui::Context, scene: Option<SceneFrame>) {
    ctx.data_mut(|d| match scene {
        Some(s) => {
            d.insert_temp(scene_key(), s);
        }
        None => {
            d.remove::<SceneFrame>(scene_key());
        }
    });
}

/// Whether to composite a window starting with `c`: when it has a backdrop
/// (`BYOB_COMPOSITOR=1` always, `=0` never). Decided once: byob's context can't change later.
pub fn wanted(c: &super::Cockpit) -> bool {
    match std::env::var("BYOB_COMPOSITOR").as_deref() {
        Ok("0") => false,
        Ok("1") => true,
        _ => !c.backdrop.is_empty(),
    }
}

/// byob's own egui context, composited onto eframe's window.
pub struct Compositor {
    inner: egui::Context,
    /// Texture changes not yet given to the renderer (a frame that wasn't painted keeps
    /// them for the next).
    textures: Arc<Mutex<egui::TexturesDelta>>,
}

impl Compositor {
    /// Set up on eframe's render state; `outer` is eframe's context (repaints go to it).
    pub fn new(rs: &egui_wgpu::RenderState, outer: &egui::Context) -> Self {
        rs.renderer.write().callback_resources.insert(Gpu::new(&rs.device, rs.target_format));
        Self::without_gpu(outer)
    }

    /// The contexts' side alone (tests: frames are passed on, nothing is drawn).
    fn without_gpu(outer: &egui::Context) -> Self {
        let inner = egui::Context::default();
        let wake = outer.clone();
        inner.set_request_repaint_callback(move |info| wake.request_repaint_after(info.delay));
        inner.data_mut(|d| d.insert_temp(compositing_key(), true));
        // Screen readers: this context's tree is the one handed to eframe each frame.
        inner.enable_accesskit();
        inner.options_mut(|o| o.zoom_with_keyboard = false);
        Self { inner, textures: Arc::default() }
    }

    /// byob's context: give it to whatever loads textures for byob (the Servo view).
    pub fn context(&self) -> &egui::Context {
        &self.inner
    }

    /// One frame: run `f` (byob) in byob's context with this frame's input, hand its output
    /// back to eframe's, and paint it composited.
    pub fn run(&mut self, ui: &mut egui::Ui, mut f: impl FnMut(&mut egui::Ui)) {
        let outer = ui.ctx().clone();
        let raw = outer.input(|i| i.raw.clone());
        let out = self.inner.run_ui(raw, |ui| f(ui));
        // The same points in both contexts: input arrives in eframe's.
        if (outer.zoom_factor() - self.inner.zoom_factor()).abs() > 1e-4 {
            outer.set_zoom_factor(self.inner.zoom_factor());
        }
        if outer.options(|o| o.zoom_with_keyboard) {
            outer.options_mut(|o| o.zoom_with_keyboard = false);
        }
        let egui::FullOutput { platform_output, textures_delta, shapes, pixels_per_point, viewport_output } = out;
        // What byob's context asks of the window. Its passes are its own: they were already
        // run, and passing them on would make eframe draw every frame again.
        outer.output_mut(|o| {
            let (passes, discard) = (o.num_completed_passes, std::mem::take(&mut o.request_discard_reasons));
            *o = egui::PlatformOutput { num_completed_passes: passes, request_discard_reasons: discard, ..platform_output };
        });
        // eframe's context has no widgets: its own (empty) tree mustn't replace byob's.
        outer.disable_accesskit();
        if let Some(v) = viewport_output.get(&egui::ViewportId::ROOT) {
            for c in &v.commands {
                outer.send_viewport_cmd(c.clone());
            }
            if v.repaint_delay < std::time::Duration::MAX {
                outer.request_repaint_after(v.repaint_delay);
            }
        }
        self.textures.lock().unwrap_or_else(PoisonError::into_inner).append(textures_delta);
        let prims = self.inner.tessellate(shapes, pixels_per_point);
        let scene = self.inner.data(|d| d.get_temp::<SceneFrame>(scene_key()));
        let paint = Paint { prims, pixels_per_point, scene, textures: Arc::clone(&self.textures) };
        outer.layer_painter(egui::LayerId::background()).add(egui_wgpu::Callback::new_paint_callback(outer.content_rect(), paint));
    }
}

impl Drop for Compositor {
    fn drop(&mut self) {
        // Changes no frame will draw any more (egui insists they're dropped on purpose).
        self.textures.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }
}

const SHADERS: &str = r"
struct Params {
    size: vec2<f32>,
    frost: f32,
    has_scene: f32,
    texel: vec2<f32>,
    dir: vec2<f32>,
}

@group(0) @binding(0) var ui_tex: texture_2d<f32>;
@group(0) @binding(1) var scene_tex: texture_2d<f32>;
@group(0) @binding(2) var soft_tex: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;
@group(0) @binding(4) var<uniform> params: Params;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let x = f32(i / 2u) * 4.0 - 1.0;
    let y = f32(i % 2u) * 4.0 - 1.0;
    return vec4<f32>(x, y, 0.0, 1.0);
}

@fragment
fn composite(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let p = vec2<i32>(pos.xy);
    let ui = textureLoad(ui_tex, p, 0);
    if (params.has_scene < 0.5) {
        return vec4<f32>(ui.rgb, 1.0);
    }
    let sharp = textureLoad(scene_tex, p, 0).rgb;
    let soft = textureSample(soft_tex, samp, pos.xy / params.size).rgb;
    let cover = clamp(ui.a * 4.0, 0.0, 1.0);
    let behind = mix(sharp, soft, params.frost * cover);
    return vec4<f32>(ui.rgb + (1.0 - ui.a) * behind, 1.0);
}

// A 9-tap Gaussian in five linear samples, along params.dir; the source is binding 2.
@fragment
fn blur(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let uv = pos.xy * params.texel;
    let d = params.dir;
    var c = textureSample(soft_tex, samp, uv) * 0.2270270270;
    c += textureSample(soft_tex, samp, uv + d * 1.3846153846) * 0.3162162162;
    c += textureSample(soft_tex, samp, uv - d * 1.3846153846) * 0.3162162162;
    c += textureSample(soft_tex, samp, uv + d * 3.2307692308) * 0.0702702703;
    c += textureSample(soft_tex, samp, uv - d * 3.2307692308) * 0.0702702703;
    return c;
}
";

/// The blurred copy is this much smaller than the window, each way.
const DOWNSCALE: u32 = 4;

/// Bytes of `Params`.
const PARAMS_SIZE: u64 = 32;

fn params(size: [f32; 2], frost: f32, has_scene: bool, texel: [f32; 2], dir: [f32; 2]) -> Vec<u8> {
    [size[0], size[1], frost, if has_scene { 1.0 } else { 0.0 }, texel[0], texel[1], dir[0], dir[1]].iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// Window-sized textures, made again when the window's size changes.
struct Targets {
    size: [u32; 2],
    ui: wgpu::TextureView,
    scene: wgpu::TextureView,
    /// The blur ping-pongs between these (¼ size).
    small: [wgpu::TextureView; 2],
    /// Composite: ui, scene, small[1]. Blur passes: scene → small[0], small[0] → small[1],
    /// small[1] → small[0], small[0] → small[1].
    composite: wgpu::BindGroup,
    blur: [wgpu::BindGroup; 4],
    params: [wgpu::Buffer; 5],
}

/// The compositor's GPU side (in eframe's callback resources).
struct Gpu {
    format: wgpu::TextureFormat,
    renderer: egui_wgpu::Renderer,
    scene: backdrop::Gpu,
    layout: wgpu::BindGroupLayout,
    composite: wgpu::RenderPipeline,
    blur: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    blank: wgpu::TextureView,
    targets: Option<Targets>,
}

impl Gpu {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let renderer = egui_wgpu::Renderer::new(device, format, egui_wgpu::RendererOptions { msaa_samples: 1, ..Default::default() });
        let tex = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("byob_compositor"),
            entries: &[
                tex(0),
                tex(1),
                tex(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("byob_compositor"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let module =
            device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("byob_compositor"), source: wgpu::ShaderSource::Wgsl(SHADERS.into()) });
        let pipeline = |entry: &str| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(entry),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(entry),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState { format, blend: None, write_mask: wgpu::ColorWrites::ALL })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("byob_compositor"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        let blank = texture(device, format, [1, 1], "byob_compositor_blank");
        Self {
            format,
            renderer,
            scene: backdrop::Gpu::new(device, format),
            composite: pipeline("composite"),
            blur: pipeline("blur"),
            layout,
            sampler,
            blank,
            targets: None,
        }
    }

    /// Make the textures for a window of `size` pixels, unless they are.
    fn ensure_targets(&mut self, device: &wgpu::Device, size: [u32; 2]) {
        let size = [size[0].max(1), size[1].max(1)];
        if self.targets.as_ref().is_none_or(|t| t.size != size) {
            let small_size = [(size[0] / DOWNSCALE).max(1), (size[1] / DOWNSCALE).max(1)];
            let ui = texture(device, self.format, size, "byob_compositor_ui");
            let scene = texture(device, self.format, size, "byob_compositor_scene");
            let small =
                [texture(device, self.format, small_size, "byob_compositor_blur_a"), texture(device, self.format, small_size, "byob_compositor_blur_b")];
            let params: [wgpu::Buffer; 5] = std::array::from_fn(|_| {
                device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("byob_compositor_params"),
                    size: PARAMS_SIZE,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                })
            });
            let group = |a: &wgpu::TextureView, b: &wgpu::TextureView, c: &wgpu::TextureView, p: &wgpu::Buffer| {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("byob_compositor"),
                    layout: &self.layout,
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(a) },
                        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(b) },
                        wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(c) },
                        wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                        wgpu::BindGroupEntry { binding: 4, resource: p.as_entire_binding() },
                    ],
                })
            };
            let composite = group(&ui, &scene, &small[1], &params[0]);
            let blank = &self.blank;
            let blur = [
                group(blank, blank, &scene, &params[1]),
                group(blank, blank, &small[0], &params[2]),
                group(blank, blank, &small[1], &params[3]),
                group(blank, blank, &small[0], &params[4]),
            ];
            self.targets = Some(Targets { size, ui, scene, small, composite, blur, params });
        }
    }
}

fn texture(device: &wgpu::Device, format: wgpu::TextureFormat, size: [u32; 2], label: &str) -> wgpu::TextureView {
    device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        })
        .create_view(&wgpu::TextureViewDescriptor::default())
}

fn pass<'e>(encoder: &'e mut wgpu::CommandEncoder, view: &wgpu::TextureView, clear: wgpu::Color, label: &str) -> wgpu::RenderPass<'e> {
    encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some(label),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations { load: wgpu::LoadOp::Clear(clear), store: wgpu::StoreOp::Store },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    })
}

/// One frame of byob, to render and composite.
struct Paint {
    prims: Vec<egui::ClippedPrimitive>,
    pixels_per_point: f32,
    scene: Option<SceneFrame>,
    textures: Arc<Mutex<egui::TexturesDelta>>,
}

impl egui_wgpu::CallbackTrait for Paint {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        screen: &egui_wgpu::ScreenDescriptor,
        encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(gpu) = resources.get_mut::<Gpu>() else {
            return Vec::new();
        };
        let mut delta = std::mem::take(&mut *self.textures.lock().unwrap_or_else(PoisonError::into_inner));
        for (id, images) in &delta.set {
            for image in images {
                gpu.renderer.update_texture(device, queue, *id, image);
            }
        }
        // Freed once this frame is drawn (below).
        let free = std::mem::take(&mut delta.free);
        delta.clear();
        let sd = egui_wgpu::ScreenDescriptor { size_in_pixels: screen.size_in_pixels, pixels_per_point: self.pixels_per_point };
        let commands = gpu.renderer.update_buffers(device, queue, encoder, &self.prims, &sd);
        if let Some(s) = &self.scene {
            gpu.scene.ensure(device, s.id, &s.source);
            gpu.scene.write(queue, &s.inputs.lock().unwrap_or_else(PoisonError::into_inner));
        }
        #[allow(clippy::cast_precision_loss)] // window sizes
        let size = [screen.size_in_pixels[0].max(1) as f32, screen.size_in_pixels[1].max(1) as f32];
        gpu.ensure_targets(device, screen.size_in_pixels);
        let gpu = &*gpu;
        let Some(t) = &gpu.targets else {
            return commands;
        };
        #[allow(clippy::cast_precision_loss)]
        let small = [t.size[0].div_ceil(DOWNSCALE).max(1) as f32, t.size[1].div_ceil(DOWNSCALE).max(1) as f32];
        let frost = self.scene.as_ref().map_or(0.0, |s| s.frost.clamp(0.0, 1.0));
        queue.write_buffer(&t.params[0], 0, &params(size, frost, self.scene.is_some(), [0.0; 2], [0.0; 2]));
        // The first blur pass reads the full-size scene into the small texture.
        let texel = [1.0 / small[0], 1.0 / small[1]];
        let dirs = [[1.0 / size[0], 0.0], [0.0, texel[1]], [texel[0], 0.0], [0.0, texel[1]]];
        for (i, dir) in dirs.iter().enumerate() {
            queue.write_buffer(&t.params[i + 1], 0, &params(size, frost, true, texel, *dir));
        }
        if let Some(s) = &self.scene {
            gpu.scene.draw(&mut pass(encoder, &t.scene, wgpu::Color::BLACK, "byob_scene"), Pass::Scene, s.id);
            if frost > 0.0 {
                let targets = [&t.small[0], &t.small[1], &t.small[0], &t.small[1]];
                for (group, target) in t.blur.iter().zip(targets) {
                    let mut p = pass(encoder, target, wgpu::Color::BLACK, "byob_blur");
                    p.set_pipeline(&gpu.blur);
                    p.set_bind_group(0, group, &[]);
                    p.draw(0..3, 0..1);
                }
            }
        }
        {
            let mut p = pass(encoder, &t.ui, wgpu::Color::TRANSPARENT, "byob_ui").forget_lifetime();
            gpu.renderer.render(&mut p, &self.prims, &sd);
        }
        if let Some(gpu) = resources.get_mut::<Gpu>() {
            for id in &free {
                gpu.renderer.free_texture(id);
            }
        }
        commands
    }

    fn paint(&self, _info: egui::PaintCallbackInfo, pass: &mut wgpu::RenderPass<'static>, resources: &egui_wgpu::CallbackResources) {
        let Some(gpu) = resources.get::<Gpu>() else {
            return;
        };
        let Some(t) = &gpu.targets else {
            return;
        };
        pass.set_pipeline(&gpu.composite);
        pass.set_bind_group(0, &t.composite, &[]);
        pass.draw(0..3, 0..1);
        if let Some(s) = &self.scene {
            gpu.scene.draw(pass, Pass::Overlay, s.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame of eframe's context with byob composited in it; eframe's output.
    fn frame(outer: &egui::Context, c: &mut Compositor, raw: egui::RawInput, byob: impl FnMut(&mut egui::Ui)) -> egui::FullOutput {
        let mut byob = byob;
        let mut out = outer.run_ui(raw, |ui| c.run(ui, &mut byob));
        out.textures_delta.clear(); // eframe's own font atlas, nothing to draw it with here
        out
    }

    fn raw(events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0))), events, ..Default::default() }
    }

    /// byob in its own context still gets the window's input, and what it asks of the
    /// window (copied text, the cursor, window commands, the accessibility tree, repaints)
    /// reaches eframe's; its extra layout passes don't make eframe draw again.
    #[test]
    fn byob_in_its_own_context_talks_to_the_window_through_eframes() {
        let outer = egui::Context::default();
        outer.enable_accesskit();
        let mut c = Compositor::without_gpu(&outer);
        assert!(compositing(c.context()) && !compositing(&outer));
        let clicked = std::cell::Cell::new(false);
        let at = std::cell::Cell::new(egui::Rect::NOTHING);
        let mut byob = |ui: &mut egui::Ui| {
            let r = ui.button("Copy");
            at.set(r.rect);
            if r.clicked() {
                clicked.set(true);
                ui.ctx().copy_text("copied".into());
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Title("byob".into()));
            }
            if r.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            // A layout pass thrown away, as egui does for sizing.
            if ui.ctx().current_pass_index() == 0 {
                ui.ctx().request_discard("sizing");
            }
        };
        let _ = frame(&outer, &mut c, raw(Vec::new()), &mut byob);
        let p = at.get().center();
        let press = |pressed| egui::Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        let _ = frame(&outer, &mut c, raw(vec![egui::Event::PointerMoved(p)]), &mut byob);
        let out = frame(&outer, &mut c, raw(vec![egui::Event::PointerMoved(p), press(true), press(false)]), &mut byob);
        assert!(clicked.get(), "the click reached byob's button");
        let p_out = &out.platform_output;
        assert!(p_out.commands.iter().any(|c| matches!(c, egui::OutputCommand::CopyText(t) if t == "copied")), "{:?}", p_out.commands);
        assert_eq!(p_out.cursor_icon, egui::CursorIcon::PointingHand);
        assert!(p_out.request_discard_reasons.is_empty(), "byob's own passes stay its own");
        let tree = p_out.accesskit_update.as_ref().expect("byob's accessibility tree");
        assert!(tree.nodes.iter().any(|(_, n)| n.label() == Some("Copy") || n.value() == Some("Copy")), "the tree is byob's");
        let commands = &out.viewport_output[&egui::ViewportId::ROOT].commands;
        assert!(commands.iter().any(|c| matches!(c, egui::ViewportCommand::Title(t) if t == "byob")), "{commands:?}");
        assert!(out.shapes.iter().any(|s| matches!(s.shape, egui::Shape::Callback(_))), "eframe paints byob's frame with one callback");
    }

    /// Zooming byob (Ctrl +) zooms eframe's context too (a frame later), so input arrives in
    /// byob's points.
    #[test]
    fn zoom_is_kept_the_same_in_both_contexts() {
        let outer = egui::Context::default();
        let mut c = Compositor::without_gpu(&outer);
        let _ = frame(&outer, &mut c, raw(Vec::new()), |ui| ui.ctx().set_zoom_factor(1.5));
        // egui applies a zoom at the next pass: byob's, then eframe's.
        for _ in 0..2 {
            let _ = frame(&outer, &mut c, raw(Vec::new()), |_| {});
        }
        assert!((outer.zoom_factor() - 1.5).abs() < 1e-4 && !outer.options(|o| o.zoom_with_keyboard));
    }

    /// The compositor's shaders compile (naga, as wgpu would).
    #[test]
    fn the_shaders_compile() {
        let module = naga::front::wgsl::parse_str(SHADERS).unwrap();
        naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::empty()).validate(&module).unwrap();
    }

    #[test]
    fn params_are_laid_out_as_the_shader_reads_them() {
        let b = params([800.0, 600.0], 0.5, true, [0.25, 0.5], [1.0, 0.0]);
        assert_eq!(b.len() as u64, PARAMS_SIZE);
        let f = |at: usize| f32::from_le_bytes(b[at..at + 4].try_into().unwrap());
        assert_eq!((f(0), f(8), f(12), f(16), f(24)), (800.0, 0.5, 1.0, 0.25, 1.0));
    }
}
