//! Shared GStreamer building blocks: element construction, the CPU ingest
//! adapters, colorimetry pinning, the owned [`MediaPipeline`], link-caps
//! inspection and the loopback format check.
//!
//! The stream itself is two pipelines joined by a latest-frame slot
//! (`docs/adr/0009-ingest-process-output.md`): one ingest pipeline per source
//! ([`crate::ingest`]) and one long-lived output pipeline
//! ([`crate::output`]).
//!
//! **Ingest adapters.** The camera format decides what runs before
//! `cudaupload` ([`IngestPath`]): formats `cudaconvert` accepts are uploaded
//! as captured (GPU-direct); YUY2/UYVY are converted to NV12 by
//! `videoconvert` on the CPU (`cudaconvert` does not accept packed 4:2:2);
//! MJPEG is decoded by `jpegdec` on the CPU into I420. A GPU (nvJPEG) decode
//! is future work. The CPU convert path keeps the camera's colorimetry label
//! through `videoconvert` ([`pinned_caps`]).
//!
//! An ingest of canonical frames in system memory has no upload: its
//! `videoconvertscale` takes the camera's frames as captured (YUY2/UYVY
//! included, with no separate `videoconvert`) or as decoded by `jpegdec`
//! ([`ingest_adapter`]).
//!
//! **The camera end** ([`camera_head`]). The camera source's `src` pad
//! carries the corrupt-frame guard, which drops frames the driver flagged
//! and incomplete JPEGs before anything else sees them
//! ([`crate::frame_guard`]). A raw camera with a range quirk row (full
//! range under a limited label, [`crate::quirks`]) is relabelled full range
//! right after its capsfilter, or in a system-memory ingest of a limited
//! contract gets the full-to-limited levels after its queue, relabelled
//! limited range; a raw camera
//! with no row has its first frames' luma looked at once, which only logs
//! ([`crate::colour_range`]).
//!
//! **CUDA memory from `cudaupload` on.** The CUDAMemory capsfilter right
//! after `cudaupload` is load-bearing: without it, `cudaupload` and the next
//! CUDA element may negotiate plain system memory and silently pass frames
//! through on the CPU. The "CUDAMemory on every link" guarantee
//! ([`all_cuda_between`]) starts at `cudaupload`: the ingest adapters before
//! it are CPU stages by design and run in system memory.

use gst::glib;
use gst::prelude::*;
use obc_core::config::VideoMemory;
use obc_core::{ColourRange, OutputContract, OutputFormat, PixelFormat, UsbId, VideoMode};
use serde::Serialize;
use std::sync::Arc;

use crate::capability::IngestPath;
use crate::frame_guard::FrameGuard;
pub use crate::process::GpuStage;

/// Element names in the ingest, output and consumer pipelines.
pub mod names {
    pub const CAMERA: &str = "camera";
    pub const CAMERA_CAPS: &str = "camera_caps";
    pub const QUEUE: &str = "queue";
    /// `jpegdec` (CPU decode path only).
    pub const DECODE: &str = "decode";
    /// `videobalance` of the system-memory full-to-limited range
    /// conversion (MJPEG, or a range quirk camera; limited contract only).
    pub const RANGE_BALANCE: &str = "range_balance";
    /// `capssetter` relabelling the balanced frames as limited range (the
    /// MJPEG path's, and a range quirk camera's in system memory).
    pub const RANGE_RELABEL: &str = "range_relabel";
    /// `capssetter` relabelling a raw camera's frames as full range, for a
    /// camera with a range quirk row ([`crate::colour_range`]).
    pub const RANGE_QUIRK_RELABEL: &str = "range_quirk_relabel";
    /// `videoconvert` (CPU convert path in CUDA memory only).
    pub const INGEST_CONVERT: &str = "ingest_convert";
    /// System-memory capsfilter after the CPU adapter.
    pub const INGEST_CAPS: &str = "ingest_caps";
    pub const UPLOAD: &str = "upload";
    pub const CUDA_CAPS: &str = "cuda_caps";
    pub const TO_BGR: &str = "to_bgr";
    pub const BGR_CAPS: &str = "bgr_caps";
    pub const TO_OUT: &str = "to_out";
    pub const OUT_CUDA_CAPS: &str = "out_cuda_caps";
    pub const DOWNLOAD: &str = "download";
    /// Output: system-memory capsfilter after `cudadownload` (canonical
    /// colour label).
    pub const DOWNLOAD_CAPS: &str = "download_caps";
    /// Output: `capssetter` giving the frames the sink's colour label.
    pub const RELABEL: &str = "relabel";
    pub const OUT_CAPS: &str = "out_caps";
    /// The output pipeline's sink: `v4l2sink`, `fakesink` without a
    /// loopback device, or the `appsink` forwarding to a loopback writer;
    /// also the loopback writer's sink (`v4l2sink` or `fakesink`).
    pub const LOOPBACK: &str = "loopback";
    /// Loopback writer: the `appsrc` placeholders and forwarded frames are
    /// pushed into.
    pub const WRITER_SRC: &str = "writer_src";

    /// Ingest: `videocrop` (crop-to-fill policy only).
    pub const CROP: &str = "crop";
    /// Ingest: `obcenhance`, the enhance stage (enhance only).
    pub const ENHANCE: &str = "enhance";
    /// Ingest: `cudaconvertscale` to the canonical frame.
    pub const SCALE: &str = "scale";
    /// Ingest: capsfilter with the canonical caps.
    pub const CANONICAL_CAPS: &str = "canonical_caps";
    /// Ingest: the `appsink` that writes canonical frames into the slot.
    pub const FRAMES_SINK: &str = "frames_sink";
    /// Output: the `appsrc` the output clock pushes canonical frames into.
    pub const FRAMES_SRC: &str = "frames_src";
    /// Output, roundtrip process stage: `cudaconvert` back to NV12.
    pub const FROM_BGR: &str = "from_bgr";
    /// Output, roundtrip process stage: the NV12 capsfilter after it.
    pub const PROCESS_CAPS: &str = "process_caps";
    /// Output, Process stage: the `obceffect` element (ADR-0012).
    pub const EFFECT: &str = "effect";

    pub const CONSUMER_SRC: &str = "consumer_src";
    pub const CONSUMER_CAPS: &str = "consumer_caps";
    pub const CONSUMER_SINK: &str = "consumer_sink";
}

