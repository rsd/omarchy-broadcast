//! The video background of the replace effect: a decode pipeline of its own
//! that plays a video file in a loop into a latest-frame `Slot`, at the
//! effect's frame size and colour, cropped to fill as the still image is
//! ([`crate::background::crop_to_fill`]).
//!
//! ```text
//! filesrc ! qtdemux|matroskademux ! <parser> ! <decoder> ! videocrop
//!         ! cudaconvertscale|videoconvertscale ! NV12 WxH ! appsink
//! ```
//!
//! * **The container** is told from the file's first bytes (an ISO base
//!   media `ftyp`/`moov`/... box, or the EBML magic of Matroska and WebM),
//!   never by `decodebin`: only `qtdemux` or `matroskademux` reads the file.
//!   The file is checked first as the still image is: an absolute path to a
//!   readable regular file, opened without blocking, of at most
//!   [`MAX_VIDEO_BYTES`]. The file so checked is the one played: `filesrc`
//!   reads `/proc/self/fd/<n>` of that open, the very file the descriptor
//!   holds, never the path again (a file put at the path after the check
//!   is never read). Not `fdsrc`: it runs in push mode only, where the
//!   demuxers take no segment seek, so the loop would not be gapless.
//! * **The decoder** is chosen from the caps of the demuxer's first video
//!   pad: H.264, HEVC, VP9 or AV1, and only once that pad's caps say a size
//!   of at most 3840x2160 (a larger one is refused before any decoder is
//!   made; the decoded caps are checked again). On CUDA frames NVDEC
//!   (`nvh264dec`, `nvh265dec`, `nvvp9dec`, `nvav1dec`), sharing the
//!   effect's `GstCudaContext` (a `GstContext` set on the pipeline), and
//!   `cudaconvertscale` into CUDA memory of that context. On system memory
//!   only VP9, with `vp9dec` and `videoconvertscale`: no CUDA element is
//!   ever made there. Other pads (audio) are never linked.
//! * **The crop** is the centred crop to fill of the decoded size, set on
//!   `videocrop` when the decoded caps arrive (on CUDA memory `videocrop`
//!   only adds crop metadata, which `cudaconvertscale` applies).
//! * **Looping**: a flushing segment seek to the start once prerolled, then
//!   a non-flushing segment seek to the start on each `SEGMENT_DONE`, so the
//!   loop has no gap; the slot keeps the last frame meanwhile. A clip that
//!   prerolls without a frame, or ends [`MAX_EMPTY_LOOPS`] times in a row
//!   with no frame decoded (a zero duration), is a failure, not a loop
//!   forever.
//! * **The slot owns its frames**: each frame is copied out of the
//!   pipeline before it is kept (see `Slot::store`), so neither the slot
//!   nor the effect's last frame given ever pins a decoder output surface.
//! * **The appsink** keeps one buffer, drops late ones and syncs to the
//!   clock, so the slot holds the frame of now at the video's own rate.
//! * **Failures** (a file that cannot be read, an unsupported codec, a
//!   decoder that is missing or refuses, a decode error mid-file) are
//!   reported once, the first one, through the reporter; the slot keeps
//!   whatever frame it last had, and an error stops the pipeline (to
//!   `NULL`, its decoder released) until the source is dropped.
//!
//! Dropping a `VideoSource` stops its pipeline (to `NULL`) and joins its
//! bus thread; `VideoSource::detach` stops it without waiting (the bus
//! thread sets it to `NULL` on its own), for a caller that must not wait
//! for a decoder's teardown (the streaming thread).

use std::ffi::c_void;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

use gst::prelude::*;
use libloading::Library;

use crate::background::crop_to_fill;
use crate::cuda;

/// The largest video file played, in bytes.
pub const MAX_VIDEO_BYTES: u64 = 512 * 1024 * 1024;

