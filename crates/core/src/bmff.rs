//! Minimal ISO base media file reader/writer for Nikon N-RAW (.NEV) and R3D NE (.R3D)
//! clips: ftyp 'niko', moov (video 'NRAW'/'NR3D', lpcm audio, tmcd), udta/NCDT camera
//! metadata, free padding, mdat. Only what is needed to replace video samples while
//! keeping every other byte (audio, timecode, metadata) intact.

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::Duration;

use crate::{Error, Result};

pub const fn fourcc(s: &[u8; 4]) -> u32 {
    (s[0] as u32) << 24 | (s[1] as u32) << 16 | (s[2] as u32) << 8 | s[3] as u32
}

pub fn fourcc_str(t: u32) -> String {
    t.to_be_bytes().iter().map(|&c| if (32..127).contains(&c) { c as char } else { '?' }).collect()
}

#[inline]
fn rd32(d: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(d[at..at + 4].try_into().unwrap())
}

#[inline]
fn rd64(d: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(d[at..at + 8].try_into().unwrap())
}

#[inline]
fn wr32(d: &mut [u8], at: usize, v: u32) {
    d[at..at + 4].copy_from_slice(&v.to_be_bytes());
}

fn fail<T>(what: impl Into<String>) -> Result<T> {
    Err(Error(what.into()))
}

#[derive(Debug, Clone, Default)]
pub struct BoxNode {
    pub kind: u32,
    /// File (or buffer) offset of the box header.
    pub offset: u64,
    /// Total size including header.
    pub size: u64,
    pub header_size: u32,
    pub children: Vec<BoxNode>,
}

impl BoxNode {
    pub fn payload(&self) -> u64 {
        self.offset + self.header_size as u64
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Sample {
    pub offset: u64,
    pub size: u32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Chunk {
    pub offset: u64,
    /// Sum of its samples.
    pub size: u64,
    pub first_sample: u32,
    pub samples: u32,
}

#[derive(Debug, Clone, Default)]
pub struct Track {
    /// 'vide', 'soun', 'tmcd'.
    pub handler: u32,
    /// Sample entry type.
    pub codec: u32,
    pub samples: Vec<Sample>,
    pub chunks: Vec<Chunk>,
    // Locations (inside moov) of fields we may rewrite.
    pub tkhd_offset: usize,
    pub stsd_entry_offset: usize,
    pub stsz_offset: u64,
    pub stco_offset: u64,
    pub width: u32,
    pub height: u32,
    /// mdhd timescale.
    pub timescale: u32,
    /// First stts entry.
    pub sample_delta: u32,
}

/// Read-only clip file with thread-safe positional reads.
///
/// Reads survive transient failures: an external drive waking up or briefly dropping off
/// the bus, a sharing violation or an antivirus scan holding the file, or Windows running
/// short of kernel resources for a large read. A failed or empty read is retried with
/// backoff on a freshly opened handle (see [`READ_RETRY_DELAYS`]); only when every retry
/// fails does the read fail, with the operating system's own error text.
pub struct InputFile {
    path: PathBuf,
    file: RwLock<File>,
    size: u64,
}

/// Waits between read attempts, about 15 seconds in all: long enough for a sleeping disk
/// to spin up or a USB drive to come back, short enough that a dead drive is reported soon.
pub const READ_RETRY_DELAYS: [Duration; 7] = [
    Duration::from_millis(100),
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
];

/// Largest single read handed to the OS. Very large reads on Windows can fail with
/// ERROR_NO_SYSTEM_RESOURCES (1450) under memory pressure; smaller ones do not.
const MAX_READ: usize = 4 << 20;

/// Start of the message of a read that failed after every retry (see [`is_read_error`]).
pub const READ_ERROR: &str = "read error in ";

/// True for errors from reading a source clip (after retries), as opposed to a bad clip.
/// These usually mean the drive or file became unreachable.
pub fn is_read_error(e: &Error) -> bool {
    e.0.contains(READ_ERROR)
}

/// The retry loop of [`InputFile::read_into`], separate from the file so tests can drive it.
/// `read(buf, offset)` is one positional read, `reopen()` replaces the handle.
pub(crate) fn read_retrying(
    dst: &mut [u8],
    offset: u64,
    delays: &[Duration],
    read: &mut dyn FnMut(&mut [u8], u64) -> std::io::Result<usize>,
    reopen: &mut dyn FnMut() -> std::io::Result<()>,
    sleep: &mut dyn FnMut(Duration),
    describe: &dyn Fn() -> String,
) -> Result<()> {
    let mut done = 0;
    let mut attempt = 0;
    while done < dst.len() {
        let at = offset + done as u64;
        let end = dst.len().min(done + MAX_READ);
        let problem = match read(&mut dst[done..end], at) {
            Ok(0) => "the drive returned no data (end of file before the clip's size)".to_string(),
            Ok(n) => {
                // Progress: a later failure gets the full set of retries again.
                done += n;
                attempt = 0;
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => e.to_string(),
        };
        if attempt >= delays.len() {
            return fail(format!("{READ_ERROR}{} at byte {at}: {problem} (after {attempt} retries)", describe()));
        }
        sleep(delays[attempt]);
        attempt += 1;
        // A handle to a drive that went away stays broken; open the file again. When that
        // fails too the next read fails and is retried, until the attempts run out.
        let _ = reopen();
    }
    Ok(())
}

impl InputFile {
    pub fn open(path: &Path) -> Result<InputFile> {
        let file = open_retrying(path)?;
        let size = file.metadata().map_err(|e| Error(format!("cannot stat {}: {e}", path.display())))?.len();
        Ok(InputFile { path: path.to_path_buf(), file: RwLock::new(file), size })
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read_into(&self, offset: u64, dst: &mut [u8]) -> Result<()> {
        if offset + dst.len() as u64 > self.size {
            return fail(format!("read past end of {}", self.path.display()));
        }
        read_retrying(
            dst,
            offset,
            &READ_RETRY_DELAYS,
            &mut |buf, at| {
                let f = self.file.read().unwrap_or_else(|p| p.into_inner());
                let n = buf.len();
                crate::drive::global().run(n, || {
                    #[cfg(unix)]
                    let r = std::os::unix::fs::FileExt::read_at(&*f, buf, at);
                    #[cfg(windows)]
                    let r = std::os::windows::fs::FileExt::seek_read(&*f, buf, at);
                    r
                })
            },
            &mut || {
                let f = File::open(&self.path)?;
                *self.file.write().unwrap_or_else(|p| p.into_inner()) = f;
                Ok(())
            },
            &mut std::thread::sleep,
            &|| self.path.display().to_string(),
        )
    }

    pub fn read(&self, offset: u64, n: usize) -> Result<Vec<u8>> {
        let mut v = vec![0u8; n];
        self.read_into(offset, &mut v)?;
        Ok(v)
    }
}

/// Opens a clip, retrying briefly as reads do (a drive waking up, a file held by a scan).
fn open_retrying(path: &Path) -> Result<File> {
    let mut last = None;
    for d in std::iter::once(Duration::ZERO).chain(READ_RETRY_DELAYS.iter().copied().take(4)) {
        std::thread::sleep(d);
        match File::open(path) {
            Ok(f) => return Ok(f),
            // A path that is not there (and whose folder is) will not appear by waiting.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && path.parent().is_some_and(|p| p.exists()) => {
                return fail(format!("cannot open {}: {e}", path.display()))
            }
            Err(e) => last = Some(e),
        }
    }
    fail(format!("cannot open {}: {}", path.display(), last.map(|e| e.to_string()).unwrap_or_default()))
}

/// True when the clip can be opened and a byte read from it, straight from the drive.
pub fn source_reachable(path: &Path) -> bool {
    let Ok(f) = File::open(path) else { return false };
    let Ok(m) = f.metadata() else { return false };
    let mut b = [0u8; 1];
    #[cfg(unix)]
    let r = std::os::unix::fs::FileExt::read_at(&f, &mut b, m.len() / 2);
    #[cfg(windows)]
    let r = std::os::windows::fs::FileExt::seek_read(&f, &mut b, m.len() / 2);
    m.len() == 0 || matches!(r, Ok(1))
}

fn is_container(t: u32) -> bool {
    [b"moov", b"trak", b"mdia", b"minf", b"stbl", b"dinf", b"edts", b"udta"].iter().any(|c| t == fourcc(c))
}

/// Parses boxes in buf[begin, end) with offsets relative to `buf`.
fn parse_boxes(buf: &[u8], begin: u64, end: u64) -> Result<Vec<BoxNode>> {
    let mut out = Vec::new();
    let mut at = begin;
    while at + 8 <= end {
        let a = at as usize;
        let mut size = rd32(buf, a) as u64;
        let kind = rd32(buf, a + 4);
        let mut header_size = 8;
        if size == 1 {
            if at + 16 > end {
                return fail("truncated box header");
            }
            size = rd64(buf, a + 8);
            header_size = 16;
        } else if size == 0 {
            size = end - at;
        }
        if size < header_size as u64 || at + size > end {
            return fail(format!("bad box size in {}", fourcc_str(kind)));
        }
        let children =
            if is_container(kind) { parse_boxes(buf, at + header_size as u64, at + size)? } else { Vec::new() };
        out.push(BoxNode { kind, offset: at, size, header_size, children });
        at += size;
    }
    Ok(out)
}

fn find<'a>(boxes: &'a [BoxNode], kind: &[u8; 4]) -> Option<&'a BoxNode> {
    boxes.iter().find(|b| b.kind == fourcc(kind))
}

fn find_path<'a>(root: &'a BoxNode, path: &[&[u8; 4]]) -> Option<&'a BoxNode> {
    let mut cur = root;
    for p in path {
        cur = find(&cur.children, p)?;
    }
    Some(cur)
}

