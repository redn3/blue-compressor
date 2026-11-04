//! Same-resolution rate control on the GPU through wgpu (Metal on a Mac, Vulkan or DX12 on
//! Windows and Linux): the rate controlled pass of [`crate::requant`] for frames whose
//! lossless repack does not fit, one workgroup per slice (see requant.wgsl). It gives the
//! same bytes as the CPU. Without a usable GPU, or when a frame trips anything the shader
//! does not handle, the caller codes the frame on the CPU.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

use wgpu::util::DeviceExt;

use crate::tico::{Layout, SIG_GROUP};

const SOURCE: &str = include_str!("requant.wgsl");
/// Frames on the GPU at once (bounds its memory; the CPU prepares the next meanwhile).
const MAX_IN_FLIGHT: usize = 4;

struct Engine {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    name: String,
    slots: (Mutex<usize>, Condvar),
}

/// True when the environment turns the GPU off for compression.
pub fn disabled_by_env() -> bool {
    std::env::var_os("BLUECOMPRESS_NO_GPU").is_some_and(|v| !v.is_empty() && v != "0")
}

/// `BLUECOMPRESS_GPU_VERIFY=1`: code every GPU frame on the CPU as well and keep the CPU's.
pub fn verify() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| std::env::var_os("BLUECOMPRESS_GPU_VERIFY").is_some_and(|v| !v.is_empty() && v != "0"))
}

static FRAMES: AtomicUsize = AtomicUsize::new(0);
static MISMATCHES: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn note_mismatch() {
    MISMATCHES.fetch_add(1, Ordering::Relaxed);
}

/// (frames coded on the GPU, frames that differed from the CPU's under [`verify`]).
pub fn counts() -> (usize, usize) {
    (FRAMES.load(Ordering::Relaxed), MISMATCHES.load(Ordering::Relaxed))
}

fn engine() -> Option<&'static Engine> {
    static E: OnceLock<Option<Engine>> = OnceLock::new();
    E.get_or_init(|| {
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
        // A software rasterizer is slower than the CPU path (allowed for tests).
        if info.device_type == wgpu::DeviceType::Cpu && std::env::var_os("BLUECOMPRESS_GPU_ANY").is_none() {
            return None;
        }
        let limits = adapter.limits();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("requant"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits {
                max_storage_buffer_binding_size: limits.max_storage_buffer_binding_size,
                max_buffer_size: limits.max_buffer_size,
                max_storage_buffers_per_shader_stage: limits.max_storage_buffers_per_shader_stage.max(6),
                ..wgpu::Limits::default()
            },
            ..Default::default()
        }))
        .ok()?;
        device.on_uncaptured_error(std::sync::Arc::new(|e| eprintln!("bluecompress: GPU error: {e}")));
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("requant"),
            source: wgpu::ShaderSource::Wgsl(SOURCE.into()),
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("requant"),
            layout: None,
            module: &module,
            entry_point: Some("requant"),
            compilation_options: Default::default(),
            cache: None,
        });
        if let Some(err) = pollster::block_on(scope.pop()) {
            if std::env::var_os("BLUECOMPRESS_GPU_DEBUG").is_some() {
                eprintln!("bluecompress: GPU compression unavailable: {err}");
            }
            return None;
        }
        let api = match info.backend {
            wgpu::Backend::Metal => "Metal",
            wgpu::Backend::Vulkan => "Vulkan",
            wgpu::Backend::Dx12 => "DX12",
            _ => "GPU",
        };
        Some(Engine {
            name: format!("{api} ({})", info.name),
            device,
            queue,
            pipeline,
            slots: (Mutex::new(0), Condvar::new()),
        })
    })
    .as_ref()
}

/// The GPU compression runs on, as "Metal (Apple M3 Max)", or None.
pub fn name() -> Option<String> {
    engine().map(|e| e.name.clone())
}

/// Words per band line in the layout table.
const LINE_WORDS: usize = 12;