/// The caps feature that marks buffers living in CUDA device memory.
pub const CUDA_MEMORY_FEATURE: &str = "memory:CUDAMemory";

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("GStreamer element {factory:?} is unavailable ({source}); is the plugin installed?")]
    MissingElement {
        factory: String,
        source: glib::BoolError,
    },
    #[error("invalid caps {0:?}")]
    Caps(String),
    #[error("failed to link the pipeline: {0}")]
    Link(String),
    #[error("state change failed: {0}")]
    State(String),
    /// A state change that failed with the error an element posted on the
    /// bus for it ([`MediaPipeline::state_failure`]): which element, the
    /// factory it was made from, and its error.
    #[error("state change failed: {element} ({factory}): {message}")]
    ElementState {
        element: String,
        factory: String,
        message: String,
    },
    /// v4l2loopback keeps the format of an earlier stream while any consumer
    /// still holds the device open, and refuses every other one.
    #[error(
        "loopback {device} now accepts only {accepted}, but this stream is {wanted}. \
         v4l2loopback keeps the format of an earlier stream while any consumer (e.g. OBS) \
         still holds the device open: close or reopen the consumer's video source, or \
         stream in that format"
    )]
    LoopbackFormatHeld {
        device: String,
        accepted: String,
        wanted: String,
        /// The held format as an output contract, when it is one this
        /// daemon can stream ([`LoopbackMismatch::held_as`]).
        held_as: Option<OutputContract>,
    },
    /// The loopback's accepted caps are not held by a consumer, but still
    /// do not admit the output contract.
    #[error("loopback {device} does not accept {wanted} (it accepts {accepted})")]
    LoopbackRefused {
        device: String,
        accepted: String,
        wanted: String,
    },
    #[error("CUDA: {0}")]
    Cuda(String),
    /// Effect settings that cannot be applied, or an output that has no
    /// effect element (`docs/adr/0012-video-effects-and-maxine-backend.md`).
    #[error("effect: {0}")]
    Effect(String),
    /// Module stages that cannot be built or applied, or an output that
    /// has no element for them (`crate::module`).
    #[error("module stage: {0}")]
    Module(String),
    #[error("camera control on {device}: {message}")]
    Control { device: String, message: String },
    #[error("invalid {property} {value:?} for {factory} (valid: {valid})")]
    InvalidProperty {
        factory: String,
        property: String,
        value: String,
        valid: String,
    },
}

/// A stop that failed ([`MediaPipeline::shutdown_checked`] and the
/// `stop_checked` of [`crate::Ingest`], [`crate::Output`] and
/// [`crate::Session`]): its error, and whether every pipeline it took down
/// reached NULL all the same.
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct StopError {
    pub error: PipelineError,
    /// Every pipeline the stop took down was confirmed in NULL: none of its
    /// elements runs any more (their streaming threads stopped, their
    /// devices and GPU resources released), and the error is of something
    /// else (a camera control not restored, an output clock thread that
    /// had panicked and was joined). `false` when a pipeline ended outside
    /// NULL, or its state change to NULL failed: some of its elements may
    /// still run.
    pub reached_null: bool,
}

impl From<StopError> for PipelineError {
    fn from(e: StopError) -> Self {
        e.error
    }
}

/// Two stops' outcomes as one: every pipeline reached NULL if both say so,
/// and the error is the first one, one that left a pipeline outside NULL
/// first.
pub(crate) fn both_stopped(
    first: Result<(), StopError>,
    then: Result<(), StopError>,
) -> Result<(), StopError> {
    match (first, then) {
        (Ok(()), r) | (r, Ok(())) => r,
        (Err(a), Err(b)) if a.reached_null && !b.reached_null => Err(b),
        (Err(mut a), Err(b)) => {
            a.reached_null &= b.reached_null;
            Err(a)
        }
    }
}

pub(crate) fn make(factory: &str, name: &str) -> Result<gst::Element, PipelineError> {
    gst::ElementFactory::make(factory)
        .name(name)
        .build()
        .map_err(|source| PipelineError::MissingElement {
            factory: factory.to_string(),
            source,
        })
}

/// Sets enum property `property` from its nick, returning an error (instead
/// of `set_property_from_str`'s panic) when the nick is not a valid value.
pub(crate) fn set_enum(
    element: &gst::Element,
    property: &str,
    nick: &str,
) -> Result<(), PipelineError> {
    let factory = || {
        element
            .factory()
            .map(|f| f.name().to_string())
            .unwrap_or_else(|| element.name().to_string())
    };
    let invalid = |valid: String| PipelineError::InvalidProperty {
        factory: factory(),
        property: property.to_string(),
        value: nick.to_string(),
        valid,
    };
    let class = element
        .find_property(property)
        .and_then(|p| p.downcast::<glib::ParamSpecEnum>().ok())
        .map(|p| p.enum_class())
        .ok_or_else(|| invalid("not an enum property".into()))?;
    let Some(value) = class.to_value_by_nick(nick) else {
        let valid = class
            .values()
            .iter()
            .map(|v| v.nick())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(invalid(valid));
    };
    element.set_property_from_value(property, &value);
    Ok(())
}

/// Checks that `nick` is a valid `v4l2src io-mode` without opening any device.
pub fn check_io_mode(nick: &str) -> Result<(), PipelineError> {
    let probe = make("v4l2src", "io_mode_check")?;
    set_enum(&probe, "io-mode", nick)
}

/// Raw formats whose colorimetry label is a YUV one (matrix, range), valid
/// on the NV12/I420 caps `cudaconvert` and `videoconvert` produce here.
fn is_yuv_format(format: &str) -> bool {
    matches!(format, "NV12" | "NV21" | "I420" | "YV12" | "YUY2" | "UYVY")
}

/// The caps to pin on a capsfilter (caps `base`, a YUV format) given the
/// `upstream` caps of the frames converted into it: `base` plus the
/// upstream `colorimetry`. `None` (leave the converter's choice) when the
/// upstream caps carry no colorimetry or are not YUV: an RGB/BGR label has an
/// RGB matrix (`sRGB`, `matrix=rgb`), wrong on YUV caps.
pub fn pinned_caps(base: &str, upstream: &gst::CapsRef) -> Option<gst::Caps> {
    let s = upstream.structure(0)?;
    if !s.get::<&str>("format").is_ok_and(is_yuv_format) {
        return None;
    }
    let colorimetry = s.get::<String>("colorimetry").ok()?;
    format!("{base},colorimetry={colorimetry}").parse().ok()
}

/// Copies the `colorimetry` of the caps flowing out of `from` onto the
/// capsfilter `to` (caps `base`, e.g. `video/x-raw,format=NV12`), before `to`
/// negotiates, when [`pinned_caps`] allows it. The sticky CAPS event passes
/// this probe before it reaches `to`, so the converter in between keeps the
/// upstream label instead of picking its own. Panic-free (probe context).
fn pin_colorimetry(from: &gst::Element, to: &gst::Element, base: String) {
    let Some(pad) = from.static_pad("src") else {
        return;
    };
    let to = to.downgrade();
    pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
        let Some(gst::EventView::Caps(ev)) = info.event().map(|e| e.view()) else {
            return gst::PadProbeReturn::Ok;
        };
        let Some(caps) = pinned_caps(&base, ev.caps()) else {
            return gst::PadProbeReturn::Ok;
        };
        let Some(to) = to.upgrade() else {
            return gst::PadProbeReturn::Ok;
        };
        let current = to.property::<Option<gst::Caps>>("caps");
        if current.as_ref() != Some(&caps) {
            to.set_property("caps", &caps);
        }
        gst::PadProbeReturn::Ok
    });
}

/// The CPU convert adapter: `videoconvert ! capsfilter video/x-raw,format=NV12`
/// ([`names::INGEST_CONVERT`], [`names::INGEST_CAPS`]), with the colorimetry
/// of the caps leaving `camera_caps` pinned onto the capsfilter.
pub(crate) fn cpu_convert_stage(
    camera_caps: &gst::Element,
    format: &str,
) -> Result<[gst::Element; 2], PipelineError> {
    let base = format!("video/x-raw,format={format}");
    let ingest_caps = capsfilter(names::INGEST_CAPS, &base)?;
    pin_colorimetry(camera_caps, &ingest_caps, base);
    Ok([make("videoconvert", names::INGEST_CONVERT)?, ingest_caps])
}

pub(crate) fn capsfilter(name: &str, caps: &str) -> Result<gst::Element, PipelineError> {
    let caps: gst::Caps = caps
        .parse()
        .map_err(|_| PipelineError::Caps(caps.to_string()))?;
    let e = make("capsfilter", name)?;
    e.set_property("caps", &caps);
    Ok(e)
}

