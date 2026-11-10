//! A consumer of the virtual camera across a daemon crash: the
//! `omarchy-broadcastd` binary on a private D-Bus daemon and the loopback
//! named by `OBC_TEST_SINK`, a raw V4L2 reader of that node in this process,
//! the daemon killed with SIGKILL and a new one started.
//!
//! Run with
//! `OBC_TEST_SINK=/dev/video11 cargo test -p omarchy-broadcastd --test crash -- --ignored
//! --nocapture --test-threads=1`.
//!
//! No camera and no GPU: the daemon starts `off` (its state file says
//! `enabled = false`), so it never opens a session; its loopback writer
//! sends black placeholders, which is all the reader needs. The test reads
//! the loopback's `keep_format`, `sustain_framerate` and `timeout` controls
//! before it starts and puts them back at the end, also when it fails (the
//! daemon's crash cover sets `timeout` and never resets it).
//! `timeout_image_io` is a button, with no value to read or restore, but
//! not stateless: pressing it leaves a device-wide timeout-image mode
//! pending until a descriptor takes the timeout image, which the cover does
//! at once. A cover that failed (or never reported) may have left it
//! pending, to catch the next program to open the node: then the test takes
//! it back at the end, as the cover itself does
//! (`obc_pipeline::loopback_cover::clear_timeout_image_io`).

mod common;

use std::io::BufRead;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use common::{DaemonEnv, Guard, PrivateBus};
use obc_ipc::OmarchyBroadcastProxy;
use obc_pipeline::controls::{get_ctrl, set_ctrl};
use obc_pipeline::loopback_cover::{LoopbackControls, clear_timeout_image_io};
use omarchy_broadcastd::consumers::{NodeId, count_holders};
use zbus::proxy::CacheProperties;

const IGNORE: &str = "needs OBC_TEST_SINK (a free v4l2loopback device with exclusive_caps=1, \
     v4l2loopback with the timeout controls) and /usr/bin/dbus-daemon; no camera, no GPU";

/// The longest a consumer may go without a frame: Chromium restarts its
/// capture after about 1 s without one, which fails while no writer
/// streams.
const MAX_GAP: Duration = Duration::from_millis(600);

/// How long the test leaves the loopback without a writer.
const OUTAGE: Duration = Duration::from_millis(1500);

