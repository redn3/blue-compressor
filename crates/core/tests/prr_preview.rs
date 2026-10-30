//! Previewer on a ProRes RAW clip. Runs only with BC_PRR_CLIP set to a ProRes RAW .MOV
//! (and optionally BC_PRR_NEV to the same clip converted to N-RAW, to compare the look).
use bluecompress::{preview_region_for, thumbnail, Backend, Resolution, Target};
use std::path::Path;

fn mean_rgb(rgba: &[u8]) -> [f64; 3] {
    let n = (rgba.len() / 4) as f64;
    let mut s = [0f64; 3];
    for p in rgba.chunks(4) {
        for c in 0..3 {
            s[c] += p[c] as f64;
        }
    }
    s.map(|v| v / n)
}

#[test]
fn prores_raw_preview_gives_a_frame() {
    let Some(clip) = std::env::var_os("BC_PRR_CLIP") else { return };
    let clip = Path::new(&clip);
    for frame in [0u32, 40, 83] {
        let t = std::time::Instant::now();
        let p = preview_region_for(
            clip,
            frame,
            Target::quality(2),
            Resolution::Original,
            Backend::Cpu,
            [0.0, 0.0, 1.0, 1.0],
            960,
        )
        .expect("preview");
        assert_eq!(p.before_rgba.len(), (p.width * p.height * 4) as usize);
        let (b, a) = (mean_rgb(&p.before_rgba), mean_rgb(&p.after_rgba));
        println!(
            "frame {frame}: {}x{} of {} frames, {:.1}s, before {b:.1?} after {a:.1?}, bytes {} -> {}",
            p.width,
            p.height,
            p.frames,
            t.elapsed().as_secs_f64(),
            p.source_frame_bytes,
            p.output_frame_bytes
        );
        assert!(b.iter().sum::<f64>() > 30.0, "black picture");
    }
    // Zoomed-in crop of the cached frame.
    let z =
        preview_region_for(clip, 83, Target::quality(2), Resolution::Original, Backend::Cpu, [0.4, 0.4, 0.1, 0.1], 600)
            .unwrap();
    println!("crop: {}x{} mean {:.1?}", z.width, z.height, mean_rgb(&z.before_rgba));
    let th = thumbnail(clip, 20, 320).expect("thumbnail");
    println!("thumbnail {}x{} mean {:.1?}", th.width, th.height, mean_rgb(&th.rgba));
    if let Some(nev) = std::env::var_os("BC_PRR_NEV") {
        let n = preview_region_for(
            Path::new(&nev),
            40,
            Target::quality(2),
            Resolution::Original,
            Backend::Cpu,
            [0.0, 0.0, 1.0, 1.0],
            960,
        )
        .unwrap();
        println!("converted NEV frame 40: mean {:.1?}", mean_rgb(&n.before_rgba));
    }
}

/// Compress preview vs Develop render of the same frame at defaults (as shot, Rec.709 view).
/// Runs with BC_LOOK_CLIPS set to clip paths separated by '|'.
#[test]
fn compress_preview_matches_develop() {
    let Some(list) = std::env::var_os("BC_LOOK_CLIPS") else { return };
    for clip in list.to_string_lossy().split('|') {
        let clip = Path::new(clip);
        let info = bluecompress::develop::clip_info(clip).expect("clip info");
        let frame = (info.frames / 2) as usize;
        let p = preview_region_for(
            clip,
            frame as u32,
            Target::quality(1),
            Resolution::Original,
            Backend::Cpu,
            [0.0, 0.0, 1.0, 1.0],
            960,
        )
        .unwrap();
        let params = bluecompress::develop::Params::as_shot(&info);
        let d = bluecompress::develop::preview(
            clip,
            frame,
            &params,
            [0.0, 0.0, 1.0, 1.0],
            p.width as usize,
            p.height as usize,
            true,
        )
        .unwrap();
        let (a, b) = (mean_rgb(&p.before_rgba), mean_rgb(&d));
        println!(
            "{}: compress {a:.1?} develop {b:.1?} (kelvin {}, iso {}, rmd {:?})",
            clip.file_name().unwrap().to_string_lossy(),
            info.kelvin,
            info.iso,
            info.sidecar.as_ref().and_then(|s| s.exposure)
        );
        for c in 0..3 {
            assert!((a[c] - b[c]).abs() < 4.0, "looks differ");
        }
    }
}

/// Develop export of a ProRes RAW clip. Runs with BC_PRR_CLIP and BC_EXPORT_DIR set.
#[test]
fn prores_raw_develop_export() {
    let (Some(clip), Some(dir)) = (std::env::var_os("BC_PRR_CLIP"), std::env::var_os("BC_EXPORT_DIR")) else { return };
    use bluecompress::develop::{export, Export, Output, Params};
    let clip = Path::new(&clip).to_path_buf();
    let info = bluecompress::develop::clip_info(&clip).unwrap();
    for out in [Output::NLog, Output::Log3G10, Output::Rec709] {
        let mut params = Params::as_shot(&info);
        params.output = out;
        let output = Path::new(&dir).join(format!("prr_{out:?}.mov"));
        let e = Export { input: clip.clone(), output, params, width: 1920, kbps: 20_000, cbr: false, encoder: None };
        let t = std::time::Instant::now();
        let d = export(&e, &std::sync::atomic::AtomicBool::new(false), &mut |_, _| {}).expect("export");
        println!(
            "{out:?}: {}x{} {} frames, {} via {}, {:.1}s, {} bytes",
            d.width,
            d.height,
            d.frames,
            d.encoder,
            d.renderer,
            t.elapsed().as_secs_f64(),
            d.output_bytes
        );
        assert_eq!(d.frames, info.frames);
    }
}