/// The largest decoded size played: 3840x2160, either way round.
pub const MAX_VIDEO_LONG_SIDE: u32 = 3840;
pub const MAX_VIDEO_SHORT_SIDE: u32 = 2160;

/// How many ends of the clip in a row with no frame decoded make it a
/// failure (`"it has no frames to play"`) rather than a loop.
pub const MAX_EMPTY_LOOPS: u32 = 3;

/// Why a size is not played, if it is not: larger than 3840x2160 either
/// way round.
fn too_large(w: u32, h: u32) -> Option<String> {
    (w.max(h) > MAX_VIDEO_LONG_SIDE || w.min(h) > MAX_VIDEO_SHORT_SIDE)
        .then(|| format!("its video is {w}x{h}, larger than 3840x2160"))
}

/// The report of a video background that cannot play (or stopped playing):
/// `"Video background <path> could not be played (<why>)"`. The element
/// keeps the background in force (the previous one, or the load-time still).
pub fn video_unplayable(path: &Path, why: &str) -> String {
    format!(
        "Video background {} could not be played ({why})",
        path.display()
    )
}

/// The report of a backend that takes no per-frame background
/// (`InferenceBackend::supports_live_background`): `"<backend> shows the
/// video background as a still"`.
pub fn video_as_still(backend: &str) -> String {
    format!("{backend} shows the video background as a still")
}

/// The container of a video file, by its first bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    /// MP4, M4V, MOV (ISO base media): `qtdemux`.
    IsoMedia,
    /// MKV, WebM: `matroskademux`.
    Matroska,
}

impl Container {
    /// The demuxer element.
    pub fn demuxer(self) -> &'static str {
        match self {
            Container::IsoMedia => "qtdemux",
            Container::Matroska => "matroskademux",
        }
    }

    /// The container of a file starting with `head`, if it is one played.
    pub fn of(head: &[u8]) -> Option<Container> {
        if head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
            return Some(Container::Matroska);
        }
        // An ISO base media file starts with a box: 4 bytes of size, then
        // the box type.
        let kind = head.get(4..8)?;
        [
            b"ftyp", b"moov", b"mdat", b"free", b"wide", b"skip", b"pnot",
        ]
        .iter()
        .any(|k| kind == *k)
        .then_some(Container::IsoMedia)
    }
}

/// The video codecs played.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
    Vp9,
    Av1,
}

impl Codec {
    /// The codec of a demuxer pad's caps structure name, if played.
    pub fn of_caps(name: &str) -> Option<Codec> {
        match name {
            "video/x-h264" => Some(Codec::H264),
            "video/x-h265" => Some(Codec::H265),
            "video/x-vp9" => Some(Codec::Vp9),
            "video/x-av1" => Some(Codec::Av1),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::H265 => "HEVC",
            Codec::Vp9 => "VP9",
            Codec::Av1 => "AV1",
        }
    }

    /// The parser between the demuxer and the decoder.
    pub fn parser(self) -> &'static str {
        match self {
            Codec::H264 => "h264parse",
            Codec::H265 => "h265parse",
            Codec::Vp9 => "vp9parse",
            Codec::Av1 => "av1parse",
        }
    }

    /// The decoder: NVDEC on CUDA frames, `vp9dec` (VP9 only) on system
    /// memory; else why the codec cannot play there.
    pub fn decoder(self, cuda: bool) -> Result<&'static str, String> {
        match (self, cuda) {
            (Codec::H264, true) => Ok("nvh264dec"),
            (Codec::H265, true) => Ok("nvh265dec"),
            (Codec::Vp9, true) => Ok("nvvp9dec"),
            (Codec::Av1, true) => Ok("nvav1dec"),
            (Codec::Vp9, false) => Ok("vp9dec"),
            (other, false) => Err(format!(
                "its video is {}, and without the GPU only VP9 is played",
                other.name()
            )),
        }
    }
}

