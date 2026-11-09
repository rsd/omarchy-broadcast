//! The Ingest stage: one pipeline per source, from the camera to canonical
//! frames in the output's slot (`docs/adr/0009-ingest-process-output.md`).
//!
//! ```text
//! v4l2src device=<camera> io-mode=mmap
//!   ! capsfilter  <camera mode>
//!   ! queue       max-size-buffers=2 leaky=downstream
//!   [ ! videoconvert ! capsfilter video/x-raw,format=NV12 ]   CPU convert (YUY2, UYVY)
//!   [ ! jpegdec      ! capsfilter video/x-raw,format=I420 ]   CPU decode (MJPEG)
//!   ! cudaupload
//!   ! capsfilter  video/x-raw(memory:CUDAMemory),format=<uploaded format>
//!   [ ! videocrop ]                                          crop-to-fill policy only
//!   ! cudaconvertscale add-borders=<letterbox>
//!   ! capsfilter  <canonical caps>
//!   ! appsink     max-buffers=1 leaky-type=downstream -> latest-frame slot
//! ```
//!
//! Everything from `cudaupload` on is the **canonical tail**
//! ([`canonical_tail`]), shared by every source: it can be fed from any
//! system-memory source (a test pattern, in the GPU-only tests).
//!
//! **Crop and scale in one pass.** `videocrop` on CUDA memory copies
//! nothing: it attaches a crop region (`GstVideoCropMeta`), which
//! `cudaconvertscale` honours while scaling. The crop rectangle comes from
//! [`crate::geometry::crop_for`]. With the letterbox policy there is no
//! `videocrop`, and `cudaconvertscale` adds the black borders.
//!
//! **Colour.** The canonical caps fix the colorimetry, so `cudaconvertscale`
//! converts whatever was uploaded to the contract's label in the same pass:
//! the full-range BT.601 output of `jpegdec` (`1:4:0:0`) becomes limited
//! range for a limited contract, and a camera label that differs from the
//! contract's is converted. A camera mode that already matches the canonical
//! caps passes through without CUDA work. The CPU convert adapter keeps the
//! camera's label up to `cudaupload` ([`crate::pipeline::pinned_caps`]).
//!
//! **One CUDA context.** [`Ingest::start`] sets the output pipeline's CUDA
//! context on the ingest pipeline before it opens anything, so canonical
//! frames reach the output as CUDA memory of the same context, without a
//! copy.
//!
//! **System memory.** Inside the crate the same stage can also produce
//! canonical frames in system memory (`FrameMemory::System`), for the
//! CPU-only tests: no `cudaupload`, `videoconvertscale` in place of
//! `cudaconvertscale`, and no CUDA context.
//!
//! **Camera controls.** An ingest optionally pins the UVC
//! `exposure_dynamic_framerate` control off while it runs (a camera with it
//! on may lower its frame rate in dim light) and restores it when stopped or
//! dropped.
//!
//! **Failure.** An ingest that fails while streaming (the camera unplugged,
//! a streaming error, an end of stream) only stops feeding the slot: the
//! output carries on. [`Ingest::poll_failure`] tells the owner, without
//! blocking, so it can switch to another source.

use std::path::PathBuf;
use std::sync::Mutex;

use gst::prelude::*;
use obc_core::{AspectPolicy, VideoMode};

use crate::capability::IngestPath;
use crate::controls::{CID_EXPOSURE_AUTO_PRIORITY, ControlGuard, CtrlSkip, get_ctrl, probe_ctrl};
use crate::frame::{Canonical, FrameMemory, FrameWriter};
use crate::geometry::{Crop, crop_for};
use crate::pipeline::{
    MediaPipeline, PipelineError, StopError, add_linked, both_stopped, camera_head, capsfilter,
    cuda_caps, make, names,
};

/// The GStreamer context type that carries a `GstCudaContext`.
pub const CUDA_CONTEXT_TYPE: &str = "gst.cuda.context";

/// The `GstCudaContext` object a `gst.cuda.context` context carries: two
/// pipelines share CUDA memory without copies when these are the same
/// object.
pub fn cuda_context_object(ctx: &gst::Context) -> Option<gst::Object> {
    ctx.structure().get::<gst::Object>(CUDA_CONTEXT_TYPE).ok()
}

