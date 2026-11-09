//! The live preview encoder: small JPEG snapshots of the frames the
//! persistent loopback writer sends ([`crate::loopback`]), written to a file
//! that a client (the bar plugin's panel, through `omarchy-broadcast
//! preview`) shows.
//!
//! ```text
//! LoopbackWriter ──FrameTap──> decimate by PTS ──> latest-frame slot ──wake──> obc-preview thread
//!                                                    appsrc <writer caps> ! videoscale ! videoconvert
//!                                                    ! video/x-raw,format=RGB,<width>x<height>
//!                                                    ! jpegenc ! appsink
//!                                                    ──> <dir>/preview.jpg.tmp ──rename──> <dir>/preview.jpg
//! ```
//!
//! **What it shows.** The tap sees exactly what the loopback sink sees
//! (placeholders and forwarded frames alike), so the preview is black at
//! idle and while an effect loads, and the camera image only when the
//! loopback carries it. It never opens the loopback device and makes no
//! CUDA call: the writer's frames are system memory.
//!
//! **The tap never blocks.** [`PreviewEncoder::tap`] decimates by the
//! writer's PTS first (at 15 fps from a 30 fps contract, every second
//! frame; a frame is taken when its PTS is due within half a contract
//! period, so the writer's timing jitter does not skip one), then keeps
//! only the latest frame (a reference, no copy) and wakes the worker; a
//! frame the worker has not taken yet is replaced (counted as
//! superseded). Nothing is scaled or encoded on the pushing thread.
//!
//! **Encoding.** On its own thread (`obc-preview`), with a small GStreamer
//! pipeline: bilinear scale to [`PREVIEW_WIDTH`] pixels wide with the
//! frame's aspect kept (480x270 for the 16:9 contracts), conversion to RGB
//! (so the JPEG, which is full range, gets the limited-range frame's
//! levels right), `jpegenc` at [`PreviewConfig::quality`]. One frame in
//! flight: the worker pushes it and pulls the JPEG back before taking the
//! next. A pipeline error rebuilds the pipeline (counted as failed).
//!
//! **Files.** Each JPEG is written to [`TMP_FILE`] in
//! [`PreviewConfig::dir`] (created mode 0600, `O_NOFOLLOW`, truncated; its
//! mode forced to 0600 should it exist), then renamed over [`FRAME_FILE`],
//! so a reader never sees a partial frame. The open never blocks
//! (`O_NONBLOCK`: a FIFO planted there cannot hold the worker, and with it
//! the stop) and writes only to a regular file with no other link: anything
//! else in its place (a symbolic link, a FIFO, a hard link) is unlinked and
//! the file created anew, once per frame. The caller provides a directory
//! only it can write to. After each rename the frame callback is called
//! with the frame's sequence number, from [`PreviewConfig::seq`] (shared,
//! so a caller that rebuilds the encoder keeps one strictly increasing
//! sequence). [`PreviewEncoder::stop`] (or the drop) joins the worker and
//! removes both files.
//!
//! The library logs nothing: [`PreviewEncoder::counts`] tells what happened.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gst::prelude::*;
use obc_core::OutputContract;
use serde::Serialize;

use crate::loopback::{FrameTap, writer_caps_string};
use crate::pipeline::{PipelineError, add_linked, make};

/// The frame file in [`PreviewConfig::dir`].
pub const FRAME_FILE: &str = "preview.jpg";
/// The file each frame is written to before it is renamed over
/// [`FRAME_FILE`].
pub const TMP_FILE: &str = "preview.jpg.tmp";
/// Width of the preview frames; the height keeps the frame's aspect.
pub const PREVIEW_WIDTH: u32 = 480;
/// Default JPEG quality.
pub const DEFAULT_QUALITY: u32 = 70;
/// How long the worker waits for the JPEG of a frame before it takes the
/// pipeline as failed.
const ENCODE_TIMEOUT: Duration = Duration::from_secs(2);
/// How many of the latest frames' durations [`PreviewEncoder::frame_ms_p50`]
/// is taken over.
const TIMING_WINDOW: usize = 256;