/// The raw V4L2 capture interface the reader needs (64-bit layouts).
mod v4l2 {
    pub const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
    pub const MEMORY_MMAP: u32 = 1;
    pub const BUF_FLAG_TIMESTAMP_MONOTONIC: u32 = 0x2000;
    pub const BUF_FLAG_TIMESTAMP_COPY: u32 = 0x4000;

    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    pub struct PixFormat {
        pub width: u32,
        pub height: u32,
        pub pixelformat: u32,
        pub field: u32,
        pub bytesperline: u32,
        pub sizeimage: u32,
        pub colorspace: u32,
        pub priv_: u32,
        pub flags: u32,
        pub ycbcr_enc: u32,
        pub quantization: u32,
        pub xfer_func: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub union FormatUnion {
        pub pix: PixFormat,
        pub raw: [u8; 200],
        pub align: [libc::c_ulong; 0],
    }

    #[repr(C)]
    pub struct Format {
        pub type_: u32,
        pub fmt: FormatUnion,
    }

    #[repr(C)]
    pub struct RequestBuffers {
        pub count: u32,
        pub type_: u32,
        pub memory: u32,
        pub capabilities: u32,
        pub flags: u8,
        pub reserved: [u8; 3],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub union BufferM {
        pub offset: u32,
        pub userptr: libc::c_ulong,
        pub fd: i32,
    }

    #[repr(C)]
    pub struct Buffer {
        pub index: u32,
        pub type_: u32,
        pub bytesused: u32,
        pub flags: u32,
        pub field: u32,
        pub timestamp: libc::timeval,
        pub timecode: [u32; 4],
        pub sequence: u32,
        pub memory: u32,
        pub m: BufferM,
        pub length: u32,
        pub reserved2: u32,
        pub request_fd: i32,
    }

    const _: () = assert!(std::mem::size_of::<Format>() == 208);
    const _: () = assert!(std::mem::size_of::<RequestBuffers>() == 20);
    const _: () = assert!(std::mem::size_of::<Buffer>() == 88);

    const fn ioc(dir: libc::c_ulong, nr: libc::c_ulong, size: usize) -> libc::c_ulong {
        (dir << 30) | ((size as libc::c_ulong) << 16) | ((b'V' as libc::c_ulong) << 8) | nr
    }

    pub const G_FMT: libc::c_ulong = ioc(3, 4, std::mem::size_of::<Format>());
    pub const REQBUFS: libc::c_ulong = ioc(3, 8, std::mem::size_of::<RequestBuffers>());
    pub const QUERYBUF: libc::c_ulong = ioc(3, 9, std::mem::size_of::<Buffer>());
    pub const QBUF: libc::c_ulong = ioc(3, 15, std::mem::size_of::<Buffer>());
    pub const DQBUF: libc::c_ulong = ioc(3, 17, std::mem::size_of::<Buffer>());
    pub const STREAMON: libc::c_ulong = ioc(1, 18, std::mem::size_of::<libc::c_int>());
    pub const STREAMOFF: libc::c_ulong = ioc(1, 19, std::mem::size_of::<libc::c_int>());

    /// One ioctl.
    ///
    /// # Safety
    /// `arg` must be the type `request` takes.
    pub unsafe fn ioctl<T>(
        fd: libc::c_int,
        request: libc::c_ulong,
        arg: &mut T,
    ) -> std::io::Result<()> {
        // SAFETY: the caller pairs `request` with its argument type.
        if unsafe { libc::ioctl(fd, request as _, arg as *mut T) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    /// An all-zero argument struct.
    ///
    /// # Safety
    /// All-zero bytes must be a valid `T` (plain C structs of integers).
    pub unsafe fn zeroed<T>() -> T {
        // SAFETY: guaranteed by the caller.
        unsafe { std::mem::zeroed() }
    }

    #[test]
    fn ioctl_numbers() {
        assert_eq!(G_FMT, 0xc0d0_5604);
        assert_eq!(REQBUFS, 0xc014_5608);
        assert_eq!(QUERYBUF, 0xc058_5609);
        assert_eq!(QBUF, 0xc058_560f);
        assert_eq!(DQBUF, 0xc058_5611);
        assert_eq!(STREAMON, 0x4004_5612);
        assert_eq!(STREAMOFF, 0x4004_5613);
    }
}

/// `CLOCK_MONOTONIC` now, in nanoseconds.
fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a valid `struct timespec` for the call.
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) },
        0
    );
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// One frame the reader dequeued.
#[derive(Debug, Clone)]
struct Seen {
    at: Instant,
    /// `CLOCK_MONOTONIC` when it was dequeued.
    mono_ns: u64,
    /// The buffer's timestamp, in nanoseconds.
    ts_ns: u64,
    flags: u32,
    /// Sampled luma and chroma bytes: every one 16 / 128 (limited black).
    black: bool,
    /// The first luma and chroma bytes.
    y: u8,
    chroma: u8,
}

/// A mapped capture buffer.
struct Mapped {
    ptr: *mut libc::c_void,
    len: usize,
}

impl Drop for Mapped {
    fn drop(&mut self) {
        // SAFETY: mapped once by the reader, unmapped once.
        unsafe {
            libc::munmap(self.ptr, self.len);
        }
    }
}

/// A V4L2 mmap reader of the loopback on its own thread: every frame it
/// dequeues is recorded, and the first error ends it.
struct Reader {
    stop: Arc<AtomicBool>,
    frames: Arc<Mutex<Vec<Seen>>>,
    thread: Option<JoinHandle<Result<(), String>>>,
}

impl Reader {
    fn start(node: &Path) -> Reader {
        let stop = Arc::new(AtomicBool::new(false));
        let frames: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let (s, f, node) = (Arc::clone(&stop), Arc::clone(&frames), node.to_path_buf());
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || read_loop(&node, &s, &f, &ready_tx));
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!("the V4L2 reader could not start: {e}"),
            Err(_) => panic!("the V4L2 reader did not start within 5 s"),
        }
        Reader {
            stop,
            frames,
            thread: Some(thread),
        }
    }

    fn frames(&self) -> Vec<Seen> {
        self.frames.lock().unwrap().clone()
    }

    fn wait(&self, limit: Duration, mut done: impl FnMut(&[Seen]) -> bool) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            if done(&self.frames.lock().unwrap()) {
                return true;
            }
            if Instant::now() > deadline || self.thread.as_ref().is_some_and(|t| t.is_finished()) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Stops and joins the thread; its error, if it ended on one.
    fn finish(mut self) -> Result<(), String> {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().map_or(Ok(()), |t| {
            t.join().unwrap_or(Err("the reader panicked".into()))
        })
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Opens `node`, maps its capture buffers, streams, and records every frame
/// until `stop`. Tells `ready` once streaming (or why it could not).
fn read_loop(
    node: &Path,
    stop: &AtomicBool,
    frames: &Mutex<Vec<Seen>>,
    ready: &std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    let setup = || -> Result<(std::fs::File, v4l2::PixFormat, Vec<Mapped>), String> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(node)
            .map_err(|e| format!("open {}: {e}", node.display()))?;
        let fd = file.as_raw_fd();
        // SAFETY: plain C structs, valid all-zero; each ioctl gets its type.
        let pix = unsafe {
            let mut fmt: v4l2::Format = v4l2::zeroed();
            fmt.type_ = v4l2::BUF_TYPE_VIDEO_CAPTURE;
            v4l2::ioctl(fd, v4l2::G_FMT, &mut fmt).map_err(|e| format!("G_FMT: {e}"))?;
            fmt.fmt.pix
        };
        let mut req = v4l2::RequestBuffers {
            count: 4,
            type_: v4l2::BUF_TYPE_VIDEO_CAPTURE,
            memory: v4l2::MEMORY_MMAP,
            capabilities: 0,
            flags: 0,
            reserved: [0; 3],
        };
        // SAFETY: REQBUFS takes a `struct v4l2_requestbuffers`.
        unsafe { v4l2::ioctl(fd, v4l2::REQBUFS, &mut req) }.map_err(|e| format!("REQBUFS: {e}"))?;
        let mut maps = Vec::new();
        for index in 0..req.count {
            // SAFETY: a valid all-zero `struct v4l2_buffer`, then QUERYBUF
            // and QBUF with it; the mapping is the driver's buffer.
            unsafe {
                let mut b: v4l2::Buffer = v4l2::zeroed();
                b.index = index;
                b.type_ = v4l2::BUF_TYPE_VIDEO_CAPTURE;
                b.memory = v4l2::MEMORY_MMAP;
                v4l2::ioctl(fd, v4l2::QUERYBUF, &mut b).map_err(|e| format!("QUERYBUF: {e}"))?;
                let ptr = libc::mmap(
                    std::ptr::null_mut(),
                    b.length as usize,
                    libc::PROT_READ,
                    libc::MAP_SHARED,
                    fd,
                    libc::off_t::from(b.m.offset),
                );
                if ptr == libc::MAP_FAILED {
                    return Err(format!("mmap: {}", std::io::Error::last_os_error()));
                }
                maps.push(Mapped {
                    ptr,
                    len: b.length as usize,
                });
                v4l2::ioctl(fd, v4l2::QBUF, &mut b).map_err(|e| format!("QBUF: {e}"))?;
            }
        }
        let mut kind = v4l2::BUF_TYPE_VIDEO_CAPTURE as libc::c_int;
        // SAFETY: STREAMON takes an int.
        unsafe { v4l2::ioctl(fd, v4l2::STREAMON, &mut kind) }
            .map_err(|e| format!("STREAMON: {e}"))?;
        Ok((file, pix, maps))
    };
    let (file, pix, maps) = match setup() {
        Ok(s) => {
            let _ = ready.send(Ok(()));
            s
        }
        Err(e) => {
            let _ = ready.send(Err(e.clone()));
            return Err(e);
        }
    };
    let fd = file.as_raw_fd();
    let luma = pix.bytesperline as usize * pix.height as usize;
    let size = pix.sizeimage as usize;
    let res = loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        let n = unsafe { libc::poll(&mut pfd, 1, 50) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break Err(format!("poll: {e}"));
        }
        if n == 0 {
            continue;
        }
        if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            break Err(format!("poll revents {:#x}", pfd.revents));
        }
        // SAFETY: a valid all-zero `struct v4l2_buffer` for DQBUF.
        let mut b: v4l2::Buffer = unsafe { v4l2::zeroed() };
        b.type_ = v4l2::BUF_TYPE_VIDEO_CAPTURE;
        b.memory = v4l2::MEMORY_MMAP;
        // SAFETY: DQBUF takes a `struct v4l2_buffer`.
        if let Err(e) = unsafe { v4l2::ioctl(fd, v4l2::DQBUF, &mut b) } {
            if e.raw_os_error() == Some(libc::EAGAIN) {
                continue;
            }
            break Err(format!("DQBUF: {e}"));
        }
        let (at, mono_ns) = (Instant::now(), monotonic_ns());
        let Some(map) = maps.get(b.index as usize) else {
            break Err(format!("DQBUF gave buffer {} of {}", b.index, maps.len()));
        };
        // SAFETY: the driver's buffer, mapped for reading; the driver only
        // writes it while it is queued, and it is dequeued now.
        let data = unsafe { std::slice::from_raw_parts(map.ptr.cast::<u8>(), map.len) };
        if data.len() < size || luma >= size {
            break Err(format!(
                "a {}-byte buffer for a {size}-byte frame ({luma} luma bytes)",
                data.len()
            ));
        }
        let black = data[..luma].iter().step_by(61).all(|&v| v == 16)
            && data[luma..size].iter().step_by(61).all(|&v| v == 128);
        frames.lock().unwrap().push(Seen {
            at,
            mono_ns,
            ts_ns: b.timestamp.tv_sec as u64 * 1_000_000_000 + b.timestamp.tv_usec as u64 * 1000,
            flags: b.flags,
            black,
            y: data[0],
            chroma: data[luma],
        });
        // SAFETY: QBUF takes a `struct v4l2_buffer`.
        if let Err(e) = unsafe { v4l2::ioctl(fd, v4l2::QBUF, &mut b) } {
            break Err(format!("QBUF: {e}"));
        }
    };
    let mut kind = v4l2::BUF_TYPE_VIDEO_CAPTURE as libc::c_int;
    // SAFETY: STREAMOFF takes an int.
    let _ = unsafe { v4l2::ioctl(fd, v4l2::STREAMOFF, &mut kind) };
    drop(maps);
    drop(file);
    res
}