/// One camera source.
#[derive(Debug, Clone)]
pub struct IngestConfig {
    pub camera: PathBuf,
    /// A concrete mode the camera offers (see
    /// [`crate::capability::choose_mode`]); its format decides the adapter.
    pub mode: VideoMode,
    pub aspect: AspectPolicy,
    /// `queue max-size-buffers` (leaky=downstream).
    pub queue_buffers: u32,
    /// `v4l2src io-mode` nick (`mmap`, `dmabuf`, `auto`, ...).
    pub camera_io_mode: String,
    /// Pin `exposure_dynamic_framerate` off while this ingest runs.
    pub fixed_framerate: bool,
}

impl IngestConfig {
    pub fn new(camera: PathBuf, mode: VideoMode) -> Self {
        IngestConfig {
            camera,
            mode,
            aspect: AspectPolicy::default(),
            queue_buffers: 2,
            camera_io_mode: "mmap".to_string(),
            fixed_framerate: false,
        }
    }

    /// How frames of the configured mode reach CUDA memory.
    pub fn ingest_path(&self) -> IngestPath {
        IngestPath::of(self.mode.format)
    }
}

/// The canonical tail of an ingest pipeline (see the module docs):
/// `cudaupload ! capsfilter (CUDA, upload_format) [! videocrop] !
/// cudaconvertscale ! capsfilter (canonical)`, for system-memory frames of
/// `upload_format` at `width` x `height`. Returns the elements in link order
/// and the crop applied.
pub fn canonical_tail(
    upload_format: &str,
    width: u32,
    height: u32,
    canonical: &Canonical,
    aspect: AspectPolicy,
) -> Result<(Vec<gst::Element>, Crop), PipelineError> {
    tail_in(
        FrameMemory::Cuda,
        upload_format,
        (width, height),
        canonical,
        aspect,
    )
}

/// [`canonical_tail`] for canonical frames in `memory`. In system memory
/// there is no upload and `videoconvertscale` stands in for
/// `cudaconvertscale`: `[videocrop !] videoconvertscale ! capsfilter
/// (canonical, system memory)`.
fn tail_in(
    memory: FrameMemory,
    upload_format: &str,
    (width, height): (u32, u32),
    canonical: &Canonical,
    aspect: AspectPolicy,
) -> Result<(Vec<gst::Element>, Crop), PipelineError> {
    use names::*;
    let crop = crop_for(aspect, width, height, canonical.width, canonical.height);
    let mut tail = match memory {
        FrameMemory::Cuda => vec![
            make("cudaupload", UPLOAD)?,
            capsfilter(CUDA_CAPS, &cuda_caps(upload_format))?,
        ],
        FrameMemory::System => Vec::new(),
    };
    if aspect == AspectPolicy::Crop {
        let c = make("videocrop", CROP)?;
        for (side, v) in [
            ("left", crop.left),
            ("right", crop.right),
            ("top", crop.top),
            ("bottom", crop.bottom),
        ] {
            c.set_property(side, i32::try_from(v).unwrap_or(i32::MAX));
        }
        tail.push(c);
    }
    let scaler = match memory {
        FrameMemory::Cuda => "cudaconvertscale",
        FrameMemory::System => "videoconvertscale",
    };
    let scale = make(scaler, SCALE)?;
    scale.set_property("add-borders", aspect == AspectPolicy::Letterbox);
    tail.push(scale);
    tail.push(capsfilter(CANONICAL_CAPS, &memory.caps_string(canonical))?);
    Ok((tail, crop))
}

/// The `appsink` ending an ingest pipeline: keeps at most one frame, drops
/// older ones, and hands each frame to `writer` (O(1), see
/// [`FrameWriter::put`]).
pub fn frames_sink(name: &str, writer: FrameWriter) -> gst::Element {
    let sink = gst_app::AppSink::builder()
        .name(name)
        .sync(false)
        .max_buffers(1)
        .leaky_type(gst_app::AppLeakyType::Downstream)
        .wait_on_eos(false)
        .enable_last_sample(false)
        .build();
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Flushing)?;
                if let Some(buffer) = sample.buffer_owned() {
                    writer.put(buffer);
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    sink.upcast()
}

/// What happened to the camera's `exposure_dynamic_framerate` control.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ControlReport {
    /// Why the control was not read (absent, disabled, unreadable).
    pub skipped: Option<CtrlSkip>,
    /// Its value before this ingest started.
    pub before: Option<i32>,
    /// Its value while this ingest runs.
    pub during: Option<i32>,
    /// This ingest set it (and restores it on stop).
    pub pinned: bool,
    /// Pinning was asked for but the control is read-only.
    pub read_only: bool,
}

