//! The Develop pass on the GPU through wgpu (Metal, Vulkan or DX12): noise reduction,
//! debayer, white balance, tone and color, sharpening, the user LUT and the output format
//! in one submission per frame. See develop.wgsl; the CPU path in develop.rs is the reference.

use std::sync::{Condvar, Mutex, OnceLock};

use wgpu::util::DeviceExt;

use crate::develop::{RawPlanes, Shade};
use crate::lut::Lut;

const SOURCE: &str = include_str!("develop.wgsl");

/// What the GPU hands back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// RGBA 8-bit, `ow` x `oh`.
    Rgba8,
    /// P010 (Y plane then interleaved CbCr), `ow` and `oh` even; BT.709 or BT.2020 matrix.
    P010 { bt2020: bool },
}

/// Noise reduction settings for one frame.
#[derive(Debug, Clone, Copy)]
pub struct NrSettings {
    pub luma: f32,
    pub chroma: f32,
    /// Noise gain from [`crate::denoise::noise_gain`].
    pub a: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Geo {
    w: u32,
    h: u32,
    ow: u32,
    oh: u32,
    ox: f32,
    oy: f32,
    sx: f32,
    sy: f32,
    debayer: u32,
    sharpen: f32,
    format: u32,
    bt2020: u32,
    lut_kind: u32,
    lut_size: u32,
    p0: u32,
    p1: u32,
    lut_min: [f32; 4],
    lut_max: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Nr {
    w: u32,
    h: u32,
    radius: u32,
    horizontal: u32,
    k: f32,
    sigma_scale: f32,
    black: f32,
    a: f32,
    keep: f32,
    luma_on: u32,
    chroma_on: u32,
    pad: u32,
}

fn bytes<T: Copy>(v: &T) -> &[u8] {
    // SAFETY: T is a plain repr(C) struct of 4-byte fields with no padding.
    unsafe { std::slice::from_raw_parts(v as *const T as *const u8, std::mem::size_of::<T>()) }
}

struct Engine {
    device: wgpu::Device,
    queue: wgpu::Queue,
    nr_prep: wgpu::ComputePipeline,
    nr_merge: wgpu::ComputePipeline,
    bilateral: wgpu::ComputePipeline,
    develop: wgpu::ComputePipeline,
    finish_rgba: wgpu::ComputePipeline,
    finish_p010: wgpu::ComputePipeline,
    name: String,
    /// Buffers for a frame size, reused.
    pool: Mutex<Vec<Bufs>>,
    /// Frames in flight at once (bounds the GPU memory).
    slots: (Mutex<usize>, Condvar),
}

struct Bufs {
    n: usize,
    px: usize,
    out_px: usize,
    planes: wgpu::Buffer,
    aux: [wgpu::Buffer; 6], // g, sigma, dg, cr, cb, tmp
    img: wgpu::Buffer,
    out: wgpu::Buffer,
    read: wgpu::Buffer,
}

const MAX_IN_FLIGHT: usize = 3;

fn module(device: &wgpu::Device, section: &str) -> wgpu::ShaderModule {
    let common = SOURCE.split("//#SECTION").next().unwrap_or("");
    let body =
        SOURCE.split("//#SECTION ").skip(1).find(|s| s.starts_with(section)).map(|s| &s[section.len()..]).unwrap_or("");
    device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(section),
        source: wgpu::ShaderSource::Wgsl(format!("{common}\n{body}").into()),
    })
}

fn pipeline(device: &wgpu::Device, m: &wgpu::ShaderModule, entry: &str) -> wgpu::ComputePipeline {
    device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(entry),
        layout: None,
        module: m,
        entry_point: Some(entry),
        compilation_options: Default::default(),
        cache: None,
    })
}

