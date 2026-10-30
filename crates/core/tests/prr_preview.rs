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
        let p = preview_region_for(clip, frame, Target::quality(2), Resolution::Original, Backend::Cpu, [0.0, 0.0, 1.0, 1.0], 960)
            .expect("preview");
        assert_eq!(p.before_rgba.len(), (p.width * p.height * 4) as usize);
        let (b, a) = (mean_rgb(&p.before_rgba), mean_rgb(&p.after_rgba));
        println!("frame {frame}: {}x{} of {} frames, {:.1}s, before {b:.1?} after {a:.1?}, bytes {} -> {}",
            p.width, p.height, p.frames, t.elapsed().as_secs_f64(), p.source_frame_bytes, p.output_frame_bytes);
        assert!(b.iter().sum::<f64>() > 30.0, "black picture");
    }
    // Zoomed-in crop of the cached frame.
    let z = preview_region_for(clip, 83, Target::quality(2), Resolution::Original, Backend::Cpu, [0.4, 0.4, 0.1, 0.1], 600).unwrap();
    println!("crop: {}x{} mean {:.1?}", z.width, z.height, mean_rgb(&z.before_rgba));
    let th = thumbnail(clip, 20, 320).expect("thumbnail");
    println!("thumbnail {}x{} mean {:.1?}", th.width, th.height, mean_rgb(&th.rgba));
    if let Some(nev) = std::env::var_os("BC_PRR_NEV") {
        let n = preview_region_for(Path::new(&nev), 40, Target::quality(2), Resolution::Original, Backend::Cpu, [0.0, 0.0, 1.0, 1.0], 960).unwrap();
        println!("converted NEV frame 40: mean {:.1?}", mean_rgb(&n.before_rgba));
    }
}
