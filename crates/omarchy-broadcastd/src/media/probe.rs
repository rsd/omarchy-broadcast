//! The check of a video file: container, codec, profile, size, frame rate
//! and duration, read with GStreamer without decoding a frame.
//!
//! The file is opened by the daemon first (never blocking on a FIFO or a
//! device, a regular file of at most [`MAX_VIDEO_BYTES`]), and GStreamer
//! reads that same open file through `/proc/self/fd/<n>`, so a path swapped
//! after the check cannot redirect it. The pipeline is
//! `filesrc ! typefind ! <demuxer>`, the demuxer chosen by the type found
//! (`qtdemux` for MP4, M4V and MOV, `matroskademux` for Matroska and WebM;
//! any other type is refused), the first video stream through its parser
//! (`h264parse`, `h265parse`, `vp9parse`, `av1parse`) into a `fakesink`,
//! every other stream (audio) into a `fakesink` that does not hold the
//! preroll. The caps after the parser give the size, the rate, the profile,
//! the chroma format and the bit depth; the duration comes from a query.
//! No decoder is ever created. The whole check is bounded by
//! [`PROBE_TIMEOUT`].
//!
//! Without a parser (they are in gst-plugins-bad), the demuxer's caps are
//! used: the size, the rate and, for H.264 and HEVC, the profile are there;
//! a chroma format or bit depth that is not stated is taken as unknown,
//! and only a stated one outside 8-bit 4:2:0 is refused.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gst::prelude::*;

/// The largest video file accepted, in bytes (512 MiB).
pub const MAX_VIDEO_BYTES: u64 = 512 * 1024 * 1024;

/// The widest video accepted, in pixels.
pub const MAX_VIDEO_WIDTH: u32 = 3840;

/// The tallest video accepted, in pixels.
pub const MAX_VIDEO_HEIGHT: u32 = 2160;

/// How long a check may take before it is abandoned.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// The container of a video, found from its content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Container {
    /// ISO base media (MP4, M4V): `video/quicktime, variant=iso`.
    Mp4,
    /// QuickTime (MOV).
    Mov,
    /// Matroska (MKV).
    Matroska,
    /// WebM (Matroska with the `webm` document type).
    WebM,
}

impl Container {
    /// The container's name: `mp4`, `mov`, `matroska` or `webm`.
    pub fn as_str(self) -> &'static str {
        match self {
            Container::Mp4 => "mp4",
            Container::Mov => "mov",
            Container::Matroska => "matroska",
            Container::WebM => "webm",
        }
    }

    /// The extension a stored file of this container gets.
    pub fn extension(self) -> &'static str {
        match self {
            Container::Mp4 => "mp4",
            Container::Mov => "mov",
            Container::Matroska => "mkv",
            Container::WebM => "webm",
        }
    }

    /// The ffmpeg demuxer forced when ffmpeg reads a file of this
    /// container (`-f`).
    pub fn ffmpeg_format(self) -> &'static str {
        match self {
            Container::Mp4 | Container::Mov => "mov",
            Container::Matroska | Container::WebM => "matroska",
        }
    }

    /// The container of typefind caps, if accepted.
    fn from_caps(s: &gst::StructureRef) -> Option<Container> {
        match s.name().as_str() {
            "video/quicktime" => Some(match s.get::<&str>("variant") {
                Ok("iso") => Container::Mp4,
                _ => Container::Mov,
            }),
            "video/x-matroska" => Some(Container::Matroska),
            "video/webm" => Some(Container::WebM),
            _ => None,
        }
    }

    fn demuxer(self) -> &'static str {
        match self {
            Container::Mp4 | Container::Mov => "qtdemux",
            Container::Matroska | Container::WebM => "matroskademux",
        }
    }
}

/// The video codec of a video stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    /// H.264 / AVC.
    H264,
    /// H.265 / HEVC.
    Hevc,
    /// VP9.
    Vp9,
    /// AV1.
    Av1,
}