fn engine() -> Option<&'static Engine> {
    static E: OnceLock<Option<Engine>> = OnceLock::new();
    E.get_or_init(|| {
        if std::env::var_os("BLUECOMPRESS_DEVELOP_CPU").is_some() {
            return None;
        }
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
        desc.backends = wgpu::Backends::METAL | wgpu::Backends::VULKAN | wgpu::Backends::DX12;
        let instance = wgpu::Instance::new(desc);
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
            ..Default::default()
        }))
        .ok()?;
        let info = adapter.get_info();
        // Software rasterizers are slower than the CPU path.
        if info.device_type == wgpu::DeviceType::Cpu && std::env::var_os("BLUECOMPRESS_DEVELOP_ANY_GPU").is_none() {
            return None;
        }
        let limits = adapter.limits();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("develop"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits {
                max_storage_buffer_binding_size: limits.max_storage_buffer_binding_size,
                max_buffer_size: limits.max_buffer_size,
                ..wgpu::Limits::default()
            },
            ..Default::default()
        }))
        .ok()?;
        device.on_uncaptured_error(std::sync::Arc::new(|e| eprintln!("bluecompress: GPU error: {e}")));
        let (nr, bl, dv, fin) = (
            module(&device, "nr"),
            module(&device, "bilateral"),
            module(&device, "develop"),
            module(&device, "finish"),
        );
        let api = match info.backend {
            wgpu::Backend::Metal => "Metal",
            wgpu::Backend::Vulkan => "Vulkan",
            wgpu::Backend::Dx12 => "DX12",
            _ => "GPU",
        };
        Some(Engine {
            nr_prep: pipeline(&device, &nr, "nr_prep"),
            nr_merge: pipeline(&device, &nr, "nr_merge"),
            bilateral: pipeline(&device, &bl, "bilateral"),
            develop: pipeline(&device, &dv, "develop"),
            finish_rgba: pipeline(&device, &fin, "finish_rgba"),
            finish_p010: pipeline(&device, &fin, "finish_p010"),
            name: format!("{api} ({})", info.name),
            device,
            queue,
            pool: Mutex::new(Vec::new()),
            slots: (Mutex::new(0), Condvar::new()),
        })
    })
    .as_ref()
}

/// The GPU used, as "Metal (Apple M4 Max)", or None when Develop runs on the CPU.
pub fn name() -> Option<String> {
    engine().map(|e| e.name.clone())
}

impl Engine {
    fn buffer(&self, size: usize, usage: wgpu::BufferUsages) -> wgpu::Buffer {
        self.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: size.max(16) as u64,
            usage,
            mapped_at_creation: false,
        })
    }

    fn take(&self, n: usize, px: usize, out_px: usize) -> Bufs {
        {
            let mut p = self.pool.lock().unwrap();
            if let Some(i) = p.iter().position(|b| b.n == n && b.px == px && b.out_px == out_px) {
                return p.swap_remove(i);
            }
        }
        let st = wgpu::BufferUsages::STORAGE;
        let out_bytes = out_px * 4;
        Bufs {
            n,
            px,
            out_px,
            planes: self.buffer(n * 16, st | wgpu::BufferUsages::COPY_DST),
            aux: std::array::from_fn(|_| self.buffer(n * 4, st)),
            img: self.buffer(px * 8, st),
            out: self.buffer(out_bytes, st | wgpu::BufferUsages::COPY_SRC),
            read: self.buffer(out_bytes, wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST),
        }
    }

    fn give(&self, b: Bufs) {
        let mut p = self.pool.lock().unwrap();
        p.push(b);
        // Keep a few sizes only.
        while p.len() > MAX_IN_FLIGHT + 1 {
            p.remove(0);
        }
    }

    fn bind(&self, pipe: &wgpu::ComputePipeline, entries: &[&wgpu::Buffer]) -> wgpu::BindGroup {
        let e: Vec<wgpu::BindGroupEntry> = entries
            .iter()
            .enumerate()
            .map(|(i, b)| wgpu::BindGroupEntry { binding: i as u32, resource: b.as_entire_binding() })
            .collect();
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &pipe.get_bind_group_layout(0),
            entries: &e,
        })
    }

    fn uniform(&self, data: &[u8]) -> wgpu::Buffer {
        self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: None,
            contents: data,
            usage: wgpu::BufferUsages::UNIFORM,
        })
    }
}

