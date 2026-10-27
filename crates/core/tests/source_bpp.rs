//! Source bits per pixel counts the TICO codestream bytes of the video frames only: not
//! the container (ftyp, moov, free, udta metadata), not the audio samples in mdat and not
//! the NRAW header in front of each frame's codestream.

use std::path::{Path, PathBuf};

use bluecompress::bmff::{InputFile, Movie};
use bluecompress::nraw::{codestream_bytes, sample_header_len};
use bluecompress::quality::source_bpp;

const W: u16 = 64;
const H: u16 = 32;

fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
    v.extend_from_slice(kind);
    v.extend_from_slice(payload);
    v
}

/// An NRAW sample: NRFM, an NRFH of `meta` extra bytes, NRTH, then `cs_len` bytes of
/// "codestream" (SOC, filler, EOC; only its length matters here).
fn nraw_sample(meta: usize, cs_len: usize) -> Vec<u8> {
    let mut s = vec![0, 0, 0, 0, b'N', b'R', b'A', b'W'];
    s.extend(bx(b"NRFM", b"0100"));
    let mut nrfh = bx(b"NRHM", b"0100");
    nrfh.extend(bx(b"NRMT", &vec![0x11; meta]));
    s.extend(bx(b"NRFH", &nrfh));
    let mut nrti = b"0100".to_vec();
    nrti.extend((cs_len as u32).to_be_bytes());
    let mut nrth = bx(b"NRTI", &nrti);
    nrth.extend(bx(b"NRTA", &[0; 4]));
    s.extend(bx(b"NRTH", &nrth));
    let mut cs = vec![0x5a; cs_len];
    cs[..2].copy_from_slice(&[0xff, 0x10]);
    cs[cs_len - 2..].copy_from_slice(&[0xff, 0x11]);
    s.extend(cs);
    let n = s.len() as u32;
    s[..4].copy_from_slice(&n.to_be_bytes());
    s
}

/// A track with one sample per chunk at `offsets`.
fn trak(handler: &[u8; 4], codec: &[u8; 4], sizes: &[usize], offsets: &[u64]) -> Vec<u8> {
    let tkhd = bx(b"tkhd", &[0; 84]);
    let mut mdhd = vec![0u8; 24];
    mdhd[12..16].copy_from_slice(&25u32.to_be_bytes());
    let mut hdlr = vec![0u8; 24];
    hdlr[8..12].copy_from_slice(handler);
    let mut entry = vec![0u8; 86];
    entry[..4].copy_from_slice(&86u32.to_be_bytes());
    entry[4..8].copy_from_slice(codec);
    entry[32..34].copy_from_slice(&W.to_be_bytes());
    entry[34..36].copy_from_slice(&H.to_be_bytes());
    let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
    stsd.extend(entry);
    let n = sizes.len() as u32;
    let mut stts = vec![0, 0, 0, 0, 0, 0, 0, 1];
    stts.extend(n.to_be_bytes());
    stts.extend(1u32.to_be_bytes());
    let mut stsc = vec![0, 0, 0, 0, 0, 0, 0, 1];
    stsc.extend(1u32.to_be_bytes());
    stsc.extend(1u32.to_be_bytes());
    stsc.extend(1u32.to_be_bytes());
    let mut stsz = vec![0u8; 8];
    stsz.extend(n.to_be_bytes());
    for &s in sizes {
        stsz.extend((s as u32).to_be_bytes());
    }
    let mut co64 = vec![0u8; 4];
    co64.extend(n.to_be_bytes());
    for &o in offsets {
        co64.extend(o.to_be_bytes());
    }
    let mut stbl = bx(b"stsd", &stsd);
    stbl.extend(bx(b"stts", &stts));
    stbl.extend(bx(b"stsc", &stsc));
    stbl.extend(bx(b"stsz", &stsz));
    stbl.extend(bx(b"co64", &co64));
    let mut mdia = bx(b"mdhd", &mdhd);
    mdia.extend(bx(b"hdlr", &hdlr));
    mdia.extend(bx(b"minf", &bx(b"stbl", &stbl)));
    let mut t = tkhd;
    t.extend(bx(b"mdia", &mdia));
    bx(b"trak", &t)
}