/// Puts the loopback's valued controls back when dropped, and takes back a
/// timeout-image mode that a failed cover may have left pending (see the
/// module docs).
struct RestoreControls {
    node: PathBuf,
    saved: Vec<(String, u32, i32)>,
    /// The daemons' logs, for their crash cover lines.
    logs: Vec<Arc<Mutex<Vec<String>>>>,
}

impl RestoreControls {
    fn save(node: &Path) -> RestoreControls {
        let found = LoopbackControls::query(node)
            .unwrap_or_else(|e| panic!("cannot query the controls of {}: {e}", node.display()));
        assert!(
            found.timeout.is_some() && found.timeout_image_io.is_some(),
            "{} has no v4l2loopback timeout controls: {found:?}. {IGNORE}",
            node.display()
        );
        let saved: Vec<(String, u32, i32)> =
            [found.keep_format, found.sustain_framerate, found.timeout]
                .into_iter()
                .flatten()
                .map(|c| {
                    let v = get_ctrl(node, c.id).unwrap();
                    (c.info.name, c.id, v)
                })
                .collect();
        eprintln!(
            "loopback controls before: {:?}; timeout_image_io is a button (no value)",
            saved
                .iter()
                .map(|(n, _, v)| format!("{n}={v}"))
                .collect::<Vec<_>>()
        );
        RestoreControls {
            node: node.to_path_buf(),
            saved,
            logs: Vec::new(),
        }
    }

