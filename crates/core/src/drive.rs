//! Drive protection: adaptive I/O throttling and retrying output writes.
//!
//! Every clip read ([`crate::bmff::InputFile`]) and output write ([`OutFile`]) goes
//! through one process wide [`Throttle`]. It times each operation, keeps a baseline of how
//! long a megabyte normally takes, and treats the drive as degraded when operations get
//! much slower than that (a latency spike, throughput collapsing, or a failed operation).
//! Degraded: it halves how many operations may run at once and adds a pause before each
//! one (multiplicative decrease). After a run of normal operations it removes the pause
//! step by step, then allows one more operation at a time (additive increase), until it is
//! back to full speed. Workers that read ahead also hold fewer frames while throttled, and
//! output buffers flush in smaller pieces.
//!
//! Off with `BLUECOMPRESS_NO_THROTTLE=1` or [`set_enabled`]`(false)` (the CLI's
//! `--no-throttle`); retries stay on either way.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::bmff::READ_RETRY_DELAYS;
use crate::{Error, Result};

/// Most operations allowed at once when the drive is healthy.
pub const MAX_CONCURRENCY: usize = 16;
/// Longest pause added before an operation.
const MAX_PACE: Duration = Duration::from_millis(400);
/// Normal operations needed before easing off one step.
const RECOVER_AFTER: u32 = 24;
/// At most one decrease per this long, so one slow burst is not counted many times.
const DECREASE_GAP: Duration = Duration::from_millis(500);
/// Operations smaller than this are timed as if they were this big (fixed costs dominate).
const MIN_COST_BYTES: f64 = 256.0 * 1024.0;

#[derive(Debug)]
struct State {
    limit: usize,
    in_flight: usize,
    pace: Duration,
    /// Seconds per MiB: a smoothed average, and the normal level (a slowly rising floor).
    ewma: Option<f64>,
    baseline: Option<f64>,
    good: u32,
    last_decrease: Option<Instant>,
}

/// Adaptive limit on concurrent drive operations (AIMD). See the module docs.
#[derive(Debug)]
pub struct Throttle {
    max: usize,
    /// Shortest operation that counts as a spike, and as part of a sustained slowdown.
    /// Cached reads make the normal level very low; these keep a plain disk read (tens of
    /// milliseconds) from looking like trouble.
    spike_floor: f64,
    sustained_floor: f64,
    enabled: AtomicBool,
    state: Mutex<State>,
    cv: Condvar,
    /// Number of times the drive was found degraded.
    pub degraded_events: AtomicU64,
}

impl Throttle {
    pub fn new(max: usize) -> Throttle {
        Throttle::with_floors(max, Duration::from_millis(250), Duration::from_millis(100))
    }

