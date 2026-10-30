//! N-RAW to R3D NE conversion on synthetic ZR clips: the output reads back through our own
//! parser as NR3D with the R3D NE tags, and every other byte is the source's.

use std::path::Path;
use std::sync::atomic::AtomicBool;

use bluecompress::bmff::{InputFile, Movie};
use bluecompress::r3d::{self, nctg_tags, tag_value, Direction, R3dOptions};

const NEV_MATRIX: [u8; 37] = [
    0x00, 0x3f, 0x1b, 0x09, 0x0e, 0x3e, 0xc3, 0x35, 0xe2, 0xbd, 0x15, 0x2e, 0xa6, 0x3e, 0x69, 0xb7, 0x6a, 0x3f, 0x86,
    0x2f, 0xfe, 0xbe, 0x8d, 0x9b, 0xac, 0x3b, 0xcd, 0x14, 0x18, 0xbe, 0x31, 0x39, 0xd2, 0x3f, 0xa0, 0xc0, 0x65,
];

fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut b = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
    b.extend_from_slice(kind);
    b.extend_from_slice(payload);
    b
}

fn tag(id: u32, ty: u16, value: &[u8]) -> Vec<u8> {
    let unit = match ty {
        3 | 8 => 2,
        4 | 9 => 4,
        5 | 10 => 8,
        _ => 1,
    };
    let mut b = id.to_be_bytes().to_vec();
    b.extend_from_slice(&ty.to_be_bytes());
    b.extend_from_slice(&((value.len() / unit) as u16).to_be_bytes());
    b.extend_from_slice(value);
    b
}

fn nrmt(key: u32, value: &[u8]) -> Vec<u8> {
    let mut p = key.to_be_bytes().to_vec();
    p.extend_from_slice(value);
    bx(b"NRMT", &p)
}

/// A small ZR style N-RAW clip with the tags the converter rewrites. `quality` is NCTG 0x24.
fn synthetic_nev(path: &Path, quality: u16, model: &str, frames: usize) -> Vec<Vec<u8>> {
    synthetic(path, quality, model, frames, true)
}