    /// Watches a daemon's log for its crash cover outcome.
    fn watch(&mut self, log: &Arc<Mutex<Vec<String>>>) {
        self.logs.push(Arc::clone(log));
    }

    /// A daemon whose cover failed, or never reported: it may have left
    /// the timeout-image mode pending.
    fn cover_may_have_failed(&self) -> bool {
        self.logs.iter().any(|log| {
            let log = log.lock().unwrap();
            log.iter().any(|l| l.contains("crash cover not in place"))
                || !log.iter().any(|l| l.contains("crash cover: "))
        })
    }
}

impl Drop for RestoreControls {
    fn drop(&mut self) {
        let node = self.node.display();
        if self.cover_may_have_failed() {
            match clear_timeout_image_io(&self.node) {
                Ok(true) => eprintln!("took back a pending timeout-image mode on {node}"),
                Ok(false) => eprintln!("no timeout-image mode was pending on {node}"),
                Err(e) => eprintln!("FAILED to take back a timeout-image mode on {node}: {e}"),
            }
        }
        for (name, id, value) in &self.saved {
            match set_ctrl(&self.node, *id, *value) {
                Ok(()) => eprintln!("restored {name}={value} on {node}"),
                Err(e) => eprintln!("FAILED to restore {name}={value} on {node}: {e}"),
            }
        }
    }
}