    /// [`Throttle::new`] with other latency floors (tests run faster drives).
    pub fn with_floors(max: usize, spike: Duration, sustained: Duration) -> Throttle {
        Throttle {
            max: max.max(1),
            spike_floor: spike.as_secs_f64(),
            sustained_floor: sustained.as_secs_f64(),
            enabled: AtomicBool::new(true),
            state: Mutex::new(State {
                limit: max.max(1),
                in_flight: 0,
                pace: Duration::ZERO,
                ewma: None,
                baseline: None,
                good: 0,
                last_decrease: None,
            }),
            cv: Condvar::new(),
            degraded_events: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::SeqCst);
        if !on {
            let mut s = self.lock();
            s.limit = self.max;
            s.pace = Duration::ZERO;
            s.good = 0;
        }
        self.cv.notify_all();
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// True while easing off (fewer operations at once, or a pause before each).
    pub fn active(&self) -> bool {
        let s = self.lock();
        s.limit < self.max || !s.pace.is_zero()
    }

    /// Operations currently allowed at once.
    pub fn limit(&self) -> usize {
        self.lock().limit
    }

    pub fn pace(&self) -> Duration {
        self.lock().pace
    }

    /// Runs one drive operation of about `bytes` under the throttle, and learns from how
    /// long it took and whether it failed.
    pub fn run<T>(&self, bytes: usize, op: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        if !self.enabled() {
            return op();
        }
        let pace = {
            let mut s = self.lock();
            while s.in_flight >= s.limit && self.enabled() {
                s = self.cv.wait_timeout(s, Duration::from_millis(100)).unwrap_or_else(|p| p.into_inner()).0;
            }
            s.in_flight += 1;
            s.pace
        };
        if !pace.is_zero() {
            std::thread::sleep(pace);
        }
        let t = Instant::now();
        let r = op();
        let dt = t.elapsed();
        {
            let mut s = self.lock();
            s.in_flight -= 1;
        }
        self.observe(bytes, dt, r.is_ok());
        self.cv.notify_all();
        r
    }

    /// Learns from one operation (public within the crate for tests).
    pub(crate) fn observe(&self, bytes: usize, dt: Duration, ok: bool) {
        let secs = dt.as_secs_f64();
        let cost = secs / (bytes as f64).max(MIN_COST_BYTES) * 1_048_576.0;
        let mut s = self.lock();
        let base = *s.baseline.get_or_insert(cost);
        let ewma = s.ewma.map_or(cost, |e| 0.8 * e + 0.2 * cost);
        s.ewma = Some(ewma);
        // Spikes need some absolute size too: a 1 ms read three times slower is no problem.
        let spike = cost > 8.0 * base && secs > self.spike_floor;
        let sustained = ewma > 4.0 * base && secs > self.sustained_floor;
        let stalled = secs > 2.0;
        let degraded = !ok || spike || sustained || stalled;
        if degraded {
            s.good = 0;
            let now = Instant::now();
            if s.last_decrease.is_none_or(|t| now - t >= DECREASE_GAP) {
                s.last_decrease = Some(now);
                s.limit = (s.limit / 2).max(1);
                s.pace = if s.pace.is_zero() { Duration::from_millis(10) } else { (s.pace * 2).min(MAX_PACE) };
                self.degraded_events.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            // The normal level follows faster operations quickly and slower ones slowly, so
            // a drive that is simply slower than at the start becomes the new normal, and a
            // run of cached reads does not make every real read look slow.
            s.baseline = Some(if cost < base { base * 0.9 + cost * 0.1 } else { base * 0.995 + cost * 0.005 });
            s.good += 1;
            if s.good >= RECOVER_AFTER {
                s.good = 0;
                if !s.pace.is_zero() {
                    s.pace /= 2;
                    if s.pace < Duration::from_millis(2) {
                        s.pace = Duration::ZERO;
                    }
                } else if s.limit < self.max {
                    s.limit += 1;
                }
            }
        }
    }
}

/// The throttle shared by all jobs in this process.
pub fn global() -> &'static Throttle {
    static T: OnceLock<Throttle> = OnceLock::new();
    T.get_or_init(|| {
        let t = Throttle::new(MAX_CONCURRENCY);
        let off = std::env::var("BLUECOMPRESS_NO_THROTTLE")
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false);
        t.set_enabled(!off);
        t
    })
}

/// Turns adaptive throttling on or off for this process.
pub fn set_enabled(on: bool) {
    global().set_enabled(on);
}

/// True while the drive throttle is easing off.
pub fn throttling() -> bool {
    global().active()
}

/// How many frames a reader with `normal` frames in flight may hold now.
pub fn read_ahead(normal: usize) -> usize {
    let t = global();
    if !t.enabled() {
        return normal;
    }
    (normal * t.limit()).div_ceil(t.max).max(1)
}

/// Largest single write handed to the OS.
const MAX_WRITE: usize = 4 << 20;

/// True for errors that waiting will not fix: the output drive is full.
fn is_disk_full(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(28) if cfg!(unix)) || matches!(e.raw_os_error(), Some(39 | 112) if cfg!(windows))
}

/// Start of the message of a write that failed after every retry.
pub const WRITE_ERROR: &str = "write error in ";