/// The layout tables the shader reads, after 32 words of parameters (filled in by `run`).
fn tables(l: &Layout) -> Option<(Vec<u32>, usize, usize, usize)> {
    let nb = l.bands.len();
    let mut lines: Vec<[u32; LINE_WORDS]> = Vec::new();
    let mut pks: Vec<[u32; 4]> = Vec::new();
    let mut state_off = vec![0usize; nb];
    let mut sg_total = 0;
    for (b, band) in l.bands.iter().enumerate() {
        state_off[b] = sg_total;
        sg_total += band.groups;
    }
    let mut g = 0usize;
    let mut sg = 0usize;
    let mut g2l = Vec::new();
    let mut sg2l = Vec::new();
    for (k, pk) in l.packets.iter().enumerate() {
        let first = lines.len();
        let pg = g;
        for b in pk.band_begin..pk.band_end {
            let band = &l.bands[b];
            let idx = lines.len();
            let prev = lines.iter().rposition(|x| x[0] as usize == b).map_or(0, |i| i + 1);
            lines.push([
                b as u32,
                k as u32,
                g as u32,
                band.groups as u32,
                band.width as u32,
                band.sig_groups as u32,
                band.gain as u32,
                band.priority as u32,
                prev as u32,
                1,
                state_off[b] as u32,
                sg as u32,
            ]);
            if prev != 0 {
                lines[prev - 1][9] = 0;
            }
            g2l.extend(std::iter::repeat_n(idx as u32, band.groups));
            sg2l.extend(std::iter::repeat_n(idx as u32, band.sig_groups));
            g += band.groups;
            sg += band.sig_groups;
        }
        pks.push([first as u32, lines.len() as u32, pg as u32, (g - pg) as u32]);
    }
    if nb > 32 || lines.len() > 64 || pks.len() > 16 || SIG_GROUP != 8 {
        return None;
    }
    let mut tab = vec![0u32; 32];
    let lines_at = tab.len();
    tab.extend(lines.iter().flatten());
    let pks_at = tab.len();
    tab.extend(pks.iter().flatten());
    let g2l_at = tab.len();
    tab.extend(&g2l);
    let sg2l_at = tab.len();
    tab.extend(&sg2l);
    tab[1] = lines.len() as u32;
    tab[2] = pks.len() as u32;
    tab[3] = g as u32;
    tab[11] = sg_total as u32;
    tab[14] = lines_at as u32;
    tab[15] = pks_at as u32;
    tab[16] = g2l_at as u32;
    tab[17] = sg2l_at as u32;
    tab[18] = sg as u32;
    Some((tab, g, sg_total, sg))
}

/// One frame for [`run`]: the codestream, the offsets of its precincts, and the rate
/// control inputs of `requant_to_budget`.
pub(crate) struct Job<'a> {
    pub cs: &'a [u8],
    pub layout: &'a Layout,
    pub precincts: &'a [usize],
    pub weights: &'a [usize],
    pub budgets: &'a [usize],
    pub seed: Option<&'a [i32]>,
    pub start: i32,
    pub hi: i32,
}

/// What the GPU coded: each slice (starting with its SLH), each precinct's rank, and
/// (requantized precincts, sum of camera ranks).
pub(crate) struct Coded {
    pub slices: Vec<Vec<u8>>,
    pub ranks: Vec<i32>,
    pub requantized: u64,
    pub rank_sum: i64,
}

fn as_bytes(v: &[u32]) -> &[u8] {
    // SAFETY: u32 slices are plain bytes.
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 4) }
}