#[derive(Debug, Clone, Default)]
pub struct Movie {
    /// Top-level boxes (headers only).
    pub top: Vec<BoxNode>,
    /// Raw moov bytes.
    pub moov: Vec<u8>,
    pub tracks: Vec<Track>,
    pub video_track: usize,
    pub brand: u32,
    /// Nikon NCTG tags 0x22/0x23: active image size (the codestream adds a margin).
    pub active_width: u32,
    pub active_height: u32,
    /// Offsets inside moov, None when absent.
    pub active_width_offset: Option<usize>,
    pub active_height_offset: Option<usize>,
}

impl Movie {
    pub fn video(&self) -> &Track {
        &self.tracks[self.video_track]
    }

    pub fn parse(input: &InputFile) -> Result<Movie> {
        let mut m = Movie::default();
        // Top level: read headers only.
        let mut at = 0u64;
        while at + 8 <= input.size() {
            let mut h = [0u8; 16];
            input.read_into(at, &mut h[..8])?;
            let mut size = rd32(&h, 0) as u64;
            let kind = rd32(&h, 4);
            let mut header_size = 8;
            if size == 1 {
                input.read_into(at + 8, &mut h[8..16])?;
                size = rd64(&h, 8);
                header_size = 16;
            } else if size == 0 {
                size = input.size() - at;
            }
            if size < header_size as u64 || at + size > input.size() {
                return fail(format!("bad top-level box {}", fourcc_str(kind)));
            }
            m.top.push(BoxNode { kind, offset: at, size, header_size, children: Vec::new() });
            at += size;
        }
        let (Some(ftyp), Some(moov)) = (find(&m.top, b"ftyp"), find(&m.top, b"moov")) else {
            return fail("not an ISO media file (no ftyp/moov)");
        };
        m.brand = rd32(&input.read(ftyp.payload(), 4)?, 0);
        m.moov = input.read(moov.offset, moov.size as usize)?;
        // Offsets inside the tree are relative to moov.
        let tree = parse_boxes(&m.moov, 0, m.moov.len() as u64)?;
        let mv = tree.first().ok_or_else(|| Error("empty moov".into()))?;
        let mm = &m.moov;

        for trak in mv.children.iter().filter(|b| b.kind == fourcc(b"trak")) {
            let (Some(tkhd), Some(hdlr), Some(stbl)) = (
                find(&trak.children, b"tkhd"),
                find_path(trak, &[b"mdia", b"hdlr"]),
                find_path(trak, &[b"mdia", b"minf", b"stbl"]),
            ) else {
                continue;
            };
            let mut t = Track { tkhd_offset: tkhd.offset as usize, ..Default::default() };
            if let Some(mdhd) = find_path(trak, &[b"mdia", b"mdhd"]) {
                let md = mdhd.payload() as usize;
                t.timescale = if mm[md] == 1 { rd32(mm, md + 20) } else { rd32(mm, md + 12) };
            }
            t.handler = rd32(mm, hdlr.payload() as usize + 8);
            let (Some(stsd), Some(stsz), Some(stsc)) =
                (find(&stbl.children, b"stsd"), find(&stbl.children, b"stsz"), find(&stbl.children, b"stsc"))
            else {
                return fail("incomplete sample table");
            };
            let (co, co64) = match (find(&stbl.children, b"co64"), find(&stbl.children, b"stco")) {
                (Some(c), _) => (c, true),
                (None, Some(c)) => (c, false),
                _ => return fail("incomplete sample table"),
            };
            t.stsd_entry_offset = stsd.payload() as usize + 8;
            t.codec = rd32(mm, t.stsd_entry_offset + 4);
            if t.handler == fourcc(b"vide") {
                let se = t.stsd_entry_offset;
                t.width = (mm[se + 32] as u32) << 8 | mm[se + 33] as u32;
                t.height = (mm[se + 34] as u32) << 8 | mm[se + 35] as u32;
            }
            if let Some(stts) = find(&stbl.children, b"stts") {
                let st = stts.payload() as usize;
                if rd32(mm, st + 4) > 0 {
                    t.sample_delta = rd32(mm, st + 12);
                }
            }
            t.stsz_offset = stsz.offset;
            t.stco_offset = co.offset;

            let sz = stsz.payload() as usize;
            let fixed = rd32(mm, sz + 4);
            let count = rd32(mm, sz + 8) as usize;
            let sizes: Vec<u32> =
                if fixed == 0 { (0..count).map(|i| rd32(mm, sz + 12 + 4 * i)).collect() } else { vec![fixed; count] };

            let cp = co.payload() as usize;
            let nchunks = rd32(mm, cp + 4) as usize;
            let chunk_off: Vec<u64> = (0..nchunks)
                .map(|i| if co64 { rd64(mm, cp + 8 + 8 * i) } else { rd32(mm, cp + 8 + 4 * i) as u64 })
                .collect();

            let sc = stsc.payload() as usize;
            let nruns = rd32(mm, sc + 4) as usize;
            let runs: Vec<(u32, u32)> =
                (0..nruns).map(|i| (rd32(mm, sc + 8 + 12 * i), rd32(mm, sc + 12 + 12 * i))).collect();

            let mut sample = 0usize;
            for (c, &off0) in chunk_off.iter().enumerate() {
                let per = runs.iter().rfind(|r| c as u32 + 1 >= r.0).map_or(0, |r| r.1);
                let mut ch = Chunk { offset: off0, first_sample: sample as u32, samples: per, size: 0 };
                let mut off = off0;
                let mut s = 0;
                while s < per && sample < count {
                    t.samples.push(Sample { offset: off, size: sizes[sample] });
                    off += sizes[sample] as u64;
                    ch.size += sizes[sample] as u64;
                    s += 1;
                    sample += 1;
                }
                t.chunks.push(ch);
            }
            m.tracks.push(t);
        }
        m.video_track =
            m.tracks.iter().position(|t| t.handler == fourcc(b"vide")).ok_or_else(|| Error("no video track".into()))?;

        // Nikon camera metadata: moov/udta/NCDT/NCTG, entries (u32 tag, u16 type, u16 count, value).
        if let Some(udta) = find(&mv.children, b"udta") {
            for b in udta.children.iter().filter(|b| b.kind == fourcc(b"NCDT")) {
                let end = (b.offset + b.size) as usize;
                let mut at = b.payload() as usize;
                while at + 8 <= end {
                    let (len, kind) = (rd32(mm, at) as usize, rd32(mm, at + 4));
                    if len < 8 || at + len > end {
                        break;
                    }
                    if kind == fourcc(b"NCTG") {
                        const UNIT: [usize; 13] = [1, 1, 1, 2, 4, 8, 1, 1, 2, 4, 8, 4, 8];
                        let mut q = at + 8;
                        while q + 8 <= at + len {
                            let tag = rd32(mm, q);
                            let ty = u16::from_be_bytes([mm[q + 4], mm[q + 5]]) as usize;
                            let cnt = u16::from_be_bytes([mm[q + 6], mm[q + 7]]) as usize;
                            let n = if ty < 13 { UNIT[ty] } else { 1 } * cnt;
                            if ty == 3 && cnt == 1 && (tag == 0x22 || tag == 0x23) {
                                let v = u16::from_be_bytes([mm[q + 8], mm[q + 9]]) as u32;
                                if tag == 0x22 {
                                    m.active_width = v;
                                    m.active_width_offset = Some(q + 8);
                                } else {
                                    m.active_height = v;
                                    m.active_height_offset = Some(q + 8);
                                }
                            }
                            q += 8 + n;
                        }
                    }
                    at += len;
                }
            }
        }
        Ok(m)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct WriteOptions {
    /// New video frame size (0 keeps the original).
    pub width: u32,
    pub height: u32,
    pub active_width: u32,
    pub active_height: u32,
}

/// A chunk of some track, in original file order.
#[derive(Debug, Clone, Copy)]
pub struct ChunkRef {
    pub track: usize,
    pub chunk: usize,
    pub offset: u64,
}

/// Chunks of every track ordered by original offset.
pub fn chunk_order(movie: &Movie) -> Vec<ChunkRef> {
    let mut v: Vec<ChunkRef> = movie
        .tracks
        .iter()
        .enumerate()
        .flat_map(|(t, tr)| {
            tr.chunks.iter().enumerate().map(move |(c, ch)| ChunkRef { track: t, chunk: c, offset: ch.offset })
        })
        .collect();
    v.sort_by_key(|c| c.offset);
    v
}

/// Streams a new file into `<path>.partial`: all chunks in original order, video samples
/// replaced. [`OutputWriter::finish`] rebuilds moov and renames; dropping an unfinished
/// writer removes the partial file.
pub struct OutputWriter<'a> {
    path: PathBuf,
    tmp_path: PathBuf,
    input: &'a InputFile,
    movie: &'a Movie,
    opt: WriteOptions,
    file: Option<crate::drive::OutFile>,
    pos: u64,
    mdat_offset: u64,
    ftyp_end: u64,
    new_chunk_offsets: Vec<Vec<u64>>,
    new_video_sizes: Vec<u32>,
    copy_buf: Vec<u8>,
    finished: bool,
    /// Replacement moov/udta box and video sample entry fourcc (R3D NE output).
    udta: Option<Vec<u8>>,
    codec: Option<u32>,
}

impl<'a> OutputWriter<'a> {
    pub fn create(path: &Path, input: &'a InputFile, movie: &'a Movie, opt: WriteOptions) -> Result<Self> {
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".partial");
        let tmp_path = PathBuf::from(tmp);
        let file = crate::drive::OutFile::create(&tmp_path)?;
        let mut w = OutputWriter {
            path: path.to_path_buf(),
            tmp_path,
            input,
            movie,
            opt,
            file: Some(file),
            pos: 0,
            mdat_offset: 0,
            ftyp_end: 0,
            new_chunk_offsets: movie.tracks.iter().map(|t| vec![0; t.chunks.len()]).collect(),
            new_video_sizes: vec![0; movie.video().samples.len()],
            copy_buf: Vec::new(),
            finished: false,
            udta: None,
            codec: None,
        };
        let ftyp = find(&movie.top, b"ftyp").unwrap();
        let ftyp_bytes = input.read(ftyp.offset, ftyp.size as usize)?;
        w.write_raw(&ftyp_bytes)?;
        w.ftyp_end = w.pos;