/// Everything needed to start the encoder.
#[derive(Debug, Clone)]
pub struct PreviewConfig {
    /// The frames the writer carries.
    pub contract: OutputContract,
    /// Where [`TMP_FILE`] and [`FRAME_FILE`] are written. Must exist; the
    /// caller makes it private.
    pub dir: PathBuf,
    /// Frames per second written (at most the contract's rate).
    pub fps: u32,
    /// `jpegenc` quality, 0 to 100.
    pub quality: u32,
    /// The last sequence number used: the next frame gets one more.
    pub seq: Arc<AtomicU32>,
}

impl PreviewConfig {
    /// The defaults for `contract`, writing into `dir` at `fps`, with a
    /// sequence of its own.
    pub fn new(contract: OutputContract, dir: PathBuf, fps: u32) -> Self {
        PreviewConfig {
            contract,
            dir,
            fps,
            quality: DEFAULT_QUALITY,
            seq: Arc::default(),
        }
    }
}

/// The size of the preview frames for a `width` x `height` frame:
/// [`PREVIEW_WIDTH`] wide, the height in proportion, even, at least 2.
pub fn preview_size(width: u32, height: u32) -> (u32, u32) {
    let w = u64::from(PREVIEW_WIDTH);
    let h = (w * u64::from(height) + u64::from(width.max(1)) / 2) / u64::from(width.max(1));
    let h = u32::try_from(h).unwrap_or(u32::MAX).max(2) & !1;
    (PREVIEW_WIDTH, h)
}

/// A snapshot of the encoder's counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct PreviewCounts {
    /// Frames the tap was offered.
    pub offered: u64,
    /// Frames the tap dropped by the frame rate.
    pub decimated: u64,
    /// Frames replaced in the slot before the worker took them.
    pub superseded: u64,
    /// Frames written and announced.
    pub written: u64,
    /// Frames that could not be encoded or written.
    pub failed: u64,
}

/// Which frames the tap keeps, by PTS (see the module docs).
#[derive(Debug)]
struct Decimator {
    interval_ns: u64,
    /// Half a contract period: how early a frame may be and still count.
    tolerance_ns: u64,
    next_due_ns: Option<u64>,
}

impl Decimator {
    fn new(fps: u32, contract: &OutputContract) -> Self {
        Decimator {
            interval_ns: interval_ns(fps),
            tolerance_ns: contract.frame_duration_ns() / 2,
            next_due_ns: None,
        }
    }

    /// Whether the frame at `pts_ns` is kept.
    fn admit(&mut self, pts_ns: u64) -> bool {
        let Some(due) = self.next_due_ns else {
            self.next_due_ns = Some(pts_ns.saturating_add(self.interval_ns));
            return true;
        };
        // The timeline went back (far ahead of this frame): start again.
        let restart = due > pts_ns.saturating_add(self.interval_ns.saturating_mul(2));
        if !restart && pts_ns.saturating_add(self.tolerance_ns) < due {
            return false;
        }
        let mut next = due.saturating_add(self.interval_ns);
        if restart || next.saturating_add(self.tolerance_ns) <= pts_ns {
            // Fell behind (a gap in the stream): due one interval from now.
            next = pts_ns.saturating_add(self.interval_ns);
        }
        self.next_due_ns = Some(next);
        true
    }
}

fn interval_ns(fps: u32) -> u64 {
    1_000_000_000 / u64::from(fps.max(1))
}

/// What the tap, the worker and the owner share.
struct Shared {
    stopped: AtomicBool,
    decimator: Mutex<Decimator>,
    latest: Mutex<Option<gst::Buffer>>,
    wake: Condvar,
    offered: AtomicU64,
    decimated: AtomicU64,
    superseded: AtomicU64,
    written: AtomicU64,
    failed: AtomicU64,
    /// How long the latest frames took, scale and encode and write, in
    /// nanoseconds (at most [`TIMING_WINDOW`]).
    frame_ns: Mutex<VecDeque<u64>>,
}

