//! Proxy encoding through a real ffmpeg (skipped when none is installed): synthetic linear
//! frames in, a 10-bit HEVC Main10 Rec.709 .mov out.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;

use bluecompress::proxy::{available_encoders, encode, find_ffmpeg, make_proxy, Encode, Grade, ProxyOptions};

fn ffprobe(ffmpeg: &Path, file: &Path) -> String {
    let probe = ffmpeg.with_file_name(if cfg!(windows) { "ffprobe.exe" } else { "ffprobe" });
    let out = Command::new(probe)
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries"])
        .arg("stream=codec_name,profile,pix_fmt,width,height,color_primaries,color_transfer,color_space,nb_frames")
        .args(["-of", "default=nw=1"])
        .arg(file)
        .output()
        .expect("ffprobe");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn frames(w: usize, h: usize) -> impl Fn(std::ops::Range<usize>) -> bluecompress::Result<Vec<Vec<[f32; 3]>>> + Sync {
    move |r| {
        Ok(r.map(|i| {
            (0..w * h)
                .map(|p| {
                    let x = (p % w) as f32 / w as f32;
                    [x * 0.5, 0.2 + 0.01 * i as f32, (1.0 - x) * 0.3]
                })
                .collect()
        })
        .collect())
    }
}

#[test]
fn encodes_main10_rec709() {
    let Some(ff) = find_ffmpeg() else {
        eprintln!("no ffmpeg, skipped");
        return;
    };
    let encoders = available_encoders(&ff);
    assert!(encoders.iter().any(|e| e == "libx265"), "libx265 fallback missing: {encoders:?}");
    let dir = std::env::temp_dir().join(format!("bc_proxy_{}", std::process::id()));
    let out = dir.join("Proxies").join("C0001.mov");
    let (w, h, n) = (320usize, 180usize, 12usize);
    let job = Encode {
        ffmpeg: &ff,
        encoder: "libx265",
        width: w,
        height: h,
        fps: 23.976,
        frames: n,
        audio: None,
        output: &out,
        cbr_kbps: None,
    };
    let grade = Grade { gains: [1.0; 3], exposure: 1.0 };
    let mut last = 0;
    encode(
        &job,
        &grade,
        &AtomicBool::new(false),
        &mut |d, t| {
            assert_eq!(t, n);
            last = d;
        },
        &frames(w, h),
    )
    .unwrap();
    assert_eq!(last, n);
    let info = ffprobe(&ff, &out);
    for want in [
        "codec_name=hevc",
        "profile=Main 10",
        "pix_fmt=yuv420p10le",
        "width=320",
        "height=180",
        "color_primaries=bt709",
        "color_transfer=bt709",
        "color_space=bt709",
        "nb_frames=12",
    ] {
        assert!(info.contains(want), "{want} missing in:\n{info}");
    }
    assert!(!dir.join("Proxies").join("C0001.mov.bluecompress.tmp").exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn cancel_leaves_nothing() {
    let Some(ff) = find_ffmpeg() else { return };
    let dir = std::env::temp_dir().join(format!("bc_proxy_cancel_{}", std::process::id()));
    let out = dir.join("x.mov");
    let job = Encode {
        ffmpeg: &ff,
        encoder: "libx265",
        width: 64,
        height: 36,
        fps: 25.0,
        frames: 50,
        audio: None,
        output: &out,
        cbr_kbps: None,
    };
    let r = encode(
        &job,
        &Grade { gains: [1.0; 3], exposure: 1.0 },
        &AtomicBool::new(true),
        &mut |_, _| {},
        &frames(64, 36),
    );
    assert!(r.is_err());
    assert!(!out.exists());
    assert!(!dir.join("x.mov.bluecompress.tmp").exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn rejects_non_clip() {
    let Some(ff) = find_ffmpeg() else { return };
    let dir = std::env::temp_dir().join(format!("bc_proxy_bad_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bad = dir.join("bad.NEV");
    std::fs::write(&bad, b"not a clip").unwrap();
    let mut o = ProxyOptions::new(&bad, &dir.join("Proxies/bad.mov"));
    o.ffmpeg = Some(ff);
    o.encoder = Some("libx265".into());
    assert!(make_proxy(&o, &AtomicBool::new(false), &mut |_, _| {}).is_err());
    assert!(!dir.join("Proxies/bad.mov").exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn cbr_rate_scales_with_size() {
    assert_eq!(bluecompress::proxy::cbr_kbps(1920, 1080, 25.0), 20_736);
    assert_eq!(bluecompress::proxy::cbr_kbps(64, 32, 25.0), 2_000);
    assert_eq!(bluecompress::proxy::cbr_kbps(1920, 1080, 120.0), 80_000);
}

#[test]
fn encodes_constant_bitrate() {
    let Some(ff) = find_ffmpeg() else {
        eprintln!("no ffmpeg, skipped");
        return;
    };
    let dir = std::env::temp_dir().join(format!("bc_proxy_cbr_{}", std::process::id()));
    let out = dir.join("C0002.mov");
    let (w, h, n) = (320usize, 180usize, 50usize);
    let kbps = 3_000;
    let job = Encode {
        ffmpeg: &ff,
        encoder: "libx265",
        width: w,
        height: h,
        fps: 25.0,
        frames: n,
        audio: None,
        output: &out,
        cbr_kbps: Some(kbps),
    };
    let grade = Grade { gains: [1.0; 3], exposure: 1.0 };
    encode(&job, &grade, &AtomicBool::new(false), &mut |_, _| {}, &frames(w, h)).unwrap();
    // A nearly flat test picture would code to almost nothing with VBR; strict CBR fills
    // it up to the asked rate.
    let size = std::fs::metadata(&out).unwrap().len() as f64;
    let rate = size * 8.0 / (n as f64 / 25.0) / 1000.0;
    assert!(rate > kbps as f64 * 0.7 && rate < kbps as f64 * 1.3, "{rate:.0} kb/s for {kbps}");
    let _ = std::fs::remove_dir_all(dir);
}