        // Reserve room for the new moov before the media data; keep mdat at its original
        // offset when the reservation fits there.
        let orig_payload = find(&movie.top, b"mdat").map_or(0, |b| b.payload());
        let est_moov = movie.moov.len() as u64 + 8 * movie.video().samples.len() as u64 + 16 * 1024;
        let mut data_start = orig_payload.max(w.pos + est_moov + 64);
        data_start = (data_start + 4095) & !4095;
        if orig_payload >= w.pos + est_moov + 64 {
            data_start = orig_payload;
        }
        let zeros = vec![0u8; (data_start - w.pos) as usize];
        w.write_raw(&zeros)?;
        w.mdat_offset = data_start;
        Ok(w)
    }

    fn write_raw(&mut self, p: &[u8]) -> Result<()> {
        let f = self.file.as_mut().unwrap();
        f.write(p)?;
        self.pos += p.len() as u64;
        Ok(())
    }

    /// Writes `udta` (a whole box) in place of moov/udta and `codec` as the video sample
    /// entry's fourcc when moov is rebuilt.
    pub fn set_metadata(&mut self, udta: Vec<u8>, codec: u32) {
        self.udta = Some(udta);
        self.codec = Some(codec);
    }

    /// Appends the next video sample (samples must arrive in chunk order).
    pub fn write_video_sample(&mut self, sample: usize, data: &[u8]) -> Result<()> {
        let vt = self.movie.video_track;
        let v = &self.movie.tracks[vt];
        if let Some(c) = v
            .chunks
            .iter()
            .position(|ch| sample as u32 >= ch.first_sample && (sample as u32) < ch.first_sample + ch.samples)
        {
            if sample as u32 == v.chunks[c].first_sample {
                self.new_chunk_offsets[vt][c] = self.pos;
            }
        }
        self.new_video_sizes[sample] = data.len() as u32;
        self.write_raw(data)
    }