/// `full`: every tag a ZR writes; otherwise without the R3D NE specific ones (like clips
/// from cameras that never record R3D NE).
fn synthetic(path: &Path, quality: u16, model: &str, frames: usize, full: bool) -> Vec<Vec<u8>> {
    let mut codestreams = Vec::new();
    let mut samples = Vec::new();
    for f in 0..frames {
        let mut cs = vec![0xff, 0x10];
        cs.extend((0..2044).map(|i| ((i * 7 + f * 13) % 251) as u8));
        cs.extend([0xff, 0x11]); // EOC
        let mut nrfh = bx(b"NRHM", b"0100");
        nrfh.extend(nrmt(0x0110_0100, &[0, 0, 0, 0x17, 0xac]));
        nrfh.extend(nrmt(0x0110_0101, &[0, 0, 0, 0x0d, 0x52]));
        nrfh.extend(nrmt(0x0190_0011, &[0x00, 0x0f, 0xc1, 0x12, 0xe1, 0xf0, 0xce]));
        nrfh.extend(nrmt(0x0190_0012, &NEV_MATRIX));
        if full {
            nrfh.extend(nrmt(0x0200_523d, &[0u8; 65]));
        }
        let mut nrti = b"0100".to_vec();
        nrti.extend((cs.len() as u32).to_be_bytes());
        let mut s = bx(b"NRFM", b"0100");
        s.extend(bx(b"NRFH", &nrfh));
        // NRTH ends with a zero NRTA so the codestream starts at 1024, like the camera.
        let used = 8 + s.len() + 8 + 8 + nrti.len() + 8;
        let mut nrth = bx(b"NRTI", &nrti);
        nrth.extend(bx(b"NRTA", &vec![0u8; 1024 - used]));
        s.extend(bx(b"NRTH", &nrth));
        let mut sample = bx(b"NRAW", &s);
        assert_eq!(sample.len(), 1024);
        sample.extend_from_slice(&cs);
        let n = sample.len() as u32;
        sample[..4].copy_from_slice(&n.to_be_bytes());
        samples.push(sample);
        codestreams.push(cs);
    }

    let mut gamma = b"03110310".to_vec();
    for _ in 0..2 {
        let mut slot = b"N-LOG".to_vec();
        slot.resize(20, 0);
        gamma.extend(slot);
    }
    gamma.extend([0x80, 0x01]);
    gamma.resize(108, 0);
    let mut model_v = model.as_bytes().to_vec();
    model_v.resize(16, 0);
    let mut nctg = Vec::new();
    nctg.extend(tag(0x1, 2, b"NIKON CORPORATION\0"));
    nctg.extend(tag(0x2, 2, &model_v));
    nctg.extend(tag(0x13, 4, &[0, 0, 0, frames as u8, 0, 0, 0, 0]));
    nctg.extend(tag(0x21, 3, &49u16.to_be_bytes()));
    nctg.extend(tag(0x24, 3, &quality.to_be_bytes()));
    nctg.extend(tag(0x26, 3, &2u16.to_be_bytes()));
    nctg.extend(tag(0x27, 3, &1u16.to_be_bytes()));
    nctg.extend(tag(0x101e, 7, &[0x5a; 36]));
    if full {
        nctg.extend(tag(0x1020, 7, &[0; 12]));
        nctg.extend(tag(0x523d, 7, &[0; 256]));
    }
    nctg.extend(tag(0x0110_8832, 4, &800u32.to_be_bytes()));
    if full {
        nctg.extend(tag(0x0200_0023, 7, &gamma));
        nctg.extend(tag(0x0200_00c0, 7, &[0x60, 0x01, 0x0c, 0x00, 0xa8, 0x01, 0x0c, 0x00]));
    }
    let mut ncdt = bx(b"NCHD", b"Nikon\0\x02\0\0\0");
    ncdt.extend(bx(b"NCTG", &nctg));
    let udta = bx(b"udta", &bx(b"NCDT", &ncdt));

    let mut tkhd = vec![0u8; 84];
    tkhd[76..80].copy_from_slice(&(6060u32 << 16).to_be_bytes());
    let mut mdhd = vec![0u8; 24];
    mdhd[12..16].copy_from_slice(&25000u32.to_be_bytes());
    let mut hdlr = vec![0u8; 24];
    hdlr[8..12].copy_from_slice(b"vide");
    let mut entry = vec![0u8; 86];
    entry[..4].copy_from_slice(&86u32.to_be_bytes());
    entry[4..8].copy_from_slice(b"NRAW");
    entry[32..34].copy_from_slice(&6060u16.to_be_bytes());
    entry[34..36].copy_from_slice(&3410u16.to_be_bytes());
    let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
    stsd.extend(entry);
    let mut stts = vec![0, 0, 0, 0, 0, 0, 0, 1];
    stts.extend((frames as u32).to_be_bytes());
    stts.extend(1000u32.to_be_bytes());
    let mut stsc = vec![0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1];
    stsc.truncate(20);
    let mut stsz = vec![0, 0, 0, 0];
    stsz.extend((samples[0].len() as u32).to_be_bytes());
    stsz.extend((frames as u32).to_be_bytes());
    let moov_for = |mdat_at: u64| {
        let mut co64 = vec![0, 0, 0, 0];
        co64.extend((frames as u32).to_be_bytes());
        for i in 0..frames {
            co64.extend((mdat_at + (i * samples[0].len()) as u64).to_be_bytes());
        }
        let mut stbl = bx(b"stsd", &stsd);
        stbl.extend(bx(b"stts", &stts));
        stbl.extend(bx(b"stsc", &stsc));
        stbl.extend(bx(b"stsz", &stsz));
        stbl.extend(bx(b"co64", &co64));
        let mut mdia = bx(b"mdhd", &mdhd);
        mdia.extend(bx(b"hdlr", &hdlr));
        mdia.extend(bx(b"minf", &bx(b"stbl", &stbl)));
        let mut trak = bx(b"tkhd", &tkhd);
        trak.extend(bx(b"mdia", &mdia));
        let mut moov = bx(b"trak", &trak);
        moov.extend(udta.clone());
        bx(b"moov", &moov)
    };
    let ftyp = bx(b"ftyp", b"niko\0\0\0\x01niko");
    let len0 = moov_for(0).len();
    let mdat_at = (ftyp.len() + len0 + 8) as u64;
    let mut file = ftyp;
    file.extend(moov_for(mdat_at));
    file.extend(bx(b"mdat", &samples.concat()));
    std::fs::write(path, file).unwrap();
    codestreams
}

fn dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("bc_r3d_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn codestreams(path: &Path) -> Vec<Vec<u8>> {
    let input = InputFile::open(path).unwrap();
    let m = Movie::parse(&input).unwrap();
    m.video()
        .samples
        .iter()
        .map(|s| {
            let buf = input.read(s.offset, s.size as usize).unwrap();
            let f = bluecompress::nraw::NrawFrame::parse(&buf).unwrap();
            buf[f.codestream_offset..f.codestream_offset + f.codestream_size].to_vec()
        })
        .collect()
}