pub(crate) fn cuda_caps(format: &str) -> String {
    format!("video/x-raw({CUDA_MEMORY_FEATURE}),format={format}")
}

/// An owned GStreamer pipeline. [`MediaPipeline::shutdown`] tears it down and
/// reports the outcome; dropping it without that (an error path) still sets
/// it to NULL first, so no path leaks a running pipeline or its CUDA context.
#[derive(Debug)]
pub struct MediaPipeline {
    pipeline: gst::Pipeline,
    /// The raw caps the pipeline feeds its loopback (format, size, rate),
    /// checked against what the device accepts by [`Self::prepare`]. `None`
    /// for a pipeline without a loopback (ingest, consumer).
    loopback_caps: Option<gst::Caps>,
}

/// The camera end of an ingest ([`camera_head`]): its elements in link
/// order, the format of the system-memory frames the last one hands the
/// canonical tail, and the camera's frame guard.
pub(crate) struct CameraHead {
    pub chain: Vec<gst::Element>,
    pub format: &'static str,
    /// The corrupt-frame guard on the camera source ([`crate::frame_guard`]).
    pub guard: Arc<FrameGuard>,
}

/// The camera end of an ingest chain of canonical frames in `memory`, up
/// to the canonical tail (`cudaupload` in CUDA memory):
/// `v4l2src ! capsfilter (mode) [! capssetter] ! queue (leaky)
/// [! videobalance ! capssetter] [! CPU adapter]` ([`camera_chain`]).
/// `usb` is the camera's USB id ([`crate::colour_range::camera_usb_id`]):
/// it selects a range quirk row.
pub(crate) fn camera_head(
    camera: &std::path::Path,
    mode: &VideoMode,
    io_mode: &str,
    queue_buffers: u32,
    memory: VideoMemory,
    range: ColourRange,
    usb: Option<UsbId>,
) -> Result<CameraHead, PipelineError> {
    let src = make("v4l2src", names::CAMERA)?;
    src.set_property("device", camera.to_string_lossy().as_ref());
    set_enum(&src, "io-mode", io_mode)?;
    camera_chain(
        src,
        &camera.display().to_string(),
        &mode.to_caps_string(),
        mode.format,
        queue_buffers,
        memory,
        range,
        usb,
    )
}

/// [`camera_head`] after its source `src` (named [`names::CAMERA`]; any
/// source in the tests): `camera_caps` the caps of the camera mode, in
/// `format`, `label` the camera for the logs.
///
/// * The frame guard goes on `src`'s `src` pad first, before any other
///   probe on it ([`crate::frame_guard`]).
/// * A raw camera with a range quirk row ([`crate::quirks`]) gets the fix
///   [`crate::colour_range::range_fix`] chooses: a full-range relabel
///   right after `camera_caps`, or the full-to-limited levels after the
///   queue followed by a limited-range relabel that keeps the rest of the
///   camera's label. The CPU adapter then pins its colorimetry from the
///   relabel, so `videoconvert` keeps the relabelled range.
/// * A raw camera with a USB id and no row gets the range watch
///   ([`crate::colour_range::watch_range`]), which only logs.
#[allow(clippy::too_many_arguments)]
pub(crate) fn camera_chain(
    src: gst::Element,
    label: &str,
    camera_caps: &str,
    format: PixelFormat,
    queue_buffers: u32,
    memory: VideoMemory,
    range: ColourRange,
    usb: Option<UsbId>,
) -> Result<CameraHead, PipelineError> {
    use crate::colour_range::{
        RangeFix, full_range_relabel, limited_range_relabel, range_fix, watch_range,
    };
    use names::*;
    let guard = crate::frame_guard::install(&src, label, format == PixelFormat::Mjpeg);

    let queue = make("queue", QUEUE)?;
    queue.set_property("max-size-buffers", queue_buffers.max(1));
    queue.set_property("max-size-bytes", 0u32);
    queue.set_property("max-size-time", 0u64);
    set_enum(&queue, "leaky", "downstream")?;

    let camera_caps = capsfilter(CAMERA_CAPS, camera_caps)?;
    let fix = range_fix(
        crate::quirks::full_range_despite_label(usb, format),
        memory,
        range,
    );
    let mut chain = vec![src, camera_caps.clone()];
    // The element whose src caps carry the camera's (relabelled) label.
    let mut labelled = camera_caps.clone();
    match (fix, usb) {
        (RangeFix::RelabelFull, _) => {
            labelled = full_range_relabel(&camera_caps)?;
            chain.push(labelled.clone());
        }
        (RangeFix::None, Some(usb)) => {
            let _ = watch_range(&camera_caps, usb, format);
        }
        _ => {}
    }
    chain.push(queue);
    if fix == RangeFix::LimitLevels {
        let balance = full_to_limited_levels()?;
        labelled = limited_range_relabel(&balance)?;
        chain.extend([balance, labelled.clone()]);
    }
    let (adapter, format) = ingest_adapter(&labelled, format, memory, range)?;
    chain.extend(adapter);
    Ok(CameraHead {
        chain,
        format,
        guard,
    })
}

/// The CPU ingest adapter after `camera_caps` (the capsfilter of the
/// camera mode, or the range quirk relabel after it: the element whose
/// `src` caps carry the camera's label) for a camera format, in an ingest
/// of canonical frames in
/// `memory` (see the module docs): the elements in link order (none on the
/// GPU-direct path) and the format of the system-memory frames they
/// deliver to the canonical tail.
///
/// In CUDA memory the format is [`IngestPath::upload_format`]. In system
/// memory the tail's `videoconvertscale` converts the format, the size and
/// the colour label in one pass, so the CPU convert path has no adapter
/// (a separate `videoconvert` would convert twice) and hands the tail the
/// camera's packed 4:2:2 frames as captured; MJPEG is still decoded by
/// `jpegdec` into I420.
///
/// `range` is the contract's colour range. For a limited contract the
/// system-memory MJPEG path converts the decoder's full range to limited
/// range itself ([`full_to_limited_range`]): `videoconvertscale` does not,
/// see there. A raw camera's range quirk is applied by [`camera_head`], not
/// here.
pub fn ingest_adapter(
    camera_caps: &gst::Element,
    format: PixelFormat,
    memory: VideoMemory,
    range: ColourRange,
) -> Result<(Vec<gst::Element>, &'static str), PipelineError> {
    use names::*;
    let path = IngestPath::of(format);
    match (path, memory) {
        (IngestPath::GpuDirect, _) | (IngestPath::CpuConvert, VideoMemory::System) => {
            Ok((Vec::new(), format.gst_name()))
        }
        (IngestPath::CpuConvert, VideoMemory::Cuda) => {
            // Keep the camera's colorimetry label through the CPU
            // conversion (videoconvert would otherwise pick its own).
            let to = path.upload_format(format);
            Ok((cpu_convert_stage(camera_caps, to)?.into(), to))
        }
        (IngestPath::CpuDecode, _) => {
            let to = path.upload_format(format);
            let decode = make("jpegdec", DECODE)?;
            let caps = capsfilter(INGEST_CAPS, &format!("video/x-raw,format={to}"))?;
            let mut chain = vec![decode, caps];
            if memory == VideoMemory::System && range == ColourRange::Limited {
                chain.extend(full_to_limited_range()?);
            }
            Ok((chain, to))
        }
    }
}