/// Codes `job` on the GPU; None when there is none or the frame is left to the CPU.
pub(crate) fn run(job: &Job) -> Option<Coded> {
    let e = engine()?;
    let l = job.layout;
    let (mut tab, groups, state, nsg) = tables(l)?;
    let n = l.precincts;
    let fits32 = |v: usize| v < (1usize << 31);
    if !fits32(job.cs.len()) || job.precincts.len() != n || job.start < 0 || job.hi < 0 {
        return None;
    }
    let mut cap_bytes = 0usize;
    for s in 0..l.slices {
        let p0 = s * l.slice_height;
        let p1 = n.min(p0 + l.slice_height);
        let w: usize = job.weights[p0..p1].iter().sum();
        if !fits32(w) || !fits32(job.budgets[s]) {
            return None;
        }
        cap_bytes = cap_bytes.max(w.max(job.budgets[s]) * 5 / 4 + 16384);
    }
    let cap = cap_bytes.div_ceil(4);
    let stride = 11 * groups + 2 * state + 64;
    let _ = nsg;
    tab[0] = l.bands.len() as u32;
    tab[4] = n as u32;
    tab[5] = l.slice_height as u32;
    tab[6] = l.slices as u32;
    tab[7] = l.precinct_header_bytes() as u32;
    tab[8] = job.hi as u32;
    tab[9] = job.seed.is_some() as u32;
    tab[10] = job.start as u32;
    tab[12] = cap as u32;
    tab[13] = job.cs.len() as u32;
    tab[19] = stride as u32;
    let mut meta = Vec::with_capacity(3 * n + l.slices);
    meta.extend(job.precincts.iter().map(|&o| o as u32));
    meta.extend(job.weights.iter().map(|&w| w as u32));
    match job.seed {
        Some(s) => meta.extend(s.iter().map(|&r| r.max(0) as u32)),
        None => meta.extend(std::iter::repeat_n(0, n)),
    }
    meta.extend(job.budgets.iter().map(|&b| b as u32));
    let mut cs_words = vec![0u32; job.cs.len().div_ceil(4)];
    for (i, c) in job.cs.chunks(4).enumerate() {
        let mut w = [0u8; 4];
        w[..c.len()].copy_from_slice(c);
        cs_words[i] = u32::from_le_bytes(w);
    }
    let out_bytes = (l.slices * cap * 4) as u64;
    let res_bytes = ((4 * l.slices + n) * 4) as u64;
    let scr_bytes = (l.slices * stride * 4) as u64;
    let max = e.device.limits().max_storage_buffer_binding_size;
    if out_bytes > max || scr_bytes > max || (cs_words.len() * 4) as u64 > max {
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
    let scope = e.device.push_error_scope(wgpu::ErrorFilter::Validation);
    let result = (|| {
        let st = wgpu::BufferUsages::STORAGE;
        let init = |data: &[u8]| {
            e.device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: None, contents: data, usage: st })
        };
        let buffer = |size: u64, usage: wgpu::BufferUsages| {
            e.device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: size.max(16),
                usage,
                mapped_at_creation: false,
            })
        };
        let tab_b = init(as_bytes(&tab));
        let cs_b = init(as_bytes(&cs_words));
        let meta_b = init(as_bytes(&meta));
        let out_b = buffer(out_bytes, st | wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST);
        let res_b = buffer(res_bytes, st | wgpu::BufferUsages::COPY_SRC);
        let scr_b = buffer(scr_bytes, st);
        let read_out = buffer(out_bytes, wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST);
        let read_res = buffer(res_bytes, wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST);
        let bind = e.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &e.pipeline.get_bind_group_layout(0),
            entries: &[&tab_b, &cs_b, &meta_b, &out_b, &res_b, &scr_b]
                .iter()
                .enumerate()
                .map(|(i, b)| wgpu::BindGroupEntry { binding: i as u32, resource: b.as_entire_binding() })
                .collect::<Vec<_>>(),
        });
        let mut enc = e.device.create_command_encoder(&Default::default());
        enc.clear_buffer(&out_b, 0, None);
        {
            let mut p = enc.begin_compute_pass(&Default::default());
            p.set_pipeline(&e.pipeline);
            p.set_bind_group(0, &bind, &[]);
            p.dispatch_workgroups(l.slices as u32, 1, 1);
        }
        enc.copy_buffer_to_buffer(&out_b, 0, &read_out, 0, out_bytes);
        enc.copy_buffer_to_buffer(&res_b, 0, &read_res, 0, res_bytes);
        let idx = e.queue.submit([enc.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        for b in [&read_out, &read_res] {
            let tx = tx.clone();
            b.slice(..).map_async(wgpu::MapMode::Read, move |r| {
                let _ = tx.send(r.is_ok());
            });
        }
        drop(tx);
        let polled = e.device.poll(wgpu::PollType::Wait { submission_index: Some(idx), timeout: None });
        if polled.is_err() || !rx.iter().take(2).all(|ok| ok) {
            return None;
        }
        let res: Vec<u32> = {
            let m = read_res.slice(..).get_mapped_range().ok()?;
            m.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect()
        };
        let out = read_out.slice(..).get_mapped_range().ok()?;
        let mut coded =
            Coded { slices: Vec::with_capacity(l.slices), ranks: Vec::with_capacity(n), requantized: 0, rank_sum: 0 };
        for s in 0..l.slices {
            let (len, err) = (res[4 * s] as usize, res[4 * s + 1]);
            let at = s * cap * 4;
            let header = [0xff, 0x20, 0x00, 0x04, (s >> 8) as u8, s as u8];
            if err != 0 || len > cap * 4 || len < 6 || out[at..at + 6] != header {
                if std::env::var_os("BLUECOMPRESS_GPU_DEBUG").is_some() {
                    eprintln!("bluecompress: GPU slice {s} left to the CPU (code {err})");
                }
                return None;
            }
            coded.slices.push(out[at..at + len].to_vec());
            coded.requantized += res[4 * s + 2] as u64;
            coded.rank_sum += res[4 * s + 3] as i64;
        }
        coded.ranks.extend(res[4 * l.slices..4 * l.slices + n].iter().map(|&r| r as i32));
        FRAMES.fetch_add(1, Ordering::Relaxed);
        Some(coded)
    })();
    let failed = pollster::block_on(scope.pop()).is_some();
    let (m, cv) = &e.slots;
    *m.lock().unwrap() -= 1;
    cv.notify_one();
    result.filter(|_| !failed)
}
