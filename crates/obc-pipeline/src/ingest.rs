//! The Ingest stage: one pipeline per source, from the camera to canonical
//! frames in the output's slot.
//!
//! ```text
//! v4l2src device=<camera> io-mode=mmap                       corrupt-frame guard on src
//!   ! capsfilter  <camera mode>
//!   [ ! capssetter <full range> ]                            range quirk row only
//!   ! queue       max-size-buffers=2 leaky=downstream
//!   [ ! videoconvert ! capsfilter video/x-raw,format=NV12 ]   CPU convert (YUY2, UYVY)
//!   [ ! jpegdec      ! capsfilter video/x-raw,format=I420 ]   CPU decode (MJPEG)
//!   ! cudaupload
//!   ! capsfilter  video/x-raw(memory:CUDAMemory),format=<uploaded format>
//!   [ ! videocrop ]                                          crop-to-fill policy only
//!   [ ! obcenhance scale=<k> ]                               enhance only
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
//! **Enhance.** With [`IngestConfig::enhance`] on and a camera mode small
//! enough for it to pay, uploaded as NV12 or I420 ([`enhance_plan`]),
//! `obcenhance` (`gst_obc::enhance`)
//! upscales the kept region by the integer factor `k` with an enhancer
//! (the one [`IngestConfig::enhance_backend`] gives, [`EnhanceBackend`]) before
//! `cudaconvertscale` fits it to the output as it fits any frame. The
//! element sits after `videocrop`, so it reads the crop region and enhances
//! only what is shown; and since the stage changes the caps between the
//! two, turning it on or off is a rebuild of the ingest (the source-switch
//! path). Without it the element is not in the pipeline at all. It fails
//! open: a failure passes the camera frames through at their own size, and
//! [`Ingest::enhance_status`] says why.
//!
//! **Colour.** The canonical caps fix the colorimetry, so `cudaconvertscale`
//! converts whatever was uploaded to the contract's label in the same pass:
//! the full-range BT.601 output of `jpegdec` (`1:4:0:0`) becomes limited
//! range for a limited contract, and a camera label that differs from the
//! contract's is converted. A camera mode that already matches the canonical
//! caps passes through without CUDA work. The CPU convert adapter keeps the
//! camera's label up to `cudaupload` ([`crate::pipeline::pinned_caps`]).
//! A raw camera with a range quirk row ([`crate::quirks`], its USB id read
//! from sysfs by the node) sends full range under a limited label: its
//! frames are relabelled full range right after the camera, so
//! `cudaconvertscale` converts them to the contract's range
//! ([`crate::colour_range`]).
//!
//! **Corrupt frames.** A probe on the camera source drops the frames the
//! driver flagged corrupted and, in MJPEG, the JPEGs that are not whole
//! (no start or end marker), before anything else sees them; the output
//! repeats the last good frame. [`Ingest::corrupt_frames`] counts them
//! ([`crate::frame_guard`]).
//!
//! **Privacy shutter.** A camera whose status row says a closed shutter
//! gives flat black frames ([`crate::quirks::StatusQuirk`]) has its luma
//! sampled a few times a second, on the camera source (raw) or after
//! `jpegdec` (MJPEG); [`Ingest::shutter`] says what it shows
//! ([`crate::shutter`]).
//!
//! **One CUDA context.** [`Ingest::start`] sets the output pipeline's CUDA
//! context on the ingest pipeline before it opens anything, so canonical
//! frames reach the output as CUDA memory of the same context, without a
//! copy.
//!
//! **System memory.** An ingest writing for an output built in system
//! memory ([`crate::output::Output::build_in`] with [`VideoMemory::System`];
//! the memory comes with the [`FrameWriter`]) produces canonical frames in
//! system memory, with no CUDA element and no CUDA context:
//!
//! ```text
//! v4l2src ! capsfilter <camera mode>
//!   [ ! capssetter <full range> ]                           range quirk, full contract
//!   ! queue
//!   [ ! videobalance ]                                      range quirk, limited contract
//!   [ ! jpegdec ! capsfilter video/x-raw,format=I420 ]      CPU decode (MJPEG)
//!   [ ! videobalance ! capssetter ]                         MJPEG, limited contract: range
//!   [ ! videocrop ]                                         crop-to-fill policy only
//!   ! videoconvertscale add-borders=<letterbox>
//!   ! capsfilter  <canonical caps, system memory>
//!   ! appsink
//! ```
//!
//! `videoconvertscale` converts the format, the size and the colour label
//! in one pass (but not the range when it is the only difference: the
//! decoder's full range is converted by the `videobalance` step,
//! [`crate::pipeline::ingest_adapter`], and so is a raw camera's with a
//! range quirk row, keeping its limited label), so YUY2 and UYVY reach it as captured, with no separate
//! `videoconvert` ([`crate::pipeline::ingest_adapter`]); a camera mode
//! already at the canonical caps passes through without a copy. The enhance
//! stage runs on CUDA frames only: an ingest in system memory refuses it
//! where it would apply ([`PipelineError::Cuda`]).
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

use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gst::prelude::*;
use gst_obc::{EffectStatus, EnhanceStats, ObcEnhance};
use obc_backend::Params;
use obc_backend::enhance::{EnhancerFactory, MAX_SCALE, MIN_SCALE};
use obc_core::{AspectPolicy, OutputContract, VideoMode};

use crate::capability::IngestPath;
use crate::colour_range::camera_usb_id;
use crate::controls::{CID_EXPOSURE_AUTO_PRIORITY, ControlGuard, CtrlSkip, get_ctrl, probe_ctrl};
use crate::frame::{Canonical, FrameWriter};
use crate::frame_guard::{CorruptFrames, FrameGuard};
use crate::geometry::{Crop, crop_for};
use crate::pipeline::{
    CameraHead, MediaPipeline, PipelineError, StopError, add_linked, both_stopped, camera_head,
    capsfilter, cuda_caps, make, names,
};
use crate::shutter::{ShutterState, ShutterWatch};
use obc_core::config::VideoMemory;

/// The GStreamer context type that carries a `GstCudaContext`.
pub const CUDA_CONTEXT_TYPE: &str = "gst.cuda.context";

/// The `GstCudaContext` object a `gst.cuda.context` context carries: two
/// pipelines share CUDA memory without copies when these are the same
/// object.
pub fn cuda_context_object(ctx: &gst::Context) -> Option<gst::Object> {
    ctx.structure().get::<gst::Object>(CUDA_CONTEXT_TYPE).ok()
}

/// The enhancer an ingest's enhance stage creates its instance with: a
/// factory the caller provides (the daemon's comes from the module that
/// provides the `enhance` stage). There is no default: an ingest without
/// one has no enhance stage.
#[derive(Clone)]
pub struct EnhanceBackend(pub Arc<dyn EnhancerFactory>);

impl EnhanceBackend {
    /// Any factory.
    pub fn new(factory: Arc<dyn EnhancerFactory>) -> Self {
        EnhanceBackend(factory)
    }

    /// The factory.
    pub fn factory(&self) -> &Arc<dyn EnhancerFactory> {
        &self.0
    }
}

impl fmt::Debug for EnhanceBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("EnhanceBackend")
            .field(&self.0.name())
            .finish()
    }
}

/// The uploaded formats the enhance stage reads (`obcenhance`'s sink caps).
pub const ENHANCE_FORMATS: [&str; 2] = ["NV12", "I420"];

/// The largest frame the enhance stage produces, in both dimensions: the
/// enhancer's output, `k` times what it reads, stays within 4K UHD.
pub const ENHANCE_MAX_OUTPUT: (u32, u32) = (3840, 2160);

/// Why the enhance stage does not apply to a camera mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnhanceRefusal {
    /// What the output shows of the frame is already at least half the
    /// output's resolution, or enhancing it would exceed
    /// [`ENHANCE_MAX_OUTPUT`] (or the frame is odd or empty).
    Resolution,
    /// The frames are uploaded in a format the stage cannot read (not one
    /// of [`ENHANCE_FORMATS`]).
    Format,
}

/// The integer factor the enhance stage upscales a `capture` frame by
/// before it is fitted into an `output` frame under `aspect`, or `None`
/// when the stage does not apply to that geometry.
///
/// The region that is enhanced is what the output shows of the frame: the
/// kept region ([`crop_for`]) with crop-to-fill, the whole frame with
/// letterbox. The stage applies only where it pays: that region has even
/// dimensions and at most half the pixels of the area it fills in the
/// output (the whole output with crop-to-fill; with letterbox, the frame
/// fitted into it, so both dimensions at most 1/sqrt(2) of the output's),
/// and `k` times the region stays within [`ENHANCE_MAX_OUTPUT`]. The factor
/// is the smallest one whose result covers the output in both dimensions
/// with crop-to-fill, in the dimension that fills it with letterbox, at
/// least [`MIN_SCALE`] and at most [`MAX_SCALE`]; a region that needs more
/// is upscaled by the maximum and the scaler does the rest.
pub fn enhance_scale(
    aspect: AspectPolicy,
    (width, height): (u32, u32),
    (out_w, out_h): (u32, u32),
) -> Option<u32> {
    if width == 0
        || height == 0
        || out_w == 0
        || out_h == 0
        || !width.is_multiple_of(2)
        || !height.is_multiple_of(2)
    {
        return None;
    }
    let crop = crop_for(aspect, width, height, out_w, out_h);
    let kept_w = width - crop.left - crop.right;
    let kept_h = height - crop.top - crop.bottom;
    let sq = |v: u32| u64::from(v) * u64::from(v);
    let pays = match aspect {
        AspectPolicy::Crop => {
            2 * u64::from(kept_w) * u64::from(kept_h) <= u64::from(out_w) * u64::from(out_h)
        }
        AspectPolicy::Letterbox => 2 * sq(width) <= sq(out_w) && 2 * sq(height) <= sq(out_h),
    };
    if !pays {
        return None;
    }
    let need = |have: u32, want: u32| want.div_ceil(have.max(1));
    let k = match aspect {
        AspectPolicy::Crop => need(kept_w, out_w).max(need(kept_h, out_h)),
        AspectPolicy::Letterbox => need(width, out_w).min(need(height, out_h)),
    }
    .clamp(MIN_SCALE, MAX_SCALE);
    let (max_w, max_h) = ENHANCE_MAX_OUTPUT;
    (u64::from(k) * u64::from(kept_w) <= u64::from(max_w)
        && u64::from(k) * u64::from(kept_h) <= u64::from(max_h))
    .then_some(k)
}