/// Luma gain of the full-range to limited-range conversion: 219/255 (the
/// limited luma span over the full one).
const LUMA_CONTRAST: f64 = 219.0 / 255.0;
/// Luma offset of the conversion, as `videobalance` brightness:
/// 16 * 219 / 255^2. With the gain above it maps Y=0 to 16 and Y=255 to
/// 235 (checked against `videobalance` 1.28 in the ingest tests).
const LUMA_BRIGHTNESS: f64 = 16.0 * 219.0 / (255.0 * 255.0);
/// Chroma gain of the conversion: 224/255 (the limited chroma span over the
/// full one), about the neutral 128.
const CHROMA_SATURATION: f64 = 224.0 / 255.0;
/// The colorimetry the balanced frames are relabelled with: BT.601, limited
/// range (the matrix and the transfer of `jpegdec`'s `1:4:0:0`, the range
/// of the contract).
const LIMITED_RELABEL: &str = "video/x-raw,colorimetry=(string)2:4:0:0";

/// Converts `jpegdec`'s full-range I420 to limited range:
/// `videobalance ! capssetter`.
///
/// GStreamer 1.28's `videoconvert` / `videoconvertscale` do not convert
/// the range when it is the only difference: the converter compares the
/// matrix and the primaries, so full-range BT.601 (`1:4:0:0`) reaching the
/// limited canonical caps (`2:4:5:1`) would pass through unchanged, with
/// black at Y=0. `videobalance` applies the exact levels instead (the
/// constants above), and `capssetter` then labels the frames limited range.
/// The relabel is essential: a GStreamer whose converter does check the
/// range would otherwise see full-range frames and convert them a second
/// time.
fn full_to_limited_range() -> Result<[gst::Element; 2], PipelineError> {
    let balance = full_to_limited_levels()?;
    let relabel = make("capssetter", names::RANGE_RELABEL)?;
    let caps: gst::Caps = LIMITED_RELABEL
        .parse()
        .map_err(|_| PipelineError::Link(format!("bad caps {LIMITED_RELABEL}")))?;
    relabel.set_property("caps", &caps);
    Ok([balance, relabel])
}

/// The levels of [`full_to_limited_range`] alone: a `videobalance`
/// ([`names::RANGE_BALANCE`]) that maps full-range luma and chroma to the
/// limited range, the caps passed as they are. `videobalance` 1.28 takes
/// the raw camera formats as they are (YUY2, UYVY, NV12, NV21, I420, YV12
/// among others), so a raw camera with a range quirk needs no conversion
/// before it ([`crate::colour_range`]).
pub(crate) fn full_to_limited_levels() -> Result<gst::Element, PipelineError> {
    let balance = make("videobalance", names::RANGE_BALANCE)?;
    balance.set_property("contrast", LUMA_CONTRAST);
    balance.set_property("brightness", LUMA_BRIGHTNESS);
    balance.set_property("saturation", CHROMA_SATURATION);
    Ok(balance)
}

/// Adds `chain` to `pipeline` and links it in order.
pub(crate) fn add_linked(
    pipeline: &gst::Pipeline,
    chain: &[gst::Element],
) -> Result<(), PipelineError> {
    pipeline
        .add_many(chain)
        .map_err(|e| PipelineError::Link(e.to_string()))?;
    gst::Element::link_many(chain).map_err(|e| PipelineError::Link(e.to_string()))
}

impl MediaPipeline {
    /// Wraps a built pipeline. `loopback_caps`: the raw caps it feeds its
    /// loopback (`None` when it has none), checked by [`Self::prepare`].
    pub(crate) fn from_parts(pipeline: gst::Pipeline, loopback_caps: Option<gst::Caps>) -> Self {
        MediaPipeline {
            pipeline,
            loopback_caps,
        }
    }

    /// Builds the bench consumer: `v4l2src device=<loopback> ! capsfilter ! fakesink`.
    /// `size` is the `(width, height)` of the stream feeding the loopback.
    /// It reads the virtual camera exactly as a video-call app would.
    pub fn build_consumer(
        loopback: &std::path::Path,
        size: (u32, u32),
        output: OutputFormat,
    ) -> Result<Self, PipelineError> {
        use names::*;
        let pipeline = gst::Pipeline::with_name("obc-consumer");
        let src = make("v4l2src", CONSUMER_SRC)?;
        src.set_property("device", loopback.to_string_lossy().as_ref());
        set_enum(&src, "io-mode", "mmap")?;
        let caps = capsfilter(
            CONSUMER_CAPS,
            &format!(
                "video/x-raw,format={},width={},height={}",
                output.gst_name(),
                size.0,
                size.1
            ),
        )?;
        let sink = make("fakesink", CONSUMER_SINK)?;
        sink.set_property("sync", false);
        sink.set_property("enable-last-sample", false);
        add_linked(&pipeline, &[src, caps, sink])?;
        Ok(MediaPipeline {
            pipeline,
            loopback_caps: None,
        })
    }

    pub fn pipeline(&self) -> &gst::Pipeline {
        &self.pipeline
    }

    pub fn element(&self, name: &str) -> Option<gst::Element> {
        self.pipeline.by_name(name)
    }

    pub fn bus(&self) -> gst::Bus {
        self.pipeline.bus().expect("a pipeline always has a bus")
    }

    /// Starts the pipeline: [`Self::prepare`], then PLAYING.
    pub fn play(&self) -> Result<(), PipelineError> {
        self.prepare()?;
        self.pipeline
            .set_state(gst::State::Playing)
            .map(|_| ())
            .map_err(|e| PipelineError::State(e.to_string()))
    }

    /// Takes the pipeline to READY (devices open). A pipeline that feeds a
    /// loopback then checks that the device accepts its caps:
    /// [`PipelineError::LoopbackFormatHeld`] or
    /// [`PipelineError::LoopbackRefused`] otherwise, instead of a
    /// `not-negotiated` streaming error once frames flow. On error the
    /// pipeline is back in NULL.
    pub fn prepare(&self) -> Result<(), PipelineError> {
        self.pipeline
            .set_state(gst::State::Ready)
            .map_err(|e| PipelineError::State(e.to_string()))?;
        if let Some(wanted) = &self.loopback_caps {
            self.check_loopback(wanted)?;
        }
        Ok(())
    }

    /// At READY, `v4l2sink` has opened the loopback and probed the formats
    /// it accepts now. A device a consumer still holds offers only the
    /// format, size and rate of the stream that fed it last.
    fn check_loopback(&self, wanted: &gst::Caps) -> Result<(), PipelineError> {
        let Some(sink) = self.element(names::LOOPBACK) else {
            return Ok(());
        };
        let Some(pad) = sink.static_pad("sink") else {
            return Ok(());
        };
        let accepted = pad.query_caps(None);
        let Some(m) = loopback_mismatch(wanted, &accepted) else {
            return Ok(());
        };
        let _ = self.pipeline.set_state(gst::State::Null);
        let device = sink
            .property::<Option<String>>("device")
            .unwrap_or_default();
        Err(if m.held {
            PipelineError::LoopbackFormatHeld {
                device,
                accepted: m.accepted,
                wanted: m.wanted,
                held_as: m.held_as,
            }
        } else {
            PipelineError::LoopbackRefused {
                device,
                accepted: m.accepted,
                wanted: m.wanted,
            }
        })
    }