    /// Copies a chunk of a non-video track verbatim.
    pub fn copy_chunk(&mut self, track: usize, chunk: usize) -> Result<()> {
        let ch = self.movie.tracks[track].chunks[chunk];
        self.new_chunk_offsets[track][chunk] = self.pos;
        let mut buf = std::mem::take(&mut self.copy_buf);
        buf.resize(ch.size.min(8 << 20) as usize, 0);
        let mut done = 0u64;
        while done < ch.size {
            let n = (buf.len() as u64).min(ch.size - done) as usize;
            self.input.read_into(ch.offset + done, &mut buf[..n])?;
            self.write_raw(&buf[..n])?;
            done += n as u64;
        }
        self.copy_buf = buf;
        Ok(())
    }

    pub fn finish(mut self) -> Result<u64> {
        let mdat_end = self.pos;
        let payload_size = mdat_end - self.mdat_offset;
        let movie = self.movie;
        let opt = self.opt;

        // Patch and rebuild moov.
        let mut moov = movie.moov.clone();
        let mut repl: HashMap<u64, Vec<u8>> = HashMap::new();
        for (ti, t) in movie.tracks.iter().enumerate() {
            let is_video = ti == movie.video_track;
            if is_video && opt.width != 0 && opt.height != 0 {
                let tk = t.tkhd_offset;
                let w_off = if moov[tk + 8] == 1 { 96 } else { 84 };
                wr32(&mut moov, tk + w_off, opt.width << 16);
                wr32(&mut moov, tk + w_off + 4, opt.height << 16);
                let se = t.stsd_entry_offset;
                moov[se + 32..se + 34].copy_from_slice(&(opt.width as u16).to_be_bytes());
                moov[se + 34..se + 36].copy_from_slice(&(opt.height as u16).to_be_bytes());
            }
            if is_video && opt.active_width != 0 {
                if let (Some(wo), Some(ho)) = (movie.active_width_offset, movie.active_height_offset) {
                    moov[wo..wo + 2].copy_from_slice(&(opt.active_width as u16).to_be_bytes());
                    moov[ho..ho + 2].copy_from_slice(&(opt.active_height as u16).to_be_bytes());
                }
            }
            if is_video {
                let sizes = &self.new_video_sizes;
                // Keep a constant-size table when every sample has the same size.
                let constant = sizes.iter().all(|&s| s == sizes[0]);
                let mut stsz = Vec::new();
                for v in [0, fourcc(b"stsz"), 0, if constant { sizes[0] } else { 0 }, sizes.len() as u32] {
                    stsz.extend_from_slice(&v.to_be_bytes());
                }
                if !constant {
                    for &s in sizes {
                        stsz.extend_from_slice(&s.to_be_bytes());
                    }
                }
                let n = stsz.len() as u32;
                wr32(&mut stsz, 0, n);
                repl.insert(t.stsz_offset, stsz);
            }
            let offs = &self.new_chunk_offsets[ti];
            let mut co = Vec::with_capacity(16 + 8 * offs.len());
            for v in [0, fourcc(b"co64"), 0, offs.len() as u32] {
                co.extend_from_slice(&v.to_be_bytes());
            }
            for &o in offs {
                co.extend_from_slice(&o.to_be_bytes());
            }
            let n = co.len() as u32;
            wr32(&mut co, 0, n);
            repl.insert(t.stco_offset, co);
        }
        if let Some(c) = self.codec {
            let se = movie.video().stsd_entry_offset;
            wr32(&mut moov, se + 4, c);
        }
        let tree = parse_boxes(&moov, 0, moov.len() as u64)?;
        if let Some(u) = self.udta.take() {
            match find(&tree[0].children, b"udta") {
                Some(b) => {
                    repl.insert(b.offset, u);
                }
                None => return fail("no udta to replace"),
            }
        }
        let mut new_moov = Vec::with_capacity(moov.len() + 64 * 1024);
        serialize(&tree[0], &moov, &repl, &mut new_moov);

        // mdat header goes right before the payload; 32-bit size when it fits.
        let large = payload_size + 8 > 0xffff_ffff;
        let hdr_size = if large { 16 } else { 8 };
        let ftyp_end = self.ftyp_end;
        let mdat_header_at = self.mdat_offset - hdr_size;
        let moov_len = new_moov.len() as u64;
        let moov_front = ftyp_end + moov_len + 8 <= mdat_header_at || ftyp_end + moov_len == mdat_header_at;

        let mut file = self.file.take().unwrap();
        file.flush()?;
        let mut seek_write = |at: u64, p: &[u8]| -> Result<()> { file.write_at(at, p) };
        let mut hdr = Vec::new();
        if large {
            hdr.extend_from_slice(&1u32.to_be_bytes());
            hdr.extend_from_slice(&fourcc(b"mdat").to_be_bytes());
            hdr.extend_from_slice(&(payload_size + 16).to_be_bytes());
        } else {
            hdr.extend_from_slice(&((payload_size + 8) as u32).to_be_bytes());
            hdr.extend_from_slice(&fourcc(b"mdat").to_be_bytes());
        }
        seek_write(mdat_header_at, &hdr)?;
        let free = |at: u64, len: u64| -> Option<(u64, [u8; 8])> {
            if len == 0 {
                return None;
            }
            let mut b = [0u8; 8];
            b[..4].copy_from_slice(&(len as u32).to_be_bytes());
            b[4..].copy_from_slice(&fourcc(b"free").to_be_bytes());
            Some((at, b))
        };
        let gap = if moov_front {
            seek_write(ftyp_end, &new_moov)?;
            free(ftyp_end + moov_len, mdat_header_at - (ftyp_end + moov_len))
        } else {
            seek_write(mdat_end, &new_moov)?;
            free(ftyp_end, mdat_header_at - ftyp_end)
        };
        if let Some((at, b)) = gap {
            if mdat_header_at - at < 8 {
                return fail("internal: free gap too small");
            }
            seek_write(at, &b)?;
        }
        file.sync_all()?;
        drop(file);
        std::fs::rename(&self.tmp_path, &self.path)
            .map_err(|e| Error(format!("rename failed {}: {e}", self.path.display())))?;
        self.finished = true;
        Ok(if moov_front { mdat_end } else { mdat_end + moov_len })
    }
}