/// One running (or ready-to-run) source. See the module docs.
///
/// Dropping it stops the pipeline (NULL: camera closed) and then restores
/// the camera control it pinned; [`Ingest::stop`] does the same and reports
/// the outcome.
#[derive(Debug)]
pub struct Ingest {
    // Field order is drop order: the pipeline goes to NULL before the
    // control is restored.
    pipeline: MediaPipeline,
    guard: Option<ControlGuard>,
    camera: Option<PathBuf>,
    fixed_framerate: bool,
    source_id: u64,
    path: Option<IngestPath>,
    crop: Crop,
    controls: ControlReport,
    memory: FrameMemory,
    /// The first failure [`Self::poll_failure`] found (sticky).
    failure: Mutex<Option<String>>,
}

impl Ingest {
    /// Builds (but does not start) a camera ingest writing into `writer`.
    pub fn build(cfg: &IngestConfig, writer: FrameWriter) -> Result<Self, PipelineError> {
        let path = cfg.ingest_path();
        let head = camera_head(
            &cfg.camera,
            &cfg.mode,
            &cfg.camera_io_mode,
            cfg.queue_buffers,
        )?;
        let mut ingest = Self::from_head(
            head,
            path.upload_format(cfg.mode.format),
            (cfg.mode.width, cfg.mode.height),
            cfg.aspect,
            writer,
        )?;
        ingest.camera = Some(cfg.camera.clone());
        ingest.fixed_framerate = cfg.fixed_framerate;
        ingest.path = Some(path);
        Ok(ingest)
    }

    /// Builds an ingest from any system-memory source: `head` (linked in
    /// order) delivers `upload_format` frames of `size`; the canonical tail
    /// (for the writer's frame memory) and the slot writer are appended. No
    /// camera control is involved.
    pub fn from_head(
        head: Vec<gst::Element>,
        upload_format: &str,
        size: (u32, u32),
        aspect: AspectPolicy,
        writer: FrameWriter,
    ) -> Result<Self, PipelineError> {
        let source_id = writer.source_id();
        let memory = writer.memory();
        let pipeline = gst::Pipeline::with_name(&format!("obc-ingest-{source_id}"));
        let (tail, crop) = tail_in(memory, upload_format, size, &writer.canonical(), aspect)?;
        let mut chain = head;
        chain.extend(tail);
        chain.push(frames_sink(names::FRAMES_SINK, writer));
        add_linked(&pipeline, &chain)?;
        Ok(Ingest {
            pipeline: MediaPipeline::from_parts(pipeline, None),
            guard: None,
            camera: None,
            fixed_framerate: false,
            source_id,
            path: None,
            crop,
            controls: ControlReport::default(),
            memory,
            failure: Mutex::new(None),
        })
    }

    /// Starts the source: applies the camera-control policy, shares the
    /// output's CUDA context (`cuda`, see [`crate::output::Output::cuda_context`])
    /// and goes to PLAYING. The context is required: an ingest with a
    /// context of its own would hand the output frames of another context.
    /// On error the pipeline is back in NULL and any control already pinned
    /// is restored when the ingest is dropped.
    pub fn start(&mut self, cuda: &gst::Context) -> Result<(), PipelineError> {
        self.start_with(Some(cuda))
    }

    /// [`Self::start`] with the output's CUDA context if it has one: an
    /// ingest of system-memory frames ([`FrameMemory::System`]) starts
    /// without one, an ingest of CUDA frames refuses to.
    pub(crate) fn start_with(&mut self, cuda: Option<&gst::Context>) -> Result<(), PipelineError> {
        match cuda {
            Some(cuda) if cuda.context_type() != CUDA_CONTEXT_TYPE => {
                return Err(PipelineError::Cuda(format!(
                    "an ingest needs a {CUDA_CONTEXT_TYPE} context, not {}",
                    cuda.context_type()
                )));
            }
            None if self.memory == FrameMemory::Cuda => {
                return Err(PipelineError::Cuda(format!(
                    "an ingest of CUDA frames needs the output's {CUDA_CONTEXT_TYPE} context"
                )));
            }
            _ => {}
        }
        self.apply_controls()?;
        // Before NULL -> READY: the CUDA elements take it instead of
        // creating their own.
        if let Some(cuda) = cuda {
            self.pipeline.pipeline().set_context(cuda);
        }
        if let Err(e) = self.pipeline.play() {
            let _ = self.pipeline.pipeline().set_state(gst::State::Null);
            return Err(e);
        }
        Ok(())
    }

