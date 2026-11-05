//! Same-resolution rate control on the GPU through wgpu (Metal on a Mac, Vulkan or DX12 on
//! Windows and Linux): the rate controlled pass of [`crate::requant`] for frames whose
//! lossless repack does not fit, one workgroup per slice (see requant.wgsl). It gives the
//! same bytes as the CPU. Without a usable GPU, or when a frame trips anything the shader
//! does not handle, the caller codes the frame on the CPU. Frames from the workers coding
//! a clip at once are batched into one dispatch on kept buffers, and when enough frames
//! already wait for the GPU the next is coded on the CPU meanwhile.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

use crate::tico::{Layout, SIG_GROUP};

const SOURCE: &str = include_str!("requant.wgsl");
/// Batches on the GPU at once (bounds its memory; the next batch gathers meanwhile).
const MAX_IN_FLIGHT: usize = 2;

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
                max_storage_buffers_per_shader_stage: limits.max_storage_buffers_per_shader_stage.max(7),
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

/// A frame packed for the GPU: its table, codestream words and rate control inputs, and
/// the sizes of its output, result and scratch regions (in words).
struct Packed {
    tab: Vec<u32>,
    cs: Vec<u32>,
    meta: Vec<u32>,
    slices: usize,
    precincts: usize,
    out: usize,
    res: usize,
    scr: usize,
    cap: usize,
}

/// Packs `job`; None when the shader does not take its layout.
fn pack(e: &Engine, job: &Job) -> Option<Packed> {
    let l = job.layout;
    let (mut tab, groups, state, _) = tables(l)?;
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
    let mut cs = vec![0u32; job.cs.len().div_ceil(4)];
    let (whole, rest) = job.cs.as_chunks::<4>();
    for (w, c) in cs.iter_mut().zip(whole) {
        *w = u32::from_le_bytes(*c);
    }
    if !rest.is_empty() {
        let mut w = [0u8; 4];
        w[..rest.len()].copy_from_slice(rest);
        cs[whole.len()] = u32::from_le_bytes(w);
    }
    let p = Packed {
        tab,
        cs,
        meta,
        slices: l.slices,
        precincts: n,
        out: l.slices * cap,
        res: 4 * l.slices + n,
        scr: l.slices * stride,
        cap,
    };
    let max = e.device.limits().max_storage_buffer_binding_size / 4;
    if p.out as u64 > max || p.scr as u64 > max || p.cs.len() as u64 > max {
        return None;
    }
    Some(p)
}

/// Frames waiting for or on the GPU.
static QUEUED: AtomicUsize = AtomicUsize::new(0);

/// How many frames may wait for the GPU before the next is coded on the CPU (only reached
/// with 15 or more workers; 14 measured best with 16 on an M4 Max).
fn queue_cap() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| std::env::var("BLUECOMPRESS_GPU_QUEUE").ok().and_then(|v| v.parse().ok()).unwrap_or(14))
}

/// False with `BLUECOMPRESS_GPU_ONLY=1`: frames always wait for the GPU.
fn share_with_cpu() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| std::env::var_os("BLUECOMPRESS_GPU_ONLY").is_none_or(|v| v.is_empty() || v == "0"))
}

/// Frames a batch takes at most, and how long the first frame waits for others.
const MAX_BATCH: usize = 8;
const GATHER: std::time::Duration = std::time::Duration::from_micros(1500);

struct Pending {
    packed: Packed,
    done: std::sync::mpsc::Sender<Option<Coded>>,
}

#[derive(Default)]
struct Gather {
    pending: Vec<Pending>,
    leader: bool,
}

fn gather() -> &'static (Mutex<Gather>, Condvar) {
    static G: OnceLock<(Mutex<Gather>, Condvar)> = OnceLock::new();
    G.get_or_init(Default::default)
}

/// Buffers kept between batches (grown when a batch needs more).
struct Buffers {
    words: [usize; 7],
    tab: wgpu::Buffer,
    cs: wgpu::Buffer,
    meta: wgpu::Buffer,
    out: wgpu::Buffer,
    res: wgpu::Buffer,
    scr: wgpu::Buffer,
    frames: wgpu::Buffer,
    read_out: wgpu::Buffer,
    read_res: wgpu::Buffer,
}

fn pool() -> &'static Mutex<Vec<Buffers>> {
    static P: OnceLock<Mutex<Vec<Buffers>>> = OnceLock::new();
    P.get_or_init(Default::default)
}