/// The daemon's `LoopbackConsumers` over the private bus, once it answers
/// (within 15 s).
fn loopback_consumers(env: &DaemonEnv) -> u32 {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let conn = env.connect().await;
        let proxy = OmarchyBroadcastProxy::builder(&conn)
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match proxy.loopback_consumers().await {
                Ok(n) => return n,
                Err(e) if Instant::now() > deadline => {
                    panic!("no LoopbackConsumers over D-Bus within 15 s: {e}")
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    })
}

/// What a frame the reader dequeued is, by its timestamp, whenever it was
/// dequeued: the writers stamp `CLOCK_MONOTONIC` at the push, strictly
/// increasing, and a frame the driver serves on a timeout repeats the last
/// written frame's timestamp. A repeat is not always a timeout frame: the
/// reader was measured to dequeue a new writer's first frame twice in a
/// row, with its one timestamp (written twice by `v4l2sink`, its preroll),
/// so a repeat is the timeout image only when it repeats the first daemon's
/// last frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    /// Written by the first daemon: stamped before the kill.
    First,
    /// The driver's timeout image while no writer ran: the first daemon's
    /// last timestamp again.
    Covered,
    /// Written by the second daemon: stamped after its spawn.
    Second,
    /// None of these (stamped while no daemon ran, or going back).
    Stray,
}

/// The [`Kind`] of each of `frames`, in order: the kill at `kill_mono` and
/// the second daemon's spawn at `restart_mono` (`CLOCK_MONOTONIC`, ns).
fn classify(frames: &[Seen], kill_mono: u64, restart_mono: u64) -> Vec<Kind> {
    let mut prev: Option<u64> = None;
    frames
        .iter()
        .map(|s| {
            let kind = if prev == Some(s.ts_ns) && s.ts_ns < kill_mono {
                Kind::Covered
            } else if prev.is_some_and(|p| s.ts_ns < p) {
                Kind::Stray
            } else if s.ts_ns > restart_mono {
                Kind::Second
            } else if s.ts_ns < kill_mono {
                Kind::First
            } else {
                Kind::Stray
            };
            prev = Some(s.ts_ns);
            kind
        })
        .collect()
}