    fn apply_controls(&mut self) -> Result<(), PipelineError> {
        let Some(camera) = &self.camera else {
            return Ok(());
        };
        if self.guard.is_some() {
            return Ok(());
        }
        let mut report = ControlReport::default();
        match probe_ctrl(camera, CID_EXPOSURE_AUTO_PRIORITY) {
            Err(skip) => report.skipped = Some(skip),
            Ok((info, value)) => {
                report.before = Some(value);
                if self.fixed_framerate && value != 0 {
                    if info.read_only() {
                        report.read_only = true;
                    } else {
                        let guard = ControlGuard::set(camera, CID_EXPOSURE_AUTO_PRIORITY, value, 0)
                            .map_err(|e| PipelineError::Control {
                                device: camera.display().to_string(),
                                message: format!("setting exposure_dynamic_framerate=0: {e}"),
                            })?;
                        self.guard = Some(guard);
                        report.pinned = true;
                    }
                }
                report.during = get_ctrl(camera, CID_EXPOSURE_AUTO_PRIORITY).ok();
            }
        }
        self.controls = report;
        Ok(())
    }

    /// Stops the source: the pipeline goes to NULL (the camera is closed
    /// when this returns), then a pinned control is restored.
    pub fn stop(self) -> Result<(), PipelineError> {
        self.stop_checked().map_err(Into::into)
    }

    /// [`Self::stop`], its error saying whether the pipeline reached NULL
    /// (a control that could not be restored is an error that did).
    pub fn stop_checked(self) -> Result<(), StopError> {
        let Ingest {
            pipeline,
            guard,
            camera,
            ..
        } = self;
        let stopped = pipeline.shutdown_checked();
        let restored = match guard {
            Some(g) => g.restore().map_err(|e| StopError {
                error: PipelineError::Control {
                    device: camera.map(|c| c.display().to_string()).unwrap_or_default(),
                    message: format!("restoring exposure_dynamic_framerate: {e}"),
                },
                reached_null: true,
            }),
            None => Ok(()),
        };
        both_stopped(stopped, restored)
    }

    pub fn pipeline(&self) -> &MediaPipeline {
        &self.pipeline
    }

    /// Why this ingest has failed, if it has: an ERROR or an end of stream
    /// on its pipeline's bus, or (for a camera) its device node no longer
    /// existing. Non-blocking, and sticky: once found, the same failure is
    /// returned by every later call. It reads the pipeline's bus and
    /// discards the other messages there, so it is not combined with
    /// another reader of the same bus (the hardware tests' bus logs). An
    /// ingest that has not started reports only a vanished camera node.
    pub fn poll_failure(&self) -> Option<String> {
        let mut failure = self.failure.lock().unwrap_or_else(|e| e.into_inner());
        if failure.is_none() {
            *failure = self.find_failure();
        }
        failure.clone()
    }