    /// Latency query sent upstream from the last element's sink pad:
    /// `(live, min_ns, max_ns)`, `max` `None` when unbounded. This is what
    /// the elements upstream of the sink *declare* (v4l2src, queue, cuda*),
    /// not a measurement. Querying the pipeline itself is not useful here:
    /// a `sync=false` sink answers `live=false, min=0`.
    pub fn query_latency(&self) -> Option<(bool, u64, Option<u64>)> {
        let last = self.pipeline.iterate_sinks().into_iter().flatten().next()?;
        let pad = last.static_pad("sink")?;
        let mut q = gst::query::Latency::new();
        if pad.peer_query(&mut q) {
            let (live, min, max) = q.result();
            Some((live, min.nseconds(), max.map(|m| m.nseconds())))
        } else {
            None
        }
    }

    /// Graphviz dump of the pipeline with caps on every link.
    pub fn dot(&self) -> String {
        self.pipeline
            .debug_to_dot_data(gst::DebugGraphDetails::all())
            .to_string()
    }

    /// The negotiated caps on every link, walking the linear chain from the
    /// first element's `src` pad.
    pub fn links(&self) -> Vec<LinkCaps> {
        let mut out = Vec::new();
        let Some(first) = self.pipeline.iterate_sources().into_iter().flatten().next() else {
            return out;
        };
        let mut element = first;
        for _ in 0..64 {
            let Some(src) = element.static_pad("src") else {
                break;
            };
            let Some(peer) = src.peer() else { break };
            let Some(next) = peer.parent_element() else {
                break;
            };
            let caps = src.current_caps();
            out.push(LinkCaps {
                from: format!("{}:{}", element.name(), src.name()),
                to: format!("{}:{}", next.name(), peer.name()),
                cuda_memory: caps.as_ref().is_some_and(|c| {
                    c.features(0)
                        .is_some_and(|f| f.contains(CUDA_MEMORY_FEATURE))
                }),
                caps: caps.map(|c| c.to_string()),
            });
            element = next;
        }
        out
    }

    /// Sets the pipeline to NULL (synchronously) and drops it, releasing all
    /// elements, buffer pools and the CUDA context they share. Callers must
    /// not hold element references beyond this point.
    pub fn shutdown(self) -> Result<(), PipelineError> {
        self.shutdown_checked().map_err(Into::into)
    }

    /// What the state change failure `e` was, with the bus's word: when
    /// `e` is a [`PipelineError::State`] and an element posted an error on
    /// the bus, [`PipelineError::ElementState`] naming the first one that
    /// did (its element, factory and error); else `e`. It takes the bus's
    /// pending error messages, so call it before the pipeline goes back to
    /// NULL (which flushes the bus).
    pub fn state_failure(&self, e: PipelineError) -> PipelineError {
        if !matches!(e, PipelineError::State(_)) {
            return e;
        }
        let bus = self.bus();
        while let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error]) {
            let gst::MessageView::Error(err) = msg.view() else {
                continue;
            };
            let Some(element) = msg.src().and_then(|s| s.downcast_ref::<gst::Element>()) else {
                continue;
            };
            let factory = element
                .factory()
                .map(|f| f.name().to_string())
                .unwrap_or_default();
            return PipelineError::ElementState {
                element: element.name().to_string(),
                factory,
                message: err.error().to_string(),
            };
        }
        e
    }

    /// [`Self::shutdown`], its error saying whether the pipeline reached
    /// NULL: never, as it fails only when it did not (or the state change
    /// to NULL failed).
    pub fn shutdown_checked(self) -> Result<(), StopError> {
        let res = self.pipeline.set_state(gst::State::Null);
        // set_state(Null) completes synchronously; confirm before dropping.
        let (_, cur, _) = self.pipeline.state(gst::ClockTime::from_seconds(5));
        // Drop sets NULL again: a no-op on a pipeline already in NULL.
        drop(self);
        not_null(res, cur).map_or(Ok(()), |error| {
            Err(StopError {
                error,
                reached_null: false,
            })
        })
    }
}

/// The error of a state change to NULL (`res`) after which the pipeline's
/// state is `cur`: `None` when it reached NULL.
pub(crate) fn not_null(
    res: Result<gst::StateChangeSuccess, gst::StateChangeError>,
    cur: gst::State,
) -> Option<PipelineError> {
    match res {
        Err(e) => Some(PipelineError::State(e.to_string())),
        Ok(_) if cur != gst::State::Null => {
            Some(PipelineError::State(format!("ended in {cur:?}, not Null")))
        }
        Ok(_) => None,
    }
}

