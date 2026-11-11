//! GPU backend selection.
//!
//! eframe's default asks wgpu for Vulkan/Metal/DX12 *and* GL. Initialising the GL path
//! (EGL, and on Linux usually Mesa's llvmpipe software renderer with libLLVM) costs ~70 MB
//! of resident memory even though a primary backend almost always wins. So: use the primary
//! backends, and add GL only when no primary adapter exists (old GPUs, some VMs).
//! On Windows, DX12 alone when it has an adapter: it's what Windows apps and browsers draw
//! with, while Vulkan drivers there (Intel's especially) are where crashes come from; a
//! UHD 620 laptop closed at startup with Vulkan picked. `WGPU_BACKEND` still overrides
//! everything.

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
    let found: Vec<wgpu::Backend> = block_on(instance.enumerate_adapters(primary)).iter().map(|a| a.get_info().backend).collect();
    let chosen = pick(cfg!(windows), &found);
    if chosen.contains(wgpu::Backends::GL) {
        log::warn!("no Vulkan/Metal/DX12 adapter found; falling back to OpenGL");
    }
    chosen
}

/// Backends to use given the adapters the primary ones offer (see the module docs).
fn pick(windows: bool, found: &[wgpu::Backend]) -> wgpu::Backends {
    if found.is_empty() {
        wgpu::Backends::PRIMARY | wgpu::Backends::GL
    } else if windows && found.contains(&wgpu::Backend::Dx12) {
        wgpu::Backends::DX12
    } else {
        wgpu::Backends::PRIMARY
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

#[cfg(test)]
mod tests {
    use super::*;
    use wgpu::Backend::{Dx12, Vulkan};

    #[test]
    fn windows_draws_with_dx12_when_it_can() {
        // The UHD 620 laptop: Vulkan listed first, DX12 too.
        assert_eq!(pick(true, &[Vulkan, Dx12, Dx12]), wgpu::Backends::DX12);
        assert_eq!(pick(true, &[Vulkan]), wgpu::Backends::PRIMARY);
        assert_eq!(pick(false, &[Vulkan, Dx12]), wgpu::Backends::PRIMARY);
        assert_eq!(pick(true, &[]), wgpu::Backends::PRIMARY | wgpu::Backends::GL);
    }
}
