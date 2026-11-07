//! GStreamer pipeline construction, link-caps inspection and teardown.
//!
//! The main pipeline (built element by element so every pad is addressable):
//!
//! ```text
//! v4l2src device=<camera> io-mode=mmap
//!   ! capsfilter  <camera mode>: video/x-raw,format=F,... or image/jpeg,...
//!   ! queue       max-size-buffers=2 leaky=downstream
//!   [ ! videoconvert ! capsfilter video/x-raw,format=NV12 ]   CPU convert (YUY2, UYVY)
//!   [ ! jpegdec      ! capsfilter video/x-raw,format=I420 ]   CPU decode (MJPEG)
//!   ! cudaupload
//!   ! capsfilter  video/x-raw(memory:CUDAMemory),format=<uploaded format>
//!   [ ! cudaconvert ! capsfilter (CUDAMemory) BGR ]          roundtrip only
//!   [ ! cudaconvert ! capsfilter (CUDAMemory) NV12|I420 ]    roundtrip, output != uploaded,
//!                                                            or CPU decode (to limited range)
//!   ! cudadownload
//!   ! capsfilter  video/x-raw,format=NV12|I420
//!   ! v4l2sink device=<loopback> sync=false
//! ```
//!
//! **Ingest adapters.** The camera format decides what runs before
//! `cudaupload` ([`IngestPath`]): formats `cudaconvert` accepts are uploaded
//! as captured (GPU-direct); YUY2/UYVY are converted to NV12 by
//! `videoconvert` on the CPU (`cudaconvert` does not accept packed 4:2:2);
//! MJPEG is decoded by `jpegdec` on the CPU into I420, which uploads and
//! always goes through `cudaconvert` (see the colorimetry note below). A GPU
//! (nvJPEG) decode is future work.
//!
//! **CUDA memory from `cudaupload` on.** The CUDAMemory capsfilter right
//! after `cudaupload` is load-bearing: without it, `cudaupload` and
//! `cudadownload` may negotiate plain system memory and silently pass frames
//! through on the CPU. The "CUDAMemory on every link" guarantee
//! ([`all_cuda_between`] with [`names::UPLOAD`] and [`names::DOWNLOAD`])
//! starts at `cudaupload`: the ingest adapters before it are CPU stages by
//! design and run in system memory.
//!
//! When a `cudaconvert` produces the output format from a YUV upload, the
//! colorimetry of the frames handed to `cudaupload` is copied onto the output
//! CUDA capsfilter as soon as those caps are known, so the label round-trips
//! unchanged (without it the BGR hop relabels BT.709 `2:4:5:1` as
//! sRGB-transfer `2:4:7:1`). An RGB/BGR upload is not pinned: its label
//! carries an RGB matrix, which is not a valid YUV colorimetry, so
//! `cudaconvert` picks the YUV one ([`pinned_caps`]). The CPU convert path
//! pins the camera's label onto `videoconvert`'s output the same way.
//!
//! The CPU decode path is the exception: `jpegdec` outputs full-range BT.601
//! (`1:4:0:0`, as JPEG data is), and apps that ignore the range flag would
//! crush its blacks and clip its whites. Its output CUDA capsfilter is fixed
//! to [`DECODE_OUTPUT_COLORIMETRY`] (limited range, same BT.601 matrix), so
//! `cudaconvert` rescales the levels on the GPU (Y 0..255 -> 16..235, chroma
//! -> 16..240; verified on frames) and the loopback gets the limited-range
//! label the other paths deliver. That conversion runs for I420 output too.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use gst::glib;
use gst::prelude::*;
use obc_core::{OutputFormat, VideoMode};
use serde::Serialize;

use crate::capability::IngestPath;