impl Drop for OutputWriter<'_> {
    fn drop(&mut self) {
        // Not finished (error or cancel): close and discard the partial output.
        self.file.take();
        if !self.finished {
            let _ = std::fs::remove_file(&self.tmp_path);
        }
    }
}

/// Re-serializes a box tree from `src`, substituting replacement boxes keyed by offset.
fn serialize(b: &BoxNode, src: &[u8], repl: &HashMap<u64, Vec<u8>>, out: &mut Vec<u8>) {
    if let Some(r) = repl.get(&b.offset) {
        out.extend_from_slice(r);
        return;
    }
    if b.children.is_empty() || !is_container(b.kind) {
        out.extend_from_slice(&src[b.offset as usize..(b.offset + b.size) as usize]);
        return;
    }
    let start = out.len();
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&b.kind.to_be_bytes());
    // Container boxes here never use large sizes; children follow the 8-byte header.
    for c in &b.children {
        serialize(c, src, repl, out);
    }
    let n = (out.len() - start) as u32;
    wr32(out, start, n);
}

/// The `trak` boxes of a moov tree that [`Movie::parse`] lists as tracks, in the same
/// order (so index `i` here is `movie.tracks[i]`).
fn track_boxes(mv: &BoxNode) -> Vec<&BoxNode> {
    mv.children
        .iter()
        .filter(|b| b.kind == fourcc(b"trak"))
        .filter(|t| {
            find(&t.children, b"tkhd").is_some()
                && find_path(t, &[b"mdia", b"hdlr"]).is_some()
                && find_path(t, &[b"mdia", b"minf", b"stbl"]).is_some()
        })
        .collect()
}