/// Opens `path` without blocking (as the still image is opened, see
/// `crate::background`), checks it is a readable regular file of at most
/// [`MAX_VIDEO_BYTES`], and tells its container from its first bytes.
pub fn check_video_file(path: &Path) -> Result<Container, String> {
    open_video_file(path).map(|(c, _)| c)
}

/// [`check_video_file`], returning the file opened and checked as well: the
/// one to play (its offset untouched, at the start).
fn open_video_file(path: &Path) -> Result<(Container, File), String> {
    if !path.is_absolute() {
        return Err("the path is not absolute".into());
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| format!("unreadable: {e}"))?;
    let meta = file.metadata().map_err(|e| format!("unreadable: {e}"))?;
    if !meta.is_file() {
        return Err("not a regular file".into());
    }
    if meta.len() > MAX_VIDEO_BYTES {
        return Err(format!(
            "{} bytes, more than the {} MiB limit",
            meta.len(),
            MAX_VIDEO_BYTES / (1024 * 1024)
        ));
    }
    let mut head = [0u8; 12];
    let mut n = 0;
    while n < head.len() {
        // At an offset: the file's own offset stays at the start for the
        // demuxer.
        match file.read_at(&mut head[n..], n as u64) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("unreadable: {e}")),
        }
    }
    let container =
        Container::of(&head[..n]).ok_or_else(|| "not an MP4, MOV, MKV or WebM file".to_string())?;
    Ok((container, file))
}

/// `gst_context_new_cuda_context` of GStreamer's CUDA library: a
/// `GstContext` carrying a `GstCudaContext`, for the elements of the
/// video pipeline to use the effect's context.
type NewCudaContext = unsafe extern "C" fn(*mut c_void) -> *mut gst::ffi::GstContext;

fn new_cuda_context_fn() -> Result<NewCudaContext, String> {
    static FN: OnceLock<Result<(Library, NewCudaContext), String>> = OnceLock::new();
    FN.get_or_init(|| {
        // SAFETY: the library has no load-time preconditions; the element
        // already opened it (`crate::cuda`), so this takes a reference.
        let lib = unsafe { Library::new(cuda::LIBRARY) }
            .map_err(|e| format!("{} could not be loaded: {e}", cuda::LIBRARY))?;
        // SAFETY: the documented signature (gstcudautils.h, 1.22+).
        let f = unsafe { lib.get::<NewCudaContext>(b"gst_context_new_cuda_context\0") }
            .map(|s| *s)
            .map_err(|e| format!("{} has no gst_context_new_cuda_context: {e}", cuda::LIBRARY))?;
        Ok((lib, f))
    })
    .as_ref()
    .map(|(_, f)| *f)
    .map_err(Clone::clone)
}

/// A `GstContext` of the CUDA context `context` (a live `GstCudaContext`):
/// set on the video pipeline, its NVDEC decoder and converter use that
/// context, so their frames are memory the effect can read directly.
///
/// # Safety
/// `context` must be a live `GstCudaContext`.
pub(crate) unsafe fn share_cuda_context(context: *mut c_void) -> Result<gst::Context, String> {
    let f = new_cuda_context_fn()?;
    // SAFETY: the caller guarantees `context`; the returned context is
    // new (transfer full) and holds its own reference to `context`.
    let ptr = unsafe { f(context) };
    if ptr.is_null() {
        return Err("could not share the CUDA context".into());
    }
    // SAFETY: a new GstContext, transfer full.
    Ok(unsafe { gst::Context::from_glib_full(ptr) })
}

/// What a source plays, and into what.
#[derive(Debug, Clone)]
pub(crate) struct SourceSpec {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    /// The colorimetry of the effect's caps: the frames are converted to it.
    pub colorimetry: String,
    /// The effect's CUDA context, shared: CUDA frames. `None`: system
    /// memory (no CUDA element is made).
    pub cuda: Option<gst::Context>,
}

