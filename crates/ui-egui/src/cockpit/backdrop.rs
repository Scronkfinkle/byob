//! Drawing a backdrop: the scene ([`super::wgsl`]) as a full-window triangle under
//! everything egui draws, through an egui-wgpu paint callback.
//!
//! The app hands over its wgpu render state once ([`install`]); without it (another
//! renderer, tests) there are no backdrops and the look says so. The pipeline is built in
//! the callback's `prepare` the first time a scene is drawn, and again when it changes; the
//! inputs ([`Inputs`]) are written every frame.

use std::sync::{Arc, Mutex, PoisonError};

use egui_wgpu::wgpu;

use super::wgsl;

/// What the scene sees this frame (the uniform `u`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Inputs {
    pub size: [f32; 2],
    pub mouse: [f32; 2],
    pub time: f32,
    pub busy: f32,
    pub scale: f32,
    pub accent: [f32; 4],
    pub background: [f32; 4],
    pub text: [f32; 4],
    /// The control under the pointer (x0, y0, x1, y1), and (0 none / 1 hovered / 2 pressed,
    /// seconds hovered, 0, 0).
    pub hot: [f32; 4],
    pub hot_state: [f32; 4],
    /// (x0, y0, x1, y1) in points; the first [`wgsl::MAX_PANELS`] are used.
    pub panels: Vec<[f32; 4]>,
}

/// Bytes in the uniform buffer (the WGSL `Byob` struct).
const UNIFORM_SIZE: usize = 112 + 16 * wgsl::MAX_PANELS;

impl Inputs {
    /// The uniform's bytes, laid out as WGSL's `Byob` (16-byte aligned vec4s).
    pub fn bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(UNIFORM_SIZE);
        let count = u32::try_from(self.panels.len().min(wgsl::MAX_PANELS)).unwrap_or_default();
        for v in self.size.iter().chain(&self.mouse).chain([&self.time, &self.busy]) {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&count.to_le_bytes());
        b.extend_from_slice(&self.scale.to_le_bytes());
        let panels = (0..wgsl::MAX_PANELS).flat_map(|i| self.panels.get(i).copied().unwrap_or_default());
        for v in self.accent.iter().chain(&self.background).chain(&self.text).chain(&self.hot).chain(&self.hot_state).copied().chain(panels) {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b
    }
}

/// GPU errors seen since the app last asked (a scene that compiled but that the GPU still
/// refused), shared with wgpu's error handler.
#[derive(Clone, Default)]
pub struct Errors(Arc<Mutex<Vec<String>>>);

impl Errors {
    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }

    fn push(&self, e: String) {
        let mut v = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if v.len() < 8 {
            v.push(e);
        }
    }
}

/// What lives in egui-wgpu's callback resources.
struct Gpu {
    format: wgpu::TextureFormat,
    pipeline_layout: wgpu::PipelineLayout,
    buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    /// The scene the pipelines are for (by [`scene_id`]): under everything, and over it
    /// (when the scene has an overlay).
    pipeline: Option<(u64, Option<wgpu::RenderPipeline>, Option<wgpu::RenderPipeline>)>,
}

/// Which of a scene's two passes a callback draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pass {
    /// `scene`, under everything.
    Scene,
    /// `overlay`, over everything.
    Overlay,
}

/// Make backdrops drawable with this render state. wgpu errors are logged (and kept for
/// the app to report) instead of wgpu's default, which ends the process.
pub fn install(rs: &egui_wgpu::RenderState) -> Errors {
    let errors = Errors::default();
    let sink = errors.clone();
    rs.device.on_uncaptured_error(Arc::new(move |e: wgpu::Error| {
        log::error!("wgpu: {e}");
        sink.push(e.to_string());
    }));
    let device = &rs.device;
    let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("byob_backdrop_bind_layout"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        }],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("byob_backdrop_layout"),
        bind_group_layouts: &[Some(&bind_layout)],
        immediate_size: 0,
    });
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("byob_backdrop_uniforms"),
        size: UNIFORM_SIZE as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("byob_backdrop_bind_group"),
        layout: &bind_layout,
        entries: &[wgpu::BindGroupEntry { binding: 0, resource: buffer.as_entire_binding() }],
    });
    let gpu = Gpu { format: rs.target_format, pipeline_layout, buffer, bind_group, pipeline: None };
    rs.renderer.write().callback_resources.insert(gpu);
    errors
}