/// Pairs each track of `template` with the first unused track of `source` that has the
/// same handler (vide, soun, tmcd): (template track, source track).
pub fn track_pairs(template: &Movie, source: &Movie) -> Vec<(usize, usize)> {
    let mut used = vec![false; source.tracks.len()];
    let mut out = Vec::new();
    for (ti, t) in template.tracks.iter().enumerate() {
        if let Some(si) = (0..source.tracks.len()).find(|&i| !used[i] && source.tracks[i].handler == t.handler) {
            used[si] = true;
            out.push((ti, si));
        }
    }
    out
}

/// Duration field of an mvhd / tkhd / mdhd box: (offset, is 64-bit). `at` is the box offset.
fn duration_field(buf: &[u8], b: &BoxNode) -> (usize, bool) {
    let p = b.payload() as usize;
    let v1 = buf[p] == 1;
    let k = b.kind;
    let off = if k == fourcc(b"tkhd") {
        if v1 {
            p + 28
        } else {
            p + 20
        }
    } else if v1 {
        p + 24
    } else {
        p + 16
    };
    (off, v1)
}

fn read_duration(buf: &[u8], b: &BoxNode) -> u64 {
    let (o, w) = duration_field(buf, b);
    if w {
        rd64(buf, o)
    } else {
        rd32(buf, o) as u64
    }
}

fn write_duration(buf: &mut [u8], b: &BoxNode, v: u64) {
    let (o, w) = duration_field(buf, b);
    if w {
        buf[o..o + 8].copy_from_slice(&v.to_be_bytes());
    } else {
        wr32(buf, o, v.min(u32::MAX as u64) as u32);
    }
}

/// Timescale of an mvhd / mdhd box.
fn timescale_field(buf: &[u8], b: &BoxNode) -> usize {
    let p = b.payload() as usize;
    if buf[p] == 1 {
        p + 20
    } else {
        p + 12
    }
}

/// A new moov built from `template`'s (sample descriptions, track headers, everything the
/// decoders know a camera file by) with the timing and sample tables of `source`: every
/// paired track takes the source track's stts and stsc (and, but for video, its stsz),
/// durations, timescales and edit list; video takes `video_sizes`; chunk offsets are
/// `chunk_offsets[source track]`. Template tracks without a source track are dropped.
/// `udta` replaces moov/udta; `codec` is the video sample entry's fourcc.
pub fn moov_from_template(
    template: &Movie,
    source: &Movie,
    video_sizes: &[u32],
    chunk_offsets: &[Vec<u64>],
    udta: &[u8],
    codec: u32,
) -> Result<Vec<u8>> {
    let mut moov = template.moov.clone();
    let tt = parse_boxes(&moov, 0, moov.len() as u64)?;
    let st = parse_boxes(&source.moov, 0, source.moov.len() as u64)?;
    let (tmv, smv) = (&tt[0], &st[0]);
    let src = &source.moov;
    let mut repl: HashMap<u64, Vec<u8>> = HashMap::new();
    let whole = |b: &BoxNode| src[b.offset as usize..(b.offset + b.size) as usize].to_vec();

    if let (Some(a), Some(b)) = (find(&tmv.children, b"mvhd"), find(&smv.children, b"mvhd")) {
        let (ta, tb) = (timescale_field(&moov, a), timescale_field(src, b));
        let ts = rd32(src, tb);
        wr32(&mut moov, ta, ts);
        let d = read_duration(src, b);
        write_duration(&mut moov, a, d);
    }
    let (tb, sb) = (track_boxes(tmv), track_boxes(smv));
    let pairs = track_pairs(template, source);
    for (ti, trak) in tb.iter().enumerate() {
        let Some(&(_, si)) = pairs.iter().find(|p| p.0 == ti) else {
            repl.insert(trak.offset, Vec::new());
            continue;
        };
        let strak = sb[si];
        let is_video = ti == template.video_track;
        if let (Some(a), Some(b)) = (find(&trak.children, b"tkhd"), find(&strak.children, b"tkhd")) {
            let d = read_duration(src, b);
            write_duration(&mut moov, a, d);
        }
        if let (Some(a), Some(b)) = (find_path(trak, &[b"mdia", b"mdhd"]), find_path(strak, &[b"mdia", b"mdhd"])) {
            let ts = rd32(src, timescale_field(src, b));
            let o = timescale_field(&moov, a);
            wr32(&mut moov, o, ts);
            let d = read_duration(src, b);
            write_duration(&mut moov, a, d);
        }
        match (find(&trak.children, b"edts"), find(&strak.children, b"edts")) {
            (Some(a), Some(b)) => {
                repl.insert(a.offset, whole(b));
            }
            (Some(a), None) => {
                repl.insert(a.offset, Vec::new());
            }
            _ => {}
        }
        let (tstbl, sstbl) = (
            find_path(trak, &[b"mdia", b"minf", b"stbl"]).unwrap(),
            find_path(strak, &[b"mdia", b"minf", b"stbl"]).unwrap(),
        );
        let mut placed_co = false;
        for c in &tstbl.children {
            let k = &c.kind.to_be_bytes();
            match k {
                b"stts" | b"stsc" => match find(&sstbl.children, &[k[0], k[1], k[2], k[3]]) {
                    Some(s) => {
                        repl.insert(c.offset, whole(s));
                    }
                    None => return fail(format!("source track without {}", fourcc_str(c.kind))),
                },
                b"stsz" => {
                    let b = if is_video {
                        let mut stsz = Vec::new();
                        let constant = !video_sizes.is_empty() && video_sizes.iter().all(|&s| s == video_sizes[0]);
                        let first = if constant { video_sizes[0] } else { 0 };
                        for v in [0, fourcc(b"stsz"), 0, first, video_sizes.len() as u32] {
                            stsz.extend_from_slice(&v.to_be_bytes());
                        }
                        if !constant {
                            for &s in video_sizes {
                                stsz.extend_from_slice(&s.to_be_bytes());
                            }
                        }
                        let n = stsz.len() as u32;
                        wr32(&mut stsz, 0, n);
                        stsz
                    } else {
                        match find(&sstbl.children, b"stsz") {
                            Some(s) => whole(s),
                            None => return fail("source track without stsz"),
                        }
                    };
                    repl.insert(c.offset, b);
                }
                b"stco" | b"co64" => {
                    if placed_co {
                        repl.insert(c.offset, Vec::new());
                        continue;
                    }
                    placed_co = true;
                    let offs = &chunk_offsets[si];
                    let mut co = Vec::with_capacity(16 + 8 * offs.len());
                    for v in [0, fourcc(b"co64"), 0, offs.len() as u32] {
                        co.extend_from_slice(&v.to_be_bytes());
                    }
                    for &o in offs {
                        co.extend_from_slice(&o.to_be_bytes());
                    }
                    let n = co.len() as u32;
                    wr32(&mut co, 0, n);
                    repl.insert(c.offset, co);
                }
                // Tables that would describe the template's own samples.
                b"stss" | b"ctts" | b"sdtp" | b"stps" | b"cslg" => {
                    repl.insert(c.offset, Vec::new());
                }
                _ => {}
            }
        }
    }
    let se = template.video().stsd_entry_offset;
    wr32(&mut moov, se + 4, codec);
    match find(&tmv.children, b"udta") {
        Some(b) => {
            repl.insert(b.offset, udta.to_vec());
        }
        None => return fail("template has no udta"),
    }
    let mut out = Vec::with_capacity(moov.len() + 64 * 1024);
    serialize(tmv, &moov, &repl, &mut out);
    Ok(out)
}