fn buffers(e: &Engine, need: [usize; 7]) -> Buffers {
    let mut p = pool().lock().unwrap();
    if let Some(i) = p.iter().position(|b| b.words.iter().zip(need).all(|(&h, n)| h >= n)) {
        return p.swap_remove(i);
    }
    // Grow the largest kept set rather than keep sets of every size.
    let old = p.pop();
    drop(p);
    let words: [usize; 7] = std::array::from_fn(|i| {
        let had = old.as_ref().map_or(0, |b| b.words[i]);
        if need[i] <= had {
            had
        } else {
            (need[i] + need[i] / 4).max(4)
        }
    });
    let st = wgpu::BufferUsages::STORAGE;
    let cd = wgpu::BufferUsages::COPY_DST;
    let cs = wgpu::BufferUsages::COPY_SRC;
    let rd = wgpu::BufferUsages::MAP_READ | cd;
    let mk = |w: usize, usage| {
        e.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (w as u64 * 4).max(16),
            usage,
            mapped_at_creation: false,
        })
    };
    Buffers {
        tab: mk(words[0], st | cd),
        cs: mk(words[1], st | cd),
        meta: mk(words[2], st | cd),
        out: mk(words[3], st | cd | cs),
        res: mk(words[4], st | cd | cs),
        scr: mk(words[5], st),
        frames: mk(words[6], st | cd),
        read_out: mk(words[3], rd),
        read_res: mk(words[4], rd),
        words,
    }
}

/// Codes `job` on the GPU; None when there is none or the frame is left to the CPU.
/// Frames from the workers coding a clip at once are batched into one dispatch, which
/// keeps the GPU busy (a frame alone has only a workgroup per slice).
pub(crate) fn run(job: &Job) -> Option<Coded> {
    let e = engine()?;
    let packed = pack(e, job)?;
    let (tx, rx) = std::sync::mpsc::channel();
    let (m, cv) = gather();
    let mut g = m.lock().unwrap();
    // With the GPU busy and the next batch already full, this frame is coded sooner on
    // the CPU (the same bytes), so both work at once.
    if QUEUED.load(Ordering::Relaxed) >= queue_cap() && share_with_cpu() {
        return None;
    }
    QUEUED.fetch_add(1, Ordering::Relaxed);
    g.pending.push(Pending { packed, done: tx });
    if !g.leader {
        g.leader = true;
        let until = std::time::Instant::now() + GATHER;
        while g.pending.len() < MAX_BATCH {
            let now = std::time::Instant::now();
            if now >= until {
                break;
            }
            g = cv.wait_timeout(g, until - now).unwrap().0;
        }
        let batch = std::mem::take(&mut g.pending);
        g.leader = false;
        drop(g);
        {
            let (m, cv) = &e.slots;
            let mut busy = m.lock().unwrap();
            while *busy >= MAX_IN_FLIGHT {
                busy = cv.wait(busy).unwrap();
            }
            *busy += 1;
        }
        dispatch(e, batch);
        let (m, cv) = &e.slots;
        *m.lock().unwrap() -= 1;
        cv.notify_one();
    } else {
        cv.notify_all();
        drop(g);
    }
    let r = rx.recv().ok().flatten();
    QUEUED.fetch_sub(1, Ordering::Relaxed);
    r
}