impl Shared {
    fn timed(&self, took: Duration) {
        let mut q = self.frame_ns.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() == TIMING_WINDOW {
            q.pop_front();
        }
        q.push_back(u64::try_from(took.as_nanos()).unwrap_or(u64::MAX));
    }

    /// The tap: decimate, keep the latest, wake the worker. Never blocks
    /// beyond two short locks.
    fn offer(&self, buffer: &gst::Buffer) {
        if self.stopped.load(Ordering::Acquire) {
            return;
        }
        self.offered.fetch_add(1, Ordering::Relaxed);
        let Some(pts) = buffer.pts() else {
            self.decimated.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let admitted = self
            .decimator
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .admit(pts.nseconds());
        if !admitted {
            self.decimated.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let old = self
            .latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(buffer.clone());
        if old.is_some() {
            self.superseded.fetch_add(1, Ordering::Relaxed);
        }
        self.wake.notify_one();
    }

    /// The next frame, waiting for one; `None` once stopped.
    fn next(&self) -> Option<gst::Buffer> {
        let mut latest = self.latest.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if self.stopped.load(Ordering::Acquire) {
                return None;
            }
            if let Some(b) = latest.take() {
                return Some(b);
            }
            latest = self.wake.wait(latest).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn stop(&self) {
        // Under the slot's lock, so the worker cannot miss the wake-up
        // between its check and its wait.
        let mut latest = self.latest.lock().unwrap_or_else(|e| e.into_inner());
        self.stopped.store(true, Ordering::Release);
        latest.take();
        self.wake.notify_all();
    }
}

/// The scale-and-encode pipeline the worker pushes frames through.
struct Encoder {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    sink: gst_app::AppSink,
    error: Arc<Mutex<Option<String>>>,
}

impl Encoder {
    fn build(contract: &OutputContract, quality: u32) -> Result<Encoder, PipelineError> {
        let caps_str = writer_caps_string(contract);
        let caps: gst::Caps = caps_str
            .parse()
            .map_err(|_| PipelineError::Caps(caps_str.clone()))?;
        let (width, height) = preview_size(contract.width, contract.height);
        let out_str =
            format!("video/x-raw,format=RGB,width={width},height={height},pixel-aspect-ratio=1/1");
        let out_caps: gst::Caps = out_str
            .parse()
            .map_err(|_| PipelineError::Caps(out_str.clone()))?;
        let src = gst_app::AppSrc::builder()
            .name("preview_src")
            .caps(&caps)
            .is_live(false)
            .format(gst::Format::Time)
            .do_timestamp(false)
            .block(false)
            .max_buffers(2)
            .max_bytes(0)
            .max_time(gst::ClockTime::ZERO)
            .build();
        let scale = make("videoscale", "preview_scale")?;
        scale.set_property("add-borders", false);
        let convert = make("videoconvert", "preview_convert")?;
        let filter = make("capsfilter", "preview_caps")?;
        filter.set_property("caps", &out_caps);
        let jpeg = make("jpegenc", "preview_jpeg")?;
        jpeg.set_property("quality", i32::try_from(quality.min(100)).unwrap_or(70));
        let sink = gst_app::AppSink::builder()
            .name("preview_sink")
            .sync(false)
            .max_buffers(2)
            .enable_last_sample(false)
            .wait_on_eos(false)
            .build();
        let pipeline = gst::Pipeline::with_name("obc-preview");
        add_linked(
            &pipeline,
            &[
                src.clone().upcast(),
                scale,
                convert,
                filter,
                jpeg,
                sink.clone().upcast(),
            ],
        )?;
        let error: Arc<Mutex<Option<String>>> = Arc::default();
        let recorder = Arc::downgrade(&error);
        pipeline
            .bus()
            .expect("a pipeline always has a bus")
            .set_sync_handler(move |_, msg| {
                if let gst::MessageView::Error(e) = msg.view()
                    && let Some(slot) = recorder.upgrade()
                {
                    slot.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get_or_insert_with(|| e.error().to_string());
                }
                gst::BusSyncReply::Drop
            });
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| PipelineError::State(e.to_string()))?;
        Ok(Encoder {
            pipeline,
            src,
            sink,
            error,
        })
    }

    /// The JPEG of `frame`.
    fn encode(&self, frame: gst::Buffer) -> Result<Vec<u8>, String> {
        if let Some(e) = self.error.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return Err(e);
        }
        self.src
            .push_buffer(frame)
            .map_err(|e| format!("push: {e:?}"))?;
        let sample = self
            .sink
            .try_pull_sample(gst::ClockTime::from_nseconds(
                u64::try_from(ENCODE_TIMEOUT.as_nanos()).unwrap_or(u64::MAX),
            ))
            .ok_or_else(|| "no JPEG came out of the encoder".to_string())?;
        let buffer = sample
            .buffer()
            .ok_or_else(|| "a JPEG sample without a buffer".to_string())?;
        let map = buffer
            .map_readable()
            .map_err(|_| "the JPEG buffer cannot be mapped".to_string())?;
        Ok(map.as_slice().to_vec())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Opens `tmp` for writing without blocking and without following a
/// link, creating it mode 0600; `Ok(None)` when something else than a
/// regular file with one link is there (nothing is truncated then).
fn open_tmp(tmp: &Path) -> io::Result<Option<fs::File>> {
    let file = match fs::OpenOptions::new()
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(tmp)
    {
        Ok(f) => f,
        // A symbolic link (O_NOFOLLOW), a FIFO without a reader.
        Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP | libc::ENXIO)) => {
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    let meta = file.metadata()?;
    if !meta.file_type().is_file() || std::os::unix::fs::MetadataExt::nlink(&meta) != 1 {
        return Ok(None);
    }
    Ok(Some(file))
}

/// Writes `jpeg` to [`TMP_FILE`] in `dir` and renames it over
/// [`FRAME_FILE`] (see the module docs, "Files").
pub fn write_frame(dir: &Path, jpeg: &[u8]) -> io::Result<()> {
    let tmp = dir.join(TMP_FILE);
    let mut file = match open_tmp(&tmp)? {
        Some(f) => f,
        None => {
            // Not ours to write through: unlinked (a link itself, not its
            // target), then created anew.
            fs::remove_file(&tmp)?;
            open_tmp(&tmp)?.ok_or_else(|| {
                io::Error::other(format!("{} is not a regular file", tmp.display()))
            })?
        }
    };
    file.set_len(0)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(jpeg)?;
    drop(file);
    fs::rename(&tmp, dir.join(FRAME_FILE))
}

/// Removes [`FRAME_FILE`] and [`TMP_FILE`] from `dir`, if they are there
/// (a symbolic link in their place is removed, not followed).
pub fn remove_frames(dir: &Path) {
    for name in [FRAME_FILE, TMP_FILE] {
        let _ = fs::remove_file(dir.join(name));
    }
}

/// The worker: frame in, JPEG out, file replaced, callback.
fn worker(
    shared: &Shared,
    cfg: &PreviewConfig,
    on_frame: &(dyn Fn(u32) + Send),
    mut encoder: Option<Encoder>,
) {
    while let Some(frame) = shared.next() {
        if encoder.is_none() {
            encoder = Encoder::build(&cfg.contract, cfg.quality).ok();
        }
        let Some(enc) = encoder.as_ref() else {
            shared.failed.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let started = Instant::now();
        let jpeg = match enc.encode(frame) {
            Ok(j) => j,
            Err(_) => {
                // Built anew for the next frame.
                encoder = None;
                shared.failed.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        if shared.stopped.load(Ordering::Acquire) {
            break;
        }
        if write_frame(&cfg.dir, &jpeg).is_err() {
            shared.failed.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        shared.timed(started.elapsed());
        shared.written.fetch_add(1, Ordering::Relaxed);
        let seq = cfg.seq.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        on_frame(seq);
    }
}

/// The running encoder. See the module docs.
pub struct PreviewEncoder {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    cfg: PreviewConfig,
}

impl std::fmt::Debug for PreviewEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreviewEncoder")
            .field("dir", &self.cfg.dir)
            .field("contract", &self.cfg.contract)
            .field("counts", &self.counts())
            .finish_non_exhaustive()
    }
}

impl PreviewEncoder {
    /// Builds the pipeline and starts the `obc-preview` thread. `on_frame`
    /// is called on that thread after each frame file replaced, with its
    /// sequence number; it must not block for long. Fails for an invalid
    /// contract, a missing element or a thread that cannot start. Nothing
    /// is written before the tap ([`Self::tap`]) is installed and offered a
    /// frame.
    pub fn start(
        cfg: PreviewConfig,
        on_frame: impl Fn(u32) + Send + 'static,
    ) -> Result<Self, PipelineError> {
        cfg.contract
            .validate()
            .map_err(|e| PipelineError::Caps(e.to_string()))?;
        gst::init().map_err(|e| PipelineError::State(e.to_string()))?;
        let encoder = Encoder::build(&cfg.contract, cfg.quality)?;
        let shared = Arc::new(Shared {
            stopped: AtomicBool::new(false),
            decimator: Mutex::new(Decimator::new(cfg.fps, &cfg.contract)),
            latest: Mutex::default(),
            wake: Condvar::new(),
            offered: AtomicU64::new(0),
            decimated: AtomicU64::new(0),
            superseded: AtomicU64::new(0),
            written: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            frame_ns: Mutex::default(),
        });
        let (thread_shared, thread_cfg) = (Arc::clone(&shared), cfg.clone());
        let handle = std::thread::Builder::new()
            .name("obc-preview".into())
            .spawn(move || worker(&thread_shared, &thread_cfg, &on_frame, Some(encoder)))
            .map_err(|e| PipelineError::State(format!("preview thread: {e}")))?;
        Ok(PreviewEncoder {
            shared,
            thread: Some(handle),
            cfg,
        })
    }

    /// The tap to install on the loopback writer (alone or composed with
    /// another). It holds the shared state, not the thread: once the
    /// encoder is stopped it drops every frame.
    pub fn tap(&self) -> FrameTap {
        let shared = Arc::clone(&self.shared);
        Arc::new(move |b: &gst::Buffer| shared.offer(b))
    }

    /// Changes the frame rate from the next frame kept on.
    pub fn set_fps(&mut self, fps: u32) {
        self.cfg.fps = fps;
        self.shared
            .decimator
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .interval_ns = interval_ns(fps);
    }

    pub fn config(&self) -> &PreviewConfig {
        &self.cfg
    }

    pub fn counts(&self) -> PreviewCounts {
        let s = &self.shared;
        PreviewCounts {
            offered: s.offered.load(Ordering::Relaxed),
            decimated: s.decimated.load(Ordering::Relaxed),
            superseded: s.superseded.load(Ordering::Relaxed),
            written: s.written.load(Ordering::Relaxed),
            failed: s.failed.load(Ordering::Relaxed),
        }
    }

    /// The median time the worker took for one frame written (scale,
    /// encode and file replace), over the latest ones, in milliseconds;
    /// `None` before the first.
    pub fn frame_ms_p50(&self) -> Option<f64> {
        let mut v: Vec<u64> = self
            .shared
            .frame_ns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .copied()
            .collect();
        if v.is_empty() {
            return None;
        }
        v.sort_unstable();
        Some(v[v.len() / 2] as f64 / 1e6)
    }

    /// Stops the worker (joined before this returns; no callback follows)
    /// and removes the frame files. Also done on drop.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(handle) = self.thread.take() {
            self.shared.stop();
            let _ = handle.join();
            remove_frames(&self.cfg.dir);
        }
    }
}

impl Drop for PreviewEncoder {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::loopback::black_contract_frame;
    use crate::test_support::require_elements;

    const ELEMENTS: [&str; 6] = [
        "appsrc",
        "videoscale",
        "videoconvert",
        "capsfilter",
        "jpegenc",
        "appsink",
    ];

    fn wait_until(limit: Duration, what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + limit;
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A writer-like frame of `contract` at `pts_ns`: black with a grey
    /// luma bar, so the picture is not uniform.
    fn frame(contract: &OutputContract, pts_ns: u64) -> gst::Buffer {
        let mut b = black_contract_frame(contract).unwrap();
        {
            let b = b.make_mut();
            {
                let mut map = b.map_writable().unwrap();
                let w = contract.width as usize;
                for row in 0..(contract.height as usize / 4) {
                    map[row * w..row * w + w / 2].fill(128);
                }
            }
            b.set_pts(gst::ClockTime::from_nseconds(pts_ns));
        }
        b
    }

    /// The width and height in a baseline/progressive JPEG's SOF segment.
    fn jpeg_size(data: &[u8]) -> Option<(u32, u32)> {
        if data.get(..2)? != [0xFF, 0xD8] {
            return None;
        }
        let mut i = 2;
        while i + 4 <= data.len() {
            if data[i] != 0xFF {
                return None;
            }
            let marker = data[i + 1];
            let len = usize::from(u16::from_be_bytes([data[i + 2], data[i + 3]]));
            if matches!(marker, 0xC0..=0xC3) {
                let h = u16::from_be_bytes([*data.get(i + 5)?, *data.get(i + 6)?]);
                let w = u16::from_be_bytes([*data.get(i + 7)?, *data.get(i + 8)?]);
                return Some((u32::from(w), u32::from(h)));
            }
            i += 2 + len;
        }
        None
    }

    #[test]
    fn sizes_keep_the_aspect() {
        assert_eq!(preview_size(1920, 1080), (480, 270));
        assert_eq!(preview_size(1280, 720), (480, 270));
        assert_eq!(preview_size(640, 480), (480, 360));
        assert_eq!(preview_size(1, 10_000), (480, 4_800_000));
        assert_eq!(preview_size(10_000, 1), (480, 2));
    }

    #[test]
    fn the_decimator_keeps_every_second_frame_at_half_the_rate() {
        let contract = OutputContract::default();
        let period = contract.frame_duration_ns();
        for (fps, jitter, kept) in [
            (15, 0i64, 50),
            (15, 4_000_000, 50),
            (15, -4_000_000, 50),
            (30, 0, 100),
            (30, 6_000_000, 100),
        ] {
            let mut d = Decimator::new(fps, &contract);
            let n = (0..100u64)
                .filter(|k| {
                    // Alternating early and late by `jitter`.
                    let j = if k % 2 == 0 { jitter } else { -jitter };
                    let pts = (k * period + 10_000_000).saturating_add_signed(j);
                    d.admit(pts)
                })
                .count();
            assert_eq!(n, kept, "{fps} fps, jitter {jitter}");
        }
        // A gap: the next frame is taken, then the rate resumes.
        let mut d = Decimator::new(15, &contract);
        assert!(d.admit(0));
        assert!(!d.admit(period));
        assert!(d.admit(60 * period));
        assert!(!d.admit(61 * period));
        assert!(d.admit(62 * period));
        // The timeline restarts behind: taken at once.
        assert!(d.admit(period));
    }

    #[test]
    fn the_encoder_writes_decimated_jpegs_atomically_and_removes_them_on_stop() {
        require_elements(&ELEMENTS);
        let dir = tempfile::tempdir().unwrap();
        let contract = OutputContract::default();
        let seq = Arc::new(AtomicU32::new(41));
        let cfg = PreviewConfig {
            seq: Arc::clone(&seq),
            ..PreviewConfig::new(contract, dir.path().to_path_buf(), 15)
        };
        let seen: Arc<Mutex<Vec<u32>>> = Arc::default();
        let s = Arc::clone(&seen);
        let enc = PreviewEncoder::start(cfg, move |n| s.lock().unwrap().push(n)).unwrap();
        let tap = enc.tap();
        let period = contract.frame_duration_ns();
        // 30 frames of a 30 fps stream, offered at the writer's pace but
        // only after the previous one was handled, so none is superseded.
        for k in 0..30u64 {
            let before = enc.counts();
            tap(&frame(&contract, k * period));
            wait_until(Duration::from_secs(10), "the frame handled", || {
                let c = enc.counts();
                c.decimated + c.written + c.failed
                    > before.decimated + before.written + before.failed
            });
        }
        let c = enc.counts();
        assert_eq!(c.offered, 30);
        assert_eq!((c.written, c.decimated, c.failed), (15, 15, 0), "{c:?}");
        assert_eq!(c.superseded, 0);
        assert!(enc.frame_ms_p50().is_some_and(|ms| ms > 0.0));
        let numbers = seen.lock().unwrap().clone();
        assert_eq!(numbers, (42..57).collect::<Vec<u32>>(), "seq continues");
        assert_eq!(seq.load(Ordering::SeqCst), 56);

        let path = dir.path().join(FRAME_FILE);
        let data = fs::read(&path).unwrap();
        assert_eq!(&data[..2], &[0xFF, 0xD8], "a JPEG");
        assert_eq!(jpeg_size(&data), Some((480, 270)));
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(!dir.path().join(TMP_FILE).exists(), "renamed, not left");
        // The decoded picture is the frame: the grey bar top left.
        let decoded = decode_luma(&data);
        assert!(decoded.0 > 100 && decoded.1 < 10, "{decoded:?}");

        enc.stop();
        assert!(!path.exists(), "removed on stop");
        assert!(!dir.path().join(TMP_FILE).exists());
        // A stopped encoder's tap drops frames.
        tap(&frame(&contract, 100 * period));
        assert_eq!(seen.lock().unwrap().len(), 15);
    }

    /// The decoded luma at the top left and the bottom right of `jpeg`.
    fn decode_luma(jpeg: &[u8]) -> (u8, u8) {
        let pipeline = gst::Pipeline::new();
        let src = gst_app::AppSrc::builder()
            .caps(&"image/jpeg".parse::<gst::Caps>().unwrap())
            .build();
        let dec = make("jpegdec", "dec").unwrap();
        let conv = make("videoconvert", "conv").unwrap();
        let filter = make("capsfilter", "f").unwrap();
        filter.set_property(
            "caps",
            "video/x-raw,format=GRAY8".parse::<gst::Caps>().unwrap(),
        );
        let sink = gst_app::AppSink::builder().sync(false).build();
        add_linked(
            &pipeline,
            &[
                src.clone().upcast(),
                dec,
                conv,
                filter,
                sink.clone().upcast(),
            ],
        )
        .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        src.push_buffer(gst::Buffer::from_slice(jpeg.to_vec()))
            .unwrap();
        let _ = src.end_of_stream();
        let sample = sink
            .try_pull_sample(gst::ClockTime::from_seconds(5))
            .expect("decoded frame");
        let info = gst_video::VideoInfo::from_caps(sample.caps().unwrap()).unwrap();
        let map = sample.buffer().unwrap().map_readable().unwrap();
        let stride = info.stride()[0] as usize;
        let at = |x: usize, y: usize| map[y * stride + x];
        let r = (at(10, 10), at(470, 260));
        drop(map);
        pipeline.set_state(gst::State::Null).unwrap();
        r
    }

    #[test]
    fn a_frame_file_in_the_way_is_replaced_and_a_planted_link_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere");
        fs::write(&target, b"keep").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join(TMP_FILE)).unwrap();
        // O_NOFOLLOW: not written through; the link is replaced by the frame.
        write_frame(dir.path(), b"\xFF\xD8x").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"keep");
        assert_eq!(fs::read(dir.path().join(FRAME_FILE)).unwrap(), b"\xFF\xD8x");
        assert!(fs::symlink_metadata(dir.path().join(TMP_FILE)).is_err());
        // A link left in place is removed, not its target.
        std::os::unix::fs::symlink(&target, dir.path().join(TMP_FILE)).unwrap();
        remove_frames(dir.path());
        assert!(target.exists(), "the link removed, not its target");
        assert!(fs::symlink_metadata(dir.path().join(TMP_FILE)).is_err());
        // A hard link to another file: that file is neither truncated nor
        // written.
        fs::hard_link(&target, dir.path().join(TMP_FILE)).unwrap();
        write_frame(dir.path(), b"\xFF\xD8h").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"keep");
        assert_eq!(fs::read(dir.path().join(FRAME_FILE)).unwrap(), b"\xFF\xD8h");
        // A loose tmp file with another mode: written 0600 all the same.
        fs::write(dir.path().join(TMP_FILE), b"old").unwrap();
        fs::set_permissions(dir.path().join(TMP_FILE), fs::Permissions::from_mode(0o644)).unwrap();
        write_frame(dir.path(), b"\xFF\xD8y").unwrap();
        let frame = dir.path().join(FRAME_FILE);
        assert_eq!(fs::read(&frame).unwrap(), b"\xFF\xD8y");
        assert_eq!(
            fs::metadata(&frame).unwrap().permissions().mode() & 0o777,
            0o600
        );
        remove_frames(dir.path());
        assert!(!frame.exists());
    }