/// The daemon (verbose) with its standard error captured and echoed.
fn spawn_daemon(env: &DaemonEnv) -> (Guard, Arc<Mutex<Vec<String>>>) {
    let mut child = env
        .command()
        .arg("-v")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn omarchy-broadcastd");
    let err = child.stderr.take().unwrap();
    let log: Arc<Mutex<Vec<String>>> = Arc::default();
    let l = Arc::clone(&log);
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(err).lines() {
            let Ok(line) = line else { break };
            eprintln!("{line}");
            l.lock().unwrap().push(line);
        }
    });
    (
        Guard::new(child, "omarchy-broadcastd", Duration::from_secs(10)),
        log,
    )
}

/// Waits for the daemon's crash cover line; panics with it when the cover
/// is not in place.
fn wait_cover(log: &Arc<Mutex<Vec<String>>>, what: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(line) = log
            .lock()
            .unwrap()
            .iter()
            .find(|l| l.contains("crash cover"))
            .cloned()
        {
            assert!(
                line.contains("crash cover: black timeout image"),
                "{what}: the crash cover is not in place: {line}"
            );
            return line;
        }
        assert!(
            Instant::now() < deadline,
            "{what}: no crash cover line in the daemon's log within 15 s"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Frames are told apart by their timestamps, whenever they were dequeued:
/// a frame written just before the kill but dequeued after it is still the
/// first daemon's, a timeout frame repeats its last one, and a repeat of
/// the second daemon's frame is the second daemon's.
#[test]
fn frames_are_classified_by_their_timestamps() {
    let seen = |ts_ns: u64| Seen {
        at: Instant::now(),
        mono_ns: 0,
        ts_ns,
        flags: 0,
        black: true,
        y: 16,
        chroma: 128,
    };
    // Kill at 1000, second daemon spawned at 2000.
    let frames: Vec<Seen> = [900, 990, 990, 990, 2100, 2133, 2133, 2166, 1500, 2200, 1200]
        .into_iter()
        .map(seen)
        .collect();
    use Kind::*;
    assert_eq!(
        classify(&frames, 1000, 2000),
        [
            First, First, Covered, Covered, Second, Second, Second, Second, Stray, Second, Stray
        ]
    );
    // Stamped between the kill and the spawn: no daemon ran.
    assert_eq!(classify(&[seen(1500)], 1000, 2000), [Stray]);
}

/// The daemon killed with SIGKILL while a V4L2 reader streams the loopback:
/// the reader gets no error and keeps receiving frames (no gap above
/// [`MAX_GAP`]), black ones (the crash cover's timeout image, not the
/// driver's zero-filled green one) while no writer streams, then the new
/// daemon's frames. Buffer timestamps never go back, across the kill and
/// the restart, and the new writer's are `CLOCK_MONOTONIC` times. Each
/// daemon reports the loopback's consumers without its own descriptors
/// (the writer's, the crash cover's): none before the reader, then the
/// reader alone, from the second daemon's first count on.
#[test]
#[ignore = "needs OBC_TEST_SINK (a free v4l2loopback device with exclusive_caps=1, v4l2loopback with the timeout controls) and /usr/bin/dbus-daemon; no camera, no GPU"]
fn a_consumer_keeps_receiving_black_frames_across_a_daemon_crash() {
    let sink = PathBuf::from(
        std::env::var("OBC_TEST_SINK")
            .unwrap_or_else(|_| panic!("this test needs OBC_TEST_SINK=/dev/videoN. {IGNORE}")),
    );
    let sink = std::fs::canonicalize(&sink).unwrap_or(sink);
    let held = count_holders(Path::new("/proc"), &NodeId::of(&sink), std::process::id());
    assert_eq!(
        held,
        0,
        "{} is held by another process: not touching it",
        sink.display()
    );
    let mut restore = RestoreControls::save(&sink);

    let dir = tempfile::tempdir().unwrap();
    let bus = PrivateBus::start(dir.path());
    let env = DaemonEnv::new(dir.path(), &bus);
    std::fs::write(
        env.config_file(),
        format!(
            "version = 1\n[loopback]\ndevice = {:?}\n",
            sink.display().to_string()
        ),
    )
    .unwrap();
    // Off: no session, no camera, no GPU; the writer sends placeholders.
    std::fs::create_dir_all(env.state_file().parent().unwrap()).unwrap();
    std::fs::write(env.state_file(), "enabled = false\n").unwrap();

    let (mut first, first_log) = spawn_daemon(&env);
    restore.watch(&first_log);
    let cover = wait_cover(&first_log, "the first daemon");
    eprintln!("first daemon: {cover}");
    // Its writer and its cover hold the node: not consumers.
    let first_count = loopback_consumers(&env);

    let reader = Reader::start(&sink);
    assert!(
        reader.wait(Duration::from_secs(10), |f| f.len() >= 30),
        "the reader got {} frames from the first daemon",
        reader.frames().len()
    );

    let kill_mono = monotonic_ns();
    let killed_at = Instant::now();
    first.signal(libc::SIGKILL);
    assert!(
        first.wait_exit(Duration::from_secs(10)).is_some(),
        "the first daemon did not die"
    );
    let dead_at = Instant::now();
    eprintln!(
        "first daemon killed; dead after {:.1} ms",
        ms(dead_at - killed_at)
    );
    std::thread::sleep(OUTAGE.saturating_sub(dead_at - killed_at));

    let restart_mono = monotonic_ns();
    let restart_at = Instant::now();
    let (_second, second_log) = spawn_daemon(&env);
    restore.watch(&second_log);
    assert!(
        reader.wait(Duration::from_secs(15), |f| {
            f.iter().filter(|s| s.ts_ns > restart_mono).count() >= 30
        }),
        "the second daemon's frames did not reach the reader: {} frames stamped after its start",
        reader
            .frames()
            .iter()
            .filter(|s| s.ts_ns > restart_mono)
            .count()
    );
    let second_cover = wait_cover(&second_log, "the second daemon");
    // Its consumer count settles with the reader in it.
    std::thread::sleep(Duration::from_millis(500));
    let second_count = loopback_consumers(&env);
    let frames = reader.frames();
    let read = reader.finish();

    // Report.
    let kinds = classify(&frames, kill_mono, restart_mono);
    let of = |kind: Kind| -> Vec<&Seen> {
        frames
            .iter()
            .zip(&kinds)
            .filter(|(_, k)| **k == kind)
            .map(|(s, _)| s)
            .collect()
    };
    let (before, covered, after) = (of(Kind::First), of(Kind::Covered), of(Kind::Second));
    let gaps: Vec<Duration> = frames.windows(2).map(|w| w[1].at - w[0].at).collect();
    let max_gap = gaps.iter().copied().max().unwrap_or_default();
    let flags = |v: &[&Seen]| {
        (
            v.iter()
                .filter(|s| s.flags & v4l2::BUF_FLAG_TIMESTAMP_COPY != 0)
                .count(),
            v.iter()
                .filter(|s| s.flags & v4l2::BUF_FLAG_TIMESTAMP_MONOTONIC != 0)
                .count(),
        )
    };
    let first_after = after.first().expect("frames after the restart");
    let last_before = before.last().expect("frames before the kill");
    eprintln!(
        "reader: {} frames of the first daemon ({} dequeued after the kill), {} timeout frames \
         (first: Y={} chroma={}, arrival gaps {:?} ms), {} from the new daemon (first {:.1} ms \
         after its start, {:.1} ms after the kill); longest gap {:.1} ms; timestamp flags \
         (copy, monotonic): before {:?}, covered {:?}, after {:?}; last timestamp before {:.3} \
         s, first after {:.3} s (CLOCK_MONOTONIC {:.3} s at its arrival); LoopbackConsumers: \
         first daemon {first_count} before the reader, second {second_count}; second daemon: \
         {second_cover}",
        before.len(),
        before.iter().filter(|s| s.mono_ns > kill_mono).count(),
        covered.len(),
        covered.first().map_or(0, |s| s.y),
        covered.first().map_or(0, |s| s.chroma),
        covered
            .windows(2)
            .map(|w| ms(w[1].at - w[0].at).round())
            .collect::<Vec<_>>(),
        after.len(),
        ms(first_after.at - restart_at),
        ms(first_after.at - killed_at),
        ms(max_gap),
        flags(&before),
        flags(&covered),
        flags(&after),
        last_before.ts_ns as f64 / 1e9,
        first_after.ts_ns as f64 / 1e9,
        first_after.mono_ns as f64 / 1e9,
    );

    assert_eq!(read, Ok(()), "the reader ended on an error");
    // Timestamps never go back (a frame of the timeout image repeats the
    // last written frame's).
    if let Some(w) = frames.windows(2).find(|w| w[1].ts_ns < w[0].ts_ns) {
        panic!(
            "a timestamp went back: {:.6} s after {:.6} s",
            w[1].ts_ns as f64 / 1e9,
            w[0].ts_ns as f64 / 1e9
        );
    }
    assert!(
        !kinds.contains(&Kind::Stray),
        "a frame stamped while no daemon ran: {:?}",
        frames.iter().zip(&kinds).find(|(_, k)| **k == Kind::Stray)
    );
    assert!(
        before.len() >= 30,
        "{} frames before the kill",
        before.len()
    );
    // The first daemon's frames, then the timeout image, then the second
    // daemon's: the timer never fired while a writer ran.
    assert!(
        kinds.windows(2).all(|w| w[0] <= w[1]),
        "frames out of order (kind, ms after the restart at its arrival, timestamp ms after \
         the restart): {:?}",
        frames
            .iter()
            .zip(&kinds)
            .map(|(s, k)| (
                *k,
                (ms(s.at.saturating_duration_since(restart_at))
                    - ms(restart_at.saturating_duration_since(s.at)))
                .round(),
                ((s.ts_ns as f64 - restart_mono as f64) / 1e6).round()
            ))
            .collect::<Vec<_>>()
    );
    assert!(
        covered.iter().all(|s| s.mono_ns > kill_mono),
        "a timeout frame before the kill"
    );
    // While no writer streamed: the timeout image, black, a few times.
    assert!(
        covered.len() >= 3,
        "{} frames while no writer streamed ({:.0} ms)",
        covered.len(),
        ms(OUTAGE)
    );
    assert!(
        covered.iter().all(|s| s.black),
        "not every frame of the outage was black: {:?}",
        covered.iter().find(|s| !s.black)
    );
    assert!(
        frames.iter().all(|s| s.black),
        "a frame that is not black: {:?}",
        frames.iter().find(|s| !s.black)
    );
    assert!(
        max_gap <= MAX_GAP,
        "the reader went {:.1} ms without a frame (at most {:.0} ms)",
        ms(max_gap),
        ms(MAX_GAP)
    );
    // The new writer's timestamps are CLOCK_MONOTONIC (within 1 s of the
    // arrival: the writer's latency is a few frames).
    for s in &after {
        let skew = s.mono_ns.abs_diff(s.ts_ns);
        assert!(
            skew < 1_000_000_000,
            "a timestamp {:.3} s away from CLOCK_MONOTONIC at its arrival",
            skew as f64 / 1e9
        );
    }
    // The consumers each daemon reports: never its own descriptors. The
    // first, before the reader: none (over D-Bus). The second: the reader,
    // once, from its first count on, across its cover going in (every
    // change of the count is logged; over D-Bus at the end).
    assert_eq!(
        first_count, 0,
        "the first daemon counted a loopback consumer before the reader"
    );
    let counts: Vec<String> = second_log
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.contains("consumers: "))
        .cloned()
        .collect();
    assert!(
        counts.len() == 1 && counts[0].contains("(loopback 1,"),
        "the second daemon's consumer count did not go to the reader alone and stay there: \
         {counts:?}"
    );
    assert_eq!(second_count, 1, "the second daemon's LoopbackConsumers");
}