fn groups(n: usize, size: usize) -> u32 {
    n.div_ceil(size) as u32
}

/// Develops `pl` on the GPU. `crop` is fractions of the frame; the image is debayered at
/// full resolution when the output has more pixels across than the quad planes.
#[allow(clippy::too_many_arguments)]
pub fn run(
    pl: &RawPlanes,
    s: &Shade,
    nr: Option<NrSettings>,
    sharpen: f32,
    lut: Option<&Lut>,
    crop: [f32; 4],
    ow: usize,
    oh: usize,
    format: Format,
) -> Option<Vec<u8>> {
    let e = engine()?;
    let n = pl.w * pl.h;
    if n == 0 || ow == 0 || oh == 0 || pl.w > 65535 * 256 {
        return None;
    }
    let p010 = matches!(format, Format::P010 { .. });
    if p010 && (!ow.is_multiple_of(2) || !oh.is_multiple_of(2)) {
        return None;
    }
    // u32 words of output: RGBA one per pixel, P010 3 bytes per pixel.
    let out_px = if p010 { ow * oh * 3 / 4 } else { ow * oh };
    let max = e.device.limits().max_storage_buffer_binding_size as usize;
    if n * 16 > max || ow * oh * 8 > max {
        return None;
    }
    {
        let (m, cv) = &e.slots;
        let mut busy = m.lock().unwrap();
        while *busy >= MAX_IN_FLIGHT {
            busy = cv.wait(busy).unwrap();
        }
        *busy += 1;
    }
    let release = || {
        let (m, cv) = &e.slots;
        *m.lock().unwrap() -= 1;
        cv.notify_one();
    };
    let b = e.take(n, ow * oh, out_px);
    for (c, d) in pl.data.iter().enumerate() {
        // SAFETY: f32 slices are plain bytes.
        let raw = unsafe { std::slice::from_raw_parts(d.as_ptr() as *const u8, d.len() * 4) };
        e.queue.write_buffer(&b.planes, (c * n * 4) as u64, raw);
    }
    let mut enc = e.device.create_command_encoder(&Default::default());
    // Uniform buffers live until the submission is done.
    let mut keep: Vec<wgpu::Buffer> = Vec::new();
    if let Some(nr) = nr.filter(|v| v.luma > 0.0 || v.chroma > 0.0) {
        let (luma, chroma) = (nr.luma.clamp(0.0, 100.0) / 100.0, nr.chroma.clamp(0.0, 100.0) / 100.0);
        let base = Nr {
            w: pl.w as u32,
            h: pl.h as u32,
            black: pl.black,
            a: nr.a,
            keep: if luma > 0.0 { 1.0 - 0.8 * luma } else { 1.0 },
            ..Default::default()
        };
        let u = e.uniform(bytes(&base));
        let [g, sg, dg, cr, cb, tmp] = &b.aux;
        let prep = e.bind(&e.nr_prep, &[&b.planes, g, sg, dg, cr, cb, &u]);
        {
            let mut p = enc.begin_compute_pass(&Default::default());
            p.set_pipeline(&e.nr_prep);
            p.set_bind_group(0, &prep, &[]);
            p.dispatch_workgroups(groups(n, 256), 1, 1);
        }
        let mut smooth = |enc: &mut wgpu::CommandEncoder, buf: &wgpu::Buffer, radius: u32, k: f32, scale: f32| {
            for horizontal in [1u32, 0] {
                let prm = Nr { radius, horizontal, k, sigma_scale: scale, ..base };
                let u = e.uniform(bytes(&prm));
                let (src, dst) = if horizontal == 1 { (buf, tmp) } else { (tmp, buf) };
                let bg = e.bind(&e.bilateral, &[src, dst, sg, &u]);
                let mut p = enc.begin_compute_pass(&Default::default());
                p.set_pipeline(&e.bilateral);
                p.set_bind_group(0, &bg, &[]);
                p.dispatch_workgroups(groups(pl.w, 16), groups(pl.h, 16), 1);
                drop(p);
                keep.push(u);
            }
        };
        if luma > 0.0 {
            smooth(&mut enc, g, 2, 0.5 + 2.5 * luma, std::f32::consts::FRAC_1_SQRT_2);
        }
        if chroma > 0.0 {
            let radius = 2 + (chroma * 4.0).round() as u32;
            let k = 1.0 + 3.0 * chroma;
            smooth(&mut enc, cr, radius, k, 1.2247);
            smooth(&mut enc, cb, radius, k, 1.2247);
        }
        let merge = e.bind(&e.nr_merge, &[&b.planes, g, sg, dg, cr, cb, &u]);
        let mut p = enc.begin_compute_pass(&Default::default());
        p.set_pipeline(&e.nr_merge);
        p.set_bind_group(0, &merge, &[]);
        p.dispatch_workgroups(groups(n, 256), 1, 1);
        drop(p);
        keep.push(u);
    }
    let (sx, sy) = (crop[2] * 2.0 * pl.w as f32 / ow as f32, crop[3] * 2.0 * pl.h as f32 / oh as f32);
    let mut geo = Geo {
        w: pl.w as u32,
        h: pl.h as u32,
        ow: ow as u32,
        oh: oh as u32,
        ox: crop[0] * 2.0 * pl.w as f32,
        oy: crop[1] * 2.0 * pl.h as f32,
        sx,
        sy,
        debayer: u32::from(sx < 2.0 || sy < 2.0),
        sharpen,
        format: u32::from(p010),
        bt2020: u32::from(matches!(format, Format::P010 { bt2020: true })),
        lut_min: [0.0; 4],
        lut_max: [1.0; 4],
        ..Default::default()
    };
    let lut_buf = match lut {
        Some(l) => {
            geo.lut_kind = if l.three_d { 2 } else { 1 };
            geo.lut_size = l.size as u32;
            geo.lut_min = [l.min[0], l.min[1], l.min[2], 0.0];
            geo.lut_max = [l.max[0], l.max[1], l.max[2], 1.0];
            let v: Vec<f32> = l.table.iter().flat_map(|c| [c[0], c[1], c[2], 0.0]).collect();
            // SAFETY: f32 slice as bytes.
            let raw = unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 4) };
            e.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: None,
                contents: raw,
                usage: wgpu::BufferUsages::STORAGE,
            })
        }
        None => e.buffer(16, wgpu::BufferUsages::STORAGE),
    };
    let su = e.uniform(bytes(s));
    let gu = e.uniform(bytes(&geo));
    let dbg = e.bind(&e.develop, &[&b.planes, &b.img, &su, &gu]);
    let fin = if p010 { &e.finish_p010 } else { &e.finish_rgba };
    let fbg = e.bind(fin, &[&b.img, &b.out, &lut_buf, &gu]);
    {
        let mut p = enc.begin_compute_pass(&Default::default());
        p.set_pipeline(&e.develop);
        p.set_bind_group(0, &dbg, &[]);
        p.dispatch_workgroups(groups(ow, 16), groups(oh, 16), 1);
        p.set_pipeline(fin);
        p.set_bind_group(0, &fbg, &[]);
        if p010 {
            p.dispatch_workgroups(groups(ow / 2, 16), groups(oh / 2, 16), 1);
        } else {
            p.dispatch_workgroups(groups(ow, 16), groups(oh, 16), 1);
        }
    }
    let len = (out_px * 4) as u64;
    enc.copy_buffer_to_buffer(&b.out, 0, &b.read, 0, len);
    let idx = e.queue.submit([enc.finish()]);
    let slice = b.read.slice(0..len);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r.is_ok());
    });
    let polled = e.device.poll(wgpu::PollType::Wait { submission_index: Some(idx), timeout: None });
    let ok = polled.is_ok() && rx.recv().unwrap_or(false);
    let result = if ok {
        let v = slice.get_mapped_range().ok().map(|m| m.to_vec());
        b.read.unmap();
        v
    } else {
        None
    };
    drop(keep);
    e.give(b);
    release();
    result
}