impl Drop for MediaPipeline {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Why a loopback refuses a stream ([`loopback_mismatch`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopbackMismatch {
    /// What the device accepts, for an error message (`NV12 1920x1080@60`).
    pub accepted: String,
    /// What the stream would send.
    pub wanted: String,
    /// The accepted caps (DMABuf variants aside) are fixed: a consumer holds
    /// the device in the format of an earlier stream.
    pub held: bool,
    /// With `held`: the held format as an output contract (limited range,
    /// as every loopback stream), when it is one an output can be built
    /// for (NV12 or I420, a valid size and rate); `None` otherwise.
    pub held_as: Option<OutputContract>,
}

/// `None` when the loopback's `accepted` caps admit the `wanted` stream;
/// otherwise both, described, and whether a consumer holds the device in
/// another fixed format (as opposed to a device that does not accept the
/// format at all).
///
/// A held device lists its colour label, but also lists each format a
/// second time without one, so caps intersection alone admits any label;
/// and GStreamer compares labels as strings (`bt601` vs `2:4:5:4`). When
/// the held structures list a label, the `wanted` label is compared with
/// them by meaning ([`same_colorimetry`]), and a mismatch is a held format:
/// `v4l2sink` would refuse it with `not-negotiated` once frames flow.
pub fn loopback_mismatch(wanted: &gst::Caps, accepted: &gst::Caps) -> Option<LoopbackMismatch> {
    if !accepted.can_intersect(wanted) {
        let held = is_held(accepted);
        return Some(LoopbackMismatch {
            accepted: describe_raw_caps(accepted),
            wanted: describe_raw_caps(wanted),
            held,
            held_as: held.then(|| held_contract(accepted)).flatten(),
        });
    }
    if !is_held(accepted) {
        return None;
    }
    let listed = held_colorimetries(accepted);
    let wanted_label = wanted
        .structure(0)
        .and_then(|s| s.get::<String>("colorimetry").ok())?;
    if listed.is_empty() || listed.iter().any(|l| same_colorimetry(l, &wanted_label)) {
        return None;
    }
    Some(LoopbackMismatch {
        accepted: format!(
            "{} colorimetry {}",
            describe_raw_caps(accepted),
            listed.join("|")
        ),
        wanted: format!("{} colorimetry {wanted_label}", describe_raw_caps(wanted)),
        held: true,
        held_as: held_contract(accepted),
    })
}

/// The output contract of a held loopback's fixed `caps` (the first
/// system-memory structure; [`is_held`]): its format, size and rate, in
/// limited range. `None` when that is no contract an output can carry.
///
/// A loopback held in a format no output carries (YUY2, say: another
/// producer's stream, since the daemon only streams NV12 and I420) gets
/// no fallback: no writer of ours can feed it. That is inherent; the
/// mismatch names the format held and the writer is tried again.
fn held_contract(caps: &gst::Caps) -> Option<OutputContract> {
    let (s, _) = caps
        .iter_with_features()
        .find(|(_, f)| !f.contains("memory:DMABuf"))?;
    let rate = s.get::<gst::Fraction>("framerate").ok()?;
    let c = OutputContract {
        format: s.get::<&str>("format").ok()?.parse::<OutputFormat>().ok()?,
        width: u32::try_from(s.get::<i32>("width").ok()?).ok()?,
        height: u32::try_from(s.get::<i32>("height").ok()?).ok()?,
        fps_num: u32::try_from(rate.numer()).ok()?,
        fps_den: u32::try_from(rate.denom()).ok()?,
        range: obc_core::ColourRange::Limited,
    };
    c.validate().ok().map(|()| c)
}

/// The colour labels the system-memory structures of `caps` list (a
/// string or a list of strings), deduplicated.
fn held_colorimetries(caps: &gst::Caps) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (s, features) in caps.iter_with_features() {
        if features.contains("memory:DMABuf") {
            continue;
        }
        let Ok(v) = s.value("colorimetry") else {
            continue;
        };
        let labels: Vec<String> = if let Ok(one) = v.get::<String>() {
            vec![one]
        } else if let Ok(list) = v.get::<gst::List>() {
            list.iter().filter_map(|l| l.get::<String>().ok()).collect()
        } else {
            Vec::new()
        };
        for l in labels {
            if !out.contains(&l) {
                out.push(l);
            }
        }
    }
    out
}

/// Whether two GStreamer colorimetry strings describe the same colour
/// handling (`bt601` and `2:4:5:4` do), as `v4l2sink` compares them: equal
/// range, matrix and primaries, and transfer functions that are
/// interchangeable at 8 bits. Unparsable labels compare as strings.
pub fn same_colorimetry(a: &str, b: &str) -> bool {
    use std::str::FromStr;
    match (
        gst_video::VideoColorimetry::from_str(a),
        gst_video::VideoColorimetry::from_str(b),
    ) {
        (Ok(x), Ok(y)) => x.is_equivalent(8, &y, 8),
        _ => a == b,
    }
}

/// Whether every system-memory structure of `caps` (and there is at least
/// one) fixes the stream format: one format, width, height and frame rate.
/// That is the shape of a loopback a consumer holds. Other fields are not
/// considered: a held v4l2loopback lists its colorimetry as a one-element
/// list (`{ bt601 }`), which GStreamer does not count as fixed. `v4l2sink`
/// also lists DMABuf variants of the same format, which are ignored.
fn is_held(caps: &gst::Caps) -> bool {
    let mut any = false;
    for (s, features) in caps.iter_with_features() {
        if features.contains("memory:DMABuf") {
            continue;
        }
        let fixed = s.get::<&str>("format").is_ok()
            && s.get::<i32>("width").is_ok()
            && s.get::<i32>("height").is_ok()
            && s.get::<gst::Fraction>("framerate").is_ok();
        if !fixed {
            return false;
        }
        any = true;
    }
    any
}

/// `FORMAT WxH@RATE` for each fixed system-memory raw-video structure
/// (deduplicated, `, `-joined; `v4l2sink` also lists DMABuf variants of the
/// same formats); the caps string for anything else.
fn describe_raw_caps(caps: &gst::Caps) -> String {
    let one = |s: &gst::StructureRef| -> Option<String> {
        let format = s.get::<&str>("format").ok()?;
        let width = s.get::<i32>("width").ok()?;
        let height = s.get::<i32>("height").ok()?;
        let rate = s.get::<gst::Fraction>("framerate").ok()?;
        let rate = if rate.denom() == 1 {
            rate.numer().to_string()
        } else {
            format!("{}/{}", rate.numer(), rate.denom())
        };
        Some(format!("{format} {width}x{height}@{rate}"))
    };
    let mut out: Vec<String> = Vec::new();
    for (s, features) in caps.iter_with_features() {
        if features.contains("memory:DMABuf") {
            continue;
        }
        let Some(d) = one(s) else {
            return caps.to_string();
        };
        if !out.contains(&d) {
            out.push(d);
        }
    }
    if out.is_empty() {
        "nothing".to_string()
    } else {
        out.join(", ")
    }
}

/// Negotiated caps on one link.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LinkCaps {
    pub from: String,
    pub to: String,
    pub caps: Option<String>,
    pub cuda_memory: bool,
}

/// Whether every link from `upload`'s src pad up to and including the link
/// into `download` carries CUDA memory. `None` if either element is not in
/// the chain or there is no link between them.
///
/// The scope starts at `cudaupload`: links before it (camera, queue and the
/// CPU ingest adapters `videoconvert`/`jpegdec`) are system memory by design.
pub fn all_cuda_between(links: &[LinkCaps], upload: &str, download: &str) -> Option<bool> {
    let start = links
        .iter()
        .position(|l| l.from.split(':').next() == Some(upload))?;
    let end = links
        .iter()
        .position(|l| l.to.split(':').next() == Some(download))?;
    if end < start {
        return None;
    }
    Some(links[start..=end].iter().all(|l| l.cuda_memory))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::require_elements;

    fn link(from: &str, to: &str, cuda: bool) -> LinkCaps {
        LinkCaps {
            from: format!("{from}:src"),
            to: format!("{to}:sink"),
            caps: None,
            cuda_memory: cuda,
        }
    }

    fn stop_err(what: &str, reached_null: bool) -> Result<(), StopError> {
        Err(StopError {
            error: PipelineError::State(what.into()),
            reached_null,
        })
    }

    /// `(error, reached_null)` of a combined stop.
    fn seen(r: Result<(), StopError>) -> Option<(String, bool)> {
        r.err().map(|e| (e.to_string(), e.reached_null))
    }

    #[test]
    fn two_stops_reach_null_only_if_both_do_and_report_an_unclean_one_first() {
        let s = |what: &str, null| Some((format!("state change failed: {what}"), null));
        assert_eq!(seen(both_stopped(Ok(()), Ok(()))), None);
        assert_eq!(
            seen(both_stopped(stop_err("a", true), Ok(()))),
            s("a", true)
        );
        assert_eq!(
            seen(both_stopped(Ok(()), stop_err("b", false))),
            s("b", false)
        );
        // The first error, unless only the second left a pipeline outside
        // NULL.
        assert_eq!(
            seen(both_stopped(stop_err("a", true), stop_err("b", true))),
            s("a", true)
        );
        assert_eq!(
            seen(both_stopped(stop_err("a", false), stop_err("b", true))),
            s("a", false)
        );
        assert_eq!(
            seen(both_stopped(stop_err("a", false), stop_err("b", false))),
            s("a", false)
        );
        assert_eq!(
            seen(both_stopped(stop_err("a", true), stop_err("b", false))),
            s("b", false)
        );
    }

    #[test]
    fn a_change_to_null_is_clean_only_when_it_worked_and_ended_in_null() {
        use gst::{State, StateChangeError, StateChangeSuccess};
        assert!(not_null(Ok(StateChangeSuccess::Success), State::Null).is_none());
        assert_eq!(
            not_null(Ok(StateChangeSuccess::Success), State::Ready).map(|e| e.to_string()),
            Some("state change failed: ended in Ready, not Null".into())
        );
        // Failed, whatever the state read afterwards says.
        assert!(not_null(Err(StateChangeError), State::Null).is_some());
    }

    #[test]
    fn cuda_between_all_true() {
        let links = vec![
            link("camera", "queue", false),
            link("queue", "upload", false),
            link("upload", "cuda_caps", true),
            link("cuda_caps", "to_bgr", true),
            link("to_bgr", "download", true),
            link("download", "loopback", false),
        ];
        assert_eq!(all_cuda_between(&links, "upload", "download"), Some(true));
    }

    #[test]
    fn cuda_between_detects_system_memory_hop() {
        let links = vec![
            link("upload", "cuda_caps", true),
            link("cuda_caps", "to_bgr", false),
            link("to_bgr", "download", true),
        ];
        assert_eq!(all_cuda_between(&links, "upload", "download"), Some(false));
    }

    #[test]
    fn cuda_between_missing_elements() {
        let links = vec![link("camera", "loopback", false)];
        assert_eq!(all_cuda_between(&links, "upload", "download"), None);
        // Reversed order is not a valid span.
        let links = vec![link("x", "download", true), link("upload", "y", true)];
        assert_eq!(all_cuda_between(&links, "upload", "download"), None);
    }

    #[test]
    fn invalid_enum_nick_is_an_error_not_a_panic() {
        gst::init().unwrap();
        let q = make("queue", "q").unwrap();
        set_enum(&q, "leaky", "downstream").unwrap();
        let e = set_enum(&q, "leaky", "sideways").unwrap_err().to_string();
        assert!(e.contains("sideways") && e.contains("downstream"), "{e}");
        assert!(set_enum(&q, "max-size-buffers", "2").is_err());
        assert!(set_enum(&q, "no-such-property", "x").is_err());
    }

    #[test]
    fn io_mode_check() {
        require_elements(&["v4l2src"]);
        check_io_mode("mmap").unwrap();
        check_io_mode("dmabuf").unwrap();
        assert!(matches!(
            check_io_mode("mmapp"),
            Err(PipelineError::InvalidProperty { .. })
        ));
    }

    #[test]
    fn dropping_a_playing_pipeline_sets_null() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::with_name("drop-test");
        let src = make("videotestsrc", "src").unwrap();
        src.set_property("is-live", true);
        let sink = make("fakesink", "sink").unwrap();
        pipeline.add_many([&src, &sink]).unwrap();
        src.link(&sink).unwrap();
        let p = MediaPipeline {
            loopback_caps: None,
            pipeline: pipeline.clone(),
        };
        p.play().unwrap();
        let (res, cur, _) = pipeline.state(gst::ClockTime::from_seconds(5));
        assert!(res.is_ok() && cur == gst::State::Playing, "{res:?} {cur:?}");
        drop(p);
        let (_, cur, _) = pipeline.state(gst::ClockTime::ZERO);
        assert_eq!(cur, gst::State::Null);
    }

