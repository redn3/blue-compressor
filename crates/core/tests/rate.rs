//! Bits per pixel rate control on synthetic frames (camera footage is not part of the
//! repository; see tests/clips.rs for checks on real clips).

use bluecompress::frame::{
    decode_frame, encode_frame, encode_frame_budget, forward_star_tetrix, forward_wavelet, Coeffs, Planes,
};
use bluecompress::requant::{requant_codestream, RequantParams};
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
                pl.data[c * l.cw * l.ch + y * l.cw + x] = (v + 260.0 * noise(x, y, seed + c as u32)) as i32;
            }
        }
    }
    forward_star_tetrix(&mut pl);
    let mut coeffs = vec![0i32; Coeffs::len(l)];
    forward_wavelet(l, &pl, &mut coeffs);
    coeffs
}

fn decoded(cs: &[u8]) -> Vec<i32> {
    let (h, _) = TicoHeader::parse(cs).unwrap();
    let l = Layout::make(&h, h.width as usize, h.height as usize).unwrap();
    let mut c = vec![0; Coeffs::len(&l)];
    decode_frame(cs, &h, &l, &mut c).unwrap();
    c
}

fn mse(a: &[i32], b: &[i32]) -> f64 {
    a.iter().zip(b).map(|(&x, &y)| ((x - y) as f64).powi(2)).sum::<f64>() / a.len() as f64
}

const W: u16 = 2048;
const H: u16 = 512;

/// A "camera" codestream (fine rank, like the Z8's) and its bpp.
fn source(seed: u32) -> (Vec<u8>, f64) {
    let (h, l) = header(W, H);
    let cs = encode_frame(&h, &l, &scene(&l, seed), 120).unwrap();
    let bpp = bpp_of(cs.len() as f64, W as u32, H as u32);
    (cs, bpp)
}

fn requant_bpp(cs: &[u8], bpp: f64) -> Vec<u8> {
    let budget = Target::bpp(bpp).budget(W as u32, H as u32);
    requant_codestream(cs, &RequantParams { budget, ..Default::default() }, 0).unwrap().0
}

#[test]
fn requant_hits_the_target() {
    for seed in [1, 2] {
        let (cs, src_bpp) = source(seed);
        eprintln!("source {src_bpp:.3} bpp");
        // Never finer than the camera, so the lossless repack is as large as it gets.
        let repack = requant_codestream(&cs, &RequantParams::default(), 0).unwrap().0;
        let repack_bpp = bpp_of(repack.len() as f64, W as u32, H as u32);
        eprintln!("source {src_bpp:.3} bpp, lossless repack {repack_bpp:.3} bpp");
        let mut last_err = f64::MAX;
        for frac in [0.3, 0.5, 0.7, 0.8] {
            let target = (repack_bpp * frac * 10.0).round() / 10.0;
            let out = requant_bpp(&cs, target);
            let got = bpp_of(out.len() as f64, W as u32, H as u32);
            let err = mse(&decoded(&cs), &decoded(&out));
            eprintln!("target {target:.1} bpp: {got:.4} bpp ({:+.2}%), mse {err:.1}", (got / target - 1.0) * 100.0);
            assert!(got <= target, "over budget: {got} > {target}");
            assert!(got >= target * 0.98, "too far under: {got} < {target}");
            assert!(err < last_err, "more bytes should mean less error");
            last_err = err;
        }
    }
}

#[test]
fn target_at_or_above_source_is_the_lossless_repack() {
    let (cs, src_bpp) = source(3);
    let lossless = requant_codestream(&cs, &RequantParams::default(), 0).unwrap().0;
    let repack_bpp = bpp_of(lossless.len() as f64, W as u32, H as u32);
    for target in [repack_bpp + 0.05, src_bpp, src_bpp + 1.0, 8.0] {
        assert_eq!(requant_bpp(&cs, target), lossless);
    }
    assert_eq!(decoded(&lossless), decoded(&cs));
}

