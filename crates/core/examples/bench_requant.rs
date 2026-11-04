//! Benchmark of same-resolution bpp transcoding (the Compress path) on synthetic 6K frames:
//! `cargo run --release --example bench_requant [-- frames bpp]`. Frames are coded one
//! after another with seeds carried over, as in a clip; slices run on all cores
//! (`RAYON_NUM_THREADS=1` for one). `BENCH_GPU=1` runs the rate control on the GPU; the
//! printed hash of the output is the same either way.

use std::time::Instant;

use bluecompress::frame::{encode_frame, forward_star_tetrix, forward_wavelet, Coeffs, Planes};
use bluecompress::requant::{requant_codestream_seeded, RequantParams};
use bluecompress::tico::{Layout, TicoHeader};
use bluecompress::{bpp_of, Target};

/// A TICO RAW header like the cameras' (4 components, 5 horizontal and 1 vertical levels,
/// 25 bands), `width` x `height` Bayer samples, 8 precinct rows per slice.
fn header(width: u16, height: u16) -> (TicoHeader, Layout) {
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
    for (hl, vl) in [(5u8, 1u8), (5, 1), (0, 0), (5, 1)] {
        hdr.extend_from_slice(&[14, 0x11, (hl << 4) | vl]);
    }
    let nbands = 25usize;
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
                pl.data[c * l.cw * l.ch + y * l.cw + x] = (v + 900.0 * noise(x, y, seed + c as u32)) as i32;
            }
        }
    }
    forward_star_tetrix(&mut pl);
    let mut coeffs = vec![0i32; Coeffs::len(l)];
    forward_wavelet(l, &pl, &mut coeffs);
    coeffs
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let frames: usize = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(4);
    let bpp: f64 = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(1.25);
    let (w, h) = (6048u16, 3200u16);
    let (hd, l) = header(w, h);
    let src: Vec<Vec<u8>> =
        (0..frames).map(|i| encode_frame(&hd, &l, &scene(&l, 1 + i as u32 % 2), 120).unwrap()).collect();
    println!("source {:.2} bpp, target {bpp} bpp", bpp_of(src[0].len() as f64, w as u32, h as u32));
    let budget = Target::bpp(bpp).budget(w as u32, h as u32);
    let gpu = std::env::var_os("BENCH_GPU").is_some();
    let p = RequantParams { budget, gpu, ..Default::default() };
    if gpu {
        println!("GPU: {:?}", bluecompress::compress_gpu(bluecompress::Backend::Auto));
    }
    let t = Instant::now();
    for cs in &src {
        requant_codestream_seeded(cs, &RequantParams::default(), 1, &mut Vec::new()).unwrap();
    }
    println!("lossless repack alone: {:.1} ms per frame", t.elapsed().as_secs_f64() * 1e3 / frames as f64);
    let mut seed = Vec::new();
    let t = Instant::now();
    let mut out_len = 0;
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for cs in &src {
        let out = requant_codestream_seeded(cs, &p, 1, &mut seed).unwrap().0;
        out_len = out.len();
        for b in out {
            hash = (hash ^ b as u64).wrapping_mul(0x100_0000_01b3);
        }
    }
    let s = t.elapsed().as_secs_f64();
    println!(
        "{frames} frames in {s:.2} s: {:.2} fps per thread, out {:.3} bpp, hash {hash:016x}",
        frames as f64 / s,
        bpp_of(out_len as f64, w as u32, h as u32)
    );
}