/// A scene's id: its text's hash.
pub fn scene_id(source: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut h);
    h.finish()
}

impl Gpu {
    fn build(&mut self, device: &wgpu::Device, id: u64, source: &str) {
        let text = wgsl::module(source, self.format.is_srgb());
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("byob_backdrop"), source: wgpu::ShaderSource::Wgsl(text.into()) });
        let pipeline = |entry: &str, blend: Option<wgpu::BlendState>| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(entry),
                layout: Some(&self.pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("byob_vs"),
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
                    targets: &[Some(wgpu::ColorTargetState { format: self.format, blend, write_mask: wgpu::ColorWrites::ALL })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let scene = pipeline("byob_fs", None);
        let overlay = wgsl::has_overlay(source).then(|| pipeline("byob_overlay_fs", Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING)));
        self.pipeline = Some((id, Some(scene), overlay));
    }
}

/// A paint callback: each frame while a backdrop is on, one for the scene (under
/// everything) and, when it has one, one for its overlay (over everything).
pub struct Paint {
    pub pass: Pass,
    pub id: u64,
    pub source: Arc<str>,
    /// Filled at the end of the frame, after the panels are placed (read in `prepare`).
    pub inputs: Arc<Mutex<Inputs>>,
}

impl egui_wgpu::CallbackTrait for Paint {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(gpu) = resources.get_mut::<Gpu>() else {
            return Vec::new();
        };
        if gpu.pipeline.as_ref().is_none_or(|(id, _, _)| *id != self.id) {
            gpu.build(device, self.id, &self.source);
        }
        if self.pass == Pass::Scene {
            let bytes = self.inputs.lock().unwrap_or_else(PoisonError::into_inner).bytes();
            queue.write_buffer(&gpu.buffer, 0, &bytes);
        }
        Vec::new()
    }

    fn paint(&self, _info: egui::PaintCallbackInfo, pass: &mut wgpu::RenderPass<'static>, resources: &egui_wgpu::CallbackResources) {
        let Some(gpu) = resources.get::<Gpu>() else {
            return;
        };
        let Some((id, scene, overlay)) = &gpu.pipeline else {
            return;
        };
        let pipeline = match self.pass {
            Pass::Scene => scene,
            Pass::Overlay => overlay,
        };
        if let Some(pipeline) = pipeline
            && *id == self.id
        {
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &gpu.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes match WGSL's layout of `Byob`: 112 bytes of scalars, colours and the hot
    /// control, then 16 panels of 16 bytes, the count where `panel_count` is.
    #[test]
    fn inputs_are_laid_out_as_the_shader_reads_them() {
        let i = Inputs {
            size: [800.0, 600.0],
            time: 2.0,
            scale: 2.0,
            accent: [1.0, 0.5, 0.0, 1.0],
            hot: [5.0, 6.0, 7.0, 8.0],
            hot_state: [2.0, 0.5, 0.0, 0.0],
            panels: vec![[1.0, 2.0, 3.0, 4.0]; 20],
            ..Default::default()
        };
        let b = i.bytes();
        assert_eq!(b.len(), UNIFORM_SIZE);
        let f = |at: usize| f32::from_le_bytes(b[at..at + 4].try_into().unwrap());
        assert_eq!((f(0), f(4), f(16), f(28), f(32), f(36)), (800.0, 600.0, 2.0, 2.0, 1.0, 0.5));
        assert_eq!(u32::from_le_bytes(b[24..28].try_into().unwrap()), 16, "at most 16 panels");
        assert_eq!((f(80), f(92), f(96), f(100)), (5.0, 8.0, 2.0, 0.5));
        assert_eq!((f(112), f(124)), (1.0, 4.0), "the panels after hot and hot_state");
    }
}