/// A new file from scratch: the template's ftyp, room for the moov, then the media data in
/// the order it is written; [`RawWriter::finish`] places the moov (in front when it fits)
/// and the mdat header. Dropping an unfinished writer removes the file.
pub struct RawWriter {
    tmp_path: PathBuf,
    file: Option<crate::drive::OutFile>,
    pos: u64,
    ftyp_end: u64,
    mdat_offset: u64,
    finished: bool,
    buf: Vec<u8>,
}

impl RawWriter {
    pub fn create(tmp_path: &Path, template_input: &InputFile, template: &Movie, source: &Movie) -> Result<RawWriter> {
        let file = crate::drive::OutFile::create(tmp_path)?;
        let mut w = RawWriter {
            tmp_path: tmp_path.to_path_buf(),
            file: Some(file),
            pos: 0,
            ftyp_end: 0,
            mdat_offset: 0,
            finished: false,
            buf: Vec::new(),
        };
        let ftyp = find(&template.top, b"ftyp").ok_or_else(|| Error("template has no ftyp".into()))?;
        let fb = template_input.read(ftyp.offset, ftyp.size as usize)?;
        w.write(&fb)?;
        w.ftyp_end = w.pos;
        let samples: usize = source.tracks.iter().map(|t| t.samples.len() + t.chunks.len()).sum();
        let est = (template.moov.len() + source.moov.len() + 12 * samples + 64 * 1024) as u64;
        let start = (w.pos + est + 4095) & !4095;
        let zeros = vec![0u8; (start - w.pos) as usize];
        w.write(&zeros)?;
        w.mdat_offset = start;
        Ok(w)
    }

    pub fn pos(&self) -> u64 {
        self.pos
    }

    pub fn write(&mut self, p: &[u8]) -> Result<()> {
        let f = self.file.as_mut().unwrap();
        f.write(p)?;
        self.pos += p.len() as u64;
        Ok(())
    }

    /// Copies `size` bytes at `offset` of `input`.
    pub fn copy_from(&mut self, input: &InputFile, offset: u64, size: u64) -> Result<()> {
        let mut buf = std::mem::take(&mut self.buf);
        buf.resize(size.min(8 << 20) as usize, 0);
        let mut done = 0u64;
        while done < size {
            let n = (buf.len() as u64).min(size - done) as usize;
            input.read_into(offset + done, &mut buf[..n])?;
            self.write(&buf[..n])?;
            done += n as u64;
        }
        self.buf = buf;
        Ok(())
    }

    /// Writes `moov` and the mdat header, then renames the file to `path`. Returns the size.
    pub fn finish(mut self, moov: &[u8], path: &Path) -> Result<u64> {
        let mdat_end = self.pos;
        let payload = mdat_end - self.mdat_offset;
        let large = payload + 8 > 0xffff_ffff;
        let hdr_at = self.mdat_offset - if large { 16 } else { 8 };
        let front = self.ftyp_end + moov.len() as u64 + 8 <= hdr_at;
        let mut file = self.file.take().unwrap();
        file.flush()?;
        let mut at = |o: u64, p: &[u8]| -> Result<()> { file.write_at(o, p) };
        let mut h = Vec::new();
        if large {
            h.extend_from_slice(&1u32.to_be_bytes());
            h.extend_from_slice(b"mdat");
            h.extend_from_slice(&(payload + 16).to_be_bytes());
        } else {
            h.extend_from_slice(&((payload + 8) as u32).to_be_bytes());
            h.extend_from_slice(b"mdat");
        }
        at(hdr_at, &h)?;
        let (gap_at, gap_len) = if front {
            at(self.ftyp_end, moov)?;
            (self.ftyp_end + moov.len() as u64, hdr_at - (self.ftyp_end + moov.len() as u64))
        } else {
            at(mdat_end, moov)?;
            (self.ftyp_end, hdr_at - self.ftyp_end)
        };
        if gap_len > 0 {
            if gap_len < 8 {
                return fail("internal: free gap too small");
            }
            let mut b = (gap_len as u32).to_be_bytes().to_vec();
            b.extend_from_slice(b"free");
            at(gap_at, &b)?;
        }
        file.sync_all()?;
        drop(file);
        std::fs::rename(&self.tmp_path, path).map_err(|e| Error(format!("rename failed {}: {e}", path.display())))?;
        self.finished = true;
        Ok(if front { mdat_end } else { mdat_end + moov.len() as u64 })
    }
}