/// [`enhance_scale`] for frames uploaded in `upload_format`
/// ([`IngestPath::upload_format`]): the factor, or why the stage does not
/// apply. The geometry is judged first.
pub fn enhance_plan(
    aspect: AspectPolicy,
    upload_format: &str,
    size: (u32, u32),
    output: (u32, u32),
) -> Result<u32, EnhanceRefusal> {
    let k = enhance_scale(aspect, size, output).ok_or(EnhanceRefusal::Resolution)?;
    if !ENHANCE_FORMATS.contains(&upload_format) {
        return Err(EnhanceRefusal::Format);
    }
    Ok(k)
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
    /// Enhance a mode smaller than the output (see the module docs); no
    /// effect when [`Self::enhance_scale`] is `None`.
    pub enhance: bool,
    /// The enhancer of the enhance stage; `None`: no enhance stage, whatever
    /// [`Self::enhance`] says.
    pub enhance_backend: Option<EnhanceBackend>,
    /// The enhance stage's load-time parameters (the module's keys), which
    /// the enhancer is loaded with; empty: its defaults.
    pub enhance_params: Params,
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
            enhance: false,
            enhance_backend: None,
            enhance_params: Params::new(),
        }
    }

    /// The factor the enhance stage would upscale this mode by for
    /// `contract`, whether or not it is on, or why it does not apply
    /// ([`enhance_plan`], with the format this mode is uploaded in).
    pub fn enhance_plan(&self, contract: &OutputContract) -> Result<u32, EnhanceRefusal> {
        enhance_plan(
            self.aspect,
            self.ingest_path().upload_format(self.mode.format),
            (self.mode.width, self.mode.height),
            (contract.width, contract.height),
        )
    }

    /// [`Self::enhance_plan`]'s factor: `None` when the stage does not
    /// apply to this mode.
    pub fn enhance_scale(&self, contract: &OutputContract) -> Option<u32> {
        self.enhance_plan(contract).ok()
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
        VideoMemory::Cuda,
        upload_format,
        (width, height),
        canonical,
        aspect,
        None,
    )
}

/// One element of the canonical tail, in link order ([`tail_steps`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TailStep {
    /// `cudaupload` ([`names::UPLOAD`]).
    Upload,
    /// The CUDA capsfilter after it ([`names::CUDA_CAPS`]).
    CudaCaps,
    /// `videocrop` ([`names::CROP`]).
    Crop,
    /// `obcenhance` ([`names::ENHANCE`]).
    Enhance,
    /// `cudaconvertscale` or `videoconvertscale` ([`names::SCALE`]).
    Scale,
    /// The canonical capsfilter ([`names::CANONICAL_CAPS`]).
    Canonical,
}

/// The elements of the canonical tail for frames in `memory` under
/// `aspect`, with the enhance stage when `enhance` (CUDA frames only: in
/// system memory it is never there).
fn tail_steps(memory: VideoMemory, aspect: AspectPolicy, enhance: bool) -> Vec<TailStep> {
    let cuda = memory == VideoMemory::Cuda;
    let mut steps = Vec::with_capacity(6);
    if cuda {
        steps.extend([TailStep::Upload, TailStep::CudaCaps]);
    }
    if aspect == AspectPolicy::Crop {
        steps.push(TailStep::Crop);
    }
    if cuda && enhance {
        steps.push(TailStep::Enhance);
    }
    steps.extend([TailStep::Scale, TailStep::Canonical]);
    steps
}

/// The enhance stage of an ingest being built: its factor, enhancer and
/// load-time parameters.
#[derive(Debug, Clone)]
struct EnhancePlan {
    scale: u32,
    backend: EnhanceBackend,
    params: Params,
}

/// The `obcenhance` element of `plan`, named [`names::ENHANCE`].
fn enhance_element(plan: &EnhancePlan) -> Result<gst::Element, PipelineError> {
    gst_obc::enhance::register()
        .map_err(|e| PipelineError::Cuda(format!("cannot register obcenhance: {e}")))?;
    let e = ObcEnhance::new(Some(names::ENHANCE), plan.scale);
    e.set_factory(Some(Arc::clone(plan.backend.factory())));
    e.set_params(plan.params.clone());
    Ok(e.upcast())
}

/// [`canonical_tail`] for canonical frames in `memory`, with the enhance
/// stage when `enhance` is given. In system memory there is no upload and
/// `videoconvertscale` stands in for `cudaconvertscale`: `[videocrop !]
/// videoconvertscale ! capsfilter (canonical, system memory)`; an enhance
/// stage is refused there.
fn tail_in(
    memory: VideoMemory,
    upload_format: &str,
    (width, height): (u32, u32),
    canonical: &Canonical,
    aspect: AspectPolicy,
    enhance: Option<&EnhancePlan>,
) -> Result<(Vec<gst::Element>, Crop), PipelineError> {
    use names::*;
    if enhance.is_some() && memory != VideoMemory::Cuda {
        return Err(PipelineError::Cuda(
            "the enhance stage needs an ingest of CUDA frames".into(),
        ));
    }
    let crop = crop_for(aspect, width, height, canonical.width, canonical.height);
    let mut tail = Vec::new();
    for step in tail_steps(memory, aspect, enhance.is_some()) {
        tail.push(match step {
            TailStep::Upload => make("cudaupload", UPLOAD)?,
            TailStep::CudaCaps => capsfilter(CUDA_CAPS, &cuda_caps(upload_format))?,
            TailStep::Crop => {
                let c = make("videocrop", CROP)?;
                for (side, v) in [
                    ("left", crop.left),
                    ("right", crop.right),
                    ("top", crop.top),
                    ("bottom", crop.bottom),
                ] {
                    c.set_property(side, i32::try_from(v).unwrap_or(i32::MAX));
                }
                c
            }
            TailStep::Enhance => enhance_element(enhance.expect("planned with an enhance stage"))?,
            TailStep::Scale => {
                let scaler = match memory {
                    VideoMemory::Cuda => "cudaconvertscale",
                    VideoMemory::System => "videoconvertscale",
                };
                let scale = make(scaler, SCALE)?;
                scale.set_property("add-borders", aspect == AspectPolicy::Letterbox);
                scale
            }
            TailStep::Canonical => capsfilter(CANONICAL_CAPS, &canonical.caps_in(memory))?,
        });
    }
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
    memory: VideoMemory,
    /// The enhance stage, when the pipeline has one.
    enhance: Option<ObcEnhance>,
    /// The first failure [`Self::poll_failure`] found (sticky).
    failure: Mutex<Option<String>>,
    /// The camera's corrupt-frame guard (a camera ingest only).
    guard_frames: Option<Arc<FrameGuard>>,
    /// The camera's closed-shutter watch ([`crate::shutter`]), for a camera
    /// with a status row saying so.
    shutter: Option<Arc<ShutterWatch>>,
    /// A clone of the writer its sink writes with: the session retargets
    /// it to a prepared output ([`crate::frame`], "Fan-out").
    writer: FrameWriter,
}

impl Ingest {
    /// Builds (but does not start) a camera ingest writing into `writer`,
    /// in the writer's memory ([`FrameWriter::memory`]; see the module
    /// docs). In system memory an enhance stage is refused
    /// ([`PipelineError::Cuda`]): it runs on CUDA frames only, so a caller
    /// building a system-memory session leaves [`IngestConfig::enhance`]
    /// off.
    pub fn build(cfg: &IngestConfig, writer: FrameWriter) -> Result<Self, PipelineError> {
        let path = cfg.ingest_path();
        let usb = camera_usb_id(std::path::Path::new(crate::xu::SYSFS_ROOT), &cfg.camera);
        let CameraHead {
            chain: head,
            format: tail_format,
            guard,
            shutter,
        } = camera_head(
            &cfg.camera,
            &cfg.mode,
            &cfg.camera_io_mode,
            cfg.queue_buffers,
            writer.memory(),
            writer.canonical().range,
            usb,
        )?;
        let enhance = cfg
            .enhance
            .then(|| cfg.enhance_backend.clone())
            .flatten()
            .map(|b| (b, cfg.enhance_params.clone()));
        let mut ingest = Self::from_head_planned(
            head,
            tail_format,
            (cfg.mode.width, cfg.mode.height),
            cfg.aspect,
            enhance,
            writer,
        )?;
        ingest.camera = Some(cfg.camera.clone());
        ingest.fixed_framerate = cfg.fixed_framerate;
        ingest.path = Some(path);
        ingest.guard_frames = Some(guard);
        ingest.shutter = shutter;
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
        Self::from_head_enhanced(head, upload_format, size, aspect, None, writer)
    }

    /// [`Self::from_head`] with the enhance stage on (`enhance`: its
    /// enhancer) when the stage applies to `size` in `upload_format` for the
    /// writer's canonical frame ([`enhance_plan`]); without it when it does
    /// not, so a mode the stage cannot read never fails the ingest. The
    /// stage needs CUDA frames.
    pub fn from_head_enhanced(
        head: Vec<gst::Element>,
        upload_format: &str,
        size: (u32, u32),
        aspect: AspectPolicy,
        enhance: Option<EnhanceBackend>,
        writer: FrameWriter,
    ) -> Result<Self, PipelineError> {
        let enhance = enhance.map(|b| (b, Params::new()));
        Self::from_head_planned(head, upload_format, size, aspect, enhance, writer)
    }