fn tags_of(path: &Path) -> std::collections::HashMap<u32, Vec<u8>> {
    let m = Movie::parse(&InputFile::open(path).unwrap()).unwrap();
    let t = nctg_tags(&m);
    t.iter().map(|x| (x.id, tag_value(&m, &t, x.id).unwrap().to_vec())).collect()
}

fn frame_value(path: &Path, key: [u8; 4]) -> Vec<u8> {
    let input = InputFile::open(path).unwrap();
    let m = Movie::parse(&input).unwrap();
    let s = m.video().samples[0];
    let buf = input.read(s.offset, 1024).unwrap();
    let mut pat = b"NRMT".to_vec();
    pat.extend(key);
    let at = buf.windows(8).position(|w| w == pat).unwrap();
    let len = u32::from_be_bytes(buf[at - 4..at].try_into().unwrap()) as usize;
    buf[at + 8..at - 4 + len].to_vec()
}

fn run(src: &Path, out: &Path, rmd: bool) -> bluecompress::R3dOutcome {
    let o = R3dOptions { rmd, ..R3dOptions::new(src, out) };
    r3d::convert(&o, &AtomicBool::new(false), &mut |_, _| {}).unwrap()
}

fn assert_r3d_tags(out: &Path, name: &str) {
    let info = bluecompress::probe(out).unwrap();
    assert_eq!(info.codec, bluecompress::Codec::R3dNe);
    let t = tags_of(out);
    assert_eq!(t[&0x21], 65u16.to_be_bytes());
    assert_eq!(t[&0x26], 4u16.to_be_bytes());
    assert_eq!(t[&0x27], 3u16.to_be_bytes());
    assert_eq!(t[&0x1020], [0, 1, 0, 0, 0, 2, 0, 0, 0, 0, 0x03, 0x20], "real ISO, no faked gain");
    assert_eq!(t[&0x0200_00c0], [0x48, 0x01, 0x0c, 0x00, 0x78, 0x01, 0x0c, 0x00]);
    let g = &t[&0x0200_0023];
    assert_eq!(&g[8..18], b"RWGLOG3G10");
    assert_eq!(&g[28..38], b"RWGLOG3G10");
    assert_eq!(g[49], 3);
    let file = format!("{name}.R3D");
    assert!(t[&0x523d].windows(file.len()).any(|w| w == file.as_bytes()));
    assert_eq!(&frame_value(out, [0x01, 0x90, 0x00, 0x12])[..4], &[0x00, 0x3f, 0x14, 0xd5]);
    assert_eq!(&frame_value(out, [0x02, 0x00, 0x52, 0x3d])[..5], &[0x00, 0x00, 0x05, 0x30, 0xaf]);
}

#[test]
fn rewraps_zr_to_r3d_ne_in_place() {
    let d = dir("high");
    let src = d.join("A007_C002_10299S.NEV");
    let cs = synthetic_nev(&src, 1, "NIKON ZR", 3);
    let out = r3d::r3d_path(&d.join("Compressed Lossless").join("A007_C002_10299S.NEV"), &src);
    assert!(out.ends_with("R3D NE/A007_C002_10299S.R3D"), "{}", out.display());
    let r = run(&src, &out, true);
    assert_eq!((r.frames, r.iso, r.experimental, r.direction), (3, 800, false, Direction::ToR3d));
    assert_r3d_tags(&out, "A007_C002_10299S");
    assert_eq!(codestreams(&out), cs);
    assert_eq!(t_white(&src), t_white(&out), "white balance tag is kept");
    // +0.5 EV in the RSX sidecar for the ZR.
    let rmd = std::fs::read_to_string(r.rmd.unwrap()).unwrap();
    assert!(rmd.contains("type=\"RSX\"") && rmd.contains("<exposureadjust type=\"float\" value=\"0.50000\"/>"));
    assert!(rmd.contains("<iso type=\"int\" value=\"800\"/>"));
    // Same size, only tags changed.
    let (a, b) = (std::fs::read(&src).unwrap(), std::fs::read(&out).unwrap());
    assert_eq!(a.len(), b.len());
    let diff = a.iter().zip(&b).filter(|(x, y)| x != y).count();
    assert!(diff > 0 && diff < 900, "{diff} bytes changed");
    let _ = std::fs::remove_dir_all(d);
}

fn t_white(p: &Path) -> Vec<u8> {
    tags_of(p)[&0x101e].clone()
}