/// Runs a batch and sends each frame its result.
fn dispatch(e: &Engine, batch: Vec<Pending>) {
    // Offsets of each frame's parts (words), as the shader reads them from `frames`.
    let mut at = [0usize; 6];
    let mut table = Vec::with_capacity(batch.len() * 8);
    for p in &batch {
        table.extend(at.iter().map(|&v| v as u32));
        table.push(p.packed.slices as u32);
        table.push(0);
        for (a, n) in at.iter_mut().zip([
            p.packed.tab.len(),
            p.packed.cs.len(),
            p.packed.meta.len(),
            p.packed.out,
            p.packed.res,
            p.packed.scr,
        ]) {
            *a += n;
        }
    }
    let max = (e.device.limits().max_storage_buffer_binding_size / 4) as usize;
    if at.iter().any(|&v| v > max || v >= 1 << 30) {
        // Too much for one dispatch: halve it.
        if batch.len() > 1 {
            let mut a = batch;
            let b = a.split_off(a.len() / 2);
            dispatch(e, a);
            dispatch(e, b);
        } else {
            for p in batch {
                let _ = p.done.send(None);
            }
        }
        return;
    }
    let b = buffers(e, [at[0], at[1], at[2], at[3], at[4], at[5], table.len()]);
    let scope = e.device.push_error_scope(wgpu::ErrorFilter::Validation);
    let put = |buf: &wgpu::Buffer, parts: &mut dyn Iterator<Item = &Vec<u32>>| {
        let mut off = 0u64;
        for v in parts {
            if !v.is_empty() {
                e.queue.write_buffer(buf, off, as_bytes(v));
            }
            off += v.len() as u64 * 4;
        }
    };
    put(&b.tab, &mut batch.iter().map(|p| &p.packed.tab));
    put(&b.cs, &mut batch.iter().map(|p| &p.packed.cs));
    put(&b.meta, &mut batch.iter().map(|p| &p.packed.meta));
    e.queue.write_buffer(&b.frames, 0, as_bytes(&table));
    let bind = e.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &e.pipeline.get_bind_group_layout(0),
        entries: &[&b.tab, &b.cs, &b.meta, &b.out, &b.res, &b.scr, &b.frames]
            .iter()
            .enumerate()
            .map(|(i, b)| wgpu::BindGroupEntry { binding: i as u32, resource: b.as_entire_binding() })
            .collect::<Vec<_>>(),
    });
    let (out_bytes, res_bytes) = (at[3] as u64 * 4, at[4] as u64 * 4);
    let mut enc = e.device.create_command_encoder(&Default::default());
    // Kept buffers hold the last batch: clear what the shader adds to or may leave.
    enc.clear_buffer(&b.out, 0, Some(out_bytes.max(4)));
    enc.clear_buffer(&b.res, 0, Some(res_bytes.max(4)));
    {
        let mut p = enc.begin_compute_pass(&Default::default());
        p.set_pipeline(&e.pipeline);
        p.set_bind_group(0, &bind, &[]);
        let wide = batch.iter().map(|p| p.packed.slices).max().unwrap_or(1);
        p.dispatch_workgroups(wide as u32, batch.len() as u32, 1);
    }
    enc.copy_buffer_to_buffer(&b.out, 0, &b.read_out, 0, out_bytes.max(4));
    enc.copy_buffer_to_buffer(&b.res, 0, &b.read_res, 0, res_bytes.max(4));
    let t0 = std::time::Instant::now();
    let idx = e.queue.submit([enc.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    for (buf, len) in [(&b.read_out, out_bytes.max(4)), (&b.read_res, res_bytes.max(4))] {
        let tx = tx.clone();
        buf.slice(..len).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r.is_ok());
        });
    }
    drop(tx);
    let polled = e.device.poll(wgpu::PollType::Wait { submission_index: Some(idx), timeout: None });
    let mapped = polled.is_ok() && rx.iter().take(2).all(|ok| ok);
    let failed = pollster::block_on(scope.pop()).is_some();
    if std::env::var_os("BLUECOMPRESS_GPU_DEBUG").is_some() {
        eprintln!("bluecompress: GPU batch of {} frames in {:.1} ms", batch.len(), t0.elapsed().as_secs_f64() * 1e3);
    }
    let mut results: Vec<Option<Coded>> = Vec::with_capacity(batch.len());
    if mapped && !failed {
        let res = b.read_res.slice(..res_bytes.max(4)).get_mapped_range();
        let out = b.read_out.slice(..out_bytes.max(4)).get_mapped_range();
        if let (Ok(res), Ok(out)) = (res, out) {
            let word = |i: usize| u32::from_le_bytes(res[4 * i..4 * i + 4].try_into().unwrap());
            for (k, p) in batch.iter().enumerate() {
                let (ro, oo) = (table[8 * k + 4] as usize, table[8 * k + 3] as usize * 4);
                results.push(collect(&p.packed, |i| word(ro + i), &out[oo..oo + p.packed.out * 4]));
            }
        }
    }
    b.read_out.unmap();
    b.read_res.unmap();
    let mut pl = pool().lock().unwrap();
    if pl.len() < MAX_IN_FLIGHT {
        pl.push(b);
    }
    drop(pl);
    results.resize_with(batch.len(), || None);
    for (p, r) in batch.into_iter().zip(results) {
        let _ = p.done.send(r);
    }
}

/// One frame's slices out of the batch's output.
fn collect(p: &Packed, res: impl Fn(usize) -> u32, out: &[u8]) -> Option<Coded> {
    let (cap, n) = (p.cap, p.precincts);
    let mut coded =
        Coded { slices: Vec::with_capacity(p.slices), ranks: Vec::with_capacity(n), requantized: 0, rank_sum: 0 };
    for s in 0..p.slices {
        let (len, err) = (res(4 * s) as usize, res(4 * s + 1));
        let at = s * cap * 4;
        let header = [0xff, 0x20, 0x00, 0x04, (s >> 8) as u8, s as u8];
        if err != 0 || len > cap * 4 || len < 6 || out[at..at + 6] != header {
            if std::env::var_os("BLUECOMPRESS_GPU_DEBUG").is_some() {
                eprintln!("bluecompress: GPU slice {s} left to the CPU (code {err})");
            }
            return None;
        }
        coded.slices.push(out[at..at + len].to_vec());
        coded.requantized += res(4 * s + 2) as u64;
        coded.rank_sum += res(4 * s + 3) as i64;
    }
    coded.ranks.extend((0..n).map(|i| res(4 * p.slices + i) as i32));
    FRAMES.fetch_add(1, Ordering::Relaxed);
    Some(coded)
}
