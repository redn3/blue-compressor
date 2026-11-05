//! The GPU rate control (wgpu) against the CPU on synthetic frames: same bytes and the
//! same ranks, frame after frame. Skipped without a usable GPU; set
//! BLUECOMPRESS_GPU_ANY=1 to run it on a software Vulkan driver.

use bluecompress::frame::{encode_frame, forward_star_tetrix, forward_wavelet, Coeffs, Planes};
use bluecompress::requant::{requant_codestream_seeded, RequantParams};
use bluecompress::tico::{Layout, TicoHeader};
use bluecompress::{Backend, Target};

/// A TICO RAW header like the cameras' (4 components, 5 horizontal and 1 vertical levels,
/// 25 bands), `width` x `height` Bayer samples, 8 precinct rows per slice.
fn header_with(width: u16, height: u16, levels: [(u8, u8); 4]) -> (TicoHeader, Layout) {
    let mut hdr = vec![0xff, 0x10];
    let mut pih = vec![0u8; 37];
    pih[8..10].copy_from_slice(&width.to_be_bytes());
    pih[10..12].copy_from_slice(&height.to_be_bytes());
    pih[14..16].copy_from_slice(&8u16.to_be_bytes());
    pih[16] = 4;
    pih[17] = 4;
    pih[18] = 8;
    pih[20] = 0x44;
    pih[21] = 0x10;
    pih[22] = 0x01;
    pih[23] = 0x14;
    pih[24 + 3] = 0x02;
    pih[24 + 4] = 0x58;
    pih[24 + 7] = 0x01;
    pih[24 + 10] = 0x08;
    hdr.extend_from_slice(&[0xff, 0x12, 0x00, 39]);
    hdr.extend_from_slice(&pih);
    hdr.extend_from_slice(&[0xff, 0x13, 0x00, 14]);
    for (hl, vl) in levels {
        hdr.extend_from_slice(&[14, 0x11, (hl << 4) | vl]);
    }
    let nbands: usize = levels.iter().map(|&(hl, _)| if hl == 0 { 1 } else { hl as usize + 3 }).sum();
    hdr.extend_from_slice(&[0xff, 0x14]);
    hdr.extend_from_slice(&((2 + 2 * nbands) as u16).to_be_bytes());
    // Camera-like weights: coarser gain for the finer high-pass bands.
    for i in 0..nbands {
        let gain = [0u8, 1, 1, 2, 2, 3, 4][i % 7];
        hdr.extend_from_slice(&[gain, i as u8]);
    }
    hdr.extend_from_slice(&[0xff, 0x20, 0x00, 0x04, 0x00, 0x00]);
    let (h, _) = TicoHeader::parse(&hdr).unwrap();
    let l = Layout::make(&h, width as usize, height as usize).unwrap();
    (h, l)
}

fn noise(x: usize, y: usize, seed: u32) -> f32 {
    let mut h =
        (x as u32).wrapping_mul(374_761_393) ^ (y as u32).wrapping_mul(668_265_263) ^ seed.wrapping_mul(2_246_822_519);
    h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
    ((h ^ (h >> 16)) as f32 / u32::MAX as f32) - 0.5
}

/// Coefficients of a scene with sky, a hill line, foliage texture and sensor noise, so
/// precincts differ in how hard they are to code.
fn scene(l: &Layout, seed: u32) -> Vec<i32> {
    let mut pl = Planes::new(l.cw, l.ch);
    for c in 0..4 {
        for y in 0..l.ch {
            for x in 0..l.cw {
                let (fx, fy) = (x as f32 / l.cw as f32, y as f32 / l.ch as f32);
                let hill = 0.5 + 0.1 * (fx * 9.0).sin();
                let v = if fy < hill {
                    9000.0 - 4000.0 * fy + 300.0 * c as f32
                } else {
                    let leaf = (fx * 400.0).sin() * (fy * 300.0).cos();
                    -4000.0 + 5000.0 * leaf + 1500.0 * noise(x / 3, y / 3, seed + 7)
                };
                pl.data[c * l.cw * l.ch + y * l.cw + x] = (v + 260.0 * noise(x, y, seed + c as u32)) as i32;
            }
        }
    }
    forward_star_tetrix(&mut pl);
    let mut coeffs = vec![0i32; Coeffs::len(l)];
    forward_wavelet(l, &pl, &mut coeffs);
    coeffs
}

