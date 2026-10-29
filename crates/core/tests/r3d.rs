//! N-RAW to R3D NE conversion on synthetic ZR clips: the output reads back through our own
//! parser as NR3D with the R3D NE tags, and every other byte is the source's.

use std::path::Path;
use std::sync::atomic::AtomicBool;

use bluecompress::bmff::{InputFile, Movie};
use bluecompress::r3d::{self, nctg_tags, tag_value, R3dOptions};

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
    let mut codestreams = Vec::new();
    let mut samples = Vec::new();
    for f in 0..frames {
        let mut cs = vec![0xff, 0x10];
        cs.extend((0..2046).map(|i| ((i * 7 + f * 13) % 251) as u8));
        let mut nrfh = bx(b"NRHM", b"0100");
        nrfh.extend(nrmt(0x0110_0100, &[0, 0, 0, 0x17, 0xac]));
        nrfh.extend(nrmt(0x0110_0101, &[0, 0, 0, 0x0d, 0x52]));
        nrfh.extend(nrmt(0x0190_0011, &[0x00, 0x0f, 0xc1, 0x12, 0xe1, 0xf0, 0xce]));
        nrfh.extend(nrmt(0x0190_0012, &[0x11; 37]));
        nrfh.extend(nrmt(0x0200_523d, &[0u8; 65]));
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
    gamma.resize(108, 0xaa);
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
    nctg.extend(tag(0x1020, 7, &[0; 12]));
    nctg.extend(tag(0x523d, 7, &[0; 256]));
    nctg.extend(tag(0x0110_8832, 4, &800u32.to_be_bytes()));
    nctg.extend(tag(0x0200_0023, 7, &gamma));
    nctg.extend(tag(0x0200_00c0, 7, &[0x60, 0x01, 0x0c, 0x00, 0xa8, 0x01, 0x0c, 0x00]));
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

#[test]
fn converts_high_to_r3d_ne() {
    let d = dir("high");
    let src = d.join("A007_C002_10299S.NEV");
    let cs = synthetic_nev(&src, 1, "NIKON ZR", 3);
    let out = r3d::r3d_path(&d.join("Compressed Lossless").join("A007_C002_10299S.NEV"), &src);
    assert!(out.ends_with("R3D NE/A007_C002_10299S.R3D"), "{}", out.display());
    let o = R3dOptions::new(&src, &out);
    let r = r3d::convert(&o, &AtomicBool::new(false), &mut |_, _| {}).unwrap();
    assert_eq!(r.frames, 3);
    assert_eq!(r.iso, 1600);

    // Our parser reads it as R3D NE.
    let info = bluecompress::probe(&out).unwrap();
    assert_eq!(info.codec, bluecompress::Codec::R3dNe);
    assert_eq!(info.frames, 3);
    let input = InputFile::open(&out).unwrap();
    let m = Movie::parse(&input).unwrap();
    let tags = nctg_tags(&m);
    let v = |id| tag_value(&m, &tags, id).unwrap().to_vec();
    assert_eq!(v(0x21), 65u16.to_be_bytes());
    assert_eq!(v(0x24), 1u16.to_be_bytes());
    assert_eq!(v(0x26), 4u16.to_be_bytes());
    assert_eq!(v(0x27), 3u16.to_be_bytes());
    assert_eq!(v(0x1020), [0, 1, 0, 0, 0, 2, 0, 0, 0, 0, 0x06, 0x40]);
    assert_eq!(v(0x0200_00c0), [0x48, 0x01, 0x0c, 0x00, 0x78, 0x01, 0x0c, 0x00]);
    assert_eq!(v(0x101e), [0x5a; 36], "white balance tag is kept");
    let g = v(0x0200_0023);
    assert_eq!(&g[8..18], b"RWGLOG3G10");
    assert_eq!(&g[28..38], b"RWGLOG3G10");
    assert_eq!(g[49], 3);
    assert!(g[50..].iter().all(|&b| b == 0xaa));
    let red = v(0x523d);
    assert!(red.windows(20).any(|w| w == b"A007_C002_10299S.R3D"));

    // Frames: same size, codestream untouched, R3D NE frame tags.
    let vt = m.video();
    for (i, s) in vt.samples.iter().enumerate() {
        let buf = input.read(s.offset, s.size as usize).unwrap();
        let f = bluecompress::nraw::NrawFrame::parse(&buf).unwrap();
        assert_eq!(&buf[f.codestream_offset..f.codestream_offset + f.codestream_size], &cs[i][..]);
        let mat = buf.windows(8).position(|w| w == [b'N', b'R', b'M', b'T', 0x01, 0x90, 0x00, 0x12]).unwrap();
        assert_eq!(&buf[mat + 8..mat + 12], &[0x00, 0x3f, 0x14, 0xd5]);
        let rf = buf.windows(8).position(|w| w == [b'N', b'R', b'M', b'T', 0x02, 0x00, 0x52, 0x3d]).unwrap();
        assert_eq!(&buf[rf + 8..rf + 13], &[0x00, 0x00, 0x05, 0x30, 0xaf]);
    }

    // Byte for byte the source except the rewritten tags.
    let (a, b) = (std::fs::read(&src).unwrap(), std::fs::read(&out).unwrap());
    assert_eq!(a.len(), b.len());
    let diff = a.iter().zip(&b).filter(|(x, y)| x != y).count();
    assert!(diff > 0 && diff < 600, "{diff} bytes changed");
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn refuses_normal_other_cameras_and_r3d() {
    let d = dir("refuse");
    let normal = d.join("A001_C001_0101AB.NEV");
    synthetic_nev(&normal, 2, "NIKON ZR", 2);
    let e = r3d::check(&normal).unwrap_err();
    assert!(e.0.contains("N-RAW Normal not supported for R3D"), "{e}");
    let z8 = d.join("Z8.NEV");
    synthetic_nev(&z8, 1, "NIKON Z 8", 2);
    assert!(r3d::check(&z8).unwrap_err().0.contains("Nikon ZR clips only"));

    let high = d.join("A001_C002_0101AB.NEV");
    synthetic_nev(&high, 1, "NIKON ZR", 2);
    let out = d.join("A001_C002_0101AB.R3D");
    let cancel = AtomicBool::new(false);
    r3d::convert(&R3dOptions::new(&high, &out), &cancel, &mut |_, _| {}).unwrap();
    assert!(r3d::check(&out).unwrap_err().0.contains("already an R3D NE"));
    // Nothing is left behind by a refused job.
    let o2 = d.join("x").join("N.R3D");
    assert!(r3d::convert(&R3dOptions::new(&normal, &o2), &cancel, &mut |_, _| {}).is_err());
    assert!(!o2.exists());
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn iso_factor_is_a_parameter() {
    let d = dir("iso");
    let src = d.join("A001_C003_0101AB.NEV");
    synthetic_nev(&src, 1, "NIKON ZR", 1);
    let out = d.join("o.R3D");
    let o = R3dOptions { iso_factor: 1.0, ..R3dOptions::new(&src, &out) };
    assert_eq!(r3d::convert(&o, &AtomicBool::new(false), &mut |_, _| {}).unwrap().iso, 800);
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn output_is_constant_bitrate_and_vbr_sources_are_refused() {
    let d = dir("cbr");
    let src = d.join("A001_C004_0101AB.NEV");
    synthetic_nev(&src, 1, "NIKON ZR", 4);
    let out = d.join("A001_C004_0101AB.R3D");
    let r = r3d::convert(&R3dOptions::new(&src, &out), &AtomicBool::new(false), &mut |_, _| {}).unwrap();
    let m = Movie::parse(&InputFile::open(&out).unwrap()).unwrap();
    assert!(m.video().samples.iter().all(|s| s.size == r.frame_bytes));

    // A .nev whose frames vary in size (not a camera original) cannot become R3D NE.
    let mut bytes = std::fs::read(&src).unwrap();
    let stsz = bytes.windows(4).position(|w| w == b"stsz").unwrap();
    let n = u32::from_be_bytes(bytes[stsz + 12..stsz + 16].try_into().unwrap());
    let size = u32::from_be_bytes(bytes[stsz + 8..stsz + 12].try_into().unwrap());
    // Rewrite stsz as a table with one frame a little shorter (contents do not matter here).
    let mut table = Vec::new();
    for i in 0..n {
        table.extend(if i == 1 { size - 16 } else { size }.to_be_bytes());
    }
    let old_len = u32::from_be_bytes(bytes[stsz - 4..stsz].try_into().unwrap()) as usize;
    let mut new_box = ((20 + table.len()) as u32).to_be_bytes().to_vec();
    new_box.extend(b"stsz");
    new_box.extend([0, 0, 0, 0, 0, 0, 0, 0]);
    new_box.extend(n.to_be_bytes());
    new_box.extend(table);
    let grow = new_box.len() - old_len;
    bytes.splice(stsz - 4..stsz - 4 + old_len, new_box);
    // Fix the parents' sizes (stbl, minf, mdia, trak, moov) and the chunk offsets.
    for kind in [b"stbl", b"minf", b"mdia", b"trak", b"moov"] {
        let at = bytes.windows(4).position(|w| w == kind).unwrap() - 4;
        let v = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) + grow as u32;
        bytes[at..at + 4].copy_from_slice(&v.to_be_bytes());
    }
    let co = bytes.windows(4).position(|w| w == b"co64").unwrap();
    for i in 0..n as usize {
        let at = co + 12 + 8 * i;
        let v = u64::from_be_bytes(bytes[at..at + 8].try_into().unwrap()) + grow as u64;
        bytes[at..at + 8].copy_from_slice(&v.to_be_bytes());
    }
    let vbr = d.join("A001_C005_0101AB.NEV");
    std::fs::write(&vbr, bytes).unwrap();
    let e = r3d::check(&vbr).unwrap_err();
    assert!(e.0.contains("camera original"), "{e}");
    let _ = std::fs::remove_dir_all(d);
}