impl Codec {
    /// The codec's name: `h264`, `hevc`, `vp9` or `av1`.
    pub fn as_str(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
            Codec::Vp9 => "vp9",
            Codec::Av1 => "av1",
        }
    }

    fn from_caps_name(name: &str) -> Option<Codec> {
        match name {
            "video/x-h264" => Some(Codec::H264),
            "video/x-h265" => Some(Codec::Hevc),
            "video/x-vp9" => Some(Codec::Vp9),
            "video/x-av1" => Some(Codec::Av1),
            _ => None,
        }
    }

    fn parser(self) -> &'static str {
        match self {
            Codec::H264 => "h264parse",
            Codec::Hevc => "h265parse",
            Codec::Vp9 => "vp9parse",
            Codec::Av1 => "av1parse",
        }
    }
}

/// What the check found in an accepted video.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoInfo {
    /// The container.
    pub container: Container,
    /// The codec of the video stream.
    pub codec: Codec,
    /// The codec profile as the parser names it (`high`, `main`, `0`, …);
    /// empty when not stated.
    pub profile: String,
    /// The width in pixels.
    pub width: u32,
    /// The height in pixels.
    pub height: u32,
    /// The frame rate's numerator; 0 when the rate is not stated
    /// (variable).
    pub fps_num: u32,
    /// The frame rate's denominator (at least 1).
    pub fps_den: u32,
    /// The duration in milliseconds; 0 when not known.
    pub duration_ms: u64,
}

/// Opens `path` for the check: without blocking, a regular file of at
/// most [`MAX_VIDEO_BYTES`]. The reason names the path.
pub fn open_video(path: &Path) -> Result<File, String> {
    let shown = path.display();
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| format!("the video file {shown} is unreadable: {e}"))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("the video file {shown} is unreadable: {e}"))?;
    if !meta.is_file() {
        return Err(format!("the video file {shown} is not a regular file"));
    }
    if meta.len() > MAX_VIDEO_BYTES {
        return Err(format!(
            "the video file {shown} is {} bytes, more than the {} MiB limit",
            meta.len(),
            MAX_VIDEO_BYTES / (1024 * 1024)
        ));
    }
    Ok(file)
}

/// Checks the video at `path` (see the module documentation) and returns
/// what it found. Refused, with the reason: an unreadable or oversized
/// file, a container other than MP4/M4V/MOV/Matroska/WebM, no video
/// stream, a codec other than H.264, HEVC, VP9 or AV1, a size over
/// [`MAX_VIDEO_WIDTH`] x [`MAX_VIDEO_HEIGHT`], a stated chroma format other
/// than 4:2:0 or a stated bit depth other than 8, and a check that takes
/// longer than [`PROBE_TIMEOUT`].
pub fn probe_video(path: &Path) -> Result<VideoInfo, String> {
    let file = open_video(path)?;
    probe_open(&file).map_err(|reason| format!("the video file {}: {reason}", path.display()))
}