fn header(width: u16, height: u16) -> (TicoHeader, Layout) {
    header_with(width, height, [(5, 1), (5, 1), (0, 0), (5, 1)])
}

fn gpu() -> bool {
    match bluecompress::compress_gpu(Backend::Auto) {
        Some(name) => {
            eprintln!("GPU: {name}");
            true
        }
        None => {
            eprintln!("no usable GPU, skipped");
            false
        }
    }
}

/// Codes `frames` on the CPU and on the GPU with seeds carried over, and checks both agree.
fn same(frames: &[Vec<u8>], w: u16, h: u16, bpp: f64) {
    let budget = Target::bpp(bpp).budget(w as u32, h as u32);
    let (mut s_cpu, mut s_gpu) = (Vec::new(), Vec::new());
    let before = bluecompress::compress_gpu_counts().0;
    for (i, cs) in frames.iter().enumerate() {
        let cpu =
            requant_codestream_seeded(cs, &RequantParams { budget, ..Default::default() }, 0, &mut s_cpu).unwrap();
        let gpu =
            requant_codestream_seeded(cs, &RequantParams { budget, gpu: true, ..Default::default() }, 0, &mut s_gpu)
                .unwrap();
        assert!(
            cpu.0 == gpu.0,
            "{w}x{h} at {bpp} bpp, frame {i}: GPU bytes differ ({} vs {})",
            gpu.0.len(),
            cpu.0.len()
        );
        assert_eq!(s_cpu, s_gpu, "{w}x{h} at {bpp} bpp, frame {i}: ranks differ");
        assert_eq!((cpu.1.requantized, cpu.1.rank_sum), (gpu.1.requantized, gpu.1.rank_sum));
    }
    // Frames that do not fit as they are go through the GPU.
    assert!(bluecompress::compress_gpu_counts().0 > before, "nothing ran on the GPU");
}

#[test]
fn gpu_matches_cpu_across_targets() {
    if !gpu() {
        return;
    }
    let (w, h) = (2048u16, 512u16);
    let (hd, l) = header(w, h);
    let frames: Vec<Vec<u8>> = (0..3).map(|i| encode_frame(&hd, &l, &scene(&l, 1 + i), 120).unwrap()).collect();
    for bpp in [0.5, 1.0, 1.25, 1.5, 2.5] {
        same(&frames, w, h, bpp);
    }
}

#[test]
fn gpu_matches_cpu_on_other_layouts() {
    if !gpu() {
        return;
    }
    // Odd sizes (partial code and significance groups, an odd number of precinct rows and
    // a short last slice), and coarse and fine camera ranks.
    for (w, h, levels, rank) in [
        (1000u16, 202u16, [(5u8, 1u8), (5, 1), (0, 0), (5, 1)], 120),
        (1334, 90, [(5, 1), (5, 1), (0, 0), (5, 1)], 150),
        (2048, 256, [(5, 1), (5, 1), (0, 0), (5, 1)], 100),
        (1024, 160, [(4, 1), (3, 1), (0, 0), (5, 1)], 120),
    ] {
        let (hd, l) = header_with(w, h, levels);
        let frames: Vec<Vec<u8>> = (0..2).map(|i| encode_frame(&hd, &l, &scene(&l, 9 + i), rank).unwrap()).collect();
        for bpp in [0.7, 1.3] {
            same(&frames, w, h, bpp);
        }
    }
}

#[test]
fn gpu_batches_frames_of_different_layouts() {
    if !gpu() {
        return;
    }
    // Workers coding at once share GPU dispatches; each frame must still match the CPU.
    let cases = [
        (2048u16, 512u16, [(5u8, 1u8), (5, 1), (0, 0), (5, 1)], 0.9),
        (1000, 202, [(5, 1), (5, 1), (0, 0), (5, 1)], 1.1),
        (1024, 160, [(4, 1), (3, 1), (0, 0), (5, 1)], 0.7),
        (1334, 90, [(5, 1), (5, 1), (0, 0), (5, 1)], 1.3),
    ];
    std::thread::scope(|sc| {
        for (k, (w, h, levels, bpp)) in cases.into_iter().enumerate() {
            sc.spawn(move || {
                let (hd, l) = header_with(w, h, levels);
                let frames: Vec<Vec<u8>> =
                    (0..3).map(|i| encode_frame(&hd, &l, &scene(&l, 20 + k as u32 + i), 120).unwrap()).collect();
                same(&frames, w, h, bpp);
            });
        }
    });
}