    #[test]
    fn colorimetry_is_pinned_from_yuv_only() {
        gst::init().unwrap();
        let caps = |s: &str| s.parse::<gst::Caps>().unwrap();
        let out = cuda_caps("NV12");
        for (upstream, want) in [
            (
                "video/x-raw,format=NV12,colorimetry=2:4:5:1",
                Some("2:4:5:1"),
            ),
            (
                "video/x-raw,format=I420,colorimetry=1:4:0:0",
                Some("1:4:0:0"),
            ),
            ("video/x-raw,format=YUY2,colorimetry=bt709", Some("bt709")),
            // RGB/BGR camera on the GPU-direct path: an RGB matrix must not
            // land on NV12 caps; cudaconvert picks the YUV colorimetry.
            ("video/x-raw,format=RGB,colorimetry=sRGB", None),
            ("video/x-raw,format=BGR,colorimetry=1:1:5:1", None),
            // Nothing to copy.
            ("video/x-raw,format=NV12", None),
        ] {
            let got = pinned_caps(&out, &caps(upstream));
            assert_eq!(
                got.as_ref().map(|c| {
                    let s = c.structure(0).unwrap();
                    assert_eq!(s.get::<&str>("format").unwrap(), "NV12");
                    assert!(c.features(0).unwrap().contains(CUDA_MEMORY_FEATURE));
                    s.get::<String>("colorimetry").unwrap()
                }),
                want.map(str::to_string),
                "{upstream}"
            );
        }
    }

    /// CUDA-free: the CPU convert adapter the pipeline builds keeps the
    /// camera's colorimetry label on its NV12 output.
    #[test]
    fn cpu_convert_stage_keeps_camera_colorimetry() {
        require_elements(&["videotestsrc", "videoconvert", "fakesink"]);
        let colorimetry = |caps: Option<gst::Caps>| -> Option<String> {
            caps?.structure(0)?.get::<String>("colorimetry").ok()
        };
        // Labels videoconvert would not pick on its own for SD NV12.
        for camera in ["2:4:5:1", "1:4:0:0", "2:4:7:1"] {
            let pipeline = gst::Pipeline::with_name("colorimetry-test");
            let src = make("videotestsrc", "src").unwrap();
            src.set_property("num-buffers", 2i32);
            let camera_caps = capsfilter(
                names::CAMERA_CAPS,
                &format!("video/x-raw,format=YUY2,width=640,height=480,colorimetry={camera}"),
            )
            .unwrap();
            let [convert, ingest_caps] = cpu_convert_stage(&camera_caps, "NV12").unwrap();
            let sink = make("fakesink", "sink").unwrap();
            sink.set_property("sync", false);
            let chain = [src, camera_caps.clone(), convert, ingest_caps.clone(), sink];
            pipeline.add_many(&chain).unwrap();
            gst::Element::link_many(&chain).unwrap();
            let p = MediaPipeline {
                pipeline,
                loopback_caps: None,
            };
            p.play().unwrap();
            let msg = p
                .bus()
                .timed_pop_filtered(
                    gst::ClockTime::from_seconds(10),
                    &[gst::MessageType::Eos, gst::MessageType::Error],
                )
                .expect("EOS or error");
            assert!(matches!(msg.view(), gst::MessageView::Eos(_)), "{msg:?}");
            let src_label = colorimetry(camera_caps.static_pad("src").unwrap().current_caps());
            let out_caps = ingest_caps.static_pad("src").unwrap().current_caps();
            let out_label = colorimetry(out_caps.clone());
            p.shutdown().unwrap();
            // Both labels must be present: two absent labels are not "equal".
            assert_eq!(src_label.as_deref(), Some(camera), "camera caps");
            let out_caps = out_caps.expect("NV12 caps negotiated");
            assert_eq!(
                out_caps
                    .structure(0)
                    .unwrap()
                    .get::<&str>("format")
                    .unwrap(),
                "NV12"
            );
            assert_eq!(out_label.as_deref(), Some(camera), "{out_caps}");
        }
    }