/// The retry loop of positional writes, separate so tests can drive it. `write(buf, at)`
/// is one positional write, `reopen()` replaces the handle.
pub(crate) fn write_retrying(
    src: &[u8],
    offset: u64,
    delays: &[Duration],
    write: &mut dyn FnMut(&[u8], u64) -> io::Result<usize>,
    reopen: &mut dyn FnMut() -> io::Result<()>,
    sleep: &mut dyn FnMut(Duration),
    describe: &dyn Fn() -> String,
) -> Result<()> {
    let mut done = 0;
    let mut attempt = 0;
    while done < src.len() {
        let at = offset + done as u64;
        let end = src.len().min(done + MAX_WRITE);
        let problem = match write(&src[done..end], at) {
            Ok(0) => "the drive accepted no data".to_string(),
            Ok(n) => {
                done += n;
                attempt = 0;
                continue;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if is_disk_full(&e) => {
                return Err(Error(format!("{WRITE_ERROR}{}: the output drive is full ({e})", describe())))
            }
            Err(e) => e.to_string(),
        };
        if attempt >= delays.len() {
            return Err(Error(format!(
                "{WRITE_ERROR}{} at byte {at}: {problem} (after {attempt} retries)",
                describe()
            )));
        }
        sleep(delays[attempt]);
        attempt += 1;
        let _ = reopen();
    }
    Ok(())
}

fn pwrite(f: &File, buf: &[u8], at: u64) -> io::Result<usize> {
    #[cfg(unix)]
    return std::os::unix::fs::FileExt::write_at(f, buf, at);
    #[cfg(windows)]
    return std::os::windows::fs::FileExt::seek_write(f, buf, at);
}

/// An output file written at explicit positions, so a failed write can be retried on a
/// reopened handle without losing its place. Appends are buffered (less while throttled).
pub struct OutFile {
    path: PathBuf,
    file: File,
    buf: Vec<u8>,
    buf_at: u64,
}

impl OutFile {
    pub fn create(path: &Path) -> Result<OutFile> {
        let file = File::create(path).map_err(|e| Error(format!("cannot create {}: {e}", path.display())))?;
        Ok(OutFile { path: path.to_path_buf(), file, buf: Vec::with_capacity(8 << 20), buf_at: 0 })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bytes written or buffered so far (the next append position).
    pub fn len(&self) -> u64 {
        self.buf_at + self.buf.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Appends `p`.
    pub fn write(&mut self, p: &[u8]) -> Result<()> {
        self.buf.extend_from_slice(p);
        let cap = if throttling() { 1 << 20 } else { 8 << 20 };
        if self.buf.len() >= cap {
            self.flush()?;
        }
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let buf = std::mem::take(&mut self.buf);
        let r = self.write_at_unbuffered(self.buf_at, &buf);
        self.buf_at += buf.len() as u64;
        self.buf = buf;
        self.buf.clear();
        r
    }

    /// Writes `p` at `at` (after flushing appends).
    pub fn write_at(&mut self, at: u64, p: &[u8]) -> Result<()> {
        self.flush()?;
        self.write_at_unbuffered(at, p)
    }

    fn write_at_unbuffered(&mut self, at: u64, p: &[u8]) -> Result<()> {
        let path = self.path.clone();
        let file = &mut self.file;
        let cell = std::cell::RefCell::new(file);
        write_retrying(
            p,
            at,
            &READ_RETRY_DELAYS,
            &mut |b, o| {
                let f = cell.borrow();
                global().run(b.len(), || pwrite(&f, b, o))
            },
            &mut || {
                let f = OpenOptions::new().write(true).open(&path)?;
                **cell.borrow_mut() = f;
                Ok(())
            },
            &mut std::thread::sleep,
            &|| path.display().to_string(),
        )
    }

    /// Flushes and makes the data durable, with the same retries.
    pub fn sync_all(&mut self) -> Result<()> {
        self.flush()?;
        let mut last = None;
        for d in std::iter::once(Duration::ZERO).chain(READ_RETRY_DELAYS.iter().copied()) {
            std::thread::sleep(d);
            match self.file.sync_all() {
                Ok(()) => return Ok(()),
                Err(e) => last = Some(e),
            }
        }
        Err(Error(format!(
            "{WRITE_ERROR}{}: {} (while flushing to the drive)",
            self.path.display(),
            last.map(|e| e.to_string()).unwrap_or_default()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const MB: usize = 1 << 20;

    #[test]
    fn healthy_drive_stays_at_full_speed() {
        let t = Throttle::new(8);
        for _ in 0..500 {
            t.observe(MB, Duration::from_millis(10), true);
        }
        assert!(!t.active());
        assert_eq!(t.limit(), 8);
    }

    #[test]
    fn latency_spike_halves_and_paces_then_ramps_back() {
        let t = Throttle::new(8);
        for _ in 0..50 {
            t.observe(MB, Duration::from_millis(10), true);
        }
        t.observe(MB, Duration::from_millis(400), true);
        assert_eq!(t.limit(), 4);
        assert_eq!(t.pace(), Duration::from_millis(10));
        assert!(t.active());
        // Stable again: the pause goes first, then one more operation at a time.
        let mut steps = Vec::new();
        for _ in 0..(RECOVER_AFTER as usize * 20) {
            t.observe(MB, Duration::from_millis(10), true);
            steps.push((t.limit(), t.pace()));
        }
        let first_raise = steps.iter().position(|s| s.0 > 4).unwrap();
        assert!(steps[..first_raise].iter().any(|s| s.1.is_zero()), "pace clears before the limit rises");
        assert!(steps.windows(2).all(|w| w[1].0 <= w[0].0 + 1), "additive increase");
        assert!(!t.active(), "back to full speed: {:?}", steps.last());
    }

    #[test]
    fn errors_and_stalls_count_as_degraded_and_decreases_are_spaced() {
        let t = Throttle::new(16);
        t.observe(MB, Duration::from_millis(5), true);
        t.observe(MB, Duration::from_millis(5), false);
        assert_eq!(t.limit(), 8);
        // A burst of slow operations right after counts once.
        t.observe(MB, Duration::from_secs(3), true);
        t.observe(MB, Duration::from_secs(3), true);
        assert_eq!(t.limit(), 8);
        assert_eq!(t.degraded_events.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn cached_reads_do_not_make_disk_reads_look_slow() {
        let t = Throttle::new(8);
        // Many cache hits (0.2 ms per MiB), then ordinary disk reads (40 ms for 6 MiB).
        for _ in 0..200 {
            t.observe(MB, Duration::from_micros(200), true);
        }
        for i in 0..200 {
            if i % 2 == 0 {
                t.observe(6 * MB, Duration::from_millis(40), true);
            } else {
                t.observe(MB, Duration::from_micros(200), true);
            }
        }
        assert!(!t.active());
    }

    #[test]
    fn small_fast_operations_do_not_trigger() {
        let t = Throttle::new(8);
        t.observe(4096, Duration::from_micros(50), true);
        // 20x slower but still only 1 ms: not a problem worth easing off for.
        for _ in 0..10 {
            t.observe(4096, Duration::from_millis(1), true);
        }
        assert!(!t.active());
    }

    #[test]
    fn a_drive_that_is_just_slower_becomes_the_new_normal() {
        let t = Throttle::new(8);
        for _ in 0..20 {
            t.observe(MB, Duration::from_millis(5), true);
        }
        // Twice as slow for good: below the trigger, so never throttled.
        for _ in 0..2000 {
            t.observe(MB, Duration::from_millis(10), true);
        }
        assert!(!t.active());
    }

    #[test]
    fn disabled_throttle_passes_through() {
        let t = Throttle::new(8);
        t.set_enabled(false);
        for _ in 0..10 {
            let _ = t.run(MB, || -> io::Result<()> { Err(io::Error::other("x")) });
        }
        assert!(!t.active());
    }

    /// A simulated drive that collapses when more than two operations overlap: each
    /// operation takes 2 ms alone and much longer per extra concurrent one. Eight threads
    /// hammer it; the throttle must bring concurrency down and keep it there.
    #[test]
    fn simulated_overloaded_drive_gets_less_concurrency() {
        let t = Arc::new(Throttle::with_floors(8, Duration::from_millis(20), Duration::from_millis(10)));
        let busy = Arc::new(AtomicU64::new(0));
        let peak_late = Arc::new(AtomicU64::new(0));
        let start = Instant::now();
        std::thread::scope(|s| {
            for _ in 0..8 {
                let (t, busy, peak_late) = (t.clone(), busy.clone(), peak_late.clone());
                s.spawn(move || {
                    for _ in 0..60 {
                        t.run(MB, || -> io::Result<()> {
                            let n = busy.fetch_add(1, Ordering::SeqCst) + 1;
                            if start.elapsed() > Duration::from_millis(600) {
                                peak_late.fetch_max(n, Ordering::SeqCst);
                            }
                            let ms = if n <= 2 { 2 } else { 2 + 40 * (n - 2) };
                            std::thread::sleep(Duration::from_millis(ms));
                            busy.fetch_sub(1, Ordering::SeqCst);
                            Ok(())
                        })
                        .unwrap();
                    }
                });
            }
        });
        assert!(t.degraded_events.load(Ordering::Relaxed) >= 1);
        assert!(peak_late.load(Ordering::SeqCst) <= 4, "late peak {}", peak_late.load(Ordering::SeqCst));
    }

    #[test]
    fn flaky_writes_are_retried_at_the_same_position() {
        let mut disk = [0u8; 64];
        let mut fails = 2;
        let mut reopens = 0;
        let mut slept = 0;
        write_retrying(
            &[1, 2, 3, 4],
            10,
            &READ_RETRY_DELAYS,
            &mut |b, at| {
                if fails > 0 {
                    fails -= 1;
                    return Err(io::Error::from_raw_os_error(5));
                }
                // Short write: one byte at a time.
                disk[at as usize] = b[0];
                Ok(1)
            },
            &mut || {
                reopens += 1;
                Ok(())
            },
            &mut |_| slept += 1,
            &|| "OUT.R3D".into(),
        )
        .unwrap();
        assert_eq!(&disk[10..14], &[1, 2, 3, 4]);
        assert_eq!((reopens, slept), (2, 2));
    }

    #[test]
    fn dead_output_drive_reports_the_os_error_and_full_drive_fails_at_once() {
        let e = write_retrying(
            &[1],
            0,
            &READ_RETRY_DELAYS,
            &mut |_, _| Err(io::Error::from_raw_os_error(5)),
            &mut || Ok(()),
            &mut |_| {},
            &|| "OUT.R3D".into(),
        )
        .unwrap_err();
        assert!(e.0.starts_with(WRITE_ERROR) && e.0.contains(&io::Error::from_raw_os_error(5).to_string()), "{e}");
        let full = if cfg!(windows) { 112 } else { 28 };
        let mut calls = 0;
        let e = write_retrying(
            &[1],
            0,
            &READ_RETRY_DELAYS,
            &mut |_, _| {
                calls += 1;
                Err(io::Error::from_raw_os_error(full))
            },
            &mut || Ok(()),
            &mut |_| {},
            &|| "OUT.R3D".into(),
        )
        .unwrap_err();
        assert_eq!(calls, 1);
        assert!(e.0.contains("output drive is full"), "{e}");
    }

    #[test]
    fn out_file_appends_and_patches() {
        let dir = std::env::temp_dir().join(format!("bc-out-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("o.bin");
        let mut f = OutFile::create(&p).unwrap();
        f.write(b"hello ").unwrap();
        f.write(b"world").unwrap();
        assert_eq!(f.len(), 11);
        f.write_at(0, b"J").unwrap();
        f.sync_all().unwrap();
        drop(f);
        assert_eq!(std::fs::read(&p).unwrap(), b"Jello world");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