/// Element names in the main pipeline.
pub mod names {
    pub const CAMERA: &str = "camera";
    pub const CAMERA_CAPS: &str = "camera_caps";
    pub const QUEUE: &str = "queue";
    /// `jpegdec` (CPU decode path only).
    pub const DECODE: &str = "decode";
    /// `videoconvert` (CPU convert path only).
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
    pub const OUT_CAPS: &str = "out_caps";
    pub const LOOPBACK: &str = "loopback";

    pub const CONSUMER_SRC: &str = "consumer_src";
    pub const CONSUMER_CAPS: &str = "consumer_caps";
    pub const CONSUMER_SINK: &str = "consumer_sink";
}

/// The caps feature that marks buffers living in CUDA device memory.
pub const CUDA_MEMORY_FEATURE: &str = "memory:CUDAMemory";

/// What happens to frames while they are in CUDA memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuStage {
    /// Upload and download only (plus an output conversion if the output
    /// format differs from the camera's).
    #[default]
    Passthrough,
    /// NV12 -> BGR -> NV12/I420 with `cudaconvert`, in CUDA memory: the
    /// conversion cost an effect stage working on BGR pays.
    Roundtrip,
}

impl FromStr for GpuStage {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "passthrough" => Ok(GpuStage::Passthrough),
            "roundtrip" => Ok(GpuStage::Roundtrip),
            _ => Err(format!("unknown GPU stage {s:?} (passthrough|roundtrip)")),
        }
    }
}

impl fmt::Display for GpuStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            GpuStage::Passthrough => "passthrough",
            GpuStage::Roundtrip => "roundtrip",
        })
    }
}

/// Everything needed to build the main pipeline. Device paths are resolved
/// by discovery at runtime, never configured.
#[derive(Debug, Clone, Serialize)]
pub struct PipelineConfig {
    pub camera: PathBuf,
    pub sink: PathBuf,
    pub mode: VideoMode,
    pub gpu_stage: GpuStage,
    pub output: OutputFormat,
    /// `queue max-size-buffers` (leaky=downstream).
    pub queue_buffers: u32,
    /// `v4l2src io-mode` nick (`mmap`, `dmabuf`, `auto`, ...).
    pub camera_io_mode: String,
}

impl PipelineConfig {
    /// `mode` is a concrete mode the camera offers (see
    /// [`crate::capability::choose_mode`]); its format decides the ingest path.
    pub fn new(camera: PathBuf, sink: PathBuf, mode: VideoMode) -> Self {
        PipelineConfig {
            camera,
            sink,
            mode,
            gpu_stage: GpuStage::default(),
            output: OutputFormat::default(),
            queue_buffers: 2,
            camera_io_mode: "mmap".to_string(),
        }
    }

    /// How frames of the configured mode reach CUDA memory.
    pub fn ingest(&self) -> IngestPath {
        IngestPath::of(self.mode.format)
    }
}

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
    },
    #[error("invalid {property} {value:?} for {factory} (valid: {valid})")]
    InvalidProperty {
        factory: String,
        property: String,
        value: String,
        valid: String,
    },
}

