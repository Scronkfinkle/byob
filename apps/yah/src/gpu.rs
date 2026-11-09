//! GPU backend selection.
//!
//! eframe's default asks wgpu for Vulkan/Metal/DX12 *and* GL. Initialising the GL path
//! (EGL, and on Linux usually Mesa's llvmpipe software renderer with libLLVM) costs ~70 MB
//! of resident memory even though a primary backend almost always wins. So: use the primary
//! backends, and add GL only when no primary adapter exists (old GPUs, some VMs).
//! `WGPU_BACKEND` still overrides everything.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use eframe::egui_wgpu::{self, WgpuSetup, WgpuSetupCreateNew};
use eframe::wgpu;

pub fn options() -> egui_wgpu::WgpuConfiguration {
    let backends = wgpu::Backends::from_env().unwrap_or_else(choose);
    log::info!("wgpu backends: {backends:?}");
    let mut setup = WgpuSetupCreateNew::without_display_handle();
    setup.instance_descriptor.backends = backends;
    egui_wgpu::WgpuConfiguration { wgpu_setup: WgpuSetup::CreateNew(setup), ..Default::default() }
}

fn choose() -> wgpu::Backends {
    let primary = wgpu::Backends::PRIMARY;
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor { backends: primary, ..WgpuSetupCreateNew::without_display_handle().instance_descriptor });
    if block_on(instance.enumerate_adapters(primary)).is_empty() {
        log::warn!("no Vulkan/Metal/DX12 adapter found; falling back to OpenGL");
        primary | wgpu::Backends::GL
    } else {
        primary
    }
}

/// Native adapter enumeration resolves immediately; poll it without pulling in an executor.
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
        std::thread::yield_now();
    }
}