/// Writes a clip whose frames are `frames` (interleaved with audio chunks), with camera
/// metadata in udta and a free box, and returns its path.
fn clip(name: &str, frames: &[Vec<u8>], codec: &[u8; 4]) -> PathBuf {
    let audio: Vec<Vec<u8>> = frames.iter().enumerate().map(|(i, _)| vec![i as u8; 3840]).collect();
    let ftyp = bx(b"ftyp", b"niko\0\0\0\0niko");
    let free = bx(b"free", &[0; 2000]);
    let udta = bx(b"udta", &bx(b"NCDT", &bx(b"NCTG", &[0; 64])));
    let moov_for = |vo: &[u64], ao: &[u64]| {
        let mut m = trak(b"vide", codec, &frames.iter().map(|f| f.len()).collect::<Vec<_>>(), vo);
        m.extend(trak(b"soun", b"lpcm", &audio.iter().map(|a| a.len()).collect::<Vec<_>>(), ao));
        m.extend(&udta);
        bx(b"moov", &m)
    };
    let zeros = vec![0u64; frames.len()];
    let start = (ftyp.len() + moov_for(&zeros, &zeros).len() + free.len() + 8) as u64;
    let (mut vo, mut ao, mut mdat, mut at) = (Vec::new(), Vec::new(), Vec::new(), start);
    for (f, a) in frames.iter().zip(&audio) {
        vo.push(at);
        mdat.extend(f);
        at += f.len() as u64;
        ao.push(at);
        mdat.extend(a);
        at += a.len() as u64;
    }
    let mut file = ftyp;
    file.extend(moov_for(&vo, &ao));
    file.extend(free);
    file.extend(bx(b"mdat", &mdat));
    let dir = std::env::temp_dir().join(format!("bc_source_bpp_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, file).unwrap();
    p
}

fn open(p: &Path) -> (InputFile, Movie) {
    let f = InputFile::open(p).unwrap();
    let m = Movie::parse(&f).unwrap();
    (f, m)
}

const LENS: [usize; 10] = [1024, 2048, 3072, 2048, 1024, 4096, 2048, 3072, 1024, 5120];

fn expected_bpp(lens: &[usize]) -> f64 {
    8.0 * lens.iter().sum::<usize>() as f64 / (lens.len() as f64 * W as f64 * H as f64)
}

#[test]
fn counts_codestream_bytes_only() {
    for (name, codec) in [("A.NEV", b"NRAW"), ("A.R3D", b"NR3D")] {
        let frames: Vec<_> = LENS.iter().map(|&l| nraw_sample(700, l)).collect();
        let p = clip(name, &frames, codec);
        let (f, m) = open(&p);
        assert_eq!(codestream_bytes(&f, &m).unwrap(), LENS.iter().sum::<usize>() as u64);
        assert_eq!(source_bpp(&f, &m), expected_bpp(&LENS));
        let info = bluecompress::probe(&p).unwrap();
        assert_eq!(info.codestream_bytes, LENS.iter().sum::<usize>() as u64);
        assert_eq!(info.bpp, expected_bpp(&LENS));
        // The old measure counted the frame headers too, and the file holds a lot more.
        assert!(info.video_bytes > info.codestream_bytes);
        assert!(info.file_bytes > info.video_bytes + 10 * 3840);
        let header = sample_header_len(&f, m.video().samples[0]).unwrap();
        assert_eq!(info.video_bytes - info.codestream_bytes, header * LENS.len() as u64);
        // The C API gives the same figure.
        let ci = bluecompress::ffi::bc_clip_info {
            width: info.width,
            height: info.height,
            frames: info.frames,
            fps: info.fps,
            file_bytes: info.file_bytes,
            video_bytes: info.video_bytes,
            codec: [0; 8],
            audio_tracks: 1,
            codestream_bytes: info.codestream_bytes,
        };
        assert_eq!(unsafe { bluecompress::ffi::bc_clip_bpp(&ci) }, info.bpp);
    }
}

#[test]
fn headers_of_different_lengths_are_read_frame_by_frame() {
    // Odd frames carry more metadata: the probed frames disagree, so every header is read.
    let lens: Vec<usize> = (0..23).map(|i| 1024 * (1 + i % 4)).collect();
    let frames: Vec<_> = lens.iter().enumerate().map(|(i, &l)| nraw_sample(300 + 200 * (i % 2), l)).collect();
    let p = clip("B.NEV", &frames, b"NRAW");
    let (f, m) = open(&p);
    assert_eq!(codestream_bytes(&f, &m).unwrap(), lens.iter().sum::<usize>() as u64);
    assert_eq!(bluecompress::probe(&p).unwrap().bpp, expected_bpp(&lens));
}

#[test]
fn unreadable_frame_headers_fall_back_to_sample_sizes() {
    let mut frames: Vec<_> = LENS.iter().map(|&l| nraw_sample(700, l)).collect();
    frames[0][4..8].copy_from_slice(b"XXXX");
    let p = clip("C.NEV", &frames, b"NRAW");
    let info = bluecompress::probe(&p).unwrap();
    assert_eq!(info.codestream_bytes, info.video_bytes);
    let n = W as f64 * H as f64 * LENS.len() as f64;
    assert_eq!(info.bpp, 8.0 * info.video_bytes as f64 / n);
}