#[test]
fn rate_control_is_deterministic() {
    let (cs, src_bpp) = source(4);
    let t = (src_bpp * 0.6 * 10.0).round() / 10.0;
    let a = requant_bpp(&cs, t);
    for threads in [1, 3, 0] {
        let budget = Target::bpp(t).budget(W as u32, H as u32);
        let b = requant_codestream(&cs, &RequantParams { budget, ..Default::default() }, threads).unwrap().0;
        assert_eq!(a, b);
    }
    let (h, l) = header(W, H);
    let c = scene(&l, 4);
    let budget = Target::bpp(1.0).budget(W as u32, H as u32).unwrap();
    assert_eq!(
        encode_frame_budget(&h, &l, &c, 125, budget).unwrap(),
        encode_frame_budget(&h, &l, &c, 125, budget).unwrap()
    );
}

#[test]
fn never_finer_than_the_camera() {
    // A source coded coarse (rank 160): a generous budget keeps every precinct as it is.
    let (h, l) = header(W, H);
    let coarse = encode_frame(&h, &l, &scene(&l, 5), 160).unwrap();
    let got = bpp_of(coarse.len() as f64, W as u32, H as u32);
    let out = requant_bpp(&coarse, got + 0.3);
    assert_eq!(decoded(&out), decoded(&coarse));
}

#[test]
fn resize_encoder_hits_the_target() {
    let (h, l) = header(W, H);
    let c = scene(&l, 6);
    let q1 = encode_frame(&h, &l, &c, 125).unwrap();
    let q1_bpp = bpp_of(q1.len() as f64, W as u32, H as u32);
    for frac in [0.4, 0.7] {
        let target = (q1_bpp * frac * 10.0).round() / 10.0;
        let budget = Target::bpp(target).budget(W as u32, H as u32).unwrap();
        let out = encode_frame_budget(&h, &l, &c, 125, budget).unwrap();
        let got = bpp_of(out.len() as f64, W as u32, H as u32);
        eprintln!("resize encoder: target {target:.1}, got {got:.4} ({:+.2}%)", (got / target - 1.0) * 100.0);
        assert!(got <= target && got >= target * 0.98);
        decoded(&out);
    }
    // Above what the finest resize rank needs: the same as the Q1 encode.
    let budget = Target::bpp(q1_bpp + 0.5).budget(W as u32, H as u32).unwrap();
    assert_eq!(encode_frame_budget(&h, &l, &c, 125, budget).unwrap(), q1);
}

#[test]
fn bpp_targets_round_and_clamp() {
    assert_eq!(Target::bpp(1.24), Target::Bpp(12));
    assert_eq!(Target::bpp(0.1), Target::Bpp(5));
    assert_eq!(Target::bpp(12.0), Target::Bpp(80));
    assert_eq!(Target::bpp(3.0).budget(6064, 4040), Some(9_186_960));
    assert_eq!(bluecompress::bpp_tag(1.2), "1.2bpp");
    assert_eq!(bluecompress::bpp_tag(3.0), "3bpp");
    assert_eq!(bluecompress::variant_name_for(Target::bpp(1.2), "4k"), "1.2bpp 4K");
}

#[test]
fn legacy_steps_map_to_fractions_of_the_source() {
    use bluecompress::legacy_bpp;
    assert_eq!(legacy_bpp(1, 2.9), None);
    assert_eq!(legacy_bpp(2, 2.9), Some(1.2));
    assert_eq!(legacy_bpp(3, 2.9), Some(1.0));
    assert_eq!(legacy_bpp(4, 2.9), Some(0.7));
    assert_eq!(legacy_bpp(4, 1.0), Some(0.5), "never below the 0.5 bpp floor");
    assert_eq!(Target::quality(1).resolve(2.9), Target::lossless());
    assert_eq!(Target::quality(3).resolve(2.9), Target::bpp(1.0));
    // A bpp at or above the source is the lossless repack.
    assert_eq!(Target::bpp(3.0).resolve(2.9), Target::lossless());
    assert_eq!(Target::bpp(2.8).resolve(2.9), Target::bpp(2.8));
    // Request fields: lossless wins, then bpp, then the legacy step (Light by default).
    assert_eq!(Target::from_fields(Some(3), Some(1.0), true), Target::lossless());
    assert_eq!(Target::from_fields(Some(3), Some(1.0), false), Target::bpp(1.0));
    assert_eq!(Target::from_fields(Some(4), None, false), Target::quality(4));
    assert_eq!(Target::from_fields(None, None, false), Target::quality(2));
    assert_eq!(bluecompress::variant_name_for(Target::lossless(), "original"), "Lossless");
    assert_eq!(bluecompress::variant_name_for(Target::bpp(1.2), "original"), "1.2bpp");
}