fn make(factory: &str, name: &str) -> Result<gst::Element, PipelineError> {
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
fn set_enum(element: &gst::Element, property: &str, nick: &str) -> Result<(), PipelineError> {
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
fn cpu_convert_stage(
    camera_caps: &gst::Element,
    format: &str,
) -> Result<[gst::Element; 2], PipelineError> {
    let base = format!("video/x-raw,format={format}");
    let ingest_caps = capsfilter(names::INGEST_CAPS, &base)?;
    pin_colorimetry(camera_caps, &ingest_caps, base);
    Ok([make("videoconvert", names::INGEST_CONVERT)?, ingest_caps])
}

fn capsfilter(name: &str, caps: &str) -> Result<gst::Element, PipelineError> {
    let caps: gst::Caps = caps
        .parse()
        .map_err(|_| PipelineError::Caps(caps.to_string()))?;
    let e = make("capsfilter", name)?;
    e.set_property("caps", &caps);
    Ok(e)
}

fn cuda_caps(format: &str) -> String {
    format!("video/x-raw({CUDA_MEMORY_FEATURE}),format={format}")
}

/// Output colorimetry of the CPU decode path: limited range, BT.601 matrix
/// (the JPEG matrix, so only the range changes), BT.709 transfer and
/// primaries, the label UVC cameras give their raw formats.
pub const DECODE_OUTPUT_COLORIMETRY: &str = "2:4:5:1";

/// An owned GStreamer pipeline. [`MediaPipeline::shutdown`] tears it down and
/// reports the outcome; dropping it without that (an error path) still sets
/// it to NULL first, so no path leaks a running pipeline or its CUDA context.
#[derive(Debug)]
pub struct MediaPipeline {
    pipeline: gst::Pipeline,
    /// The raw caps the main pipeline feeds its loopback (format, size,
    /// rate), checked against what the device accepts by [`Self::play`].
    /// `None` for the consumer pipeline.
    loopback_caps: Option<gst::Caps>,
}

impl MediaPipeline {
    /// Builds (but does not start) the main capture -> CUDA -> loopback pipeline.
    pub fn build(cfg: &PipelineConfig) -> Result<Self, PipelineError> {
        use names::*;
        let pipeline = gst::Pipeline::with_name("obc-main");

        let camera = make("v4l2src", CAMERA)?;
        camera.set_property("device", cfg.camera.to_string_lossy().as_ref());
        set_enum(&camera, "io-mode", &cfg.camera_io_mode)?;

        let queue = make("queue", QUEUE)?;
        queue.set_property("max-size-buffers", cfg.queue_buffers.max(1));
        queue.set_property("max-size-bytes", 0u32);
        queue.set_property("max-size-time", 0u64);
        set_enum(&queue, "leaky", "downstream")?;

        let ingest = cfg.ingest();
        let in_format = ingest.upload_format(cfg.mode.format);
        let out_format = cfg.output.gst_name();

        let camera_caps = capsfilter(CAMERA_CAPS, &cfg.mode.to_caps_string())?;
        let mut chain = vec![camera, camera_caps.clone(), queue];
        // CPU ingest adapters (see the module docs). The last system-memory
        // element before cudaupload carries the colorimetry of what is uploaded.
        match ingest {
            IngestPath::GpuDirect => {}
            IngestPath::CpuConvert => {
                // Keep the camera's colorimetry label through the CPU
                // conversion (videoconvert would otherwise pick its own).
                chain.extend(cpu_convert_stage(&camera_caps, in_format)?);
            }
            IngestPath::CpuDecode => {
                chain.push(make("jpegdec", DECODE)?);
                chain.push(capsfilter(
                    INGEST_CAPS,
                    &format!("video/x-raw,format={in_format}"),
                )?);
            }
        }
        let upload_input = chain.last().cloned().expect("chain is not empty");
        chain.push(make("cudaupload", UPLOAD)?);
        chain.push(capsfilter(CUDA_CAPS, &cuda_caps(in_format))?);
        if cfg.gpu_stage == GpuStage::Roundtrip {
            chain.push(make("cudaconvert", TO_BGR)?);
            chain.push(capsfilter(BGR_CAPS, &cuda_caps("BGR"))?);
        }
        if ingest == IngestPath::CpuDecode {
            // Full-range JPEG levels -> limited range (see the module docs).
            chain.push(make("cudaconvert", TO_OUT)?);
            chain.push(capsfilter(
                OUT_CUDA_CAPS,
                &format!(
                    "{},colorimetry={DECODE_OUTPUT_COLORIMETRY}",
                    cuda_caps(out_format)
                ),
            )?);
        } else if cfg.gpu_stage == GpuStage::Roundtrip || in_format != out_format {
            chain.push(make("cudaconvert", TO_OUT)?);
            let out_cuda_caps = capsfilter(OUT_CUDA_CAPS, &cuda_caps(out_format))?;
            // Pinned only for a YUV upload (see `pinned_caps`).
            pin_colorimetry(&upload_input, &out_cuda_caps, cuda_caps(out_format));
            chain.push(out_cuda_caps);
        }
        chain.push(make("cudadownload", DOWNLOAD)?);
        chain.push(capsfilter(
            OUT_CAPS,
            &format!("video/x-raw,format={out_format}"),
        )?);

        let sink = make("v4l2sink", LOOPBACK)?;
        sink.set_property("device", cfg.sink.to_string_lossy().as_ref());
        // Present frames as soon as they arrive: pacing comes from the camera.
        sink.set_property("sync", false);
        // Do not pin the last frame (one fewer buffer held from the pool).
        sink.set_property("enable-last-sample", false);
        chain.push(sink);

        pipeline
            .add_many(&chain)
            .map_err(|e| PipelineError::Link(e.to_string()))?;
        gst::Element::link_many(&chain).map_err(|e| PipelineError::Link(e.to_string()))?;
        let loopback_caps = format!(
            "video/x-raw,format={out_format},width={},height={},framerate={}/{}",
            cfg.mode.width, cfg.mode.height, cfg.mode.fps_num, cfg.mode.fps_den
        );
        let loopback_caps = loopback_caps
            .parse()
            .map_err(|_| PipelineError::Caps(loopback_caps))?;
        Ok(MediaPipeline {
            pipeline,
            loopback_caps: Some(loopback_caps),
        })
    }

    /// Builds the bench consumer: `v4l2src device=<loopback> ! capsfilter ! fakesink`.
    /// `mode` is the camera mode feeding the loopback (only its size is used).
    /// It reads the virtual camera exactly as a video-call app would.
    pub fn build_consumer(
        loopback: &std::path::Path,
        mode: &VideoMode,
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
                mode.width,
                mode.height
            ),
        )?;
        let sink = make("fakesink", CONSUMER_SINK)?;
        sink.set_property("sync", false);
        sink.set_property("enable-last-sample", false);
        let chain = [src, caps, sink];
        pipeline
            .add_many(&chain)
            .map_err(|e| PipelineError::Link(e.to_string()))?;
        gst::Element::link_many(&chain).map_err(|e| PipelineError::Link(e.to_string()))?;
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

    /// Starts the pipeline. The main pipeline first goes to READY (devices
    /// open) and checks that the loopback accepts its output
    /// ([`PipelineError::LoopbackFormatHeld`] otherwise, instead of a
    /// `not-negotiated` streaming error once the camera runs).
    pub fn play(&self) -> Result<(), PipelineError> {
        if let Some(wanted) = &self.loopback_caps {
            self.check_loopback(wanted)?;
        }
        self.pipeline
            .set_state(gst::State::Playing)
            .map(|_| ())
            .map_err(|e| PipelineError::State(e.to_string()))
    }

    /// At READY, `v4l2sink` has opened the loopback and probed the formats
    /// it accepts now. A device a consumer still holds offers only the
    /// format, size and rate of the stream that fed it last.
    fn check_loopback(&self, wanted: &gst::Caps) -> Result<(), PipelineError> {
        self.pipeline
            .set_state(gst::State::Ready)
            .map_err(|e| PipelineError::State(e.to_string()))?;
        let Some(sink) = self.element(names::LOOPBACK) else {
            return Ok(());
        };
        let Some(pad) = sink.static_pad("sink") else {
            return Ok(());
        };
        let accepted = pad.query_caps(None);
        match loopback_mismatch(wanted, &accepted) {
            None => Ok(()),
            Some((accepted, wanted)) => {
                let _ = self.pipeline.set_state(gst::State::Null);
                Err(PipelineError::LoopbackFormatHeld {
                    device: sink
                        .property::<Option<String>>("device")
                        .unwrap_or_default(),
                    accepted,
                    wanted,
                })
            }
        }
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
        let res = self.pipeline.set_state(gst::State::Null);
        // set_state(Null) completes synchronously; confirm before dropping.
        let (_, cur, _) = self.pipeline.state(gst::ClockTime::from_seconds(5));
        // Drop sets NULL again: a no-op on a pipeline already in NULL.
        drop(self);
        res.map_err(|e| PipelineError::State(e.to_string()))?;
        if cur != gst::State::Null {
            return Err(PipelineError::State(format!("ended in {cur:?}, not Null")));
        }
        Ok(())
    }
}

impl Drop for MediaPipeline {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// `None` when the loopback's `accepted` caps admit the `wanted` output;
/// otherwise both, described for an error message (`NV12 1920x1080@60`).
pub fn loopback_mismatch(wanted: &gst::Caps, accepted: &gst::Caps) -> Option<(String, String)> {
    if accepted.can_intersect(wanted) {
        return None;
    }
    Some((describe_raw_caps(accepted), describe_raw_caps(wanted)))
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

    fn link(from: &str, to: &str, cuda: bool) -> LinkCaps {
        LinkCaps {
            from: format!("{from}:src"),
            to: format!("{to}:sink"),
            caps: None,
            cuda_memory: cuda,
        }
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
    fn gpu_stage_parse() {
        assert_eq!(
            "roundtrip".parse::<GpuStage>().unwrap(),
            GpuStage::Roundtrip
        );
        assert_eq!(
            "Passthrough".parse::<GpuStage>().unwrap(),
            GpuStage::Passthrough
        );
        assert!("blur".parse::<GpuStage>().is_err());
        assert_eq!(GpuStage::Roundtrip.to_string(), "roundtrip");
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
        gst::init().unwrap();
        if gst::ElementFactory::find("v4l2src").is_none() {
            return; // plugin not installed; nothing to check against
        }
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

    /// The builder inserts the CPU adapter the camera format needs, uploads the
    /// adapter's output format, and converts to the loopback format on the GPU.
    #[test]
    fn ingest_adapters_per_format() {
        gst::init().unwrap();
        for f in [
            "v4l2src",
            "cudaupload",
            "cudaconvert",
            "jpegdec",
            "videoconvert",
        ] {
            if gst::ElementFactory::find(f).is_none() {
                eprintln!("SKIPPED: GStreamer element {f} is not installed");
                return;
            }
        }
        let caps_of = |p: &MediaPipeline, name: &str| {
            p.element(name)
                .map(|e| e.property::<gst::Caps>("caps").to_string())
        };
        for (mode, output, adapter, upload_fmt, converts) in [
            ("NV12:1920x1080@60", OutputFormat::Nv12, None, "NV12", false),
            ("NV12:1920x1080@60", OutputFormat::I420, None, "NV12", true),
            (
                "YUY2:1920x1080@60",
                OutputFormat::Nv12,
                Some(names::INGEST_CONVERT),
                "NV12",
                false,
            ),
            (
                "YUY2:1920x1080@60",
                OutputFormat::I420,
                Some(names::INGEST_CONVERT),
                "NV12",
                true,
            ),
            (
                "MJPEG:1920x1080@30",
                OutputFormat::Nv12,
                Some(names::DECODE),
                "I420",
                true,
            ),
            (
                "MJPEG:1920x1080@30",
                OutputFormat::I420,
                Some(names::DECODE),
                "I420",
                true, // range conversion
            ),
        ] {
            let mut cfg = PipelineConfig::new(
                "/dev/null-camera".into(),
                "/dev/null-sink".into(),
                mode.parse().unwrap(),
            );
            cfg.output = output;
            let p = MediaPipeline::build(&cfg).unwrap();
            let tag = format!("{mode} -> {output}");
            for a in [names::DECODE, names::INGEST_CONVERT] {
                assert_eq!(p.element(a).is_some(), adapter == Some(a), "{tag}: {a}");
            }
            assert_eq!(p.element(names::INGEST_CAPS).is_some(), adapter.is_some());
            if adapter.is_some() {
                assert_eq!(
                    caps_of(&p, names::INGEST_CAPS).unwrap(),
                    format!("video/x-raw, format=(string){upload_fmt}"),
                    "{tag}"
                );
            }
            assert_eq!(
                caps_of(&p, names::CUDA_CAPS).unwrap(),
                format!("video/x-raw(memory:CUDAMemory), format=(string){upload_fmt}"),
                "{tag}"
            );
            assert_eq!(p.element(names::TO_OUT).is_some(), converts, "{tag}");
            if adapter == Some(names::DECODE) {
                assert_eq!(
                    caps_of(&p, names::OUT_CUDA_CAPS).unwrap(),
                    format!(
                        "video/x-raw(memory:CUDAMemory), format=(string){}, colorimetry=(string){DECODE_OUTPUT_COLORIMETRY}",
                        output.gst_name()
                    ),
                    "{tag}"
                );
            }
            assert_eq!(
                caps_of(&p, names::OUT_CAPS).unwrap(),
                format!("video/x-raw, format=(string){}", output.gst_name()),
                "{tag}"
            );
            let camera_caps = caps_of(&p, names::CAMERA_CAPS).unwrap();
            assert_eq!(
                camera_caps.starts_with("image/jpeg"),
                mode.starts_with("MJPEG"),
                "{tag}: {camera_caps}"
            );
        }
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
        gst::init().unwrap();
        for f in ["videotestsrc", "videoconvert", "fakesink"] {
            if gst::ElementFactory::find(f).is_none() {
                eprintln!("SKIPPED: GStreamer element {f} is not installed");
                return;
            }
        }
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
    /// differing in rate, size or format is reported with both sides; a free
    /// loopback (formats and ranges) admits it.
    #[test]
    fn loopback_mismatch_reports_held_format() {
        gst::init().unwrap();
        let caps = |s: &str| s.parse::<gst::Caps>().unwrap();
        let wanted = caps("video/x-raw,format=NV12,width=1920,height=1080,framerate=30/1");
        let held = |rate: &str, format: &str, width: u32| {
            caps(&format!(
                "video/x-raw,format={format},width={width},height=1080,framerate={rate},\
                 colorimetry=2:4:5:1"
            ))
        };
        assert_eq!(
            loopback_mismatch(&wanted, &held("30/1", "NV12", 1920)),
            None
        );
        assert_eq!(
            loopback_mismatch(&wanted, &held("60/1", "NV12", 1920)),
            Some((
                "NV12 1920x1080@60".to_string(),
                "NV12 1920x1080@30".to_string()
            ))
        );
        let ntsc = caps("video/x-raw,format=I420,width=1920,height=1080,framerate=30000/1001");
        assert_eq!(
            loopback_mismatch(&ntsc, &held("30/1", "NV12", 1920)),
            Some((
                "NV12 1920x1080@30".to_string(),
                "I420 1920x1080@30000/1001".to_string()
            ))
        );
        assert!(loopback_mismatch(&wanted, &held("30/1", "NV12", 1280)).is_some());
        let free = caps(
            "video/x-raw,format={ I420, NV12, YUY2 },width=[ 2, 8192 ],height=[ 1, 8192 ],\
             framerate=[ 0/1, 2147483647/1 ]",
        );
        assert_eq!(loopback_mismatch(&wanted, &free), None);
        let (accepted, _) = loopback_mismatch(&wanted, &gst::Caps::new_empty()).unwrap();
        assert_eq!(accepted, "nothing");
    }

    #[test]
    fn cuda_caps_string() {
        assert_eq!(
            cuda_caps("NV12"),
            "video/x-raw(memory:CUDAMemory),format=NV12"
        );
    }
}