thread_local! {
    /// Whether this thread must never run the check (the controller's).
    static NO_PROBE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Marks the calling thread as one that never runs the check: the
/// controller's, which a check (up to [`PROBE_TIMEOUT`] per pipeline
/// state) would block. A check on it is then a bug, caught in debug
/// builds and tests by an assertion.
pub fn forbid_on_this_thread() {
    NO_PROBE.set(true);
}

/// [`probe_video`] of a file already opened by [`open_video`]; the reason
/// does not name the path.
pub fn probe_open(file: &File) -> Result<VideoInfo, String> {
    debug_assert!(
        !NO_PROBE.get(),
        "the video check ran on a thread that must not block on it (the controller's)"
    );
    gst::init().map_err(|e| format!("GStreamer: {e}"))?;
    let location = format!("/proc/self/fd/{}", file.as_raw_fd());
    let found = Arc::new(Mutex::new(Found::default()));
    let pipeline = gst::Pipeline::new();
    let result = run_probe(&pipeline, &location, &found);
    let _ = pipeline.set_state(gst::State::Null);
    let duration = result?;
    let found = found.lock().map_err(|_| "the check failed".to_string())?;
    info_from(&found, duration)
}

/// What the pipeline's callbacks record.
#[derive(Debug, Default)]
struct Found {
    container: Option<Container>,
    /// The type found, when it is not an accepted container.
    other_type: Option<String>,
    codec: Option<Codec>,
    /// A video stream of a codec not accepted.
    other_codec: Option<String>,
    /// The caps reaching the video branch's sink (after the parser).
    video_caps: Option<gst::Caps>,
    /// The demuxer's caps of the video stream (used when no parser).
    demux_caps: Option<gst::Caps>,
}

fn make(factory: &str) -> Result<gst::Element, String> {
    gst::ElementFactory::make(factory)
        .build()
        .map_err(|_| format!("the GStreamer element {factory} is not installed"))
}

/// A sink that takes a stream and lets it go: `async=false` when it must
/// not hold the preroll.
fn sink(holds_preroll: bool) -> Result<gst::Element, String> {
    let s = make("fakesink")?;
    s.set_property("sync", false);
    s.set_property("async", holds_preroll);
    Ok(s)
}

/// Builds and prerolls the pipeline; the duration in nanoseconds if known.
fn run_probe(
    pipeline: &gst::Pipeline,
    location: &str,
    found: &Arc<Mutex<Found>>,
) -> Result<Option<u64>, String> {
    let src = make("filesrc")?;
    src.set_property("location", location);
    let typefind = make("typefind")?;
    pipeline
        .add_many([&src, &typefind])
        .map_err(|e| e.to_string())?;
    src.link(&typefind).map_err(|e| e.to_string())?;

    let weak = pipeline.downgrade();
    let f = Arc::clone(found);
    typefind.connect("have-type", false, move |args| {
        let caps = args.get(2).and_then(|v| v.get::<gst::Caps>().ok());
        let (Some(caps), Some(pipeline)) = (caps, weak.upgrade()) else {
            return None;
        };
        let s = caps.structure(0)?;
        let Some(container) = Container::from_caps(s) else {
            if let Ok(mut f) = f.lock() {
                f.other_type = Some(s.name().to_string());
            }
            return None;
        };
        if let Ok(mut fl) = f.lock() {
            fl.container = Some(container);
        }
        let Ok(demux) = make(container.demuxer()) else {
            return None;
        };
        let typefind = args.first().and_then(|v| v.get::<gst::Element>().ok())?;
        if pipeline.add(&demux).is_err() || typefind.link(&demux).is_err() {
            return None;
        }
        let weak = pipeline.downgrade();
        let f = Arc::clone(&f);
        demux.connect_pad_added(move |_, pad| {
            if let Some(pipeline) = weak.upgrade() {
                demux_pad(&pipeline, pad, &f);
            }
        });
        demux.connect_no_more_pads(|demux| {
            let s = gst::Structure::new_empty(NO_MORE_PADS);
            let _ = demux.post_message(gst::message::Application::builder(s).src(demux).build());
        });
        let _ = demux.sync_state_with_parent();
        None
    });

    let bus = pipeline.bus().ok_or("the check has no bus")?;
    pipeline
        .set_state(gst::State::Paused)
        .map_err(|_| unreadable_reason(found, "it could not be read"))?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    // Until the video's parsed caps arrive (or it is clear there is no
    // video stream to wait for), then until the preroll for the duration,
    // briefly.
    let mut settle: Option<Instant> = None;
    loop {
        let now = Instant::now();
        let end = settle.map_or(deadline, |s| s.min(deadline));
        let left = end.saturating_duration_since(now);
        if left.is_zero() {
            if settle.is_some() {
                break;
            }
            return Err(format!(
                "checking it took longer than {} s",
                PROBE_TIMEOUT.as_secs()
            ));
        }
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_nseconds(left.as_nanos() as u64)) else {
            continue;
        };
        match msg.view() {
            gst::MessageView::Application(a) => {
                let name = a.structure().map(|s| s.name().to_string());
                match name.as_deref() {
                    Some(VIDEO_CAPS) => {
                        settle.get_or_insert_with(|| Instant::now() + SETTLE);
                    }
                    Some(NO_MORE_PADS) => {
                        let no_video = found.lock().map(|f| f.codec.is_none()).unwrap_or(true);
                        if no_video {
                            return Ok(None);
                        }
                    }
                    _ => {}
                }
            }
            gst::MessageView::AsyncDone(_) if settle.is_some() => break,
            gst::MessageView::Eos(_) => break,
            gst::MessageView::Error(e) => {
                return Err(unreadable_reason(
                    found,
                    &format!("it could not be read as a video: {}", e.error()),
                ));
            }
            _ => {}
        }
    }
    Ok(pipeline
        .query_duration::<gst::ClockTime>()
        .map(gst::ClockTime::nseconds))
}