#[test]
fn seeded_frames_are_accurate_and_deterministic() {
    use bluecompress::frame::encode_frame_budget_seeded;
    use bluecompress::requant::requant_codestream_seeded;
    let frames: Vec<_> = (10..14).map(source).collect();
    let src_bpp = frames[0].1;
    let t = (src_bpp * 0.6 * 10.0).round() / 10.0;
    let budget = Target::bpp(t).budget(W as u32, H as u32);
    let run = |threads| {
        let mut seed = Vec::new();
        frames
            .iter()
            .map(|(cs, _)| {
                let prm = RequantParams { budget, ..Default::default() };
                requant_codestream_seeded(cs, &prm, threads, &mut seed).unwrap().0
            })
            .collect::<Vec<_>>()
    };
    let a = run(1);
    assert_eq!(a, run(0), "the seeded run does not depend on the thread count");
    for out in &a {
        let got = bpp_of(out.len() as f64, W as u32, H as u32);
        assert!(got <= t && got >= t * 0.97, "seeded frame at {got} for {t}");
        decoded(out);
    }
    let (h, l) = header(W, H);
    let budget = Target::bpp(1.0).budget(W as u32, H as u32).unwrap();
    let mut seed = Vec::new();
    for s in 20..23 {
        let c = scene(&l, s);
        let out = encode_frame_budget_seeded(&h, &l, &c, 125, budget, &mut seed).unwrap();
        let got = bpp_of(out.len() as f64, W as u32, H as u32);
        assert!((0.97..=1.0).contains(&got), "seeded encode at {got}");
        assert!(!seed.is_empty());
    }
}

#[test]
fn calibration_codes_the_old_ranks_and_the_picks() {
    use bluecompress::calibrate::calibrate_codestreams;
    let frames: Vec<_> = (30..34).map(source).collect();
    let src_bpp = frames.iter().map(|f| f.1).sum::<f64>() / frames.len() as f64;
    let pad = |n: usize| n.div_ceil(1024) as u64 * 1024;
    let items = frames.iter().enumerate().map(|(i, (cs, _))| Ok((cs.clone(), i == 0)));
    let (source, camera_rank, v) = calibrate_codestreams(items, W as u32, H as u32, src_bpp, 0).unwrap();
    assert_eq!(source, frames.iter().map(|f| f.0.len() as u64).sum::<u64>());
    assert!((camera_rank - 120.0).abs() < 1e-9, "camera rank {camera_rank}");
    let fixed = |r: Option<i32>| -> u64 {
        frames
            .iter()
            .map(|(cs, _)| {
                pad(requant_codestream(cs, &RequantParams { target_rank: r, ..Default::default() }, 0).unwrap().0.len())
            })
            .sum()
    };
    assert_eq!(v[0].bytes, fixed(None));
    assert_eq!(v[1].bytes, fixed(Some(135)));
    assert_eq!(v[2].bytes, fixed(Some(140)));
    assert_eq!(v[3].bytes, fixed(Some(160)));
    assert!(v[1].bytes > v[2].bytes && v[2].bytes > v[3].bytes);
    for p in &v[4..] {
        let t = p.bpp_target.expect("a pick below the source is a bpp target");
        let got = bpp_of(p.bytes as f64 / frames.len() as f64, W as u32, H as u32);
        assert!(got <= t * 1.001, "{}: {got} over {t}", p.label);
    }
    assert!(v[4].bytes > v[5].bytes && v[5].bytes > v[6].bytes);
}