    /// A loopback a consumer still holds offers one fixed format: a stream
    /// differing in rate, size or format is reported with both sides as
    /// held; a free loopback (formats and ranges) admits it; a free loopback
    /// that does not offer the format is refused, not held.
    #[test]
    fn loopback_mismatch_reports_held_format() {
        gst::init().unwrap();
        let caps = |s: &str| s.parse::<gst::Caps>().unwrap();
        let wanted = caps(
            "video/x-raw,format=NV12,width=1920,height=1080,framerate=30/1,colorimetry=2:4:5:4",
        );
        // As v4l2sink reports a held device (GStreamer 1.28): each raw
        // structure with and without a one-element colorimetry list, each
        // with its DMABuf variant.
        let held = |rate: &str, format: &str, width: u32| {
            let one = |colorimetry: &str| {
                format!(
                    "video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format={format},\
                     width={width},height=1080,pixel-aspect-ratio=1/1{colorimetry},\
                     framerate={rate}; \
                     video/x-raw,format={format},width={width},height=1080,\
                     pixel-aspect-ratio=1/1{colorimetry},framerate={rate}"
                )
            };
            caps(&format!("{}; {}", one(",colorimetry={ bt601 }"), one("")))
        };
        // Held as `held_as` (`WxH@FPS`, NV12), or refused.
        let mismatch = |accepted: &str, wanted: &str, held_as: Option<&str>| LoopbackMismatch {
            accepted: accepted.to_string(),
            wanted: wanted.to_string(),
            held: held_as.is_some(),
            held_as: held_as.map(|c| c.parse().unwrap()),
        };
        assert_eq!(
            loopback_mismatch(&wanted, &held("30/1", "NV12", 1920)),
            None
        );
        // The held label is compared by meaning: `bt601` is `2:4:5:4`, and
        // a stream labelled otherwise (here BT.709 primaries, or full
        // range) is refused before streaming, as v4l2sink would refuse it.
        for other in ["2:4:5:1", "1:4:5:4", "bt709"] {
            let relabelled = caps(&format!(
                "video/x-raw,format=NV12,width=1920,height=1080,framerate=30/1,\
                 colorimetry={other}"
            ));
            assert_eq!(
                loopback_mismatch(&relabelled, &held("30/1", "NV12", 1920)),
                Some(mismatch(
                    "NV12 1920x1080@30 colorimetry bt601",
                    &format!("NV12 1920x1080@30 colorimetry {other}"),
                    Some("1920x1080@30")
                )),
                "{other}"
            );
        }
        // An unlabelled stream, or a held device that lists no label, is
        // not compared.
        let unlabelled = caps("video/x-raw,format=NV12,width=1920,height=1080,framerate=30/1");
        assert_eq!(
            loopback_mismatch(&unlabelled, &held("30/1", "NV12", 1920)),
            None
        );
        let held_unlabelled = caps("video/x-raw,format=NV12,width=1920,height=1080,framerate=30/1");
        assert_eq!(loopback_mismatch(&wanted, &held_unlabelled), None);
        assert_eq!(
            loopback_mismatch(&wanted, &held("60/1", "NV12", 1920)),
            Some(mismatch(
                "NV12 1920x1080@60",
                "NV12 1920x1080@30",
                Some("1920x1080@60")
            ))
        );
        let ntsc = caps("video/x-raw,format=I420,width=1920,height=1080,framerate=30000/1001");
        assert_eq!(
            loopback_mismatch(&ntsc, &held("30/1", "NV12", 1920)),
            Some(mismatch(
                "NV12 1920x1080@30",
                "I420 1920x1080@30000/1001",
                Some("1920x1080@30")
            ))
        );
        assert!(
            loopback_mismatch(&wanted, &held("30/1", "NV12", 1280))
                .unwrap()
                .held
        );
        // Held in I420: that contract; held in a format no output makes
        // (YUY2): held, as nothing to stream instead.
        let i420 = loopback_mismatch(&wanted, &held("25/1", "I420", 1280)).unwrap();
        assert_eq!(
            i420.held_as,
            Some(OutputContract {
                format: OutputFormat::I420,
                .."1280x1080@25".parse().unwrap()
            })
        );
        let yuy2 = loopback_mismatch(&wanted, &held("30/1", "YUY2", 1920)).unwrap();
        assert!(yuy2.held && yuy2.held_as.is_none(), "{yuy2:?}");

        let free = caps(
            "video/x-raw,format={ I420, NV12, YUY2 },width=[ 2, 8192 ],height=[ 1, 8192 ],\
             framerate=[ 0/1, 2147483647/1 ]; \
             video/x-raw(memory:DMABuf),format=DMA_DRM,width=[ 2, 8192 ],height=[ 1, 8192 ]",
        );
        assert_eq!(loopback_mismatch(&wanted, &free), None);
        // Free, but without the wanted format: not held, a plain refusal.
        let no_nv12 = caps(
            "video/x-raw,format={ YUY2, UYVY },width=[ 2, 8192 ],height=[ 1, 8192 ],\
             framerate=[ 0/1, 2147483647/1 ]",
        );
        let m = loopback_mismatch(&wanted, &no_nv12).unwrap();
        assert!(!m.held && m.held_as.is_none(), "{m:?}");
        assert_eq!(m.wanted, "NV12 1920x1080@30");
        assert!(m.accepted.contains("YUY2"), "{m:?}");
        // Several formats at a fixed size and rate: not held.
        let formats =
            caps("video/x-raw,format={ NV12, YUY2 },width=1920,height=1080,framerate=15/1");
        assert!(!loopback_mismatch(&wanted, &formats).unwrap().held);
        // A fixed size with a range of rates is not held either.
        let ranged = caps("video/x-raw,format=NV12,width=1920,height=1080,framerate=[ 1/1, 60/1 ]");
        let m = loopback_mismatch(
            &caps("video/x-raw,format=NV12,width=1280,height=720,framerate=30/1"),
            &ranged,
        )
        .unwrap();
        assert!(!m.held, "{m:?}");

        let m = loopback_mismatch(&wanted, &gst::Caps::new_empty()).unwrap();
        assert_eq!(m.accepted, "nothing");
        assert!(!m.held, "an empty answer is not a held format");
    }

    #[test]
    fn colorimetry_labels_compare_by_meaning() {
        gst::init().unwrap();
        assert!(same_colorimetry("bt601", "2:4:5:4"));
        assert!(same_colorimetry("2:4:5:1", "2:4:5:1"));
        assert!(!same_colorimetry("2:4:5:1", "2:4:5:4"), "primaries differ");
        assert!(!same_colorimetry("1:4:5:4", "2:4:5:4"), "range differs");
        assert!(!same_colorimetry("bt709", "bt601"));
        assert!(same_colorimetry("not-a-label", "not-a-label"));
        assert!(!same_colorimetry("not-a-label", "bt601"));
    }

    #[test]
    fn cuda_caps_string() {
        assert_eq!(
            cuda_caps("NV12"),
            "video/x-raw(memory:CUDAMemory),format=NV12"
        );
    }

    /// CUDA-free: a state change an element failed with an error on the
    /// bus names that element and its factory; another error is kept.
    #[test]
    fn a_state_failure_names_the_element_that_posted_the_error() {
        require_elements(&["filesrc", "fakesink"]);
        let pipeline = gst::Pipeline::with_name("state-failure-test");
        let src = make("filesrc", "upload").unwrap();
        src.set_property("location", "/nonexistent/obc-state-failure-test");
        let sink = make("fakesink", "sink").unwrap();
        pipeline.add_many([&src, &sink]).unwrap();
        src.link(&sink).unwrap();
        let p = MediaPipeline {
            pipeline,
            loopback_caps: None,
        };
        let e = p.play().unwrap_err();
        assert!(matches!(e, PipelineError::State(_)), "{e:?}");
        let e = p.state_failure(e);
        let PipelineError::ElementState {
            element,
            factory,
            message,
        } = &e
        else {
            panic!("{e:?}")
        };
        assert_eq!((element.as_str(), factory.as_str()), ("upload", "filesrc"));
        assert!(!message.is_empty());
        assert!(
            e.to_string()
                .starts_with("state change failed: upload (filesrc): "),
            "{e}"
        );
        // Back in NULL the bus is flushed: with no error on it a failure
        // is kept as it is, and an error other than a state change is
        // never looked up.
        p.pipeline().set_state(gst::State::Null).unwrap();
        let other = p.state_failure(PipelineError::State("busy".into()));
        assert!(matches!(other, PipelineError::State(m) if m == "busy"));
        let caps = p.state_failure(PipelineError::Caps("x".into()));
        assert!(matches!(caps, PipelineError::Caps(_)));
        p.shutdown().unwrap();
    }
}