/// The bus message posted when the video's caps reach its sink.
const VIDEO_CAPS: &str = "obc-probe-video-caps";

/// The bus message posted when the demuxer has added all its pads.
const NO_MORE_PADS: &str = "obc-probe-no-more-pads";

/// How long the check waits for the preroll after the caps, for the
/// duration.
const SETTLE: Duration = Duration::from_secs(2);

/// The reason for a pipeline that failed: what the callbacks found wrong
/// first (a type or codec not accepted), else `fallback`.
fn unreadable_reason(found: &Arc<Mutex<Found>>, fallback: &str) -> String {
    let Ok(f) = found.lock() else {
        return fallback.to_string();
    };
    if let Some(t) = &f.other_type {
        return not_accepted_type(t);
    }
    if let Some(c) = &f.other_codec {
        return not_accepted_codec(c);
    }
    fallback.to_string()
}

fn not_accepted_type(t: &str) -> String {
    format!("its format ({t}) is not accepted: a video must be MP4, M4V, MOV, MKV or WebM")
}

fn not_accepted_codec(c: &str) -> String {
    format!("its video codec ({c}) is not accepted: H.264, HEVC, VP9 or AV1 only")
}

/// Links a new demuxer pad: the first video stream through its parser, the
/// rest into sinks that do not hold the preroll.
fn demux_pad(pipeline: &gst::Pipeline, pad: &gst::Pad, found: &Arc<Mutex<Found>>) {
    let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
    let name = caps
        .structure(0)
        .map(|s| s.name().to_string())
        .unwrap_or_default();
    let Ok(mut f) = found.lock() else {
        return;
    };
    let first_video = name.starts_with("video/") && f.codec.is_none() && f.other_codec.is_none();
    let branch = if first_video {
        match Codec::from_caps_name(&name) {
            Some(codec) => {
                f.codec = Some(codec);
                f.demux_caps = Some(caps.clone());
                video_branch(pipeline, codec, found)
            }
            None => {
                f.other_codec = Some(name);
                None
            }
        }
    } else {
        None
    };
    let head = match branch {
        Some(b) => b,
        None => {
            // Not the video: a sink that lets the stream go.
            let Ok(s) = sink(false) else { return };
            if pipeline.add(&s).is_err() {
                return;
            }
            let _ = s.sync_state_with_parent();
            let Some(p) = s.static_pad("sink") else {
                return;
            };
            p
        }
    };
    drop(f);
    let _ = pad.link(&head);
}

