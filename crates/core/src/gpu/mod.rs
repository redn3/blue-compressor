//! GPU backends for the resize pipeline. Each returns None when unavailable.

use crate::quality::Geometry;
use crate::resize::Resizer;
use crate::Backend;

#[cfg(all(target_os = "macos", feature = "metal"))]
pub mod metal;

#[cfg(all(target_os = "macos", feature = "metal"))]
mod develop_metal;

/// The Develop pass on Metal (None when there is no Metal device).
#[cfg(all(target_os = "macos", feature = "metal"))]
pub fn develop_metal(
    pl: &crate::develop::RawPlanes,
    s: &crate::develop::Shade,
    crop: [f32; 4],
    ow: usize,
    oh: usize,
) -> Option<Vec<u16>> {
    develop_metal::render(pl, s, crop, ow, oh)
}

#[cfg(all(target_os = "macos", feature = "metal"))]
pub fn develop_metal_name() -> Option<String> {
    develop_metal::name()
}

#[cfg(feature = "cuda")]
pub mod cuda;

#[cfg(feature = "opencl")]
pub mod opencl;

pub fn create(backend: Backend, src: Geometry, dst: Geometry) -> Option<Box<dyn Resizer>> {
    #[cfg(all(target_os = "macos", feature = "metal"))]
    if matches!(backend, Backend::Auto | Backend::Metal) {
        if let Some(r) = metal::MetalResizer::new(src, dst) {
            return Some(Box::new(r));
        }
    }
    #[cfg(feature = "cuda")]
    if matches!(backend, Backend::Auto | Backend::Cuda) {
        if let Some(r) = cuda::CudaResizer::new(src, dst) {
            return Some(Box::new(r));
        }
    }
    #[cfg(feature = "opencl")]
    if matches!(backend, Backend::Auto | Backend::OpenCl) {
        let pick = if backend == Backend::OpenCl { opencl::Pick::AnyGpu } else { opencl::Pick::Auto };
        if let Some(r) = opencl::OpenClResizer::with_devices(src, dst, pick) {
            return Some(Box::new(r));
        }
    }
    let _ = (backend, src, dst);
    None
}

/// Name of the GPU a backend would use, if any.
pub fn description(backend: Backend) -> Option<String> {
    #[cfg(all(target_os = "macos", feature = "metal"))]
    if matches!(backend, Backend::Auto | Backend::Metal) {
        if let Some(d) = metal::device_name() {
            return Some(format!("Metal: {d}"));
        }
    }
    #[cfg(feature = "cuda")]
    if matches!(backend, Backend::Auto | Backend::Cuda) {
        if let Some(d) = cuda::device_name() {
            return Some(format!("CUDA: {d}"));
        }
    }
    #[cfg(feature = "opencl")]
    if matches!(backend, Backend::Auto | Backend::OpenCl) {
        let pick = if backend == Backend::OpenCl { opencl::Pick::AnyGpu } else { opencl::Pick::Auto };
        if let Some(d) = opencl::device_name(pick) {
            return Some(format!("OpenCL: {d}"));
        }
    }
    let _ = backend;
    None
}
