//! Metal backend of the Develop pass (see develop.rs and develop.metal).

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::{
    MTLBuffer, MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandEncoder, MTLCommandQueue, MTLComputeCommandEncoder,
    MTLComputePipelineState, MTLCreateSystemDefaultDevice, MTLDevice, MTLLibrary, MTLResourceOptions, MTLSize,
};

use crate::develop::{RawPlanes, Shade};

const SHADER: &str = include_str!("develop.metal");

#[repr(C)]
#[derive(Clone, Copy)]
struct Geo {
    w: u32,
    h: u32,
    ow: u32,
    oh: u32,
    ox: f32,
    oy: f32,
    sx: f32,
    sy: f32,
}

struct Gpu {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    pso: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    name: String,
}

// Metal devices, queues and pipelines may be used from any thread.
unsafe impl Send for Gpu {}
unsafe impl Sync for Gpu {}

fn gpu() -> Option<&'static Gpu> {
    static G: OnceLock<Option<Gpu>> = OnceLock::new();
    G.get_or_init(|| {
        let device = MTLCreateSystemDefaultDevice()?;
        let queue = device.newCommandQueue()?;
        let lib = match device.newLibraryWithSource_options_error(&NSString::from_str(SHADER), None) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("bluecompress: Metal develop shader failed: {}", e.localizedDescription());
                return None;
            }
        };
        let f = lib.newFunctionWithName(&NSString::from_str("develop"))?;
        let pso = device.newComputePipelineStateWithFunction_error(&f).ok()?;
        let name = format!("Metal ({})", device.name());
        Some(Gpu { device, queue, pso, name })
    })
    .as_ref()
}

pub fn name() -> Option<String> {
    gpu().map(|g| g.name.clone())
}

pub fn render(pl: &RawPlanes, s: &Shade, crop: [f32; 4], ow: usize, oh: usize) -> Option<Vec<u16>> {
    let g = gpu()?;
    let n = pl.w * pl.h;
    let mut flat = Vec::with_capacity(n * 4);
    for p in &pl.data {
        flat.extend_from_slice(p);
    }
    let geo = Geo {
        w: pl.w as u32,
        h: pl.h as u32,
        ow: ow as u32,
        oh: oh as u32,
        ox: crop[0] * pl.w as f32,
        oy: crop[1] * pl.h as f32,
        sx: crop[2] * pl.w as f32 / ow as f32,
        sy: crop[3] * pl.h as f32 / oh as f32,
    };
    unsafe {
        let inb = g.device.newBufferWithBytes_length_options(
            NonNull::new(flat.as_ptr() as *mut c_void)?,
            flat.len() * 4,
            MTLResourceOptions::StorageModeShared,
        )?;
        let outb = g.device.newBufferWithLength_options(ow * oh * 6, MTLResourceOptions::StorageModeShared)?;
        let cb = g.queue.commandBuffer()?;
        let enc = cb.computeCommandEncoder()?;
        enc.setComputePipelineState(&g.pso);
        enc.setBuffer_offset_atIndex(Some(&inb), 0, 0);
        enc.setBuffer_offset_atIndex(Some(&outb), 0, 1);
        enc.setBytes_length_atIndex(NonNull::from(s).cast(), std::mem::size_of::<Shade>(), 2);
        enc.setBytes_length_atIndex(NonNull::from(&geo).cast(), std::mem::size_of::<Geo>(), 3);
        let tw = g.pso.threadExecutionWidth().min(32);
        let th = (g.pso.maxTotalThreadsPerThreadgroup() / tw).clamp(1, 8);
        enc.dispatchThreads_threadsPerThreadgroup(
            MTLSize { width: ow, height: oh, depth: 1 },
            MTLSize { width: tw, height: th, depth: 1 },
        );
        enc.endEncoding();
        cb.commit();
        cb.waitUntilCompleted();
        if cb.status() != MTLCommandBufferStatus::Completed {
            return None;
        }
        let ptr = outb.contents().as_ptr() as *const u16;
        Some(std::slice::from_raw_parts(ptr, ow * oh * 3).to_vec())
    }
}