/// One decoded frame and its number in the slot.
#[derive(Clone)]
pub(crate) struct SlotFrame {
    /// The slot's [`Slot::id`].
    pub slot: u64,
    /// Counts the frames of the slot, from 1.
    pub seq: u64,
    pub sample: gst::Sample,
}

/// The latest decoded frame of one source, and its first failure.
pub(crate) struct Slot {
    /// Unique in the process: a new source is a new slot.
    pub id: u64,
    latest: Mutex<Option<SlotFrame>>,
    seq: AtomicU64,
    failed: AtomicBool,
}

impl Slot {
    pub(crate) fn new() -> Arc<Slot> {
        static IDS: AtomicU64 = AtomicU64::new(1);
        Arc::new(Slot {
            id: IDS.fetch_add(1, Ordering::Relaxed),
            latest: Mutex::new(None),
            seq: AtomicU64::new(0),
            failed: AtomicBool::new(false),
        })
    }

    /// The newest frame, if any arrived.
    pub(crate) fn latest(&self) -> Option<SlotFrame> {
        self.latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Keeps a copy of `sample`'s frame as the newest. The copy is what
    /// keeps the decoder free: when the decoded size and colour are the
    /// output's, the converter passes the decoder's buffer through, and a
    /// frame kept here (and the effect's last frame given, kept across
    /// reloads) would hold one of NVDEC's few output surfaces (or a pool
    /// buffer of the converter) for as long as it is kept, stalling the
    /// decode. A deep copy is memory of its own (on CUDA frames, device
    /// memory of the same context, copied by GStreamer's CUDA allocator),
    /// owned by the sample alone and never written again.
    fn store(&self, sample: gst::Sample) {
        let Some(sample) = own_copy(&sample) else {
            return;
        };
        let mut l = self.latest.lock().unwrap_or_else(|e| e.into_inner());
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        *l = Some(SlotFrame {
            slot: self.id,
            seq,
            sample,
        });
    }
}

/// `sample` with its buffer deep-copied (see [`Slot::store`]); `None` when
/// it has no buffer or the copy fails.
fn own_copy(sample: &gst::Sample) -> Option<gst::Sample> {
    let buffer = sample.buffer()?.copy_deep().ok()?;
    let mut b = gst::Sample::builder().buffer(&buffer);
    let caps = sample.caps_owned();
    if let Some(caps) = &caps {
        b = b.caps(caps);
    }
    if let Some(segment) = sample.segment() {
        b = b.segment(segment);
    }
    Some(b.build())
}

/// Called with why the source cannot play, once (the first failure).
pub(crate) type Reporter = Arc<dyn Fn(String) + Send + Sync>;

/// Reports `why` through `report` unless the slot has failed already.
fn fail_once(slot: &Slot, report: &Reporter, why: String) {
    if !slot.failed.swap(true, Ordering::SeqCst) {
        report(why);
    }
}

/// The name of the application message that wakes a bus thread to stop.
const STOP: &str = "obc-video-background-stop";

/// How long a bus thread waits for a message before it looks at its stop
/// flag again: the stop message cannot be posted on the bus of a pipeline
/// that went to `NULL` after an error (its bus is flushing then).
const BUS_POLL: gst::ClockTime = gst::ClockTime::from_mseconds(50);

/// A running video background: its pipeline and bus thread.
pub(crate) struct VideoSource {
    pipeline: gst::Pipeline,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl VideoSource {
    /// Tells the bus thread to stop, and wakes it.
    fn signal_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the thread at once; when the bus is flushing (the pipeline
        // stopped after an error) it sees the flag within `BUS_POLL`.
        let msg = gst::message::Application::new(gst::Structure::new_empty(STOP));
        let _ = self.pipeline.post_message(msg);
    }

    /// Stops the source without waiting for it: its bus thread sets the
    /// pipeline to `NULL` (the decoder's teardown) and closes the file on
    /// its own, soon after. For a caller that must not block on that
    /// teardown; the slot gets no frame meanwhile that anyone reads (it is
    /// no longer the current one).
    pub(crate) fn detach(mut self) {
        self.signal_stop();
        // Dropping the handle detaches the thread; `Drop` then has nothing
        // to join.
        drop(self.thread.take());
    }
}

fn make(factory: &str, name: &str) -> Result<gst::Element, String> {
    gst::ElementFactory::make(factory)
        .name(name)
        .build()
        .map_err(|_| format!("the {factory} element is not installed"))
}

impl VideoSource {
    /// Builds the pipeline for `spec` and starts it on a bus thread of its
    /// own; frames go to `slot`, the first failure to `report`. A failure
    /// found before anything runs (the file, the container, a missing
    /// element) is returned instead.
    pub(crate) fn start(
        spec: SourceSpec,
        slot: Arc<Slot>,
        report: Reporter,
    ) -> Result<VideoSource, String> {
        let cuda = spec.cuda.is_some();
        let pipeline = gst::Pipeline::with_name("obc-video-background");
        if let Some(ctx) = &spec.cuda {
            // Before any element runs, and inherited by the decoder added
            // later (a bin sets its contexts on every element added).
            pipeline.set_context(ctx);
        }
        let (container, file) = open_video_file(&spec.path)?;
        // The file of the open just checked, through its descriptor's own
        // link (it names the open file, not the path): the path is not
        // looked up again. The bus thread owns the file and closes it once
        // the pipeline is at `NULL`.
        let src = make("filesrc", "src")?;
        src.set_property(
            "location",
            format!("/proc/self/fd/{}", file.as_raw_fd()).as_str(),
        );
        let demux = make(container.demuxer(), "demux")?;
        let crop = make("videocrop", "crop")?;
        let convert = if cuda {
            make("cudaconvertscale", "convert")?
        } else {
            make("videoconvertscale", "convert")?
        };
        convert.set_property("add-borders", false);
        let caps = format!(
            "video/x-raw{},format=NV12,width={},height={},pixel-aspect-ratio=1/1,colorimetry={}",
            if cuda {
                format!("({})", cuda::MEMORY_FEATURE)
            } else {
                String::new()
            },
            spec.width,
            spec.height,
            spec.colorimetry
        );
        let caps: gst::Caps = caps
            .parse()
            .map_err(|_| format!("internal: bad caps {caps}"))?;
        let filter = make("capsfilter", "caps")?;
        filter.set_property("caps", &caps);
        let sink = gst_app::AppSink::builder()
            .name("sink")
            .max_buffers(1)
            .leaky_type(gst_app::AppLeakyType::Downstream)
            .sync(true)
            .enable_last_sample(false)
            .build();
        {
            let (s1, s2) = (Arc::clone(&slot), Arc::clone(&slot));
            sink.set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |sink| {
                        if let Ok(sample) = sink.pull_sample() {
                            s1.store(sample);
                        }
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .new_preroll(move |sink| {
                        // The first frame as soon as it is decoded.
                        if let Ok(sample) = sink.pull_preroll() {
                            s2.store(sample);
                        }
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            );
        }
        let sink = sink.upcast::<gst::Element>();
        pipeline
            .add_many([&src, &demux, &crop, &convert, &filter, &sink])
            .map_err(|e| format!("internal: {e}"))?;
        src.link(&demux).map_err(|e| format!("internal: {e}"))?;
        gst::Element::link_many([&crop, &convert, &filter, &sink])
            .map_err(|e| format!("internal: {e}"))?;
        watch_decoded_caps(&crop, &spec, &slot, &report);
        link_video_pad(&demux, &pipeline, &crop, cuda, &slot, &report);
        let bus = pipeline.bus().ok_or("internal: the pipeline has no bus")?;
        let stop = Arc::new(AtomicBool::new(false));
        let (p, s, r, st) = (
            pipeline.clone(),
            Arc::clone(&slot),
            Arc::clone(&report),
            Arc::clone(&stop),
        );
        let thread = std::thread::Builder::new()
            .name("obceffect-video".into())
            .spawn(move || {
                run_bus(p, bus, s, r, &st);
                drop(file);
            })
            .map_err(|e| format!("no thread to play it: {e}"))?;
        Ok(VideoSource {
            pipeline,
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for VideoSource {
    fn drop(&mut self) {
        // Detached: the bus thread stops the pipeline.
        let Some(t) = self.thread.take() else {
            return;
        };
        self.signal_stop();
        let _ = t.join();
    }
}

/// Sets the crop to fill on `crop` from the decoded caps when they arrive,
/// and refuses a decoded format or size that is not played.
fn watch_decoded_caps(crop: &gst::Element, spec: &SourceSpec, slot: &Arc<Slot>, report: &Reporter) {
    let Some(pad) = crop.static_pad("sink") else {
        return;
    };
    let weak = crop.downgrade();
    let (w, h) = (spec.width, spec.height);
    let (slot, report) = (Arc::clone(slot), Arc::clone(report));
    pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        let Some(gst::PadProbeData::Event(ev)) = &info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::EventView::Caps(c) = ev.view() else {
            return gst::PadProbeReturn::Ok;
        };
        let Ok(vi) = gst_video::VideoInfo::from_caps(c.caps()) else {
            fail_once(
                &slot,
                &report,
                format!("its decoded caps {} are not video", c.caps()),
            );
            return gst::PadProbeReturn::Drop;
        };
        if !matches!(
            vi.format(),
            gst_video::VideoFormat::Nv12
                | gst_video::VideoFormat::I420
                | gst_video::VideoFormat::Yv12
        ) {
            fail_once(
                &slot,
                &report,
                format!("its video is {:?}, not 8-bit 4:2:0", vi.format()),
            );
            return gst::PadProbeReturn::Drop;
        }
        let (vw, vh) = (vi.width(), vi.height());
        if let Some(why) = too_large(vw, vh) {
            fail_once(&slot, &report, why);
            return gst::PadProbeReturn::Drop;
        }
        if let Some(crop) = weak.upgrade() {
            let r = crop_to_fill(vw, vh, w, h);
            let px = |v: u32| i32::try_from(v).unwrap_or(0);
            crop.set_property("left", px(r.left));
            crop.set_property("right", px(r.right));
            crop.set_property("top", px(r.top));
            crop.set_property("bottom", px(r.bottom));
        }
        gst::PadProbeReturn::Ok
    });
}

/// Links the demuxer's first video pad, through the parser and decoder of
/// its codec, to `crop`; reports a codec or decoder that cannot play it, and
/// a file without video. Other pads stay unlinked.
fn link_video_pad(
    demux: &gst::Element,
    pipeline: &gst::Pipeline,
    crop: &gst::Element,
    cuda: bool,
    slot: &Arc<Slot>,
    report: &Reporter,
) {
    let linked = Arc::new(AtomicBool::new(false));
    let (pl, cr) = (pipeline.downgrade(), crop.downgrade());
    let (l1, s1, r1) = (Arc::clone(&linked), Arc::clone(slot), Arc::clone(report));
    demux.connect_pad_added(move |_, pad| {
        let Some(caps) = pad.current_caps().or_else(|| Some(pad.query_caps(None))) else {
            return;
        };
        let Some(name) = caps.structure(0).map(|s| s.name().to_string()) else {
            return;
        };
        if !name.starts_with("video/") || l1.swap(true, Ordering::SeqCst) {
            return;
        }
        let (Some(pipeline), Some(crop)) = (pl.upgrade(), cr.upgrade()) else {
            return;
        };
        // The coded size, before any decoder is made: a larger video never
        // gets one (its surfaces are sized by it). A pad without a size yet
        // is checked on its decoded caps only.
        let size = caps.structure(0).and_then(|s| {
            Some((
                u32::try_from(s.get::<i32>("width").ok()?).ok()?,
                u32::try_from(s.get::<i32>("height").ok()?).ok()?,
            ))
        });
        let linked = Codec::of_caps(&name)
            .ok_or_else(|| format!("its video is {name}, not H.264, HEVC, VP9 or AV1"))
            .and_then(|codec| match size.and_then(|(w, h)| too_large(w, h)) {
                Some(why) => Err(why),
                None => Ok(codec),
            })
            .and_then(|codec| {
                let decoder = codec.decoder(cuda)?;
                let parser = make(codec.parser(), "parse")?;
                let dec = make(decoder, "decode").map_err(|_| {
                    format!("the {decoder} decoder of {} is not installed", codec.name())
                })?;
                if cuda && dec.find_property("max-display-delay").is_some() {
                    // The decoder's minimum of surfaces: the slot takes the
                    // newest frame anyway.
                    dec.set_property("max-display-delay", 0i32);
                }
                pipeline
                    .add_many([&parser, &dec])
                    .map_err(|e| format!("internal: {e}"))?;
                gst::Element::link_many([&parser, &dec, &crop])
                    .map_err(|e| format!("{decoder} cannot output to the converter: {e}"))?;
                for e in [&dec, &parser] {
                    e.sync_state_with_parent()
                        .map_err(|e| format!("{decoder} could not start: {e}"))?;
                }
                let sink = parser.static_pad("sink").ok_or("internal: no parser pad")?;
                pad.link(&sink)
                    .map(|_| ())
                    .map_err(|e| format!("its {} stream cannot be parsed: {e:?}", codec.name()))
            });
        if let Err(why) = linked {
            fail_once(&s1, &r1, why);
        }
    });
    let (s2, r2) = (Arc::clone(slot), Arc::clone(report));
    demux.connect_no_more_pads(move |_| {
        if !linked.load(Ordering::SeqCst) {
            fail_once(&s2, &r2, "the file has no video stream".into());
        }
    });
}

/// The bus thread: prerolls, seeks to a looping segment, plays; loops on
/// each `SEGMENT_DONE`; reports the first error; and once `stop` is set
/// sets the pipeline to `NULL` and returns.
fn run_bus(
    pipeline: gst::Pipeline,
    bus: gst::Bus,
    slot: Arc<Slot>,
    report: Reporter,
    stop: &AtomicBool,
) {
    let mut started = false;
    // The ends of the clip in a row with no frame decoded since the one
    // before, and the slot's frame count at the last end.
    let mut empty_loops = 0u32;
    let mut seq_at_end = slot.seq.load(Ordering::Relaxed);
    let mut clip_ended = |slot: &Slot| -> bool {
        let seq = slot.seq.load(Ordering::Relaxed);
        if seq == seq_at_end {
            empty_loops += 1;
        } else {
            empty_loops = 0;
            seq_at_end = seq;
        }
        empty_loops >= MAX_EMPTY_LOOPS
    };
    if pipeline.set_state(gst::State::Paused).is_err() {
        // The reason follows on the bus as an error; if not, this one.
        if let Some(m) = bus.timed_pop_filtered(
            gst::ClockTime::from_mseconds(100),
            &[gst::MessageType::Error],
        ) && let gst::MessageView::Error(e) = m.view()
        {
            fail_once(&slot, &report, e.error().to_string());
        }
        fail_once(&slot, &report, "the pipeline could not start".into());
    }
    let is_pipeline = |m: &gst::Message| {
        m.src()
            .is_some_and(|s| s == pipeline.upcast_ref::<gst::Object>())
    };
    while !stop.load(Ordering::SeqCst) {
        let Some(msg) = bus.timed_pop(BUS_POLL) else {
            continue;
        };
        match msg.view() {
            gst::MessageView::Application(a) if a.structure().is_some_and(|s| s.name() == STOP) => {
                break;
            }
            gst::MessageView::AsyncDone(_) if !started && is_pipeline(&msg) => {
                started = true;
                if slot.seq.load(Ordering::Relaxed) == 0 {
                    // Prerolled on the end of the file: no frame at all
                    // (the sink stores its preroll frame before this).
                    fail_once(&slot, &report, "it has no frames to play".into());
                    let _ = pipeline.set_state(gst::State::Null);
                    continue;
                }
                if pipeline
                    .seek_simple(
                        gst::SeekFlags::FLUSH | gst::SeekFlags::SEGMENT,
                        gst::ClockTime::ZERO,
                    )
                    .is_err()
                {
                    gst::warning!(
                        gst::CAT_DEFAULT,
                        "video background: the first segment seek was refused; playing once"
                    );
                }
                if pipeline.set_state(gst::State::Playing).is_err() {
                    fail_once(&slot, &report, "the pipeline could not play".into());
                }
            }
            gst::MessageView::SegmentDone(_) | gst::MessageView::Eos(_) if clip_ended(&slot) => {
                // A clip without a frame to show (none, or of no duration)
                // would loop for ever.
                fail_once(&slot, &report, "it has no frames to play".into());
                let _ = pipeline.set_state(gst::State::Null);
            }
            gst::MessageView::SegmentDone(_) => {
                // The gapless loop: the next segment, from the start.
                let _ = pipeline.seek_simple(gst::SeekFlags::SEGMENT, gst::ClockTime::ZERO);
            }
            gst::MessageView::Eos(_) => {
                // A demuxer that did not take the segment seek: start over.
                let _ = pipeline.seek_simple(
                    gst::SeekFlags::FLUSH | gst::SeekFlags::SEGMENT,
                    gst::ClockTime::ZERO,
                );
            }
            gst::MessageView::Error(e) => {
                fail_once(&slot, &report, e.error().to_string());
                // Nothing more comes: the decoder's resources go now (the
                // slot keeps its last frame).
                let _ = pipeline.set_state(gst::State::Null);
            }
            _ => {}
        }
    }
    let _ = pipeline.set_state(gst::State::Null);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn containers_are_told_by_their_first_bytes() {
        assert_eq!(
            Container::of(&[0x1A, 0x45, 0xDF, 0xA3, 1, 2]),
            Some(Container::Matroska)
        );
        assert_eq!(
            Container::of(b"\0\0\0\x20ftypisom"),
            Some(Container::IsoMedia)
        );
        assert_eq!(Container::of(b"\0\0\0\x08moov"), Some(Container::IsoMedia));
        assert_eq!(Container::of(b"\x89PNG\r\n\x1a\n"), None);
        assert_eq!(Container::of(b"ab"), None);
    }

    #[test]
    fn system_memory_plays_vp9_only() {
        assert_eq!(Codec::Vp9.decoder(false), Ok("vp9dec"));
        for c in [Codec::H264, Codec::H265, Codec::Av1] {
            let e = c.decoder(false).unwrap_err();
            assert!(e.contains("only VP9"), "{e}");
            assert!(c.decoder(true).unwrap().starts_with("nv"));
        }
        assert_eq!(Codec::of_caps("video/x-vp9"), Some(Codec::Vp9));
        assert_eq!(Codec::of_caps("video/x-theora"), None);
    }

    #[test]
    fn a_file_that_is_no_video_container_is_refused_with_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("not-a-video.webm");
        std::fs::write(&p, b"\x89PNG\r\n\x1a\n0000").unwrap();
        let e = check_video_file(&p).unwrap_err();
        assert!(e.contains("not an MP4"), "{e}");
        let e = check_video_file(dir.path()).unwrap_err();
        assert!(e.contains("regular file"), "{e}");
        let e = check_video_file(&dir.path().join("missing.mp4")).unwrap_err();
        assert!(e.contains("unreadable"), "{e}");
        assert!(check_video_file(Path::new("rel.mp4")).is_err());
    }
}