    fn find_failure(&self) -> Option<String> {
        let bus = self.pipeline.bus();
        if let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error, gst::MessageType::Eos]) {
            let from = msg.src().map(|s| s.name().to_string()).unwrap_or_default();
            return Some(match msg.view() {
                gst::MessageView::Error(e) => format!(
                    "{from}: {}{}",
                    e.error(),
                    e.debug().map(|d| format!(" ({d})")).unwrap_or_default()
                ),
                _ => format!("{from}: end of stream"),
            });
        }
        let camera = self.camera.as_ref()?;
        (!camera.exists()).then(|| format!("camera {} disappeared", camera.display()))
    }

    /// The id of the source in the frames it writes ([`crate::frame::Frame::source_id`]).
    pub fn source_id(&self) -> u64 {
        self.source_id
    }

    /// The camera adapter (`None` for a non-camera source).
    pub fn ingest_path(&self) -> Option<IngestPath> {
        self.path
    }

    /// The crop applied to the source frames.
    pub fn crop(&self) -> Crop {
        self.crop
    }

    /// The camera-control outcome of [`Self::start`].
    pub fn controls(&self) -> &ControlReport {
        &self.controls
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use obc_core::OutputContract;

    use super::*;
    use crate::slot::Slot;
    use crate::test_support::require_elements;

    fn writer(contract: &OutputContract) -> FrameWriter {
        FrameWriter::new(
            Arc::new(Slot::new()),
            Instant::now(),
            1,
            Canonical::of(contract),
        )
    }

    /// The builder inserts the CPU adapter the camera format needs, uploads
    /// the adapter's output format, and ends in the canonical caps whatever
    /// the camera delivers.
    #[test]
    #[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
    fn gpu_ingest_adapters_per_format() {
        require_elements(&[
            "v4l2src",
            "cudaupload",
            "cudaconvertscale",
            "videocrop",
            "jpegdec",
            "videoconvert",
        ]);
        let caps_of = |p: &Ingest, name: &str| {
            p.pipeline()
                .element(name)
                .map(|e| e.property::<gst::Caps>("caps").to_string())
        };
        let contract = OutputContract::default();
        for (mode, adapter, upload_fmt) in [
            ("NV12:1920x1080@60", None, "NV12"),
            ("YUY2:1920x1080@60", Some(names::INGEST_CONVERT), "NV12"),
            ("MJPEG:1920x1080@30", Some(names::DECODE), "I420"),
            ("MJPEG:640x480@30", Some(names::DECODE), "I420"),
        ] {
            let cfg = IngestConfig::new("/dev/null-camera".into(), mode.parse().unwrap());
            let p = Ingest::build(&cfg, writer(&contract)).unwrap();
            for a in [names::DECODE, names::INGEST_CONVERT] {
                assert_eq!(
                    p.pipeline().element(a).is_some(),
                    adapter == Some(a),
                    "{mode}: {a}"
                );
            }
            assert_eq!(
                p.pipeline().element(names::INGEST_CAPS).is_some(),
                adapter.is_some()
            );
            if adapter.is_some() {
                assert_eq!(
                    caps_of(&p, names::INGEST_CAPS).unwrap(),
                    format!("video/x-raw, format=(string){upload_fmt}"),
                    "{mode}"
                );
            }
            assert_eq!(
                caps_of(&p, names::CUDA_CAPS).unwrap(),
                format!("video/x-raw(memory:CUDAMemory), format=(string){upload_fmt}"),
                "{mode}"
            );
            assert_eq!(
                caps_of(&p, names::CANONICAL_CAPS).unwrap(),
                "video/x-raw(memory:CUDAMemory), format=(string)NV12, width=(int)1920, \
                 height=(int)1080, pixel-aspect-ratio=(fraction)1/1, \
                 colorimetry=(string)2:4:5:1",
                "{mode}"
            );
            assert!(p.pipeline().element(names::TO_OUT).is_none());
            assert!(p.pipeline().element(names::DOWNLOAD).is_none());
            let camera_caps = caps_of(&p, names::CAMERA_CAPS).unwrap();
            assert_eq!(
                camera_caps.starts_with("image/jpeg"),
                mode.starts_with("MJPEG"),
                "{mode}: {camera_caps}"
            );
            assert_eq!(p.ingest_path(), Some(cfg.ingest_path()));
            // Crop-to-fill: only the 4:3 mode is cropped, 60 rows each side.
            let crop = p.pipeline().element(names::CROP).expect("videocrop");
            let want = if mode.ends_with("640x480@30") {
                (60, 60)
            } else {
                (0, 0)
            };
            assert_eq!(
                (crop.property::<i32>("top"), crop.property::<i32>("bottom")),
                want,
                "{mode}"
            );
            assert_eq!(
                (p.crop().top, p.crop().bottom),
                (want.0 as u32, want.1 as u32)
            );
            let scale = p.pipeline().element(names::SCALE).unwrap();
            assert!(!scale.property::<bool>("add-borders"));
        }
    }

    #[test]
    #[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
    fn gpu_letterbox_has_no_crop_and_adds_borders() {
        require_elements(&["videotestsrc", "cudaupload", "cudaconvertscale"]);
        let src = make("videotestsrc", "src").unwrap();
        let p = Ingest::from_head(
            vec![src],
            "NV12",
            (640, 480),
            AspectPolicy::Letterbox,
            writer(&OutputContract::default()),
        )
        .unwrap();
        assert!(p.pipeline().element(names::CROP).is_none());
        assert!(p.crop().is_empty());
        let scale = p.pipeline().element(names::SCALE).unwrap();
        assert!(scale.property::<bool>("add-borders"));
        assert!(p.pipeline().element(names::FRAMES_SINK).is_some());
        assert_eq!(p.source_id(), 1);
        assert_eq!(p.controls(), &ControlReport::default());
    }

    #[test]
    #[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
    fn gpu_starting_with_a_context_of_another_type_is_refused() {
        require_elements(&[
            "videotestsrc",
            "cudaupload",
            "cudaconvertscale",
            "videocrop",
        ]);
        let mut p = Ingest::from_head(
            vec![make("videotestsrc", "src").unwrap()],
            "NV12",
            (640, 480),
            AspectPolicy::Crop,
            writer(&OutputContract::default()),
        )
        .unwrap();
        let other = gst::Context::new("not.a.cuda.context", true);
        assert!(matches!(p.start(&other), Err(PipelineError::Cuda(_))));
        // CUDA frames need the output's context.
        assert!(matches!(p.start_with(None), Err(PipelineError::Cuda(_))));
        let (_, state, _) = p.pipeline().pipeline().state(gst::ClockTime::ZERO);
        assert_eq!(state, gst::State::Null);
    }

    /// System-memory frames: no upload, `videoconvertscale` in place of
    /// `cudaconvertscale`, the canonical caps without the CUDA feature, and
    /// the same crop and letterbox policies. CPU only.
    #[test]
    fn a_system_memory_tail_has_no_cuda_element() {
        require_elements(&["videotestsrc", "videocrop", "videoconvertscale", "appsink"]);
        let contract = OutputContract::default();
        for aspect in [AspectPolicy::Crop, AspectPolicy::Letterbox] {
            let p = Ingest::from_head(
                vec![make("videotestsrc", "src").unwrap()],
                "NV12",
                (640, 480),
                aspect,
                writer(&contract).in_memory(FrameMemory::System),
            )
            .unwrap();
            let pipeline = p.pipeline();
            assert!(pipeline.element(names::UPLOAD).is_none(), "{aspect:?}");
            assert!(pipeline.element(names::CUDA_CAPS).is_none(), "{aspect:?}");
            let scale = pipeline.element(names::SCALE).unwrap();
            assert_eq!(scale.factory().unwrap().name(), "videoconvertscale");
            let letterbox = aspect == AspectPolicy::Letterbox;
            assert_eq!(scale.property::<bool>("add-borders"), letterbox);
            assert_eq!(pipeline.element(names::CROP).is_none(), letterbox);
            // 4:3 into 16:9: 60 rows cropped at the top and the bottom.
            assert_eq!(p.crop().top, if letterbox { 0 } else { 60 });
            assert_eq!(
                pipeline
                    .element(names::CANONICAL_CAPS)
                    .unwrap()
                    .property::<gst::Caps>("caps")
                    .to_string(),
                "video/x-raw, format=(string)NV12, width=(int)1920, height=(int)1080, \
                 pixel-aspect-ratio=(fraction)1/1, colorimetry=(string)2:4:5:1"
            );
        }
    }

    /// A camera ingest whose device node disappears reports it
    /// ([`Ingest::poll_failure`]), sticky; a test source (no camera) never
    /// does on that ground. The node is a plain file here, never opened:
    /// the ingest is built, not started. CPU only.
    #[test]
    fn a_vanished_camera_node_is_reported() {
        require_elements(&["v4l2src", "videocrop", "videoconvertscale", "appsink"]);
        let dir = std::env::temp_dir().join(format!("obc-vanish-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let node = dir.join("video99");
        std::fs::write(&node, b"").unwrap();
        let contract = OutputContract::default();
        let cfg = IngestConfig::new(node.clone(), "NV12:640x480@30".parse().unwrap());
        let ingest = Ingest::build(&cfg, writer(&contract).in_memory(FrameMemory::System)).unwrap();
        assert_eq!(ingest.poll_failure(), None, "the node is there");
        std::fs::remove_file(&node).unwrap();
        std::fs::remove_dir(&dir).unwrap();
        let failure = ingest
            .poll_failure()
            .expect("the vanished node is reported");
        assert_eq!(failure, format!("camera {} disappeared", node.display()));
        assert_eq!(ingest.poll_failure(), Some(failure));

        let pattern = Ingest::from_head(
            vec![make("videotestsrc", "src").unwrap()],
            "NV12",
            (640, 480),
            AspectPolicy::Crop,
            writer(&contract).in_memory(FrameMemory::System),
        )
        .unwrap();
        assert_eq!(pattern.poll_failure(), None);
    }

    #[test]
    #[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
    fn gpu_stopping_an_unstarted_ingest_is_clean() {
        require_elements(&[
            "videotestsrc",
            "cudaupload",
            "cudaconvertscale",
            "videocrop",
        ]);
        let p = Ingest::from_head(
            vec![make("videotestsrc", "src").unwrap()],
            "NV12",
            (640, 480),
            AspectPolicy::Crop,
            writer(&OutputContract::default()),
        )
        .unwrap();
        p.stop().unwrap();
    }
}
