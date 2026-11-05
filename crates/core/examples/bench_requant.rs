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

include!("common/synthetic_6k.rs");

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