/// The video branch for `codec`: its parser (when installed) into a sink
/// that holds the preroll, whose first caps are recorded in `found` and
/// announced on the bus. The pad to link the demuxer to.
fn video_branch(
    pipeline: &gst::Pipeline,
    codec: Codec,
    found: &Arc<Mutex<Found>>,
) -> Option<gst::Pad> {
    let s = sink(true).ok()?;
    pipeline.add(&s).ok()?;
    let sink_pad = s.static_pad("sink")?;
    let f = Arc::clone(found);
    let element = s.downgrade();
    sink_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        let Some(gst::EventView::Caps(c)) = info.event().map(|e| e.view()) else {
            return gst::PadProbeReturn::Ok;
        };
        if let Ok(mut f) = f.lock() {
            f.video_caps = Some(c.caps_owned());
        }
        if let Some(e) = element.upgrade() {
            let s = gst::Structure::new_empty(VIDEO_CAPS);
            let _ = e.post_message(gst::message::Application::builder(s).src(&e).build());
        }
        gst::PadProbeReturn::Remove
    });
    let head = match make(codec.parser()) {
        Ok(parser) => {
            pipeline.add(&parser).ok()?;
            parser.link(&s).ok()?;
            let _ = parser.sync_state_with_parent();
            parser.static_pad("sink")?
        }
        Err(_) => sink_pad.clone(),
    };
    let _ = s.sync_state_with_parent();
    Some(head)
}

/// The info of a prerolled check, or the refusal.
fn info_from(found: &Found, duration_ns: Option<u64>) -> Result<VideoInfo, String> {
    if let Some(t) = &found.other_type {
        return Err(not_accepted_type(t));
    }
    let container = found
        .container
        .ok_or("it could not be read as a video: no type was found")?;
    if let Some(c) = &found.other_codec {
        return Err(not_accepted_codec(c));
    }
    let codec = found.codec.ok_or("it has no video stream")?;
    let caps = found
        .video_caps
        .clone()
        .or_else(|| found.demux_caps.clone())
        .ok_or("its video stream states no format")?;
    let s = caps
        .structure(0)
        .ok_or("its video stream states no format")?;
    let dim = |k: &str| s.get::<i32>(k).ok().and_then(|v| u32::try_from(v).ok());
    let (Some(width), Some(height)) = (dim("width"), dim("height")) else {
        return Err("its video stream states no size".into());
    };
    if width == 0 || height == 0 {
        return Err("its video stream states no size".into());
    }
    if width > MAX_VIDEO_WIDTH || height > MAX_VIDEO_HEIGHT {
        return Err(format!(
            "it is {width}x{height}, larger than the {MAX_VIDEO_WIDTH}x{MAX_VIDEO_HEIGHT} limit"
        ));
    }
    if let Ok(chroma) = s.get::<&str>("chroma-format")
        && chroma != "4:2:0"
    {
        return Err(format!(
            "its chroma format is {chroma}: only 8-bit 4:2:0 video is accepted"
        ));
    }
    if let Ok(depth) = s.get::<u32>("bit-depth-luma")
        && depth != 8
    {
        return Err(format!(
            "it is {depth}-bit: only 8-bit 4:2:0 video is accepted"
        ));
    }
    let profile = s.get::<&str>("profile").unwrap_or_default().to_string();
    // VP9 profiles 1 to 3 are 4:2:2/4:4:4 or high bit depth.
    if codec == Codec::Vp9 && !profile.is_empty() && profile != "0" {
        return Err(format!(
            "it is VP9 profile {profile}: only 8-bit 4:2:0 video is accepted"
        ));
    }
    let (fps_num, fps_den) = match s.get::<gst::Fraction>("framerate") {
        Ok(f) if f.numer() > 0 && f.denom() > 0 => (f.numer() as u32, f.denom() as u32),
        _ => (0, 1),
    };
    Ok(VideoInfo {
        container,
        codec,
        profile,
        width,
        height,
        fps_num,
        fps_den,
        duration_ms: duration_ns.map_or(0, |ns| ns / 1_000_000),
    })
}