#[test]
fn round_trip_restores_the_nev() {
    let d = dir("trip");
    let src = d.join("A001_C001_0101AB.NEV");
    synthetic_nev(&src, 1, "NIKON ZR", 3);
    let r3d_out = d.join("A001_C001_0101AB.R3D");
    run(&src, &r3d_out, false);
    assert!(!r3d::rmd_path(&r3d_out).exists(), "RMD off writes none");
    let back = d.join("back").join("A001_C001_0101AB.NEV");
    let r = run(&r3d_out, &back, true);
    assert_eq!(r.direction, Direction::ToNev);
    assert!(r.rmd.is_none());
    assert_eq!(bluecompress::probe(&back).unwrap().codec, bluecompress::Codec::NRaw);
    let t = tags_of(&back);
    assert_eq!(t[&0x21], 49u16.to_be_bytes());
    assert_eq!(&t[&0x0200_0023][8..13], b"N-LOG");
    assert_eq!(frame_value(&back, [0x01, 0x90, 0x00, 0x12]), NEV_MATRIX);
    // Byte for byte the original N-RAW clip.
    assert_eq!(std::fs::read(&src).unwrap(), std::fs::read(&back).unwrap());
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn normal_and_vbr_sources_rewrap_as_they_are() {
    let d = dir("normal");
    let src = d.join("A001_C002_0101AB.NEV");
    let cs = synthetic_nev(&src, 2, "NIKON ZR", 2);
    let out = d.join("A001_C002_0101AB.R3D");
    let r = run(&src, &out, true);
    assert_eq!(codestreams(&out), cs);
    assert_eq!(tags_of(&out)[&0x24], 2u16.to_be_bytes(), "quality is kept");
    assert!(r.frame_bytes > 0, "camera frames stay constant");
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn other_cameras_are_rebuilt_experimental() {
    let d = dir("z8");
    let src = d.join("DSC_0001.NEV");
    let cs = synthetic(&src, 1, "NIKON Z 8", 3, false);
    let out = d.join("A001_C001_0101XY.R3D");
    let r = run(&src, &out, true);
    assert!(r.experimental);
    assert_r3d_tags(&out, "A001_C001_0101XY");
    assert_eq!(codestreams(&out), cs, "image data copied as is");
    assert!(r.frame_bytes > 0, "all frames the same size");
    let rmd = std::fs::read_to_string(r.rmd.unwrap()).unwrap();
    assert!(rmd.contains("<exposureadjust type=\"float\" value=\"0.50000\"/>"));
    // And back to N-RAW.
    let back = d.join("back.NEV");
    run(&out, &back, false);
    assert_eq!(bluecompress::probe(&back).unwrap().codec, bluecompress::Codec::NRaw);
    assert_eq!(codestreams(&back), cs);
    assert_eq!(&tags_of(&back)[&0x0200_0023][8..13], b"N-LOG");
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn compress_path_writes_constant_r3d_ne() {
    let d = dir("cbr");
    let src = d.join("A001_C004_0101AB.NEV");
    let cs = synthetic_nev(&src, 1, "NIKON ZR", 4);
    let out = d.join("A001_C004_0101AB.R3D");
    // A bpp above this tiny clip's own resolves to the lossless path through the
    // compressor (real clips are requantized; checked on real data separately).
    let job = bluecompress::Options {
        input: src.clone(),
        output: d.join("x.NEV"),
        quality: 1,
        bpp: Some(0.5),
        resolution: bluecompress::Resolution::Original,
        backend: bluecompress::Backend::Cpu,
        threads: 2,
        cbr: false,
        template: None,
    };
    let r = r3d::convert_compressed(
        &job,
        &R3dOptions { exposure_ev: Some(1.5), ..R3dOptions::new(&src, &out) },
        &AtomicBool::new(false),
        &mut |_, _| {},
    )
    .unwrap();
    assert!(r.frame_bytes > 0, "R3D NE frames always the same size");
    assert_r3d_tags(&out, "A001_C004_0101AB");
    assert_eq!(codestreams(&out), cs);
    let rmd = std::fs::read_to_string(r.rmd.expect("sidecar")).unwrap();
    assert!(rmd.contains("<exposureadjust type=\"float\" value=\"1.50000\"/>"), "adjustable exposure");
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn refuses_non_nikon() {
    let d = dir("refuse");
    let src = d.join("X.NEV");
    synthetic_nev(&src, 1, "OTHER", 1);
    assert!(r3d::check(&src).unwrap_err().0.contains("Nikon"));
    let _ = std::fs::remove_dir_all(d);
}