impl Drop for RawWriter {
    fn drop(&mut self) {
        self.file.take();
        if !self.finished {
            let _ = std::fs::remove_file(&self.tmp_path);
        }
    }
}

#[cfg(test)]
mod read_retry_tests {
    use super::*;
    use std::io;

    /// A fake drive over `data`: `script` decides what each read call does in turn
    /// (None = serve normally, Some(Err) = fail, Some(Ok(0)) = empty read). Reads past the
    /// script are served normally; `short` caps every served read.
    struct Fake {
        data: Vec<u8>,
        script: Vec<Option<io::Result<usize>>>,
        calls: usize,
        short: usize,
        reopens: usize,
        slept: Vec<Duration>,
    }

    impl Fake {
        fn new(script: Vec<Option<io::Result<usize>>>) -> Fake {
            Fake {
                data: (0..=255u8).cycle().take(10_000).collect(),
                script,
                calls: 0,
                short: usize::MAX,
                reopens: 0,
                slept: Vec::new(),
            }
        }

        fn run(&mut self, offset: u64, n: usize) -> Result<Vec<u8>> {
            let mut dst = vec![0u8; n];
            let data = self.data.clone();
            let (script, calls, short) = (&mut self.script, &mut self.calls, self.short);
            let reopens = &mut self.reopens;
            let slept = &mut self.slept;
            read_retrying(
                &mut dst,
                offset,
                &READ_RETRY_DELAYS,
                &mut |buf, at| {
                    let step = script.get_mut(*calls).and_then(|s| s.take());
                    *calls += 1;
                    match step {
                        Some(r) => r,
                        None => {
                            let n = buf.len().min(short);
                            buf[..n].copy_from_slice(&data[at as usize..at as usize + n]);
                            Ok(n)
                        }
                    }
                },
                &mut || {
                    *reopens += 1;
                    Ok(())
                },
                &mut |d| slept.push(d),
                &|| "CLIP.R3D".to_string(),
            )?;
            Ok(dst)
        }
    }

    fn os_err(code: i32) -> Option<io::Result<usize>> {
        Some(Err(io::Error::from_raw_os_error(code)))
    }

    #[test]
    fn transient_errors_are_retried_on_a_new_handle() {
        // Sharing violation (32 on Windows; any OS error here), a resource error, an empty read.
        let mut f = Fake::new(vec![os_err(32), os_err(1450), Some(Ok(0))]);
        let got = f.run(100, 5000).unwrap();
        assert_eq!(got, f.data[100..5100]);
        assert_eq!(f.reopens, 3);
        assert_eq!(f.slept, READ_RETRY_DELAYS[..3].to_vec());
    }

    #[test]
    fn short_reads_are_not_errors() {
        let mut f = Fake::new(vec![]);
        f.short = 7;
        assert_eq!(f.run(3, 1000).unwrap(), f.data[3..1003]);
        assert_eq!(f.reopens, 0);
        assert!(f.slept.is_empty());
    }

    #[test]
    fn interrupted_reads_retry_at_once() {
        let mut f = Fake::new(vec![Some(Err(io::Error::from(io::ErrorKind::Interrupted)))]);
        assert_eq!(f.run(0, 10).unwrap(), f.data[..10]);
        assert!(f.slept.is_empty());
    }

    #[test]
    fn progress_resets_the_retry_budget() {
        // Fail almost every attempt, but make progress in between: never gives up.
        let mut script = Vec::new();
        for _ in 0..5 {
            for _ in 0..READ_RETRY_DELAYS.len() {
                script.push(os_err(5));
            }
            script.push(None);
        }
        let mut f = Fake::new(script);
        f.short = 100;
        assert_eq!(f.run(0, 500).unwrap(), f.data[..500]);
    }

    #[test]
    fn a_dead_drive_fails_with_the_os_error_text() {
        let mut f = Fake::new((0..20).map(|_| os_err(21)).collect());
        let e = f.run(4096, 10).unwrap_err();
        assert!(is_read_error(&e), "{e}");
        let os = io::Error::from_raw_os_error(21).to_string();
        assert!(e.0.contains(&os), "{e} lacks {os}");
        assert!(e.0.contains("CLIP.R3D at byte 4096"), "{e}");
        assert!(e.0.contains("after 7 retries"), "{e}");
        assert_eq!(f.calls, READ_RETRY_DELAYS.len() + 1);
        assert_eq!(f.reopens, READ_RETRY_DELAYS.len());
    }

    #[test]
    fn large_reads_are_split() {
        let mut f = Fake::new(vec![]);
        f.data = vec![7u8; MAX_READ * 2 + 10];
        let mut sizes = Vec::new();
        let mut dst = vec![0u8; MAX_READ * 2 + 10];
        read_retrying(
            &mut dst,
            0,
            &READ_RETRY_DELAYS,
            &mut |buf, _| {
                sizes.push(buf.len());
                buf.fill(7);
                Ok(buf.len())
            },
            &mut || Ok(()),
            &mut |_| {},
            &|| String::new(),
        )
        .unwrap();
        assert_eq!(sizes, vec![MAX_READ, MAX_READ, 10]);
    }

    #[test]
    fn a_real_file_reopens_and_reads() {
        let dir = std::env::temp_dir().join(format!("bc-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("clip.bin");
        std::fs::write(&p, (0..=255u8).collect::<Vec<_>>()).unwrap();
        let f = InputFile::open(&p).unwrap();
        assert_eq!(f.read(10, 5).unwrap(), vec![10, 11, 12, 13, 14]);
        assert!(source_reachable(&p));
        assert!(!source_reachable(&dir.join("gone.bin")));
        let e = InputFile::open(&dir.join("gone.bin")).err().unwrap();
        assert!(e.0.starts_with("cannot open"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