    /// [`Self::from_head_enhanced`] with the enhance stage's load-time
    /// parameters next to its enhancer.
    fn from_head_planned(
        head: Vec<gst::Element>,
        upload_format: &str,
        size: (u32, u32),
        aspect: AspectPolicy,
        enhance: Option<(EnhanceBackend, Params)>,
        writer: FrameWriter,
    ) -> Result<Self, PipelineError> {
        let source_id = writer.source_id();
        let memory = writer.memory();
        let canonical = writer.canonical();
        let plan = enhance.and_then(|(backend, params)| {
            enhance_plan(
                aspect,
                upload_format,
                size,
                (canonical.width, canonical.height),
            )
            .ok()
            .map(|scale| EnhancePlan {
                scale,
                backend,
                params,
            })
        });
        let pipeline = gst::Pipeline::with_name(&format!("obc-ingest-{source_id}"));
        let (tail, crop) = tail_in(
            memory,
            upload_format,
            size,
            &canonical,
            aspect,
            plan.as_ref(),
        )?;
        let enhance = tail
            .iter()
            .find_map(|e| gst_obc::enhance::from_element(e).cloned());
        let mut chain = head;
        chain.extend(tail);
        chain.push(frames_sink(names::FRAMES_SINK, writer.clone()));
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
            enhance,
            failure: Mutex::new(None),
            guard_frames: None,
            shutter: None,
            writer,
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

    /// [`Self::start`] with the output's CUDA context if it has one
    /// ([`crate::output::Output::cuda_context`]): an ingest of
    /// system-memory frames ([`VideoMemory::System`]) starts without one, an
    /// ingest of CUDA frames refuses to.
    pub fn start_with(&mut self, cuda: Option<&gst::Context>) -> Result<(), PipelineError> {
        match cuda {
            Some(cuda) if cuda.context_type() != CUDA_CONTEXT_TYPE => {
                return Err(PipelineError::Cuda(format!(
                    "an ingest needs a {CUDA_CONTEXT_TYPE} context, not {}",
                    cuda.context_type()
                )));
            }
            None if self.memory == VideoMemory::Cuda => {
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
            guard_frames,
            ..
        } = self;
        let stopped = pipeline.shutdown_checked();
        // The last drops the rate limit held back (no more frames come).
        if let Some(g) = &guard_frames {
            g.flush_log();
        }
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

    /// The writer its sink writes with (a clone sharing its targets).
    pub(crate) fn frame_writer(&self) -> &FrameWriter {
        &self.writer
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

    /// The factor of the enhance stage, when the pipeline has one.
    pub fn enhance_scale(&self) -> Option<u32> {
        self.enhance.as_ref().map(ObcEnhance::scale)
    }

    /// The state of the enhance stage, when the pipeline has one: loading
    /// until its first frame, then ready, or failed with the reason (the
    /// camera frames then pass through without it). Read from the element,
    /// not from the bus, so [`Self::poll_failure`] does not hide it.
    pub fn enhance_status(&self) -> Option<EffectStatus> {
        self.enhance.as_ref().map(ObcEnhance::status)
    }

    /// The counters and timings of the enhance stage, when the pipeline has
    /// one.
    pub fn enhance_stats(&self) -> Option<EnhanceStats> {
        self.enhance.as_ref().map(ObcEnhance::stats)
    }

    /// The corrupt camera frames dropped at the source so far, per cause
    /// ([`crate::frame_guard`]); zero for a source that is not a camera.
    pub fn corrupt_frames(&self) -> CorruptFrames {
        self.guard_frames
            .as_ref()
            .map(|g| g.counts())
            .unwrap_or_default()
    }

    /// The rate the camera delivers good frames at, in frames per second:
    /// the frames its frame guard passes, timed from their PTS over a
    /// sliding window of about 5 s, the first 15 frames of the stream not
    /// counted ([`crate::delivered_rate`]). `None` until the window spans
    /// 3 s, once no good frame has come for a second, and for a source
    /// that is not a camera.
    pub fn delivered_fps(&self) -> Option<f64> {
        self.guard_frames.as_ref().and_then(|g| g.delivered_fps())
    }

    /// Whether the camera's privacy shutter is open or closed, as its
    /// frames say ([`crate::shutter`]); `None` for a camera without a
    /// status row saying a closed shutter shows in its frames, before its
    /// first frame was sampled, and for a source that is not a camera.
    /// Reads an atomic: cheap, any time.
    pub fn shutter(&self) -> Option<ShutterState> {
        self.shutter.as_ref().and_then(|w| w.state())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use obc_core::{ColourRange, OutputContract};

    use super::*;
    use crate::slot::Slot;
    use crate::test_support::require_elements;

    /// The Maxine upscaler, which the tests run as a module would.
    fn maxine_vsr() -> EnhanceBackend {
        EnhanceBackend::new(Arc::new(
            obc_backend_maxine::vsr::MaxineVsrFactory::from_env(),
        ))
    }

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
                writer(&contract).in_memory(VideoMemory::System),
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

    /// A camera ingest in system memory, per camera format: no upload and
    /// no CUDA element; YUY2 and UYVY go to `videoconvertscale` as
    /// captured (no separate `videoconvert`), MJPEG through `jpegdec` to
    /// I420, NV12 with no adapter. Built, not started: the node is never
    /// opened. CPU only.
    #[test]
    fn a_system_memory_camera_ingest_has_the_cpu_adapters_and_no_cuda_element() {
        require_elements(&[
            "v4l2src",
            "queue",
            "jpegdec",
            "videocrop",
            "videoconvertscale",
            "appsink",
        ]);
        let contract = OutputContract::default();
        for (mode, decode) in [
            ("NV12:1920x1080@30", false),
            ("YUY2:1280x720@30", false),
            ("UYVY:640x480@30", false),
            ("MJPEG:1920x1080@30", true),
        ] {
            let cfg = IngestConfig::new("/dev/null-camera".into(), mode.parse().unwrap());
            let ingest =
                Ingest::build(&cfg, writer(&contract).in_memory(VideoMemory::System)).unwrap();
            let p = ingest.pipeline();
            crate::test_support::assert_no_cuda_element(p.pipeline());
            assert!(p.element(names::UPLOAD).is_none(), "{mode}");
            assert!(p.element(names::INGEST_CONVERT).is_none(), "{mode}");
            assert_eq!(p.element(names::DECODE).is_some(), decode, "{mode}");
            assert_eq!(
                p.element(names::INGEST_CAPS)
                    .map(|c| c.property::<gst::Caps>("caps").to_string()),
                decode.then(|| "video/x-raw, format=(string)I420".to_string()),
                "{mode}"
            );
            let scale = p.element(names::SCALE).unwrap();
            assert_eq!(scale.factory().unwrap().name(), "videoconvertscale");
            assert_eq!(
                p.element(names::CANONICAL_CAPS)
                    .unwrap()
                    .property::<gst::Caps>("caps")
                    .to_string(),
                "video/x-raw, format=(string)NV12, width=(int)1920, height=(int)1080, \
                 pixel-aspect-ratio=(fraction)1/1, colorimetry=(string)2:4:5:1",
                "{mode}"
            );
            assert_eq!(ingest.ingest_path(), Some(IngestPath::of(cfg.mode.format)));
        }
        // The CUDA ingest keeps its CPU convert adapter.
        let (adapter, format) = crate::pipeline::ingest_adapter(
            &make("capsfilter", names::CAMERA_CAPS).unwrap(),
            obc_core::PixelFormat::Yuy2,
            VideoMemory::Cuda,
            ColourRange::Limited,
        )
        .unwrap();
        assert_eq!(format, "NV12");
        assert_eq!(adapter[0].factory().unwrap().name(), "videoconvert");
    }

    /// System-memory ingests stream canonical frames from each camera
    /// format: NV12 and YUY2 as captured, MJPEG through the decode adapter
    /// (from `jpegenc`, standing in for the camera). The frames in the slot
    /// carry the canonical caps (system memory, the contract's label), with
    /// neutral chroma, and a limited-range raw black stays black (Y=16).
    /// CPU only.
    #[test]
    fn system_memory_ingests_stream_canonical_frames_per_camera_format() {
        use gst_video::prelude::*;
        require_elements(&[
            "videotestsrc",
            "capsfilter",
            "jpegenc",
            "jpegdec",
            "videocrop",
            "videoconvertscale",
            "appsink",
        ]);
        let contract: OutputContract = "320x240@30".parse().unwrap();
        let canonical = Canonical::of(&contract);
        for format in [
            obc_core::PixelFormat::Nv12,
            obc_core::PixelFormat::Yuy2,
            obc_core::PixelFormat::Mjpeg,
        ] {
            let src = make("videotestsrc", "src").unwrap();
            src.set_property("is-live", true);
            src.set_property_from_str("pattern", "black");
            let raw = if format == obc_core::PixelFormat::Mjpeg {
                "I420"
            } else {
                format.gst_name()
            };
            let raw_caps = capsfilter(
                "raw_caps",
                &format!(
                    "video/x-raw,format={raw},width=640,height=480,framerate=30/1,\
                     colorimetry=2:4:5:1"
                ),
            )
            .unwrap();
            let mut head = vec![src, raw_caps.clone()];
            let camera_caps = if format == obc_core::PixelFormat::Mjpeg {
                head.push(make("jpegenc", "encode").unwrap());
                let c = capsfilter(names::CAMERA_CAPS, "image/jpeg").unwrap();
                head.push(c.clone());
                c
            } else {
                raw_caps
            };
            let (adapter, tail_format) = crate::pipeline::ingest_adapter(
                &camera_caps,
                format,
                VideoMemory::System,
                ColourRange::Limited,
            )
            .unwrap();
            head.extend(adapter);
            let slot = Arc::new(Slot::new());
            let w = FrameWriter::new(Arc::clone(&slot), Instant::now(), 1, canonical)
                .in_memory(VideoMemory::System);
            let mut ingest =
                Ingest::from_head(head, tail_format, (640, 480), AspectPolicy::Crop, w).unwrap();
            crate::test_support::assert_no_cuda_element(ingest.pipeline().pipeline());
            ingest.start_with(None).unwrap();
            let deadline = Instant::now() + std::time::Duration::from_secs(10);
            while slot.written() < 3 {
                assert!(Instant::now() < deadline, "{format}: no frames");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let caps = ingest
                .pipeline()
                .element(names::FRAMES_SINK)
                .unwrap()
                .static_pad("sink")
                .unwrap()
                .current_caps()
                .unwrap();
            let want: gst::Caps = format!("{},framerate=30/1", canonical.system_caps_string())
                .parse()
                .unwrap();
            assert!(caps.is_subset(&want), "{format}: {caps}");
            let info = gst_video::VideoInfo::from_caps(&caps).unwrap();
            let buffer = slot.latest().unwrap().frame.buffer.clone();
            let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(&buffer, &info).unwrap();
            let y = frame.plane_data(0).unwrap();
            let uv = frame.plane_data(1).unwrap();
            let centre = 120 * frame.plane_stride()[0] as usize + 160;
            assert_eq!(uv[0], 128, "{format}: neutral chroma");
            // The decoded MJPEG frames are labelled full range; their luma
            // is not checked here (the range conversion is a separate
            // question).
            if format != obc_core::PixelFormat::Mjpeg {
                assert_eq!(y[centre], 16, "{format}: black luma");
            }
            ingest.stop().unwrap();
        }
    }

    /// The JPEG of a `size` x `size` I420 frame of one full-range luma
    /// value `y` and one chroma pair `(u, v)`, as a camera's MJPEG carries
    /// it.
    pub(super) fn full_range_jpeg(y: u8, (u, v): (u8, u8), size: usize) -> Vec<u8> {
        let pipeline = gst::parse::launch(&format!(
            "appsrc name=in format=time caps=video/x-raw,format=I420,width={size},\
             height={size},framerate=30/1 ! jpegenc quality=100 ! appsink name=out"
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let src = pipeline
            .by_name("in")
            .unwrap()
            .downcast::<gst_app::AppSrc>()
            .unwrap();
        let sink = pipeline
            .by_name("out")
            .unwrap()
            .downcast::<gst_app::AppSink>()
            .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let mut raw = vec![y; size * size];
        raw.resize(size * size * 5 / 4, u);
        raw.resize(size * size * 3 / 2, v);
        src.push_buffer(gst::Buffer::from_slice(raw)).unwrap();
        src.end_of_stream().unwrap();
        let sample = sink.pull_sample().unwrap();
        let bytes = sample.buffer().unwrap().map_readable().unwrap().to_vec();
        pipeline.set_state(gst::State::Null).unwrap();
        bytes
    }

    /// The Y plane's centre sample and the first chroma pair (U, V) of the
    /// canonical (NV12) frames a system-memory MJPEG ingest of a frame of
    /// luma `y` and chroma `uv` produces under a contract of `range`.
    fn mjpeg_ingest_levels(y: u8, uv: (u8, u8), range: ColourRange) -> (u8, (u8, u8)) {
        use gst_video::prelude::*;
        let contract = OutputContract {
            range,
            .."320x240@30".parse().unwrap()
        };
        let canonical = Canonical::of(&contract);
        let dir = std::env::temp_dir().join(format!(
            "obc-mjpeg-{}-{y}-{}-{}-{range}",
            std::process::id(),
            uv.0,
            uv.1
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("frame.jpg");
        std::fs::write(&file, full_range_jpeg(y, uv, 64)).unwrap();

        let src = make("multifilesrc", "src").unwrap();
        src.set_property("location", file.to_string_lossy().as_ref());
        src.set_property("loop", true);
        src.set_property(
            "caps",
            "image/jpeg,framerate=30/1".parse::<gst::Caps>().unwrap(),
        );
        let pace = make("identity", "pace").unwrap();
        pace.set_property("sync", true);
        let camera_caps = capsfilter(names::CAMERA_CAPS, "image/jpeg").unwrap();
        let (adapter, tail_format) = crate::pipeline::ingest_adapter(
            &camera_caps,
            obc_core::PixelFormat::Mjpeg,
            VideoMemory::System,
            range,
        )
        .unwrap();
        let mut head = vec![src, pace, camera_caps];
        head.extend(adapter);
        let slot = Arc::new(Slot::new());
        let w = FrameWriter::new(Arc::clone(&slot), Instant::now(), 1, canonical)
            .in_memory(VideoMemory::System);
        let mut ingest =
            Ingest::from_head(head, tail_format, (64, 64), AspectPolicy::Crop, w).unwrap();
        ingest.start_with(None).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while slot.written() < 3 {
            assert!(Instant::now() < deadline, "no frames");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let caps = ingest
            .pipeline()
            .element(names::FRAMES_SINK)
            .unwrap()
            .static_pad("sink")
            .unwrap()
            .current_caps()
            .unwrap();
        let info = gst_video::VideoInfo::from_caps(&caps).unwrap();
        let buffer = slot.latest().unwrap().frame.buffer.clone();
        let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(&buffer, &info).unwrap();
        let centre = 160 * frame.plane_stride()[0] as usize + 120;
        let chroma = frame.plane_data(1).unwrap();
        let levels = (frame.plane_data(0).unwrap()[centre], (chroma[0], chroma[1]));
        ingest.stop().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        levels
    }

    /// Full-range MJPEG (`jpegdec` labels it `1:4:0:0`) in a system-memory
    /// ingest of a limited contract: `videoconvertscale` leaves the range
    /// alone, so the balance step maps black to Y=16 and white to Y=235,
    /// chroma staying neutral. CPU only.
    #[test]
    fn system_memory_mjpeg_is_converted_to_limited_range() {
        require_elements(&[
            "multifilesrc",
            "identity",
            "appsrc",
            "jpegenc",
            "jpegdec",
            "videobalance",
            "capssetter",
            "videocrop",
            "videoconvertscale",
            "appsink",
        ]);
        for (y, want) in [(0u8, 16i32), (255, 235)] {
            let (luma, (u, v)) = mjpeg_ingest_levels(y, (128, 128), ColourRange::Limited);
            assert!((i32::from(luma) - want).abs() <= 1, "Y={y} gave {luma}");
            for c in [u, v] {
                assert!((i32::from(c) - 128).abs() <= 1, "Y={y}: chroma {c}");
            }
        }
    }

    /// The balance step scales chroma about the neutral 128 by 224/255
    /// ([`crate::pipeline::ingest_adapter`]'s `saturation`): a non-neutral
    /// pair lands where the limited range puts it (U=64 and V=192 at
    /// 128 -/+ 64*224/255, about 72 and 184; U=16 and V=240, the
    /// extremes, at about 30 and 226), not where neutral or an unscaled
    /// chroma would. CPU only.
    #[test]
    fn system_memory_mjpeg_chroma_is_scaled_to_the_limited_range() {
        require_elements(&[
            "multifilesrc",
            "identity",
            "appsrc",
            "jpegenc",
            "jpegdec",
            "videobalance",
            "capssetter",
            "videocrop",
            "videoconvertscale",
            "appsink",
        ]);
        let limited = |c: u8| 128.0 + (f64::from(c) - 128.0) * 224.0 / 255.0;
        for uv in [(64u8, 192u8), (16, 240)] {
            let (luma, (u, v)) = mjpeg_ingest_levels(128, uv, ColourRange::Limited);
            // Y=128 maps to 16 + 128*219/255, about 126.
            assert!((i32::from(luma) - 126).abs() <= 1, "{uv:?}: Y {luma}");
            for (got, sent) in [(u, uv.0), (v, uv.1)] {
                let want = limited(sent);
                assert!(
                    (f64::from(got) - want).abs() <= 1.5,
                    "{uv:?}: {sent} gave {got}, want {want:.1}"
                );
            }
            // Unscaled (full range) it would pass as sent.
            let (_, full) = mjpeg_ingest_levels(128, uv, ColourRange::Full);
            for (got, sent) in [(full.0, uv.0), (full.1, uv.1)] {
                assert!(
                    (i32::from(got) - i32::from(sent)).abs() <= 1,
                    "{got} {sent}"
                );
            }
        }
    }

    /// Under a full-range contract the frames are already in range: no
    /// balance step, and the luma passes through (0 stays 0, 255 stays
    /// 255). Nor does any other adapter get the step: a raw camera, or the
    /// CUDA path (where `cudaconvertscale` converts the range).
    #[test]
    fn full_range_contract_adds_no_balance_step() {
        require_elements(&[
            "multifilesrc",
            "identity",
            "appsrc",
            "jpegenc",
            "jpegdec",
            "videobalance",
            "capssetter",
            "videocrop",
            "videoconvertscale",
            "appsink",
        ]);
        let adapter = |format, memory, range| {
            let caps = make("capsfilter", names::CAMERA_CAPS).unwrap();
            let (chain, _) = crate::pipeline::ingest_adapter(&caps, format, memory, range).unwrap();
            chain
                .iter()
                .map(|e| e.name().to_string())
                .collect::<Vec<_>>()
        };
        let mjpeg = obc_core::PixelFormat::Mjpeg;
        assert_eq!(
            adapter(mjpeg, VideoMemory::System, ColourRange::Limited),
            [
                names::DECODE,
                names::INGEST_CAPS,
                names::RANGE_BALANCE,
                names::RANGE_RELABEL
            ]
        );
        assert_eq!(
            adapter(mjpeg, VideoMemory::System, ColourRange::Full),
            [names::DECODE, names::INGEST_CAPS]
        );
        assert_eq!(
            adapter(mjpeg, VideoMemory::Cuda, ColourRange::Limited),
            [names::DECODE, names::INGEST_CAPS]
        );
        for format in [obc_core::PixelFormat::Nv12, obc_core::PixelFormat::Yuy2] {
            assert!(adapter(format, VideoMemory::System, ColourRange::Limited).is_empty());
        }
        for (y, want) in [(0u8, 0u8), (255, 255)] {
            let (luma, (u, v)) = mjpeg_ingest_levels(y, (128, 128), ColourRange::Full);
            assert_eq!(luma, want, "Y={y}");
            for c in [u, v] {
                assert!((i32::from(c) - 128).abs() <= 1, "chroma {c}");
            }
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
        let ingest = Ingest::build(&cfg, writer(&contract).in_memory(VideoMemory::System)).unwrap();
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
            writer(&contract).in_memory(VideoMemory::System),
        )
        .unwrap();
        assert_eq!(pattern.poll_failure(), None);
    }

    /// The factor covers what the output shows: exactly for the common
    /// 16:9 laptop modes, the next integer otherwise, at most 4. Nothing
    /// where it does not pay (what is shown has more than half the pixels
    /// of the area it fills), where the result would exceed 4K UHD, or for
    /// an odd mode.
    #[test]
    fn the_enhance_factor_is_the_smallest_that_covers_the_output() {
        let out = (1920, 1080);
        let fill = |w, h| enhance_scale(AspectPolicy::Crop, (w, h), out);
        let fit = |w, h| enhance_scale(AspectPolicy::Letterbox, (w, h), out);
        for (w, h, k) in [
            (960, 540, Some(2)),
            (640, 360, Some(3)),
            (480, 270, Some(4)),
            (1280, 720, Some(2)),
            (320, 180, Some(4)),
            // More than half the output's pixels: not worth the passes.
            (1600, 900, None),
            (1918, 1078, None),
            (1920, 1080, None),
        ] {
            assert_eq!(fill(w, h), k, "{w}x{h} crop");
            assert_eq!(fit(w, h), k, "{w}x{h} letterbox");
        }
        // 4:3 with crop-to-fill: the kept 640x360 needs 3; letterboxed, the
        // height fills the output at 2.25, so 3 as well.
        assert_eq!(fill(640, 480), Some(3));
        assert_eq!(fit(640, 480), Some(3));
        // 800x600: crop keeps 800x450 (3); letterbox fills 1080 rows at 1.8.
        assert_eq!(fill(800, 600), Some(3));
        assert_eq!(fit(800, 600), Some(2));
        // A wide mode: crop keeps 1422x800, more than half the output.
        assert_eq!(fill(1920, 800), None);
        // A tall one: crop keeps 1280x720, which pays; letterboxed, its
        // 1080 rows already fill the output.
        assert_eq!(fill(1280, 1080), Some(2));
        assert_eq!(fit(1280, 1080), None);
        // Odd sizes and degenerate ones.
        assert_eq!(fill(641, 360), None);
        assert_eq!(fill(640, 359), None);
        assert_eq!(fill(0, 0), None);
        assert_eq!(enhance_scale(AspectPolicy::Crop, (640, 360), (0, 0)), None);
        // Odd-shaped but even: 1000x562 needs 2 in both policies.
        assert_eq!(fill(1000, 562), Some(2));
        assert_eq!(fit(1000, 562), Some(2));
        // A tiny mode is capped.
        assert_eq!(fill(160, 90), Some(4));
    }

    /// For a 4K UHD output the factor is bounded by the enhancer's largest
    /// output: 1600x900 would need 3 (4800 columns), a letterboxed 1600x1200
    /// 2 (2400 rows).
    #[test]
    fn the_enhanced_frame_stays_within_4k() {
        let out = (3840, 2160);
        let fill = |w, h| enhance_scale(AspectPolicy::Crop, (w, h), out);
        let fit = |w, h| enhance_scale(AspectPolicy::Letterbox, (w, h), out);
        assert_eq!(fill(1920, 1080), Some(2));
        assert_eq!(fill(1280, 720), Some(3));
        assert_eq!(fill(960, 540), Some(4));
        assert_eq!(fill(640, 360), Some(4), "capped; the scaler does the rest");
        assert_eq!(fill(1600, 900), None);
        assert_eq!(fit(1600, 900), None);
        assert_eq!(fit(1600, 1200), None);
        assert_eq!(fit(1280, 960), None, "3 for 2160 rows: 3840x2880");
        assert_eq!(fit(960, 720), Some(3), "2880x2160");
    }

    /// The stage reads NV12 and I420 only: any other uploaded format is
    /// refused with its own reason, so the ingest is built without it. The
    /// geometry is judged first.
    #[test]
    fn the_enhance_plan_refuses_formats_the_stage_cannot_read() {
        let out = (1920, 1080);
        let plan = |f, size| enhance_plan(AspectPolicy::Crop, f, size, out);
        assert_eq!(plan("NV12", (960, 540)), Ok(2));
        assert_eq!(plan("I420", (640, 360)), Ok(3));
        for f in ["NV21", "YV12", "RGB", "BGR", "YUY2"] {
            assert_eq!(plan(f, (960, 540)), Err(EnhanceRefusal::Format), "{f}");
            assert_eq!(
                plan(f, (1920, 1080)),
                Err(EnhanceRefusal::Resolution),
                "{f}"
            );
        }
        assert_eq!(plan("NV12", (1600, 900)), Err(EnhanceRefusal::Resolution));
    }

    #[test]
    fn the_config_says_whether_enhance_applies_to_its_mode() {
        let contract = OutputContract::default();
        let mut cfg = IngestConfig::new(
            "/dev/null-camera".into(),
            "YUY2:960x540@30".parse().unwrap(),
        );
        assert!(!cfg.enhance, "off by default");
        assert!(cfg.enhance_backend.is_none());
        assert_eq!(cfg.enhance_scale(&contract), Some(2));
        cfg.mode = "NV12:1920x1080@30".parse().unwrap();
        assert_eq!(cfg.enhance_scale(&contract), None);
        cfg.mode = "MJPEG:640x360@30".parse().unwrap();
        assert_eq!(cfg.enhance_scale(&contract), Some(3), "decoded to I420");
        // Uploaded as they come, in a format the stage cannot read.
        for mode in ["NV21:960x540@30", "YV12:960x540@30", "BGR3:960x540@30"] {
            cfg.mode = mode.parse().unwrap();
            assert_eq!(cfg.ingest_path(), IngestPath::GpuDirect, "{mode}");
            assert_eq!(
                cfg.enhance_plan(&contract),
                Err(EnhanceRefusal::Format),
                "{mode}"
            );
            assert_eq!(cfg.enhance_scale(&contract), None, "{mode}");
        }
        cfg.mode = "NV12:960x540@30".parse().unwrap();
        assert_eq!(cfg.enhance_plan(&contract), Ok(2));
    }

    /// Without enhance the tail is as before; with it, `obcenhance` comes
    /// after the crop and right before the scaler, in CUDA memory only.
    #[test]
    fn the_enhance_stage_sits_before_the_scaler_only_when_on() {
        use TailStep::*;
        let cuda = VideoMemory::Cuda;
        assert_eq!(
            tail_steps(cuda, AspectPolicy::Crop, false),
            vec![Upload, CudaCaps, Crop, Scale, Canonical]
        );
        assert_eq!(
            tail_steps(cuda, AspectPolicy::Crop, true),
            vec![Upload, CudaCaps, Crop, Enhance, Scale, Canonical]
        );
        assert_eq!(
            tail_steps(cuda, AspectPolicy::Letterbox, true),
            vec![Upload, CudaCaps, Enhance, Scale, Canonical]
        );
        assert_eq!(
            tail_steps(VideoMemory::System, AspectPolicy::Letterbox, true),
            vec![Scale, Canonical]
        );
    }

    /// The enhance element carries the factor and the stage's load-time
    /// parameters of its plan. CPU only: the element is made, not run.
    #[test]
    fn the_enhance_element_carries_the_stage_parameters() {
        gst::init().unwrap();
        let params: Params = [("method".to_string(), obc_backend::Value::from("fast"))].into();
        let plan = EnhancePlan {
            scale: 3,
            backend: maxine_vsr(),
            params: params.clone(),
        };
        let e = enhance_element(&plan).unwrap();
        assert_eq!(e.name(), names::ENHANCE);
        let e = gst_obc::enhance::from_element(&e).unwrap();
        assert_eq!(e.scale(), 3);
        assert_eq!(e.params(), params);
        let plain = enhance_element(&EnhancePlan {
            params: Params::new(),
            ..plan
        })
        .unwrap();
        assert!(
            gst_obc::enhance::from_element(&plain)
                .unwrap()
                .params()
                .is_empty()
        );
        let cfg = IngestConfig::new(
            "/dev/null-camera".into(),
            "YUY2:960x540@30".parse().unwrap(),
        );
        assert!(cfg.enhance_params.is_empty(), "the defaults unless given");
    }

    /// A system-memory ingest refuses an applicable enhance stage, and
    /// builds without the element when the stage does not apply. CPU only.
    #[test]
    fn a_system_memory_ingest_has_no_enhance_stage() {
        require_elements(&["videotestsrc", "videocrop", "videoconvertscale", "appsink"]);
        let contract = OutputContract::default();
        let build = |size| {
            Ingest::from_head_enhanced(
                vec![make("videotestsrc", "src").unwrap()],
                "NV12",
                size,
                AspectPolicy::Crop,
                Some(maxine_vsr()),
                writer(&contract).in_memory(VideoMemory::System),
            )
        };
        assert!(matches!(build((960, 540)), Err(PipelineError::Cuda(_))));
        let p = build((1920, 1080)).unwrap();
        assert!(p.pipeline().element(names::ENHANCE).is_none());
        assert_eq!(p.enhance_scale(), None);
        assert_eq!(p.enhance_status(), None);
        assert_eq!(
            format!("{:?}", maxine_vsr()),
            "EnhanceBackend(\"maxine-vsr\")"
        );
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

    /// The enhance stage on the GPU, fed by a test pattern (no camera).
    mod enhance_gpu {
        use std::time::Duration;

        use gst_video::prelude::*;
        use obc_backend::BackendError;
        use obc_backend::enhance::Enhancer;
        use obc_backend_maxine::vsr::{EnhanceFault, EnhanceFaultSwitch};

        use super::*;
        use crate::frame::Frame;

        /// An enhancer that cannot be created.
        struct Unavailable;

        impl EnhancerFactory for Unavailable {
            fn name(&self) -> &'static str {
                "unavailable"
            }
            fn probe(&self) -> Result<(), BackendError> {
                Err(BackendError::Unavailable("forced: no enhancer here".into()))
            }
            fn create(&self) -> Result<Box<dyn Enhancer>, BackendError> {
                Err(BackendError::Unavailable("forced: no enhancer here".into()))
            }
        }

        fn maxine(faults: &EnhanceFaultSwitch) -> EnhanceBackend {
            EnhanceBackend::new(Arc::new(
                obc_backend_maxine::vsr::MaxineVsrFactory::from_env().with_faults(faults.clone()),
            ))
        }

        /// The colorimetry of a pattern: BT.601 limited, as most cameras.
        const BT601: &str = "colorimetry=2:4:5:1";

        /// A live 30 fps `pinwheel` of `size` in `format` (static, with
        /// edges at every angle), in two saturated colours far apart in both
        /// U and V (a reddish orange on a blue), so a U/V swap, a misplaced
        /// chroma plane or a wrong matrix shows in the chroma; `extra` adds
        /// caps fields (colorimetry, chroma siting).
        fn pattern(format: &str, (w, h): (u32, u32), extra: &str) -> Vec<gst::Element> {
            let src = gst::ElementFactory::make("videotestsrc")
                .property("is-live", true)
                .property_from_str("pattern", "pinwheel")
                .property("foreground-color", 0xffe0_4010u32)
                .property("background-color", 0xff20_40c0u32)
                .build()
                .unwrap();
            let caps = gst::ElementFactory::make("capsfilter")
                .property(
                    "caps",
                    format!(
                        "video/x-raw,format={format},width={w},height={h},framerate=30/1,{extra}"
                    )
                    .parse::<gst::Caps>()
                    .unwrap(),
                )
                .build()
                .unwrap();
            vec![src, caps]
        }

        struct Run {
            ingest: Ingest,
            slot: Arc<Slot<Frame>>,
        }

        fn build(
            format: &str,
            size: (u32, u32),
            aspect: AspectPolicy,
            enhance: Option<EnhanceBackend>,
        ) -> Run {
            build_with(format, size, BT601, aspect, enhance)
        }

        fn build_with(
            format: &str,
            size: (u32, u32),
            extra: &str,
            aspect: AspectPolicy,
            enhance: Option<EnhanceBackend>,
        ) -> Run {
            require_elements(&[
                "videotestsrc",
                "cudaupload",
                "cudaconvertscale",
                "videocrop",
            ]);
            let slot = Arc::new(Slot::new());
            let writer = FrameWriter::new(
                Arc::clone(&slot),
                Instant::now(),
                1,
                Canonical::of(&OutputContract::default()),
            );
            let ingest = Ingest::from_head_enhanced(
                pattern(format, size, extra),
                format,
                size,
                aspect,
                enhance,
                writer,
            )
            .unwrap();
            Run { ingest, slot }
        }

        /// Plays the ingest until `n` frames reached the slot; returns the
        /// last one.
        fn frames(run: &Run, n: u64) -> gst::Buffer {
            let start = run.slot.written();
            run.ingest.pipeline().play().unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            while run.slot.written() < start + n {
                if let Some(f) = run.ingest.poll_failure() {
                    panic!("the ingest failed: {f}");
                }
                assert!(Instant::now() < deadline, "timed out waiting for frames");
                std::thread::sleep(Duration::from_millis(10));
            }
            run.slot.latest().unwrap().frame.buffer
        }

        /// Maps a canonical frame (NV12 at the output contract) through a
        /// host map of its CUDA memory.
        fn map(buffer: &gst::Buffer) -> gst_video::VideoFrameRef<&gst::BufferRef> {
            let caps: gst::Caps = Canonical::of(&OutputContract::default())
                .caps_string()
                .parse()
                .unwrap();
            let info = gst_video::VideoInfo::from_caps(&caps).unwrap();
            gst_video::VideoFrameRef::from_buffer_ref_readable(buffer.as_ref(), &info).unwrap()
        }

        /// What a canonical frame looks like, to compare two.
        #[derive(Debug, Clone, Copy)]
        struct Look {
            /// Mean luma.
            y: f64,
            /// Mean absolute horizontal luma step (sharpness).
            step: f64,
            /// Mean U and mean V.
            u: f64,
            v: f64,
        }

        fn look(buffer: &gst::Buffer) -> Look {
            let f = map(buffer);
            let (w, h) = (f.width() as usize, f.height() as usize);
            let (ys, cs) = (f.plane_stride()[0] as usize, f.plane_stride()[1] as usize);
            let (y, c) = (f.plane_data(0).unwrap(), f.plane_data(1).unwrap());
            let (mut sum, mut step, mut u, mut v) = (0u64, 0u64, 0u64, 0u64);
            for r in 0..h {
                let row = &y[r * ys..r * ys + w];
                sum += row.iter().map(|&p| u64::from(p)).sum::<u64>();
                step += row
                    .windows(2)
                    .map(|p| u64::from(p[0].abs_diff(p[1])))
                    .sum::<u64>();
            }
            for r in 0..h / 2 {
                for pair in c[r * cs..r * cs + w].as_chunks::<2>().0 {
                    u += u64::from(pair[0]);
                    v += u64::from(pair[1]);
                }
            }
            let chroma = (w / 2 * h / 2) as f64;
            Look {
                y: sum as f64 / (w * h) as f64,
                step: step as f64 / ((w - 1) * h) as f64,
                u: u as f64 / chroma,
                v: v as f64 / chroma,
            }
        }

        /// Mean absolute differences of two canonical frames, sample by
        /// sample: luma, U, V.
        fn diff(a: &gst::Buffer, b: &gst::Buffer) -> (f64, f64, f64) {
            let (fa, fb) = (map(a), map(b));
            let (w, h) = (fa.width() as usize, fa.height() as usize);
            let plane = |p: usize, rows: usize, step: usize, first: usize| {
                let (sa, sb) = (fa.plane_stride()[p] as usize, fb.plane_stride()[p] as usize);
                let (da, db) = (
                    fa.plane_data(p as u32).unwrap(),
                    fb.plane_data(p as u32).unwrap(),
                );
                let mut d = 0u64;
                let mut n = 0u64;
                for r in 0..rows {
                    for c in (first..w).step_by(step) {
                        d += u64::from(da[r * sa + c].abs_diff(db[r * sb + c]));
                        n += 1;
                    }
                }
                d as f64 / n as f64
            };
            (
                plane(0, h, 1, 0),
                plane(1, h / 2, 2, 0),
                plane(1, h / 2, 2, 1),
            )
        }

        /// Asserts `frame` shows the same image as `reference`, in geometry,
        /// luma and colour, within `luma` and `chroma` mean absolute
        /// differences per sample.
        fn assert_same_image(
            what: &str,
            frame: &gst::Buffer,
            reference: &gst::Buffer,
            luma: f64,
            chroma: f64,
        ) {
            let (got, want) = (look(frame), look(reference));
            let (dy, du, dv) = diff(frame, reference);
            eprintln!("{what}: {got:?} (plain {want:?}); diff Y {dy:.2} U {du:.2} V {dv:.2}");
            // Not black or grey: the pattern's colours.
            assert!(
                want.y > 40.0 && (want.v - want.u).abs() > 10.0,
                "{what}: {want:?}"
            );
            assert!(
                (got.y - want.y).abs() < 4.0,
                "{what}: luma {got:?} vs {want:?}"
            );
            assert!(
                (got.u - want.u).abs() < 1.5,
                "{what}: U {got:?} vs {want:?}"
            );
            assert!(
                (got.v - want.v).abs() < 1.5,
                "{what}: V {got:?} vs {want:?}"
            );
            assert!(dy < luma, "{what}: same geometry, luma diff {dy}");
            assert!(
                du < chroma && dv < chroma,
                "{what}: same colour, U {du} V {dv}"
            );
        }

        /// Whether this process is a compute process on the first GPU (it
        /// holds a CUDA context there).
        fn holds_a_cuda_context() -> bool {
            let nvml = nvml_wrapper::Nvml::init().expect("NVML");
            let device = nvml.device_by_index(0).expect("GPU 0");
            let pid = std::process::id();
            device
                .running_compute_processes()
                .unwrap_or_default()
                .iter()
                .any(|p| p.pid == pid)
        }

        fn peer_name(e: &gst::Element, pad: &str) -> String {
            e.static_pad(pad)
                .and_then(|p| p.peer())
                .and_then(|p| p.parent_element())
                .map(|e| e.name().to_string())
                .unwrap_or_default()
        }

        /// The frame the same pattern gives without the stage.
        fn reference(
            format: &str,
            size: (u32, u32),
            extra: &str,
            aspect: AspectPolicy,
        ) -> gst::Buffer {
            let plain = build_with(format, size, extra, aspect, None);
            let frame = frames(&plain, 5);
            plain.ingest.stop().unwrap();
            frame
        }

        /// The negotiated `(width, height)` on `element`'s `pad`.
        fn size_on(run: &Run, element: &str, pad: &str) -> (i32, i32) {
            let caps = run
                .ingest
                .pipeline()
                .element(element)
                .and_then(|e| e.static_pad(pad))
                .and_then(|p| p.current_caps())
                .unwrap_or_else(|| panic!("no caps on {element}:{pad}"));
            let s = caps.structure(0).unwrap();
            (s.get("width").unwrap(), s.get("height").unwrap())
        }

        /// The stage of a failed run passes the crop region through at its
        /// own size, and the scaler still makes the canonical frame of it.
        fn assert_passthrough_geometry(run: &Run, region: (i32, i32)) {
            assert_eq!(size_on(run, names::ENHANCE, "sink"), region);
            assert_eq!(size_on(run, names::ENHANCE, "src"), region, "not upscaled");
            let canonical = run
                .ingest
                .pipeline()
                .element(names::CANONICAL_CAPS)
                .and_then(|e| e.static_pad("src"))
                .and_then(|p| p.current_caps())
                .unwrap();
            let want: gst::Caps = Canonical::of(&OutputContract::default())
                .caps_string()
                .parse()
                .unwrap();
            assert!(canonical.is_subset(&want), "{canonical}");
        }

        /// An enhancer that cannot be created fails the stage open: one frame
        /// is dropped, then the camera frames pass through and are scaled to
        /// the output as without the stage, with the crop region in force;
        /// the reason is in the status.
        #[test]
        #[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
        fn gpu_enhance_that_cannot_load_passes_the_camera_frames_through() {
            let want = reference("NV12", (640, 480), BT601, AspectPolicy::Crop);
            let run = build(
                "NV12",
                (640, 480),
                AspectPolicy::Crop,
                Some(EnhanceBackend::new(Arc::new(Unavailable))),
            );
            let p = run.ingest.pipeline();
            let enhance = p.element(names::ENHANCE).expect("the enhance stage");
            assert_eq!(peer_name(&enhance, "sink"), names::CROP);
            assert_eq!(peer_name(&enhance, "src"), names::SCALE);
            assert_eq!(run.ingest.enhance_scale(), Some(3));
            let last = frames(&run, 10);
            let status = run.ingest.enhance_status().unwrap();
            assert!(
                status
                    .reason()
                    .is_some_and(|r| r.contains("forced: no enhancer here")),
                "{status}"
            );
            let stats = run.ingest.enhance_stats().unwrap();
            assert_eq!((stats.enhanced, stats.dropped), (0, 1), "{stats:?}");
            assert!(stats.passthrough >= 10, "{stats:?}");
            assert!(enhance.property::<String>("status").starts_with("failed: "));
            assert_passthrough_geometry(&run, (640, 360));
            // The very frame the ingest makes without the stage.
            assert_same_image("cannot load, 640x480 cropped", &last, &want, 1.0, 1.0);
            run.ingest.stop().unwrap();
        }

        /// A mode uploaded in a format the stage cannot read is built
        /// without it, and streams: the setting never fails an ingest.
        #[test]
        #[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
        fn gpu_enhance_is_left_out_for_formats_it_cannot_read() {
            for format in ["NV21", "YV12"] {
                let run = build(
                    format,
                    (960, 540),
                    AspectPolicy::Crop,
                    Some(EnhanceBackend::new(Arc::new(Unavailable))),
                );
                assert!(
                    run.ingest.pipeline().element(names::ENHANCE).is_none(),
                    "{format}"
                );
                assert_eq!(run.ingest.enhance_scale(), None);
                assert_eq!(run.ingest.enhance_status(), None);
                let last = frames(&run, 5);
                assert!(look(&last).y > 40.0, "{format}");
                run.ingest.stop().unwrap();
            }
        }

        /// The real enhancer: 960x540 (x2) and 640x360 (x3) in NV12, 4:3
        /// I420 modes cropped to fill (x3, through the crop region; one as
        /// `jpegdec` makes them, full range and centre-sited), BT.709, and
        /// letterbox, all at the output contract, the same image in luma and
        /// chroma as the frame scaled without the stage, and at least as
        /// sharp. Prints the GPU time per frame. After the stop the process
        /// holds no CUDA context.
        #[test]
        #[ignore = "needs an NVIDIA GPU, the GStreamer CUDA elements and the NVIDIA Maxine VFX SDK with the os-release override"]
        fn maxine_enhance_upscales_small_modes_to_the_output() {
            let faults = EnhanceFaultSwitch::default();
            let jpeg = "colorimetry=1:4:0:0,chroma-site=jpeg";
            let bt709 = "colorimetry=bt709";
            for (format, size, extra, aspect, k) in [
                ("NV12", (960, 540), BT601, AspectPolicy::Crop, 2),
                ("NV12", (640, 360), BT601, AspectPolicy::Crop, 3),
                ("I420", (640, 480), BT601, AspectPolicy::Crop, 3),
                ("I420", (640, 480), jpeg, AspectPolicy::Crop, 3),
                ("NV12", (960, 540), bt709, AspectPolicy::Crop, 2),
                ("NV12", (640, 480), BT601, AspectPolicy::Letterbox, 3),
            ] {
                let what = format!("{format} {}x{} {extra} {aspect:?}", size.0, size.1);
                let want = reference(format, size, extra, aspect);
                let run = build_with(format, size, extra, aspect, Some(maxine(&faults)));
                assert_eq!(run.ingest.enhance_scale(), Some(k), "{what}");
                let last = frames(&run, 90);
                let status = run.ingest.enhance_status().unwrap();
                assert_eq!(status, EffectStatus::Ready, "{what}: {status}");
                let stats = run.ingest.enhance_stats().unwrap();
                assert_eq!(
                    (stats.dropped, stats.passthrough),
                    (0, 0),
                    "{what}: {stats:?}"
                );
                assert!(stats.enhanced >= 90, "{what}: {stats:?}");
                eprintln!(
                    "{what} x{k}: load {:.0} ms, GPU p50 {:.3} ms p95 {:.3} ms",
                    stats.load_ms.unwrap_or(0.0),
                    stats.gpu_ms_p50.unwrap_or(0.0),
                    stats.gpu_ms_p95.unwrap_or(0.0),
                );
                assert_same_image(&what, &last, &want, 8.0, 4.0);
                assert!(
                    look(&last).step >= look(&want).step * 0.95,
                    "{what}: not blurrier"
                );
                run.ingest.stop().unwrap();
            }
            std::thread::sleep(Duration::from_millis(200));
            assert!(
                !holds_a_cuda_context(),
                "a CUDA context outlived the ingests"
            );
        }

        /// Forced failures of the real enhancer: a load that fails and a frame
        /// that fails mid-stream both fail the stage open, and the frames go
        /// on at the output contract with the camera image; the mid-stream
        /// failure with a crop region in force (640x480 cropped to 640x360)
        /// keeps the crop and the canonical caps.
        #[test]
        #[ignore = "needs an NVIDIA GPU, the GStreamer CUDA elements and the NVIDIA Maxine VFX SDK with the os-release override"]
        fn maxine_enhance_failures_fall_back_to_the_camera_frames() {
            let faults = EnhanceFaultSwitch::default();
            faults.set(Some(EnhanceFault::Load));
            let want = reference("NV12", (960, 540), BT601, AspectPolicy::Crop);
            let run = build(
                "NV12",
                (960, 540),
                AspectPolicy::Crop,
                Some(maxine(&faults)),
            );
            let last = frames(&run, 10);
            let status = run.ingest.enhance_status().unwrap();
            assert!(
                status
                    .reason()
                    .is_some_and(|r| r.contains("could not load")),
                "{status}"
            );
            assert_passthrough_geometry(&run, (960, 540));
            assert_same_image("load fault", &last, &want, 1.0, 1.0);
            run.ingest.stop().unwrap();

            faults.set(None);
            let want = reference("NV12", (640, 480), BT601, AspectPolicy::Crop);
            let run = build(
                "NV12",
                (640, 480),
                AspectPolicy::Crop,
                Some(maxine(&faults)),
            );
            frames(&run, 10);
            assert_eq!(run.ingest.enhance_status(), Some(EffectStatus::Ready));
            assert_eq!(size_on(&run, names::ENHANCE, "src"), (1920, 1080));
            faults.set(Some(EnhanceFault::Run));
            let last = frames(&run, 10);
            let status = run.ingest.enhance_status().unwrap();
            assert!(
                status.reason().is_some_and(|r| r.contains("forced fault")),
                "{status}"
            );
            let stats = run.ingest.enhance_stats().unwrap();
            assert_eq!(stats.dropped, 1, "{stats:?}");
            assert!(stats.enhanced >= 10 && stats.passthrough >= 9, "{stats:?}");
            assert_passthrough_geometry(&run, (640, 360));
            assert_same_image("run fault, 640x480 cropped", &last, &want, 1.0, 1.0);
            run.ingest.stop().unwrap();
            faults.set(None);
        }
    }
}

/// The camera head's corrupt-frame guard and range quirk, on pipelines fed
/// by an `appsrc` named as the camera. CPU only, except the one `gpu_`
/// test.
#[cfg(test)]
mod camera_head_tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use gst_video::prelude::*;
    use obc_core::{ColourRange, OutputContract, PixelFormat, UsbId};

    use super::*;
    use crate::colour_range::full_range_caps;
    use crate::live::LiveStats;
    use crate::pipeline::{camera_chain, same_colorimetry};
    use crate::slot::Slot;
    use crate::test_support::require_elements;

    const VX800: UsbId = UsbId {
        vendor: 0x045e,
        product: 0x0766,
    };
    const C930E: UsbId = UsbId {
        vendor: 0x046d,
        product: 0x0843,
    };

    /// A live `appsrc` named as the camera, with `caps`.
    fn camera_src(caps: &str) -> gst_app::AppSrc {
        gst_app::AppSrc::builder()
            .name(names::CAMERA)
            .caps(&caps.parse::<gst::Caps>().unwrap())
            .format(gst::Format::Time)
            .is_live(true)
            .build()
    }

    fn system_writer(
        slot: &Arc<Slot<crate::frame::Frame>>,
        contract: &OutputContract,
    ) -> FrameWriter {
        FrameWriter::new(Arc::clone(slot), Instant::now(), 1, Canonical::of(contract))
            .in_memory(VideoMemory::System)
    }

    fn names_of(chain: &[gst::Element]) -> Vec<String> {
        chain.iter().map(|e| e.name().to_string()).collect()
    }

    /// The elements the camera head builds, per range fix: a quirk camera
    /// gets the relabel right after its capsfilter in CUDA memory (on the
    /// CPU convert path) and under a full
    /// contract, the levels after the queue in system memory under a
    /// limited one; a camera with no row, or MJPEG, gets neither. Built,
    /// not started (no CUDA element is in the head).
    #[test]
    fn the_head_applies_the_range_fix_of_the_quirk_row() {
        require_elements(&[
            "v4l2src",
            "queue",
            "capssetter",
            "videobalance",
            "videoconvert",
            "jpegdec",
        ]);
        use ColourRange::{Full, Limited};
        use names::*;
        let head = |format: PixelFormat, memory, range, usb| {
            let src = make("v4l2src", CAMERA).unwrap();
            let caps = format!(
                "{},width=640,height=480",
                if format == PixelFormat::Mjpeg {
                    "image/jpeg".to_string()
                } else {
                    format!("video/x-raw,format={}", format.gst_name())
                }
            );
            let h = camera_chain(src, "test", &caps, format, 2, memory, range, usb).unwrap();
            names_of(&h.chain)
        };
        let yuy2 = PixelFormat::Yuy2;
        assert_eq!(
            head(yuy2, VideoMemory::Cuda, Limited, Some(VX800)),
            [
                CAMERA,
                CAMERA_CAPS,
                RANGE_QUIRK_RELABEL,
                QUEUE,
                INGEST_CONVERT,
                INGEST_CAPS
            ]
        );
        // A second row (Generalplus, YUY2) gets the same relabel.
        assert_eq!(
            head(
                yuy2,
                VideoMemory::Cuda,
                Limited,
                Some(UsbId {
                    vendor: 0x1b3f,
                    product: 0x2002
                })
            ),
            [
                CAMERA,
                CAMERA_CAPS,
                RANGE_QUIRK_RELABEL,
                QUEUE,
                INGEST_CONVERT,
                INGEST_CAPS
            ]
        );
        assert_eq!(
            head(yuy2, VideoMemory::System, Limited, Some(VX800)),
            [CAMERA, CAMERA_CAPS, QUEUE, RANGE_BALANCE, RANGE_RELABEL]
        );
        assert_eq!(
            head(yuy2, VideoMemory::System, Full, Some(VX800)),
            [CAMERA, CAMERA_CAPS, RANGE_QUIRK_RELABEL, QUEUE]
        );
        for usb in [Some(C930E), None] {
            assert_eq!(
                head(yuy2, VideoMemory::System, Limited, usb),
                [CAMERA, CAMERA_CAPS, QUEUE]
            );
        }
        // The VX-800's row lists YUY2 only.
        assert_eq!(
            head(
                PixelFormat::Mjpeg,
                VideoMemory::System,
                Limited,
                Some(VX800)
            ),
            [
                CAMERA,
                CAMERA_CAPS,
                QUEUE,
                DECODE,
                INGEST_CAPS,
                RANGE_BALANCE,
                RANGE_RELABEL
            ]
        );
    }

    /// One 320x240 YUY2 frame, full range: luma 0 in the left half, 255
    /// in the right, neutral chroma.
    fn black_and_white_yuy2(n: u64) -> gst::Buffer {
        let (w, h) = (320usize, 240usize);
        let mut data = Vec::with_capacity(w * h * 2);
        for _ in 0..h {
            for x in (0..w).step_by(2) {
                let y = if x < w / 2 { 0 } else { 255 };
                data.extend([y, 128, y, 128]);
            }
        }
        let mut b = gst::Buffer::from_slice(data);
        let r = b.get_mut().unwrap();
        r.set_pts(gst::ClockTime::from_mseconds(33 * n));
        r.set_duration(gst::ClockTime::from_mseconds(33));
        b
    }

    const YUY2_LIMITED: &str =
        "video/x-raw,format=YUY2,width=320,height=240,framerate=30/1,colorimetry=2:4:5:4";

    /// Pushes full-range YUY2 frames labelled limited through a
    /// system-memory camera ingest (`usb` selecting the quirk row or not)
    /// under `range`: the luma of the canonical frame's black and white
    /// halves, and its first chroma pair.
    fn system_levels(usb: Option<UsbId>, range: ColourRange) -> (u8, u8, (u8, u8)) {
        let contract = OutputContract {
            range,
            .."320x240@30".parse().unwrap()
        };
        let src = camera_src(YUY2_LIMITED);
        let head = camera_chain(
            src.clone().upcast(),
            "test",
            YUY2_LIMITED,
            PixelFormat::Yuy2,
            2,
            VideoMemory::System,
            range,
            usb,
        )
        .unwrap();
        let slot = Arc::new(Slot::new());
        let mut ingest = Ingest::from_head(
            head.chain,
            head.format,
            (320, 240),
            AspectPolicy::Crop,
            system_writer(&slot, &contract),
        )
        .unwrap();
        ingest.start_with(None).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut n = 0;
        while slot.written() < 3 {
            assert!(Instant::now() < deadline, "no frames");
            src.push_buffer(black_and_white_yuy2(n)).unwrap();
            n += 1;
            std::thread::sleep(Duration::from_millis(10));
        }
        let caps = ingest
            .pipeline()
            .element(names::FRAMES_SINK)
            .unwrap()
            .static_pad("sink")
            .unwrap()
            .current_caps()
            .unwrap();
        let info = gst_video::VideoInfo::from_caps(&caps).unwrap();
        let buffer = slot.latest().unwrap().frame.buffer.clone();
        let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(&buffer, &info).unwrap();
        let row = 120 * frame.plane_stride()[0] as usize;
        let y = frame.plane_data(0).unwrap();
        let uv = frame.plane_data(1).unwrap();
        let levels = (y[row + 80], y[row + 240], (uv[0], uv[1]));
        ingest.stop().unwrap();
        levels
    }

    /// A quirk camera in a system-memory ingest of a limited contract: the
    /// levels step maps its full-range black to Y=16 and white to Y=235
    /// (within 1), chroma neutral. Without the row the frames pass as they
    /// are (`videoconvertscale` leaves a range-only difference alone), and
    /// under a full contract the relabel passes them unchanged too. CPU
    /// only.
    #[test]
    fn a_quirk_camera_reaches_the_limited_range_in_system_memory() {
        require_elements(&[
            "appsrc",
            "queue",
            "videobalance",
            "capssetter",
            "videocrop",
            "videoconvertscale",
            "appsink",
        ]);
        let near = |got: u8, want: i32| (i32::from(got) - want).abs() <= 1;
        let (black, white, (u, v)) = system_levels(Some(VX800), ColourRange::Limited);
        assert!(near(black, 16) && near(white, 235), "{black} {white}");
        assert!(near(u, 128) && near(v, 128), "chroma {u} {v}");
        let (black, white, _) = system_levels(Some(C930E), ColourRange::Limited);
        assert_eq!((black, white), (0, 255), "no row: as labelled");
        let (black, white, (u, v)) = system_levels(Some(VX800), ColourRange::Full);
        assert_eq!((black, white), (0, 255), "full contract");
        assert!(near(u, 128) && near(v, 128), "chroma {u} {v}");
    }

    /// A quirk camera in a system-memory ingest of a limited contract: the
    /// balanced frames leave the relabel labelled limited range, the rest
    /// of the camera's label kept, also should the camera label them full
    /// (a later conversion must not compress them again). CPU only.
    #[test]
    fn the_balanced_frames_of_a_quirk_camera_are_relabelled_limited() {
        require_elements(&["appsrc", "queue", "videobalance", "capssetter", "fakesink"]);
        let yuy2 = "video/x-raw,format=YUY2,width=320,height=240,framerate=30/1";
        for (label, want) in [
            ("2:4:5:4", "2:4:5:4"),
            ("1:4:5:4", "2:4:5:4"),
            ("1:3:5:1", "2:3:5:1"),
        ] {
            let caps = format!("{yuy2},colorimetry={label}");
            let src = camera_src(&caps);
            let head = camera_chain(
                src.clone().upcast(),
                "test",
                &caps,
                PixelFormat::Yuy2,
                2,
                VideoMemory::System,
                ColourRange::Limited,
                Some(VX800),
            )
            .unwrap();
            let pipeline = gst::Pipeline::with_name("quirk-system-head");
            let sink = make("fakesink", "sink").unwrap();
            sink.set_property("sync", false);
            let mut chain = head.chain;
            chain.push(sink);
            add_linked(&pipeline, &chain).unwrap();
            pipeline.set_state(gst::State::Playing).unwrap();
            for n in 0..3 {
                src.push_buffer(black_and_white_yuy2(n)).unwrap();
            }
            src.end_of_stream().unwrap();
            let msg = pipeline
                .bus()
                .unwrap()
                .timed_pop_filtered(
                    gst::ClockTime::from_seconds(10),
                    &[gst::MessageType::Eos, gst::MessageType::Error],
                )
                .expect("EOS or error");
            assert!(matches!(msg.view(), gst::MessageView::Eos(_)), "{msg:?}");
            let out = pipeline
                .by_name(names::RANGE_RELABEL)
                .unwrap()
                .static_pad("src")
                .unwrap()
                .current_caps()
                .unwrap();
            pipeline.set_state(gst::State::Null).unwrap();
            let got = out
                .structure(0)
                .unwrap()
                .get::<String>("colorimetry")
                .unwrap();
            assert!(same_colorimetry(&got, want), "{label}: {got} {want}");
        }
    }

    /// The CUDA CPU convert path of a quirk camera, up to `cudaupload`
    /// (here a `fakesink`): `videoconvert`'s NV12 carries the camera's
    /// label made full range, so `cudaconvertscale` converts the range;
    /// the data are untouched. Without the row, the camera's label. CPU
    /// only.
    #[test]
    fn a_quirk_camera_is_uploaded_labelled_full_range() {
        require_elements(&["appsrc", "queue", "capssetter", "videoconvert", "fakesink"]);
        for (usb, want_full) in [(Some(VX800), true), (Some(C930E), false), (None, false)] {
            let src = camera_src(YUY2_LIMITED);
            let head = camera_chain(
                src.clone().upcast(),
                "test",
                YUY2_LIMITED,
                PixelFormat::Yuy2,
                2,
                VideoMemory::Cuda,
                ColourRange::Limited,
                usb,
            )
            .unwrap();
            assert_eq!(head.format, "NV12");
            let pipeline = gst::Pipeline::with_name("quirk-cuda-head");
            let sink = make("fakesink", "sink").unwrap();
            sink.set_property("sync", false);
            let mut chain = head.chain;
            chain.push(sink);
            add_linked(&pipeline, &chain).unwrap();
            pipeline.set_state(gst::State::Playing).unwrap();
            for n in 0..3 {
                src.push_buffer(black_and_white_yuy2(n)).unwrap();
            }
            src.end_of_stream().unwrap();
            let msg = pipeline
                .bus()
                .unwrap()
                .timed_pop_filtered(
                    gst::ClockTime::from_seconds(10),
                    &[gst::MessageType::Eos, gst::MessageType::Error],
                )
                .expect("EOS or error");
            assert!(matches!(msg.view(), gst::MessageView::Eos(_)), "{msg:?}");
            let caps = pipeline
                .by_name(names::INGEST_CAPS)
                .unwrap()
                .static_pad("src")
                .unwrap()
                .current_caps()
                .unwrap();
            pipeline.set_state(gst::State::Null).unwrap();
            let s = caps.structure(0).unwrap();
            assert_eq!(s.get::<&str>("format").unwrap(), "NV12");
            let label = s.get::<String>("colorimetry").unwrap();
            let want = if want_full {
                let full = full_range_caps(&YUY2_LIMITED.parse::<gst::Caps>().unwrap()).unwrap();
                full.structure(0)
                    .unwrap()
                    .get::<String>("colorimetry")
                    .unwrap()
            } else {
                "2:4:5:4".to_string()
            };
            assert!(same_colorimetry(&label, &want), "{usb:?}: {label} {want}");
        }
    }

    /// [`LiveStats`] probes the same camera pad as the frame guard: it
    /// records only the frames the guard passes (GStreamer calls a pad's
    /// probes in the order they were added and stops at the first DROP;
    /// the guard is installed when the head is built, before
    /// [`LiveStats::attach`]). No output ticks, so every recorded frame
    /// stays pending. CPU only.
    #[test]
    fn live_stats_never_record_a_dropped_frame() {
        require_elements(&[
            "appsrc",
            "queue",
            "videocrop",
            "videoconvertscale",
            "appsink",
        ]);
        let caps = "video/x-raw,format=NV12,width=160,height=120,framerate=30/1,\
                    colorimetry=2:4:5:1";
        let src = camera_src(caps);
        let head = camera_chain(
            src.clone().upcast(),
            "test",
            caps,
            PixelFormat::Nv12,
            2,
            VideoMemory::System,
            ColourRange::Limited,
            None,
        )
        .unwrap();
        let guard = Arc::clone(&head.guard);
        let slot = Arc::new(Slot::new());
        let mut ingest = Ingest::from_head(
            head.chain,
            head.format,
            (160, 120),
            AspectPolicy::Crop,
            system_writer(&slot, &"320x240@30".parse().unwrap()),
        )
        .unwrap();
        let stats = LiveStats::new();
        stats.attach(&ingest).unwrap();
        ingest.start_with(None).unwrap();
        let total = 10u64;
        for n in 0..total {
            let mut b = gst::Buffer::from_slice(vec![16u8; 160 * 120 * 3 / 2]);
            let r = b.get_mut().unwrap();
            r.set_pts(gst::ClockTime::from_mseconds(33 * n));
            if n % 3 == 0 {
                r.set_flags(gst::BufferFlags::CORRUPTED);
            }
            src.push_buffer(b).unwrap();
        }
        let dropped = 4; // n = 0, 3, 6, 9
        let deadline = Instant::now() + Duration::from_secs(10);
        while guard.counts().total() + (stats.pending() as u64) < total {
            assert!(Instant::now() < deadline, "frames still in flight");
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(guard.counts().flagged, dropped);
        assert_eq!(stats.pending() as u64, total - dropped);
        ingest.stop().unwrap();
    }

    /// A camera whose status row says a closed shutter gives flat black
    /// frames (the MX Brio) has its luma watched, raw on the camera source
    /// and MJPEG after the decoder (a JPEG of luma 16 from `jpegenc`
    /// standing in for the camera's): 1.5 s of flat black closes its
    /// shutter ([`Ingest::shutter`]). A camera without the row has no
    /// watch. CPU only.
    ///
    /// The frames are pushed in one burst, faster than `jpegdec` decodes
    /// them on a loaded machine, and the MJPEG watch sits after the leaky
    /// camera queue: a queue of the camera's two buffers would drop all
    /// but the last few, and the decoder's watch would see less than the
    /// second of stream time a close takes. The queue here holds every
    /// frame pushed, so none is dropped.
    #[test]
    fn a_camera_with_a_status_row_watches_for_a_closed_shutter() {
        require_elements(&[
            "appsrc",
            "queue",
            "jpegdec",
            "jpegenc",
            "videobalance",
            "capssetter",
            "videocrop",
            "videoconvertscale",
            "appsink",
        ]);
        const BRIO: UsbId = UsbId {
            vendor: 0x046d,
            product: 0x0944,
        };
        let nv12 = "video/x-raw,format=NV12,width=160,height=120,framerate=30/1,\
                    colorimetry=2:4:5:1";
        let mjpeg = "image/jpeg,width=160,height=160,framerate=30/1";
        let mut flat = vec![16u8; 160 * 120];
        flat.resize(160 * 120 * 3 / 2, 128);
        let jpeg = super::tests::full_range_jpeg(16, (128, 128), 160);
        // 1.5 s at 30 fps; the queue holds them all (see the doc).
        const FRAMES: u32 = 46;
        for (caps, format, frame, usb, want) in [
            (nv12, PixelFormat::Nv12, &flat, Some(BRIO), true),
            (mjpeg, PixelFormat::Mjpeg, &jpeg, Some(BRIO), true),
            (nv12, PixelFormat::Nv12, &flat, Some(C930E), false),
            (nv12, PixelFormat::Nv12, &flat, None, false),
        ] {
            let src = camera_src(caps);
            let head = camera_chain(
                src.clone().upcast(),
                "test",
                caps,
                format,
                FRAMES,
                VideoMemory::System,
                ColourRange::Limited,
                usb,
            )
            .unwrap();
            assert_eq!(head.shutter.is_some(), want, "{format} {usb:?}");
            let shutter = head.shutter.clone();
            let size = if format == PixelFormat::Mjpeg {
                (160, 160)
            } else {
                (160, 120)
            };
            let slot = Arc::new(Slot::new());
            let mut ingest = Ingest::from_head(
                head.chain,
                head.format,
                size,
                AspectPolicy::Crop,
                system_writer(&slot, &"320x240@30".parse().unwrap()),
            )
            .unwrap();
            assert_eq!(ingest.shutter(), None, "not a camera ingest");
            ingest.shutter = shutter;
            ingest.start_with(None).unwrap();
            for n in 0..u64::from(FRAMES) {
                let mut b = gst::Buffer::from_slice(frame.clone());
                b.get_mut()
                    .unwrap()
                    .set_pts(gst::ClockTime::from_mseconds(33 * n));
                src.push_buffer(b).unwrap();
            }
            if want {
                let deadline = Instant::now() + Duration::from_secs(10);
                while ingest.shutter() != Some(ShutterState::Closed) {
                    assert!(
                        Instant::now() < deadline,
                        "{format}: {:?}",
                        ingest.shutter()
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
            } else {
                std::thread::sleep(Duration::from_millis(100));
                assert_eq!(ingest.shutter(), None, "{usb:?}");
            }
            ingest.stop().unwrap();
        }
    }

    /// The ingest reports the rate of the frames its camera's guard
    /// passes ([`Ingest::delivered_fps`]): a camera stamping a frame every
    /// 51.4 ms (the Generalplus sensor, 19.45 fps for a nominal 30) for
    /// 5 s. Nothing before the first frame, nothing for an ingest that is
    /// not a camera. CPU only.
    #[test]
    fn the_delivered_rate_comes_from_the_camera_guard() {
        require_elements(&[
            "appsrc",
            "queue",
            "videocrop",
            "videoconvertscale",
            "appsink",
        ]);
        let caps = "video/x-raw,format=NV12,width=160,height=120,framerate=30/1,\
                    colorimetry=2:4:5:1";
        let src = camera_src(caps);
        let head = camera_chain(
            src.clone().upcast(),
            "test",
            caps,
            PixelFormat::Nv12,
            2,
            VideoMemory::System,
            ColourRange::Limited,
            None,
        )
        .unwrap();
        let guard = Arc::clone(&head.guard);
        let slot = Arc::new(Slot::new());
        let mut ingest = Ingest::from_head(
            head.chain,
            head.format,
            (160, 120),
            AspectPolicy::Crop,
            system_writer(&slot, &"320x240@30".parse().unwrap()),
        )
        .unwrap();
        assert_eq!(ingest.delivered_fps(), None, "not a camera ingest");
        ingest.guard_frames = Some(guard);
        ingest.start_with(None).unwrap();
        assert_eq!(ingest.delivered_fps(), None, "no frame yet");
        for n in 0..(5_000 / 51 + 1) {
            let mut b = gst::Buffer::from_slice(vec![16u8; 160 * 120 * 3 / 2]);
            b.get_mut()
                .unwrap()
                .set_pts(gst::ClockTime::from_nseconds(n * 51_400_000));
            src.push_buffer(b).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        let fps = loop {
            if let Some(fps) = ingest.delivered_fps() {
                break fps;
            }
            assert!(Instant::now() < deadline, "no rate");
            std::thread::sleep(Duration::from_millis(10));
        };
        ingest.stop().unwrap();
        assert!((fps - 19.455).abs() < 0.01, "{fps}");
    }

    /// `cudaconvertscale` converts a range-only difference: full-range NV12
    /// (labelled so, as the quirk relabel leaves it) reaches the limited
    /// canonical label with black at Y=16 and white at Y=235.
    #[test]
    #[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
    fn gpu_cudaconvertscale_converts_full_to_limited_range() {
        require_elements(&[
            "appsrc",
            "cudaupload",
            "cudaconvertscale",
            "cudadownload",
            "appsink",
        ]);
        let (w, h) = (320usize, 240usize);
        let pipeline = gst::parse::launch(&format!(
            "appsrc name=in format=time caps=video/x-raw,format=NV12,width={w},height={h},\
             framerate=30/1,colorimetry=1:4:5:4 \
             ! cudaupload ! video/x-raw(memory:CUDAMemory),format=NV12 \
             ! cudaconvertscale \
             ! video/x-raw(memory:CUDAMemory),format=NV12,width={w},height={h},\
             colorimetry=2:4:5:4 \
             ! cudadownload ! video/x-raw,format=NV12 ! appsink name=out"
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let src = pipeline
            .by_name("in")
            .unwrap()
            .downcast::<gst_app::AppSrc>()
            .unwrap();
        let sink = pipeline
            .by_name("out")
            .unwrap()
            .downcast::<gst_app::AppSink>()
            .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let mut data = Vec::with_capacity(w * h * 3 / 2);
        for _ in 0..h {
            data.extend((0..w).map(|x| if x < w / 2 { 0u8 } else { 255 }));
        }
        data.resize(w * h * 3 / 2, 128);
        src.push_buffer(gst::Buffer::from_slice(data)).unwrap();
        src.end_of_stream().unwrap();
        let sample = sink.pull_sample().unwrap();
        let info = gst_video::VideoInfo::from_caps(sample.caps().unwrap()).unwrap();
        let buffer = sample.buffer().unwrap();
        let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).unwrap();
        let row = 120 * frame.plane_stride()[0] as usize;
        let y = frame.plane_data(0).unwrap();
        let (black, white) = (y[row + 80], y[row + 240]);
        pipeline.set_state(gst::State::Null).unwrap();
        let near = |got: u8, want: i32| (i32::from(got) - want).abs() <= 1;
        assert!(near(black, 16) && near(white, 235), "{black} {white}");
    }
}