    /// `write_frame` on another thread, which must return within a bound.
    fn write_frame_within(dir: &Path, jpeg: &'static [u8]) -> io::Result<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        let dir = dir.to_path_buf();
        std::thread::spawn(move || {
            let _ = tx.send(write_frame(&dir, jpeg));
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("write_frame blocked")
    }

    fn mkfifo(path: &Path) {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo");
    }

    /// A FIFO planted at the temporary file, with or without a reader,
    /// neither blocks the write (nor so the encoder's stop) nor receives the
    /// frame: it is replaced by the frame file.
    #[test]
    fn a_planted_fifo_neither_blocks_nor_gets_the_frame() {
        use std::io::Read;

        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join(TMP_FILE);
        let frame = dir.path().join(FRAME_FILE);
        mkfifo(&tmp);
        write_frame_within(dir.path(), b"\xFF\xD8a").unwrap();
        assert_eq!(fs::read(&frame).unwrap(), b"\xFF\xD8a");
        assert!(fs::symlink_metadata(&tmp).is_err(), "renamed, not left");

        // With a reader: opened, seen to be no regular file, replaced.
        mkfifo(&tmp);
        let mut reader = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&tmp)
            .unwrap();
        write_frame_within(dir.path(), b"\xFF\xD8b").unwrap();
        assert_eq!(fs::read(&frame).unwrap(), b"\xFF\xD8b");
        let mut got = Vec::new();
        match reader.read_to_end(&mut got) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => panic!("reading the FIFO: {e}"),
        }
        assert!(got.is_empty(), "the FIFO got {got:?}");
        assert!(fs::symlink_metadata(&tmp).is_err());

        // Something that cannot be unlinked as a file (a directory): the
        // frame fails, at once.
        fs::create_dir(&tmp).unwrap();
        assert!(write_frame_within(dir.path(), b"\xFF\xD8c").is_err());
        assert_eq!(fs::read(&frame).unwrap(), b"\xFF\xD8b");
    }
}
