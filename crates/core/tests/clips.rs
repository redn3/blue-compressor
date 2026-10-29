//! Checks against real clips. Camera footage is not part of the repository: point
//! `BC_TEST_CLIPS` at a folder with .NEV and/or .R3D files to run these, for example
//!
//!   BC_TEST_CLIPS=~/footage cargo test --release -p bluecompress --test clips -- --nocapture
//!
//! Without the variable the tests pass without doing anything.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use bluecompress::bmff::{InputFile, Movie};
use bluecompress::frame::{decode_frame, Coeffs};
use bluecompress::nraw::NrawFrame;
use bluecompress::tico::{Layout, TicoHeader};
use bluecompress::{Backend, Options, Resolution};

fn clips() -> Vec<PathBuf> {
    let Ok(dir) = std::env::var("BC_TEST_CLIPS") else {
        eprintln!("BC_TEST_CLIPS not set, skipping");
        return Vec::new();
    };
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("BC_TEST_CLIPS is not a folder")
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("nev") || e.eq_ignore_ascii_case("r3d"))
        })
        .collect();
    v.sort();
    v
}

/// Decoded coefficients of frame `n`.
fn coeffs(path: &Path, n: usize) -> Vec<i32> {
    let f = InputFile::open(path).unwrap();
    let m = Movie::parse(&f).unwrap();
    let s = m.video().samples[n];
    let data = f.read(s.offset, s.size as usize).unwrap();
    let nf = NrawFrame::parse(&data).unwrap();
    let cs = &data[nf.codestream_offset..];
    let (h, _) = TicoHeader::parse(cs).unwrap();
    let l = Layout::make(&h, h.width as usize, h.height as usize).unwrap();
    let mut c = vec![0; Coeffs::len(&l)];
    decode_frame(cs, &h, &l, &mut c).unwrap();
    c
}

fn transcode(input: &Path, quality: u8, resolution: Resolution) -> (tempdir::Dir, PathBuf) {
    let dir = tempdir::Dir::new();
    let output = dir.0.join(input.file_name().unwrap());
    let opt = Options {
        input: input.into(),
        output: output.clone(),
        quality,
        resolution,
        backend: Backend::Auto,
        threads: 0,
        bpp: None,
        cbr: false,
    };
    let r = bluecompress::transcode(&opt, &AtomicBool::new(false), &mut |_, _| {}).unwrap();
    eprintln!(
        "{}: Q{quality} {resolution:?} {} -> {} bytes [{}]",
        input.display(),
        r.input_bytes,
        r.output_bytes,
        r.backend
    );
    assert!(r.output_bytes < r.input_bytes);
    (dir, output)
}

#[test]
fn q1_is_lossless() {
    for clip in clips() {
        let (_dir, out) = transcode(&clip, 1, Resolution::Original);
        let frames = bluecompress::probe(&clip).unwrap().frames as usize;
        assert_eq!(bluecompress::probe(&out).unwrap().frames as usize, frames);
        for n in [0, frames / 2, frames - 1] {
            assert!(coeffs(&clip, n) == coeffs(&out, n), "{}: frame {n} differs", clip.display());
        }
    }
}

#[test]
fn lossy_and_resized_outputs_decode() {
    for clip in clips() {
        for (q, res) in [(2, Resolution::Original), (4, Resolution::Original), (2, Resolution::K4)] {
            let (_dir, out) = transcode(&clip, q, res);
            let info = bluecompress::probe(&out).unwrap();
            if res == Resolution::K4 {
                assert_eq!((info.width, info.height), (3852, 2168));
            }
            coeffs(&out, info.frames as usize - 1);
        }
    }
}

/// A temporary folder removed on drop (keeps the test free of extra dependencies).
mod tempdir {
    pub struct Dir(pub std::path::PathBuf);
    impl Dir {
        pub fn new() -> Dir {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "bluecompress-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Dir(p)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
