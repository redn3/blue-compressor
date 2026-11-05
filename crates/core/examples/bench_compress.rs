//! Throughput of same-resolution bpp compression on synthetic 6K frames, the way a clip
//! runs: `workers` threads take runs of frames, each run coded in order with its seeds.
//! `cargo run --release --example bench_compress [-- frames bpp workers]`. `BENCH_GPU=1`
//! uses the GPU. Prints frames per second and a hash of all output (the same either way).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use bluecompress::frame::{encode_frame, forward_star_tetrix, forward_wavelet, Coeffs, Planes};
use bluecompress::requant::{requant_codestream_seeded, RequantParams};
use bluecompress::tico::{Layout, TicoHeader};
use bluecompress::{bpp_of, Target};

include!("common/synthetic_6k.rs");

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let frames: usize = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(64);
    let bpp: f64 = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(1.25);
    let workers: usize = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(16);
    let (w, h) = (6048u16, 3200u16);
    let (hd, l) = header(w, h);
    // Four distinct source frames, reused (coding cost does not depend on repeats).
    let src: Vec<Vec<u8>> = (0..4).map(|i| encode_frame(&hd, &l, &scene(&l, 1 + i as u32), 120).unwrap()).collect();
    println!(
        "source {:.2} bpp, target {bpp} bpp, {frames} frames, {workers} workers",
        bpp_of(src[0].len() as f64, w as u32, h as u32)
    );
    let budget = Target::bpp(bpp).budget(w as u32, h as u32);
    let gpu = std::env::var_os("BENCH_GPU").is_some();
    // BENCH_LOSSLESS=1: the lossless repack alone (the pre-pass every rate controlled frame
    // runs first on the CPU).
    let budget = if std::env::var_os("BENCH_LOSSLESS").is_some() { None } else { budget };
    let p = RequantParams { budget, gpu, ..Default::default() };
    if gpu {
        println!("GPU: {:?}", bluecompress::compress_gpu(bluecompress::Backend::Auto));
    }
    let run = 4;
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<Option<Vec<u8>>>> = Mutex::new(vec![None; frames]);
    let t = Instant::now();
    std::thread::scope(|sc| {
        for _ in 0..workers {
            sc.spawn(|| loop {
                let first = next.fetch_add(run, Ordering::SeqCst);
                if first >= frames {
                    return;
                }
                let mut seed = Vec::new();
                for i in first..frames.min(first + run) {
                    let o = requant_codestream_seeded(&src[i % 4], &p, 1, &mut seed).unwrap().0;
                    out.lock().unwrap()[i] = Some(o);
                }
            });
        }
    });
    let s = t.elapsed().as_secs_f64();
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut bytes = 0;
    for o in out.into_inner().unwrap().into_iter().flatten() {
        bytes += o.len();
        for b in o {
            hash = (hash ^ b as u64).wrapping_mul(0x100_0000_01b3);
        }
    }
    println!(
        "{frames} frames in {s:.2} s: {:.1} fps, out {:.3} bpp, hash {hash:016x}",
        frames as f64 / s,
        bpp_of(bytes as f64 / frames as f64, w as u32, h as u32)
    );
    if gpu {
        println!("GPU frames {:?}", bluecompress::compress_gpu_counts());
    }
}
