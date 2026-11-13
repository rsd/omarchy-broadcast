//! The implementation of `obceffect`.
//!
//! # Threads and locks
//!
//! * The **streaming thread** decides per frame what it gets
//!   ([`decide`]) in `submit_input_buffer`, switching BaseTransform's
//!   passthrough to match, then allocates and fills the output.
//! * The **worker thread** (one per started element) loads, switches and
//!   unloads the backend, and decodes the background image. It works from
//!   the latest request only: requests are numbered (`Control::generation`)
//!   and a superseded one is dropped.
//! * `Control` (settings, status, the CUDA handles once known) and `Engine`
//!   (the backend instance and what it has loaded) have one mutex each and
//!   no code path holds both. The streaming thread locks `Engine` only to
//!   process a frame, and only when `Control::loaded_kind` says a chain
//!   usable for the request is loaded ([`Control::usable`]), so a cold load
//!   on the worker never blocks it; a switch on the loaded instance blocks
//!   it for the switch itself: milliseconds between two effects, a stage's
//!   load when a switch adds it (see below). A frame of a toggle mid-stream
//!   (see there) only tries the lock, so it never waits either.
//!
//! # A toggle mid-stream, a start, a new source, a failure
//!
//! A requested effect (blur or replace) that is not loaded yet holds black
//! (with `on-failure=image`, the fallback image) only where the room it
//! hides was never on screen. The streaming thread keeps
//! `StreamState::raw_on_screen`: the last frame output was the unprocessed
//! picture, the input passed through or copied, or processed by the stages
//! alone (they hide nothing). It is set by such a frame and cleared by any
//! frame that goes out black (or the image) or processed by an effect, and
//! by a stop (so a start begins without it), new caps, a flush and a frame
//! with [`NEW_SOURCE_FLAG`](super::NEW_SOURCE_FLAG), before that frame is
//! decided ([`decide_frame`]):
//!
//! * **A toggle mid-stream** (`none` to blur or replace while the input, or
//!   the stages alone, are on screen) keeps sending the frames as before
//!   until the effect is ready: the input passed through, or with stages
//!   loaded alone and in force, the frame processed by them. A black hold
//!   would protect nothing: the room was already shown. While the cold load
//!   holds the instance, the stages cannot run and the input is passed
//!   through (copied, when a frame decided for the stages finds the lock
//!   taken: such a frame tries the `Engine` lock and never waits for it).
//!   The first frame decided with the effect loaded is processed by it. A
//!   toggle frame that finds the effect already installed under the lock
//!   (the worker has not published it yet) is an effect frame: processed
//!   by it, its failure the effect's, and the frames after it count the
//!   effect as loaded until the worker publishes (`StreamState::toggle_found`),
//!   so no black frame follows it. The flag moves by
//!   [`raw_on_screen_after`].
//! * **A start** with the effect requested (the element's first frames, and
//!   the first after a stop) holds black until the effect is ready, so the
//!   camera is never shown raw while an effect is in force.
//! * **A new source** (a camera switch) while the effect is requested and
//!   not ready holds black from its first frame: the new camera's room was
//!   never shown.
//! * **A failure** of the effect or of its module takes `on-failure`
//!   (black, the image, or with `none` the input), whatever was on screen.
//!   A revoked backend (`ObcEffect::revoke_backend`) is such a failure,
//!   from the next frame decided: its status is failed, so no toggle
//!   applies, and its new request number and epoch end any effect a
//!   toggle frame found. The worker drops its instance before anything
//!   else (`drop_revoked`), so neither a warm switch nor a re-armed load
//!   reuses it.
//! * Once a frame has gone out black or processed by the effect, a later
//!   state without the effect loaded (a reload for new caps, a retry after
//!   a failure, a backend that lost its chain) holds black, never the input;
//!   so does a repeat.
//!
//! A toggle from `none` always loads the effect cold on the worker, even
//! when the backend may still hold its models (a warm reload): the
//! `InferenceBackend` seam reports a reused model only after a load
//! (`LoadReport::reused_model`), so the element cannot tell a warm toggle
//! from a cold one beforehand and does not make the streaming thread wait
//! for it.
//!
//! # The stages
//!
//! The stackable stages (`docs/adr/0018-video-noise-removal.md`,
//! `docs/adr/0024-stackable-stages-and-effects-in-the-settings-schema.md`)
//! live in the same backend instance as the effect: one load request carries
//! the effect and every requested stage (`LoadRequest::stages`, in the
//! settings' order, with their load-time parameters), and they unload with
//! the effect, so they add no teardown path and nothing of them runs at
//! idle. The element knows no stage by name: every rule below applies to
//! each [`StageId`] the settings list. A stage hides nothing, so the privacy
//! rules of the effect are unchanged by it:
//!
//! * `none` with stages passes the input through until they are loaded,
//!   then processes every frame. Without a stage `none` makes no CUDA call.
//! * From `none` (with or without stages) to blur or replace, the matte
//!   loads cold: the stages alone do not count as loaded for an effect, and
//!   the streaming thread never waits for that load (under the `Engine`
//!   lock a frame decided for an effect is black if only the stages are
//!   loaded). Mid-stream the frames are those of `none` until the effect is
//!   ready (the stages' while they are loaded, else the input); at a start
//!   or on a new source they are black (see "A toggle mid-stream" above).
//! * Every other change (turning a stage on or off under an effect, a new
//!   load-time parameter of a stage, blur and replace with stages, an effect
//!   to `none` with stages) is a switch on the loaded instance, done under
//!   the `Engine` lock as for blur to replace. The streaming thread waits for
//!   it there; when the switch adds a stage under blur or replace, or goes
//!   from an effect to `none` with stages, that wait is the stage's load (a
//!   new load-time parameter costs what the backend says: a relighting
//!   stage's new light loads no model). No frame is output meanwhile, so the
//!   last one shown is the previous chain's processed frame, never the
//!   input; the next is the new chain's. A stage's live parameters travel
//!   with each frame (`FrameParams`) and change nothing that is loaded.
//! * **A stage failure drops the stage, never the effect.** The effective
//!   request of a stage is "requested and no failure recorded"
//!   (`StageCtl::failure` of `Control::stages`). A load that fails with
//!   `BackendError::Stage` for a stage drops that stage, and any other load
//!   failure while the stages are alone (`none`) drops every requested
//!   stage; the worker then loads the rest in the same job (or unloads, for
//!   `none` without another stage). A frame that fails with
//!   `BackendError::Stage` for a stage (or any frame failure of the stages
//!   alone, which drops them all) records it too, whichever request the
//!   frame was decided for, as long as the current one still asks for the
//!   stage: the streaming thread marks the loaded chain as without the stage
//!   and starts a new request, which the worker finds already loaded (or
//!   unloads, for `none`). Under blur or replace the backend has dropped the
//!   stage, so the frame runs again through the chain without it, as often
//!   as stages fail, and is output processed; only if a failure names no
//!   stage does the effect's failure path apply (black or the input, per
//!   `on-failure`). With `none` the frame is the input, and a stage left
//!   loaded runs from the next frame. The effect's status never becomes
//!   failed for a stage. The failure is forgotten when the stage's request
//!   (on or off, or its load-time parameters) changes or on a retry
//!   (`apply` with `retry`), never otherwise.
//! * Each stage's status (`StageCtl::status`) is separate from the
//!   effect's: off (not requested), loading, ready, or failed with the
//!   reason. A failure of the whole chain (the effect's load or frame) shows
//!   as failed there too, without being recorded as a failure of the stage.
//!
//! # Timings
//!
//! `gpu_ms` is measured with two CUDA events on the element's stream around
//! the backend's calls for the frame (every call when a frame runs again
//! without a stage), not around a temporal reset. Each stage's time is the
//! backend's `timings` step of that stage for the frame just processed,
//! read after the frame's stream synchronisation (a backend reports a frame
//! only once its work has completed), under a `try_lock` of `Engine` so the
//! streaming thread never waits for a load for it: while the worker holds
//! the lock, that frame's figures are skipped. The backend's status values
//! (`InferenceBackend::status_values`, auto-frame's zoom for one) are read
//! in the same `try_lock` and replace the element's; a skipped frame leaves
//! them as they were.
//!
//! # A new source
//!
//! The output clock marks the first fresh frame of each new source with
//! [`NEW_SOURCE_FLAG`](super::NEW_SOURCE_FLAG). The element then asks the
//! backend to forget its temporal state (`InferenceBackend::reset_temporal`)
//! right before the next frame it processes, so a temporal stage (noise
//! removal) does not blend frames of two cameras. A stage whose state cannot
//! be reset in place (a relighting stage, for one backend) is dropped by the
//! backend instead and loaded again fresh by the worker (a new request, the
//! stage loading meanwhile): the frames until it is ready run without it,
//! the effect alone under blur or replace (never the input), the input with
//! the stages alone gone, and no frame is processed by that stage's instance
//! that saw the previous source, so camera A's person never shows in camera
//! B's frames. A flagged frame that is not
//! processed (passed through, or black) keeps the reset pending for the
//! next processed one. While a requested effect is not ready, the flagged
//! frame and the ones after it are black, not the input (see "A toggle
//! mid-stream" above).
//!
//! # A repeat
//!
//! The output clock marks a tick that emits the last camera frame again
//! with [`REPEAT_FLAG`](super::REPEAT_FLAG). [`decide`] runs on it as on
//! any frame. When the frame is to be processed and the element holds the
//! output of the last processed frame, made under the same request
//! (`Control::generation`), epoch (`Control::epoch`: no apply, load, switch,
//! unload or failure since), per-frame parameters and caps, with no
//! temporal reset pending, that output is sent again and no effect runs:
//! a frame generation module downstream answers a repeat with its held
//! frame, so the work would be thrown away. Otherwise the repeat is
//! processed as any frame; black stays black, and a repeat is passed
//! through as it came while an effect is asked only as any frame is: in a
//! toggle mid-stream, before the effect is first shown (see above).
//!
//! The cache is written only from a processed frame (`generate_output`
//! keeps a reference to the buffer pushed downstream), and dropped by any
//! frame that goes out black or as the input, the new-source flag, new
//! caps, a flush and a stop. The re-sent buffer is a new buffer on the same
//! memory, with the repeat's timestamps and flags. Downstream never writes
//! the cached memory: it is shared with the cache's buffer, so a map for
//! writing (`obcmodule` processes in place) fails to lock it and GStreamer
//! writes into a copy of it (`gst_memory_make_mapped`). That copy is the
//! price of the cache, on every processed frame a module downstream writes
//! into. A re-sent frame counts in no frame counter of the statistics.
//!
//! # The placeholder image
//!
//! The output clock marks a placeholder tick that pushes its image (the
//! away image, not a camera frame) with
//! [`PLACEHOLDER_FLAG`](super::PLACEHOLDER_FLAG). [`decide`] does not run
//! on it: `prepare_frame` returns before anything else, with the frame
//! passed through (BaseTransform passthrough: the input buffer, no pool
//! buffer, no CUDA object made, no backend call), whatever the effect,
//! its status, its warm-up or `on-failure`. The image is no camera
//! picture, so it has nothing to hide, and replacing, blurring or blacking
//! it would undo the choice of showing it. Nothing of the stream state is
//! touched: the cache is neither written nor dropped (a repeat after the
//! placeholder still finds it, valid as before), a pending temporal reset
//! waits for the next camera frame, and `StreamState::raw_on_screen` and
//! `toggle_found` stay as the last camera frame left them. The frame
//! counts in no frame counter of the statistics.
//!
//! # The fallback image
//!
//! With `on-failure=image` and a `fallback-image`, the image is wanted for
//! the path, the file there (`background::FileStamp`: an image replaced
//! at the same path is another key, so settings applied again read it
//! again) and the negotiated size and colour (`FallbackKey`). Each new
//! key (from `apply` or `set_caps`) starts a decode on a thread of its own
//! (`obceffect-image`, [`background::decode_yuv420`]), numbered
//! (`FallbackCtl::serial`) so that a result arriving after the key changed
//! again is dropped; any other fallback, or no path, wants none. The
//! decoded NV12 is kept in `Control`, read by the streaming thread with
//! the rest of the frame's decision, uploaded once into a device frame of
//! the element's context (`GpuResources::fallback`, dropped with the other
//! CUDA objects) and copied, on the element's stream, into every frame
//! decided black and every failed frame `on-failure` hides. Until it is
//! ready, or when it cannot be read or uploaded, those frames are black
//! and the reason is reported (`FallbackCtl::error`: the statistics and the
//! status message).
//!
//! # The video background
//!
//! `ObcEffect::set_background_video` sets `VideoCtl::path`, outside the
//! settings: it is not part of what is loaded, except that replace without
//! an image loads black as its still (`Target::black_still`). The source
//! (`crate::video_background::VideoSource`) is reconciled with what is
//! wanted ([`video_want`]: replace requested and loaded, the caps known, the
//! backend taking per-frame backgrounds, and on CUDA frames the element's
//! context shared as a `GstContext`) by [`refresh_video`], called after an
//! apply, new caps and every worker job; [`stop_video`] stops it
//! on a stop. `Shared::video` serialises them and is taken before
//! `Control`, never with `Engine` held; the pipeline is stopped with no
//! lock but that one held, and a source no longer wanted is detached
//! ([`VideoSource::detach`]: its decoder's teardown is never waited for,
//! so never on the streaming thread, which calls [`refresh_video`] on new
//! caps). Only [`stop_video`], in the element's `stop`, waits for it. A
//! flush leaves the source playing where it is: its pipeline runs on its
//! own clock, apart from the stream that flushed. The statistics read
//! whether a source runs from `Shared::video_running`, never waiting for
//! that lock.
//!
//! A request without the video (a still, or another effect) does not stop
//! the video while the chain in force still has it, that is while the
//! loaded target's background ([`Target::same_background`]) is not yet the
//! requested one (`VideoCtl::in_force`: a still to decode and load, blur to
//! switch to): the source keeps playing and the frames keep its
//! background, never the black or previous still of the chain in force.
//! The source stops, and a frame in force goes back to the load-time still,
//! once the requested background is the one loaded.
//!
//! Each source has a new `Slot`, published in `VideoCtl::slot` before it
//! starts (its reporter records a failure only while its slot is the
//! current one). The streaming thread takes the slot with the frame's
//! decision, and on a fresh camera frame decided for replace, under the
//! `Engine` lock and before `process`, [`feed_background`] gives the
//! backend the slot's newest frame if it has not had it
//! (`BgState::sent`), mapped for the call (CUDA: synchronised with its
//! producer, and checked to be memory of the element's context). After
//! a load (the still back in force: every load, switch or full, resets the
//! backend's background) and until the current slot has a frame, the last
//! frame given (`BgState::last`) is given again, so a change of video or of
//! the still never shows anything but the previous background before the
//! new video's first frame. Without a video wanted, a frame in force goes
//! back to the load-time still (`None`) once the requested background is
//! loaded (above). A frame is tried once; a refusal leaves the previous
//! background and is reported (`VideoCtl::error`, cleared by the next
//! frame given). A repeat is answered from the cache, so the cache stays
//! valid; a repeat that is processed (after a load, an apply) never
//! advances the video, but it does give the last frame again when a load
//! has put the still back in force, so that the frame after a reload is
//! never the still. The placeholder never gets here.
//!
//! # CUDA context and stream
//!
//! The context is the one the first input memory belongs to (the stream's
//! shared `GstCudaContext`); every later input must be in it. The element
//! creates one `GstCudaStream` in it and runs everything there: the
//! backend's `load` and `process`, the black fills and the copies. It does
//! not adopt the input's stream: the backend binds the stream at load time,
//! and the input's stream changes with every ingest pipeline (each camera
//! switch) and differs for the output's placeholder frame, so sharing it
//! would force a full reload on each switch. Instead:
//!
//! * the input is synchronised with `gst_cuda_memory_sync` before it is read
//!   (a no-op unless its producer left work pending on another stream);
//! * the output pool carries the element's stream, so the output memory is
//!   marked as written on it (downstream CUDA elements order against it);
//! * the element's stream is synchronised before `transform` returns: the
//!   input buffer may go back to its ingest pool as soon as it is released,
//!   and its next write must not race this frame's read. Downstream
//!   (`cudadownload`, on the same thread) would wait for that work anyway.
//!
//! # System memory
//!
//! Caps without the CUDA memory feature (`Format::cuda` false) run the same
//! element with no CUDA object: what a frame gets is decided by the same
//! code for both memories ([`decide_frame`], `run_frame`), and only its
//! pixel work differs (`FramePixels`: `CudaPixels` on the element's
//! stream, `HostPixels` on the CPU). On system memory:
//!
//! * black is two row-by-row fills of the mapped output (Y to the black of
//!   the caps' range, UV to 128), the input and the decoded fallback image
//!   are row-by-row copies, and the backend reads the mapped input and
//!   writes the mapped output (`InferenceBackend::process_host`);
//! * the output buffers come from a system-memory video pool
//!   (`StreamState::host_pool`), created by the first frame the element
//!   writes; passthrough allocates nothing;
//! * the worker loads as soon as the caps are known (it waits for no
//!   context), with `InferenceBackend::load_host`, and only with a factory
//!   that lists host frames (`BackendFactory::kinds`); an instance is never
//!   called with a context current, and one created for the other memory
//!   (new caps) is unloaded and dropped before a load (`drop_revoked`);
//! * the repeat cache is the same: the re-sent buffer shares the cached
//!   memory, so a downstream element that writes in place maps a copy
//!   (`gst_memory_make_mapped`), on the CPU here;
//! * the frame size must be even both ways, as the pipeline's canonical
//!   caps always are: the CPU fills and copies (`crate::video`) refuse an
//!   odd size, so a frame the element would write fails the stream with an
//!   error instead of going out partly written. Passthrough is unaffected.
//!   The pad templates do not restrict the size: the limit is the pixel
//!   operations', and it fails closed.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard};
use std::thread::JoinHandle;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::base_transform::{
    GenerateOutputSuccess, InputBuffer, PrepareOutputBufferSuccess,
};
use gst_base::subclass::prelude::*;
use gst_video::VideoInfo;
use obc_backend::{
    BackendError, BackendFactory, BackgroundFrame, ColourSpec, DeviceNv12, EffectKind, FrameKind,
    FrameParams, GpuContext, HostNv12, HostNv12View, InferenceBackend, LoadReport, LoadRequest,
    StageId, StageList, StageTimings, StageValue,
};
use obc_gpu::{ContextGuard, CuContext, CuStream, Event};

use super::{STATUS_MESSAGE, has_new_source_flag, has_placeholder_flag, has_repeat_flag};
use crate::background;
use crate::cuda::{self, MAP_CUDA};
use crate::settings::{
    EffectFallback, EffectSettings, EffectStats, EffectStatus, FrameAction, FrameOutput,
    ScreenEvent, StageStats, Target, decide, decide_frame, fallback_unreadable, parse_effect,
    percentile, raw_on_screen_after,
};
use crate::video::{
    CudaFrame, colour_of, copy_nv12, copy_packed_nv12, fill_nv12, host_nv12, input_context,
};
use crate::video_background::{
    self, Slot, SlotFrame, SourceSpec, VideoSource, video_as_still, video_unplayable,
};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "obceffect",
        gst::DebugColorFlags::empty(),
        Some("omarchy-broadcast effect element"),
    )
});

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The negotiated frame format, as the backend sees it.
#[derive(Debug, Clone, PartialEq)]
struct Format {
    width: u32,
    height: u32,
    /// The colour the backend converts with, or why it cannot represent
    /// the caps' colorimetry (only an effect fails on that; `none` works).
    colour: Result<ColourSpec, String>,
    /// Black for this range (the matrix does not matter for black).
    black: ColourSpec,
    /// The frames are in CUDA memory; else in system memory (see "System
    /// memory" in the module docs).
    cuda: bool,
    /// The caps' colorimetry, as a caps string: the video background is
    /// converted to it.
    colorimetry: String,
}

impl Format {
    fn of(info: &VideoInfo, cuda: bool) -> Format {
        let c = info.colorimetry();
        let full = c.range() == gst_video::VideoColorRange::Range0_255;
        let black = if full {
            ColourSpec::FULL_BT601
        } else {
            ColourSpec::LIMITED_BT601
        };
        Format {
            width: info.width(),
            height: info.height(),
            colour: colour_of(&c),
            black,
            cuda,
            colorimetry: c.to_string(),
        }
    }

    /// The memory of the frames, as the backend seam names it.
    fn kind(&self) -> FrameKind {
        if self.cuda {
            FrameKind::Cuda
        } else {
            FrameKind::Host
        }
    }
}

/// What the backend instance has loaded.
#[derive(Debug, Clone, PartialEq)]
struct LoadedKey {
    target: Target,
    width: u32,
    height: u32,
    colour: ColourSpec,
    /// The memory of the frames it was loaded for.
    kind: FrameKind,
}

/// What the fallback image is decoded for: its path and the file there,
/// and the negotiated size and colour.
#[derive(Debug, Clone, PartialEq)]
struct FallbackKey {
    path: PathBuf,
    /// The file at `path` when the key was made: an image replaced at the
    /// same path is another key, decoded again.
    file: Option<background::FileStamp>,
    width: u32,
    height: u32,
    colour: ColourSpec,
}

/// The decoded fallback image, packed NV12 of its key's size.
#[derive(Debug)]
struct FallbackFrame {
    /// The [`FallbackCtl::serial`] of its decode.
    serial: u64,
    width: u32,
    height: u32,
    nv12: Vec<u8>,
}

/// The fallback image (see "The fallback image" in the module docs).
#[derive(Debug, Default)]
struct FallbackCtl {
    /// What is decoded, or being decoded; `None`: no image is wanted.
    key: Option<FallbackKey>,
    /// Counts the decodes started: the number of the one for `key`.
    serial: u64,
    /// The image of `key`, once decoded.
    frame: Option<Arc<FallbackFrame>>,
    /// Why the image of `key` cannot be shown ([`fallback_unreadable`]).
    error: Option<String>,
}

#[derive(Default)]
struct Engine {
    instance: Option<Box<dyn InferenceBackend>>,
    loaded: Option<LoadedKey>,
    /// `Control::revocations` when the instance was created: an instance
    /// of an earlier count belongs to a revoked backend, and the worker
    /// drops it before anything else ([`drop_revoked`]).
    revocation: u64,
    /// The memory of the frames the instance was created for: its calls
    /// take the CUDA context ([`FrameKind::Cuda`]) or none
    /// ([`FrameKind::Host`]).
    kind: Option<FrameKind>,
    /// The video background frames handed to the instance.
    bg: BgState,
}

/// What the instance has of the video background (see "The video
/// background" in the module docs).
#[derive(Default)]
struct BgState {
    /// The slot frame in force (slot id, number), if one is.
    sent: Option<(u64, u64)>,
    /// A frame given since the last load: the load-time still is not in
    /// force.
    live: bool,
    /// The last frame given, kept across a reload and a change of video so
    /// that it can be given again until the new video's first frame.
    last: Option<SlotFrame>,
}

struct Control {
    settings: EffectSettings,
    /// What the settings need loaded (the requested denoise included), and
    /// the number of the request for it.
    target: Target,
    generation: u64,
    /// Counts every change that a processed frame depends on and
    /// `generation` does not count: any `apply` (the per-frame parameters
    /// included), a load, switch or unload published by the worker, a
    /// failure, a stage dropped or rebuilt, new caps, a stop. The cached
    /// output of a repeat (see "A repeat" in the module docs) is valid only
    /// under the epoch it was written in.
    epoch: u64,
    status: EffectStatus,
    /// The failure and status of every stage the settings have listed, in
    /// the order they were first listed (a stage stays here once listed, so
    /// that its status can be reported off).
    stages: Vec<(StageId, StageCtl)>,
    /// The parameters of each frame for the settings
    /// ([`EffectSettings::frame_params`]), shared with the streaming thread.
    frame: Arc<FrameParams>,
    /// The effect of `Engine::loaded` (`EffectKind::None`: the stages
    /// alone), as last published by the worker (cleared before an
    /// unload, or a reload during which the streaming thread must not wait,
    /// starts).
    loaded_kind: Option<EffectKind>,
    /// The CUDA context and the element's stream, once the streaming thread
    /// has created them.
    gpu: Option<GpuContext>,
    format: Option<Format>,
    factory: Option<Arc<dyn BackendFactory>>,
    /// The factory was taken by [`Control::revoke`]: the next factory set
    /// starts a new request ([`Control::rearm`]).
    revoked: bool,
    /// How many times the backend was revoked: the worker drops an
    /// instance created before the last revocation
    /// (`Engine::revocation`), even when a new factory came before it did.
    revocations: u64,
    /// Why the factory of the last re-arm refused the background effect's
    /// parameters ([`Control::rearm`]): every load of the effect fails with
    /// it until settings checked with the factory are applied
    /// ([`ObcEffect::apply_checked`]).
    refused: Option<String>,
    stop: bool,
    last_load_ms: Option<f64>,
    last_load_reused: bool,
    loads: u64,
    fallback: FallbackCtl,
    video: VideoCtl,
}

/// The video background, as the streaming thread and the status see it.
#[derive(Default)]
struct VideoCtl {
    /// The video of replace (`background-video`), if any.
    path: Option<PathBuf>,
    /// The slot of the running source.
    slot: Option<Arc<Slot>>,
    /// Why the video does not play ([`EffectStats::background_error`]).
    error: Option<String>,
    /// The backend's name and whether it takes per-frame backgrounds, as
    /// the worker found the instance after its last load; `None` when
    /// nothing is loaded.
    live: Option<(&'static str, bool)>,
    /// The element's CUDA context as a `GstContext` for the video pipeline,
    /// made with the CUDA resources (or why it could not be).
    cuda: Option<Result<gst::Context, String>>,
    /// `error` is a refusal of a frame by the backend
    /// (`ObcEffect::video_frame_refused`): the next frame given clears it.
    refused: bool,
    /// The target in force as the worker last published it (`None`:
    /// nothing loaded).
    in_force: Option<Target>,
}

/// The running video source and what it was started for (serialises the
/// starts and stops: they may block for a pipeline's state change).
#[derive(Default)]
struct VideoHolder {
    wanted: Option<VideoWant>,
    source: Option<VideoSource>,
}

/// What the video background should be now.
#[derive(Debug, Clone, PartialEq)]
enum VideoWant {
    /// Playing `path` (the file `file` there) into frames of this size,
    /// colour and memory (`cuda`: the shared context's address).
    Play {
        path: PathBuf,
        file: Option<background::FileStamp>,
        width: u32,
        height: u32,
        colorimetry: String,
        cuda: Option<usize>,
    },
    /// No source, and this is why (reported).
    Still(String),
}

impl Default for Control {
    fn default() -> Self {
        Control {
            settings: EffectSettings::default(),
            target: Target::default(),
            generation: 0,
            epoch: 0,
            status: EffectStatus::Off,
            stages: Vec::new(),
            frame: Arc::new(FrameParams::default()),
            loaded_kind: None,
            gpu: None,
            format: None,
            factory: None,
            revoked: false,
            revocations: 0,
            refused: None,
            stop: false,
            last_load_ms: None,
            last_load_reused: false,
            loads: 0,
            fallback: FallbackCtl::default(),
            video: VideoCtl::default(),
        }
    }
}

/// What the element knows of one stage.
#[derive(Debug, Clone, Default)]
struct StageCtl {
    /// Why the stage was dropped; while set, the stage is not loaded
    /// although requested (see the module docs).
    failure: Option<String>,
    status: EffectStatus,
}

/// The status of a stage that is not listed.
static OFF: EffectStatus = EffectStatus::Off;

impl Control {
    /// What the element knows of `stage`, if it has been listed.
    fn stage(&self, stage: &str) -> Option<&StageCtl> {
        self.stages.iter().find(|(s, _)| s == stage).map(|(_, c)| c)
    }

    /// The status of `stage` (off if it has never been listed).
    fn stage_status(&self, stage: &str) -> &EffectStatus {
        self.stage(stage).map_or(&OFF, |c| &c.status)
    }

    /// What the element knows of `stage`, added if it has not been listed.
    fn stage_mut(&mut self, stage: &StageId) -> &mut StageCtl {
        let i = match self.stages.iter().position(|(s, _)| s == stage) {
            Some(i) => i,
            None => {
                self.stages.push((stage.clone(), StageCtl::default()));
                self.stages.len() - 1
            }
        };
        &mut self.stages[i].1
    }

    /// Every stage the settings have listed, in order.
    fn stage_ids(&self) -> Vec<StageId> {
        self.stages.iter().map(|(s, _)| s.clone()).collect()
    }

    /// Whether `stage` is requested and has not failed.
    fn effective(&self, stage: &str) -> bool {
        self.target.wants(stage) && self.stage(stage).is_none_or(|c| c.failure.is_none())
    }

    /// Whether any stage is requested and has not failed.
    fn any_effective(&self) -> bool {
        self.target
            .stages
            .iter()
            .any(|(s, _)| self.effective(s.as_str()))
    }

    /// Whether a requested stage has a failure recorded.
    fn any_stage_failed(&self) -> bool {
        self.stages.iter().any(|(_, c)| c.failure.is_some())
    }

    /// What the worker loads for the request: the target without the
    /// stages that failed.
    fn job_target(&self) -> Target {
        self.target
            .stages
            .iter()
            .fold(self.target.clone(), |t, (s, _)| {
                if self.effective(s.as_str()) {
                    t
                } else {
                    t.without(s.as_str())
                }
            })
    }

    /// Whether the loaded chain can process frames of the request: any
    /// chain for `none` (with a stage), one with an effect for blur and
    /// replace (the stages alone hide nothing).
    fn usable(&self) -> bool {
        match self.loaded_kind {
            None => false,
            Some(EffectKind::None) => self.target.kind == EffectKind::None,
            Some(_) => true,
        }
    }

    /// The effect status of a new request.
    fn pending_status(&self) -> EffectStatus {
        match self.target.kind {
            EffectKind::None => EffectStatus::Off,
            _ => EffectStatus::Loading,
        }
    }

    /// Sets every stage's status for a new request: off when not
    /// requested, failed while its failure is recorded, else loading.
    fn set_pending_stage_statuses(&mut self) {
        self.set_switch_stage_statuses(None);
    }

    /// [`Self::set_pending_stage_statuses`] for a new request that replaces
    /// `prev` on the loaded chain (`None`: a reload of everything), except
    /// that a ready stage whose request is the same in both stays ready: the
    /// switch keeps it loaded as it is (only the stage that changed loads,
    /// rebinds or goes), and showing it loading again would read as a reload
    /// of its model.
    fn set_switch_stage_statuses(&mut self, prev: Option<&Target>) {
        let target = &self.target;
        for (s, ctl) in &mut self.stages {
            let s = s.as_str();
            let kept = prev.is_some_and(|p| p.stage_request(s) == target.stage_request(s))
                && ctl.status == EffectStatus::Ready;
            ctl.status = match (&ctl.failure, target.wants(s)) {
                (_, false) => EffectStatus::Off,
                (Some(r), true) => EffectStatus::Failed(r.clone()),
                (None, true) if kept => EffectStatus::Ready,
                (None, true) => EffectStatus::Loading,
            };
        }
    }

    /// Records a failure of `stage`, unless the request no longer asks for
    /// the stage, one is already recorded, or (with `generation`) request
    /// `generation` is superseded. `reconcile` starts a new request so the
    /// worker brings what is loaded in line with it.
    fn record_stage_failure(
        &mut self,
        stage: &StageId,
        generation: Option<u64>,
        reason: String,
        reconcile: bool,
    ) -> bool {
        if generation.is_some_and(|g| g != self.generation)
            || !self.target.wants(stage.as_str())
            || self
                .stage(stage.as_str())
                .is_some_and(|c| c.failure.is_some())
        {
            return false;
        }
        let st = self.stage_mut(stage);
        st.status = EffectStatus::Failed(reason.clone());
        st.failure = Some(reason);
        if reconcile {
            self.generation += 1;
        }
        true
    }

    /// The path of the fallback image the settings and the caps want, if
    /// any.
    fn fallback_path(&self) -> Option<&PathBuf> {
        let path = self.settings.fallback_image.as_ref()?;
        self.format.as_ref()?;
        (self.settings.fallback == EffectFallback::Image).then_some(path)
    }

    /// The fallback image the settings and the caps want, if any, the file
    /// at its path ([`Self::fallback_path`]) being `file`.
    fn fallback_wanted(&self, file: Option<background::FileStamp>) -> Option<FallbackKey> {
        let path = self.fallback_path()?;
        let format = self.format.as_ref()?;
        Some(FallbackKey {
            path: path.clone(),
            file,
            width: format.width,
            height: format.height,
            colour: *format.colour.as_ref().unwrap_or(&format.black),
        })
    }

    /// Takes the backend away ([`ObcEffect::revoke_backend`]): the factory
    /// is cleared and a new request starts, so the worker drops the
    /// instance; the effect (unless `none`) and every effective stage fail
    /// at once with [`NO_BACKEND`], so the next frame decided follows
    /// `on-failure`. False (nothing done) without a factory.
    fn revoke(&mut self) -> bool {
        if self.factory.is_none() {
            return false;
        }
        self.factory = None;
        self.revoked = true;
        self.revocations += 1;
        self.generation += 1;
        self.epoch += 1;
        self.loaded_kind = None;
        for s in self.stage_ids() {
            if self.effective(s.as_str()) {
                self.stage_mut(&s).status = EffectStatus::Failed(NO_BACKEND.into());
            }
        }
        if self.settings.kind != EffectKind::None {
            self.status = EffectStatus::Failed(NO_BACKEND.into());
        }
        true
    }

    /// Sets `factory`; after a revocation, starts a new request for it
    /// (the effect and the stages loading again) and returns true. The
    /// stage failures the revocation caused (no backend) are forgotten;
    /// the settings applied meanwhile were checked without a factory, so
    /// what `factory` refuses of them (`refused`, [`refusals`]) fails: a
    /// refused stage is dropped as a failed one, a refused background
    /// effect fails its loads ([`Control::refused`]).
    fn rearm(&mut self, factory: Arc<dyn BackendFactory>, refused: Refusals) -> bool {
        self.factory = Some(factory);
        if !self.revoked {
            return false;
        }
        self.revoked = false;
        for (_, ctl) in &mut self.stages {
            if ctl.failure.as_deref() == Some(NO_BACKEND) {
                ctl.failure = None;
            }
        }
        for (stage, reason) in refused.stages {
            self.record_stage_failure(&stage, None, reason, false);
        }
        self.refused = refused
            .effect
            .filter(|_| self.target.kind != EffectKind::None);
        self.generation += 1;
        self.epoch += 1;
        self.status = self.pending_status();
        self.set_pending_stage_statuses();
        true
    }

    fn report(&self) -> Report {
        Report {
            fallback_error: self.fallback.error.clone(),
            background_error: self.video.error.clone(),
            kind: self.settings.kind,
            status: self.status.clone(),
            stages: self
                .stages
                .iter()
                .map(|(s, c)| (s.clone(), c.status.clone()))
                .collect(),
        }
    }
}

/// What a backend factory refuses of settings checked without it: each
/// listed stage's parameters, and the background effect's.
#[derive(Debug, Default, PartialEq)]
struct Refusals {
    stages: Vec<(StageId, String)>,
    effect: Option<String>,
}

/// The parameters of `settings` that `factory` refuses
/// (`BackendFactory::check_params`), checked as `ObcEffect::apply` checks
/// them.
fn refusals(settings: &EffectSettings, factory: &dyn BackendFactory) -> Refusals {
    Refusals {
        stages: settings
            .stages
            .iter()
            .filter_map(|s| {
                factory
                    .check_params(&s.id, &s.params, &s.live)
                    .err()
                    .map(|why| (s.id.clone(), why))
            })
            .collect(),
        effect: if settings.background_params.is_empty() {
            None
        } else {
            factory
                .check_params(
                    &StageId::BACKGROUND,
                    &settings.background_params,
                    &obc_backend::Params::new(),
                )
                .err()
        },
    }
}

/// The effect `Control::loaded_kind` takes when the worker publishes what a
/// job left loaded (`loaded`): `None` when the backend was revoked after the
/// job was built (`revocation`, its count then), so an instance it created
/// or kept never processes a frame, even with a new factory set before the
/// next job drops it.
fn published_kind(c: &Control, revocation: u64, loaded: Option<EffectKind>) -> Option<EffectKind> {
    if revocation == c.revocations {
        loaded
    } else {
        None
    }
}

/// What the status message carries.
#[derive(Debug, Clone, PartialEq)]
struct Report {
    kind: EffectKind,
    status: EffectStatus,
    /// The status of every stage the settings have listed, in order.
    stages: Vec<(StageId, EffectStatus)>,
    /// Why the fallback image cannot be shown ([`FallbackCtl::error`]).
    fallback_error: Option<String>,
    /// Why the video background does not play ([`VideoCtl::error`]).
    background_error: Option<String>,
}

impl Report {
    /// `Some(now)` when `now` differs from `self`: a message to post. A
    /// stage newly listed, and off, changes nothing.
    fn changed(&self, now: Report) -> Option<Report> {
        let same = (
            self.kind,
            &self.status,
            &self.fallback_error,
            &self.background_error,
        ) == (
            now.kind,
            &now.status,
            &now.fallback_error,
            &now.background_error,
        ) && self
            .stages
            .iter()
            .chain(&now.stages)
            .all(|(s, _)| self.stage(s.as_str()) == now.stage(s.as_str()));
        (!same).then_some(now)
    }

    fn stage(&self, stage: &str) -> &EffectStatus {
        self.stages
            .iter()
            .find(|(s, _)| s == stage)
            .map_or(&OFF, |(_, st)| st)
    }

    /// `denoise loading, relight off` (the stages, for logs).
    fn stages_text(&self) -> String {
        self.stages
            .iter()
            .map(|(s, st)| format!("{s} {st}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Default)]
struct Shared {
    control: Mutex<Control>,
    wake: Condvar,
    engine: Mutex<Engine>,
    processed: AtomicU64,
    black: AtomicU64,
    passthrough: AtomicU64,
    cuda_active: AtomicBool,
    gpu_ms: Mutex<VecDeque<f32>>,
    /// The counters of every stage that has run, in the order it first ran.
    stage_figures: Mutex<Vec<(StageId, StageFigures)>>,
    /// The status values the backend reported after the last processed
    /// frame whose figures were read (see "Timings" in the module docs).
    stage_values: Mutex<Vec<StageValue>>,
    /// The video background's source. Locked before `control`, never
    /// while `engine` is held.
    video: Mutex<VideoHolder>,
    /// A video source runs (`VideoHolder::source` is some), for the
    /// statistics: read without waiting for a start or a stop.
    video_running: AtomicBool,
    /// Mirrors `VideoCtl::refused`, so the streaming thread clears a
    /// refusal without taking the control lock on every frame.
    video_refused: AtomicBool,
}

/// The counters of one stage.
#[derive(Default)]
struct StageFigures {
    /// Processed frames the stage ran on.
    frames: u64,
    /// The stage's GPU times over its last frames.
    ms: VecDeque<f32>,
}

/// Appends `v` to a window of at most [`EffectStats::WINDOW`] values.
fn push_to(w: &mut VecDeque<f32>, v: f32) {
    if w.len() == EffectStats::WINDOW {
        w.pop_front();
    }
    w.push_back(v);
}

/// Appends `v` to a window of at most [`EffectStats::WINDOW`] values.
fn push_window(w: &Mutex<VecDeque<f32>>, v: f32) {
    push_to(&mut lock(w), v);
}

/// The element's CUDA objects, created on the streaming thread when an
/// effect first needs them and released when dropped: in `stop`, after the
/// backend is unloaded, or with the element if it is finalised without
/// stopping.
struct GpuResources {
    /// A reference to the input's `GstCudaContext`.
    context: *mut c_void,
    cu_ctx: CuContext,
    /// The element's own `GstCudaStream`.
    stream: *mut c_void,
    cu_stream: CuStream,
    /// The output pool for the negotiated caps (rebuilt when they change).
    pool: Option<gst::BufferPool>,
    /// Destroyed in `drop`, with the context current.
    start: ManuallyDrop<Event>,
    end: ManuallyDrop<Event>,
    /// The fallback image uploaded, with the serial of its decode; freed
    /// in `drop`, with the context current.
    fallback: Option<(u64, obc_gpu::DeviceNv12Buffer)>,
}

// SAFETY: the raw pointers are a GStreamer CUDA context and stream
// reference owned by this struct; both are GObject/boxed objects usable from
// any thread, and every CUDA call on them pushes the context first.
unsafe impl Send for GpuResources {}

impl GpuResources {
    fn gpu_context(&self) -> GpuContext {
        GpuContext {
            context: self.cu_ctx,
            stream: self.cu_stream,
        }
    }
}

impl Drop for GpuResources {
    /// Turns the pool off and destroys the events with the context current,
    /// then releases the stream and, last, the context reference (the
    /// context is destroyed with its last reference, ADR-0006). Runs once:
    /// `stop` releases by dropping, and a finalised element drops whatever
    /// `stop` did not take.
    fn drop(&mut self) {
        // SAFETY: the context reference is still ours (released below).
        match unsafe { ContextGuard::push(self.cu_ctx) } {
            Ok(guard) => {
                if let Some(pool) = self.pool.take() {
                    let _ = pool.set_active(false);
                }
                drop(self.fallback.take());
                // SAFETY: created in `ensure_gpu`, dropped only here, once.
                unsafe {
                    ManuallyDrop::drop(&mut self.start);
                    ManuallyDrop::drop(&mut self.end);
                }
                drop(guard);
            }
            Err(e) => {
                // Destroying the events in no (or another) context could act
                // on the wrong one: leak them, they die with the context.
                gst::error!(
                    CAT,
                    "cannot push the CUDA context to release the element's events, leaking \
                     them: {e}"
                );
                if let Some(pool) = self.pool.take() {
                    let _ = pool.set_active(false);
                }
                // Freed with the context, like the events.
                if let Some((_, buffer)) = self.fallback.take() {
                    std::mem::forget(buffer);
                }
            }
        }
        if let Ok(lib) = cuda::gst_cuda() {
            // SAFETY: our own stream and context references, released once,
            // the context last. `gst_cuda_stream_unref` pushes the context
            // itself.
            unsafe {
                (lib.gst_cuda_stream_unref)(self.stream);
                gst::ffi::gst_object_unref(self.context.cast());
            }
        }
    }
}

/// Runs a closure when dropped unless disarmed: the cleanup of every early
/// return of a function.
struct OnExit<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> OnExit<F> {
    fn new(f: F) -> Self {
        OnExit(Some(f))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl<F: FnOnce()> Drop for OnExit<F> {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

/// Leaks a backend instance that cannot be unloaded because its CUDA
/// context could not be pushed: unloading (or dropping, which unloads)
/// without it would make the SDK's CUDA runtime free its objects in no
/// context or in another one, and could create the primary context
/// (ADR-0006). Its memory is reclaimed when the context is destroyed.
fn leak_backend(instance: Box<dyn InferenceBackend>, why: &str) {
    gst::error!(
        CAT,
        "cannot push the CUDA context to unload the backend ({why}); leaking it instead of \
         unloading it outside its context"
    );
    std::mem::forget(instance);
}

#[derive(Default)]
struct StreamState {
    info: Option<VideoInfo>,
    caps: Option<gst::Caps>,
    format: Option<Format>,
    action: FrameAction,
    /// The request the action was decided for, and its per-frame settings.
    generation: u64,
    /// The effect the frame is decided as: the requested one, or `none`
    /// for a toggle mid-stream (`toggle`).
    kind: EffectKind,
    /// The frame is of a requested effect that loads, decided as `none`
    /// because the room is on screen (see "A toggle mid-stream" in the
    /// module docs): it never waits for the `Engine` lock.
    toggle: bool,
    /// The parameters of each frame: the blur strength and the stages'
    /// live parameters.
    frame: Arc<FrameParams>,
    fallback: EffectFallback,
    /// The fallback image when the action was decided (with
    /// `on-failure=image`, once decoded).
    fallback_frame: Option<Arc<FallbackFrame>>,
    /// A frame of a new source arrived
    /// ([`NEW_SOURCE_FLAG`](super::NEW_SOURCE_FLAG)): reset the backend's
    /// temporal state before the next processed frame.
    reset_pending: bool,
    /// The epoch ([`Control::epoch`]) the action was decided under.
    epoch: u64,
    /// This frame is a repeat that gets the cached output (see "A repeat"
    /// in the module docs): no effect runs on it.
    resend: bool,
    /// `render` processed this frame: its output becomes the cache.
    processed: bool,
    /// The last frame output was the unprocessed picture: the input passed
    /// through or copied, or processed by the stages alone. False at a
    /// start, after a frame that went out black or processed by an effect,
    /// and on new caps, a flush, a stop and a new source (see "A toggle
    /// mid-stream" in the module docs).
    raw_on_screen: bool,
    /// A toggle frame found the effect loaded (the worker installed it
    /// after the frame was decided, and has not published it yet) and ran
    /// it: the generation and epoch it was decided under. While the
    /// control's are the same the effect counts as loaded, so the frames
    /// after it are not black until the worker publishes it.
    toggle_found: Option<(u64, u64)>,
    /// The last processed output, while it is valid.
    cache: Option<Cached>,
    /// The frame is a fresh camera frame (no repeat): the video background
    /// may advance on it.
    fresh: bool,
    /// The video background's slot when the frame was decided, and whether
    /// a video is wanted at all (replace with a video).
    bg_slot: Option<Arc<Slot>>,
    bg_wanted: bool,
    /// Without a video wanted, while replace is loaded: the requested
    /// target, so that the video stays in force until its background is
    /// the one loaded (see "The video background" in the module docs).
    bg_request: Option<Target>,
    gpu: Option<GpuResources>,
    /// The output pool for system-memory caps (rebuilt when they change),
    /// once a frame needs it.
    host_pool: Option<gst::BufferPool>,
}

/// The last processed output buffer, kept to answer a repeat, and what it
/// was made under.
struct Cached {
    /// A reference to the very buffer pushed downstream. Holding it keeps
    /// its memory shared, so a downstream map for writing copies the
    /// memory first and never writes this one.
    buffer: gst::Buffer,
    generation: u64,
    epoch: u64,
    /// The per-frame parameters the frame was processed with.
    frame: Arc<FrameParams>,
    caps: gst::Caps,
}

#[derive(Default)]
pub struct ObcEffect {
    shared: Arc<Shared>,
    stream: Mutex<StreamState>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

#[glib::object_subclass]
impl ObjectSubclass for ObcEffect {
    const NAME: &'static str = "GstObcEffect";
    type Type = super::ObcEffect;
    type ParentType = gst_base::BaseTransform;
}

/// Posts the status message (never with a lock held: a bus sync handler
/// may call back into the element).
fn post_status(element: &super::ObcEffect, r: &Report) {
    let mut b = gst::Structure::builder(STATUS_MESSAGE)
        .field("effect", r.kind.as_str())
        .field("status", r.status.as_str())
        .field("reason", r.status.reason().unwrap_or(""))
        .field(
            "fallback-image-error",
            r.fallback_error.as_deref().unwrap_or(""),
        )
        .field(
            "background-error",
            r.background_error.as_deref().unwrap_or(""),
        );
    for (stage, status) in &r.stages {
        b = b.field(stage.as_str(), status.as_str()).field(
            format!("{stage}-reason").as_str(),
            status.reason().unwrap_or(""),
        );
    }
    let s = b.build();
    let _ = element.post_message(gst::message::Element::builder(s).src(element).build());
}

impl ObcEffect {
    /// Sets the backend factory; after a revocation `Some` re-arms the
    /// element ([`Control::rearm`]), the settings applied meanwhile checked
    /// with it first (with the `Control` lock released, as `apply` checks).
    pub(super) fn set_backend_factory(&self, factory: Option<Arc<dyn BackendFactory>>) {
        let Some(f) = factory else {
            lock(&self.shared.control).factory = None;
            return;
        };
        let posted = loop {
            let checked = {
                let c = lock(&self.shared.control);
                c.revoked.then(|| c.settings.clone())
            };
            let refused = checked
                .as_ref()
                .map(|s| refusals(s, f.as_ref()))
                .unwrap_or_default();
            let mut c = lock(&self.shared.control);
            // Settings applied, or a revocation, since the check: again.
            if c.revoked.then_some(&c.settings) != checked.as_ref() {
                continue;
            }
            let before = c.report();
            if c.rearm(Arc::clone(&f), refused) {
                self.shared.wake.notify_all();
            }
            break before.changed(c.report());
        };
        if let Some(r) = posted {
            gst::info!(
                CAT,
                imp = self,
                "backend restored: effect {}: {}; stages: {}",
                r.kind,
                r.status,
                r.stages_text()
            );
            post_status(&self.obj(), &r);
        }
    }

    /// Takes the backend away at once (see [`Control::revoke`]). Takes the
    /// `Control` lock only: never waits for a load the worker runs.
    pub(super) fn revoke_backend(&self) {
        let posted = {
            let mut c = lock(&self.shared.control);
            let before = c.report();
            if c.revoke() {
                self.shared.wake.notify_all();
            }
            before.changed(c.report())
        };
        if let Some(r) = posted {
            gst::warning!(
                CAT,
                imp = self,
                "backend revoked: effect {}: {}; stages: {}",
                r.kind,
                r.status,
                r.stages_text()
            );
            post_status(&self.obj(), &r);
        }
    }

    pub(super) fn backend_factory(&self) -> Option<Arc<dyn BackendFactory>> {
        lock(&self.shared.control).factory.clone()
    }

    pub(super) fn settings(&self) -> EffectSettings {
        lock(&self.shared.control).settings.clone()
    }

    pub(super) fn status(&self) -> EffectStatus {
        lock(&self.shared.control).status.clone()
    }

    pub(super) fn stage_status(&self, stage: &str) -> EffectStatus {
        lock(&self.shared.control).stage_status(stage).clone()
    }

    pub(super) fn stats(&self) -> EffectStats {
        let (
            effect,
            status,
            stage_statuses,
            last_load_ms,
            last_load_reused,
            loads,
            fallback_error,
            background_error,
        ) = {
            let c = lock(&self.shared.control);
            (
                c.settings.kind,
                c.status.clone(),
                c.stages
                    .iter()
                    .map(|(s, ctl)| (s.clone(), ctl.status.clone()))
                    .collect::<Vec<_>>(),
                c.last_load_ms,
                c.last_load_reused,
                c.loads,
                c.fallback.error.clone(),
                c.video.error.clone(),
            )
        };
        let background_video = self.shared.video_running.load(Ordering::SeqCst);
        // Only the values of what is ready: a stage of the list that is,
        // or one of the effect's chain while the effect is.
        let stage_values = lock(&self.shared.stage_values)
            .iter()
            .filter(
                |v| match stage_statuses.iter().find(|(s, _)| *s == v.stage) {
                    Some((_, st)) => *st == EffectStatus::Ready,
                    None => status == EffectStatus::Ready,
                },
            )
            .cloned()
            .collect();
        let gpu_window: Vec<f32> = lock(&self.shared.gpu_ms).iter().copied().collect();
        let stages = {
            let figures = lock(&self.shared.stage_figures);
            stage_statuses
                .into_iter()
                .map(|(id, status)| {
                    let f = figures.iter().find(|(s, _)| *s == id).map(|(_, f)| f);
                    let window: Vec<f32> = f
                        .map(|f| f.ms.iter().copied().collect())
                        .unwrap_or_default();
                    StageStats {
                        status,
                        frames: f.map_or(0, |f| f.frames),
                        ms_p50: percentile(&window, 50.0),
                        ms_p95: percentile(&window, 95.0),
                        id,
                    }
                })
                .collect()
        };
        let s = &self.shared;
        EffectStats {
            effect,
            status,
            stages,
            frames_processed: s.processed.load(Ordering::Relaxed),
            frames_black: s.black.load(Ordering::Relaxed),
            frames_passthrough: s.passthrough.load(Ordering::Relaxed),
            gpu_ms_p50: percentile(&gpu_window, 50.0),
            gpu_ms_p95: percentile(&gpu_window, 95.0),
            last_load_ms,
            last_load_reused,
            loads,
            cuda_active: s.cuda_active.load(Ordering::Relaxed),
            fallback_error,
            background_error,
            background_video,
            stage_values,
        }
    }

    /// Applies new settings. A change of what has to be loaded (the effect,
    /// the image of replace, a stage request: on or off, or its load-time
    /// parameters) is a new request for the worker; `retry` makes one after
    /// a failure (of the effect or of a stage) even when nothing changed. A
    /// stage's recorded failure is forgotten when its request changes or on
    /// a retry. The effect's status turns
    /// loading (off for `none`) when the effect changes or had failed; a
    /// change of a stage request alone leaves it as it is. Likewise a ready
    /// stage whose own request is unchanged stays ready: the switch keeps
    /// it loaded, and only the stage that changed shows loading (its model
    /// load, or a new load-time parameter). A change of the per-frame
    /// parameters alone (blur strength, the stages' live parameters) is
    /// never a retry: they load nothing, and a slider moved after
    /// a failure would otherwise reload what failed on every step.
    /// [`Self::apply`] of settings checked with the element's factory
    /// (`ObcEffect::apply`): a refusal of the background effect's
    /// parameters by the last re-arm ([`Control::refused`]) is over, and
    /// the retry loads the effect again.
    pub(super) fn apply_checked(&self, new: EffectSettings) {
        lock(&self.shared.control).refused = None;
        self.apply(new, true);
    }

    /// Sets the video of replace (an absolute path, or none) and applies
    /// the settings again with it: replace without an image loads black as
    /// its still then, and the source starts once replace is loaded (see
    /// "The video background" in the module docs).
    pub(super) fn set_background_video(&self, path: Option<PathBuf>) -> Result<(), String> {
        if let Some(p) = &path
            && !p.is_absolute()
        {
            return Err(format!(
                "the background video path {} is not absolute",
                p.display()
            ));
        }
        let settings = {
            let mut c = lock(&self.shared.control);
            if c.video.path == path {
                return Ok(());
            }
            c.video.path = path;
            c.settings.clone()
        };
        self.apply(settings, true);
        Ok(())
    }

    pub(super) fn background_video(&self) -> Option<PathBuf> {
        lock(&self.shared.control).video.path.clone()
    }

    /// Whether a video background is set.
    pub(super) fn has_background_video(&self) -> bool {
        lock(&self.shared.control).video.path.is_some()
    }

    pub(super) fn apply(&self, new: EffectSettings, retry: bool) {
        let posted = {
            let mut c = lock(&self.shared.control);
            c.epoch += 1;
            let retry = retry && !c.settings.changes_frame_params_only(&new);
            let before = c.report();
            let target = new.target_with_video(c.video.path.is_some());
            let effect_failed = matches!(c.status, EffectStatus::Failed(_));
            let stage_failed = c.any_stage_failed();
            let effect_changed =
                (target.kind, &target.background) != (c.target.kind, &c.target.background);
            for st in &new.stages {
                c.stage_mut(&st.id);
            }
            c.frame = Arc::new(new.frame_params());
            c.settings = new;
            if target != c.target || (retry && (effect_failed || stage_failed)) {
                let old = c.target.clone();
                for (s, ctl) in &mut c.stages {
                    if retry || target.stage_request(s.as_str()) != old.stage_request(s.as_str()) {
                        ctl.failure = None;
                    }
                }
                let prev = std::mem::replace(&mut c.target, target);
                c.generation += 1;
                if effect_changed || effect_failed {
                    c.status = c.pending_status();
                }
                c.set_switch_stage_statuses(Some(&prev));
                self.shared.wake.notify_all();
            }
            before.changed(c.report())
        };
        if let Some(r) = posted {
            gst::debug!(
                CAT,
                imp = self,
                "effect {}: {}; stages: {}",
                r.kind,
                r.status,
                r.stages_text()
            );
            post_status(&self.obj(), &r);
        }
        self.refresh_fallback();
        refresh_video(&self.shared, &self.obj().downgrade());
    }

    /// Starts decoding the fallback image the settings and caps now want
    /// (see "The fallback image" in the module docs), forgetting the
    /// previous one, when that is another; posts when its report changes.
    fn refresh_fallback(&self) {
        // The file at the path is stat'ed with the control lock released:
        // `stat` opens nothing, but it can hang on a stalled network or
        // FUSE mount, and the streaming thread takes the lock every frame.
        let path = lock(&self.shared.control).fallback_path().cloned();
        let file = path.as_deref().and_then(background::FileStamp::of);
        let (start, posted) = {
            let mut c = lock(&self.shared.control);
            // Another path is wanted now (new settings or caps): the
            // refresh that follows them stats it.
            if c.fallback_path() != path.as_ref() {
                return;
            }
            let want = c.fallback_wanted(file);
            if want == c.fallback.key {
                return;
            }
            let before = c.report();
            c.epoch += 1;
            c.fallback.serial += 1;
            c.fallback.key = want.clone();
            c.fallback.frame = None;
            c.fallback.error = None;
            (
                want.map(|k| (k, c.fallback.serial)),
                before.changed(c.report()),
            )
        };
        if let Some(r) = posted {
            post_status(&self.obj(), &r);
        }
        let Some((key, serial)) = start else {
            return;
        };
        let shared = Arc::clone(&self.shared);
        let element = self.obj().downgrade();
        let k = key.clone();
        let spawned = std::thread::Builder::new()
            .name("obceffect-image".into())
            .spawn(move || {
                let res = background::decode_yuv420(&k.path, k.width, k.height, k.colour);
                let frame = res.map(|img| FallbackFrame {
                    serial,
                    width: img.width,
                    height: img.height,
                    nv12: img.nv12_packed(),
                });
                fallback_decoded(&shared, &element, serial, &k.path, frame);
            });
        if let Err(e) = spawned {
            let why = format!("no thread to decode it: {e}");
            fallback_decoded(
                &self.shared,
                &self.obj().downgrade(),
                serial,
                &key.path,
                Err(why),
            );
        }
    }

    /// The fallback image of decode `serial` could not be put into a frame
    /// (`why`: uploaded, or copied in system memory): black in its place
    /// from now on, reported (unless superseded).
    fn fallback_upload_failed(&self, serial: u64, why: String) {
        let posted = {
            let mut c = lock(&self.shared.control);
            if c.fallback.serial != serial || c.fallback.frame.is_none() {
                return;
            }
            let before = c.report();
            c.epoch += 1;
            c.fallback.frame = None;
            c.fallback.error = c
                .fallback
                .key
                .as_ref()
                .map(|k| fallback_unreadable(&k.path, &why));
            before.changed(c.report())
        };
        if let Some(r) = posted {
            gst::warning!(
                CAT,
                imp = self,
                "{}",
                r.fallback_error.as_deref().unwrap_or("")
            );
            post_status(&self.obj(), &r);
        }
    }

    /// The backend refused a video background frame (`why`): the background
    /// in force stays; reported as the video's error (once per reason).
    fn video_frame_refused(&self, why: String) {
        let posted = {
            let mut c = lock(&self.shared.control);
            let Some(path) = c.video.path.clone() else {
                return;
            };
            let before = c.report();
            c.video.error = Some(video_unplayable(
                &path,
                &format!("the backend refused a frame: {why}"),
            ));
            c.video.refused = true;
            self.shared.video_refused.store(true, Ordering::SeqCst);
            before.changed(c.report())
        };
        if let Some(r) = posted {
            gst::warning!(
                CAT,
                imp = self,
                "{}",
                r.background_error.as_deref().unwrap_or("")
            );
            post_status(&self.obj(), &r);
        }
    }

    /// A video background frame was given after a refusal: the refusal is
    /// over (an error of another kind stays).
    fn video_frame_given(&self) {
        if !self.shared.video_refused.swap(false, Ordering::SeqCst) {
            return;
        }
        let posted = {
            let mut c = lock(&self.shared.control);
            if !c.video.refused {
                return;
            }
            let before = c.report();
            c.video.refused = false;
            c.video.error = None;
            before.changed(c.report())
        };
        if let Some(r) = posted {
            post_status(&self.obj(), &r);
        }
    }

    /// Fills `dst` with what hides the input: the fallback image `image`
    /// when there is one of `dst`'s size (uploaded into `gpu` once per
    /// decode), else black of `black`; on `stream`.
    fn fill_hidden(
        &self,
        gpu: &mut GpuResources,
        image: Option<&Arc<FallbackFrame>>,
        dst: &DeviceNv12,
        black: ColourSpec,
        stream: CuStream,
    ) -> Result<(), String> {
        if let Some(img) = image.filter(|i| (i.width, i.height) == (dst.width, dst.height)) {
            if gpu.fallback.as_ref().is_none_or(|(s, _)| *s != img.serial) {
                // The old frame first (the context is current).
                gpu.fallback = None;
                let up = obc_gpu::DeviceNv12Buffer::alloc(img.width, img.height)
                    .and_then(|b| b.upload_packed(&img.nv12).map(|()| b));
                match up {
                    Ok(b) => gpu.fallback = Some((img.serial, b)),
                    Err(e) => {
                        self.fallback_upload_failed(img.serial, format!("cannot upload it: {e}"))
                    }
                }
            }
            if let Some((_, b)) = gpu.fallback.as_ref().filter(|(s, _)| *s == img.serial) {
                // SAFETY: both frames are live allocations of our context,
                // of the same size; the image is only read.
                return unsafe { obc_gpu::copy_nv12_async(&b.frame(), dst, stream) }
                    .map_err(|e| e.to_string());
            }
        }
        // SAFETY: `dst` is our mapped output frame; `stream` is ours.
        unsafe { obc_gpu::fill_black_nv12(dst, black, stream) }.map_err(|e| e.to_string())
    }

    /// Marks request `generation` as failed (unless superseded), and posts.
    /// A requested stage that has not failed itself shows the failure too:
    /// it runs in the failed chain.
    fn fail(&self, generation: u64, reason: String) {
        let posted = {
            let mut c = lock(&self.shared.control);
            c.epoch += 1;
            if c.generation != generation || matches!(c.status, EffectStatus::Failed(_)) {
                None
            } else {
                let before = c.report();
                for s in c.stage_ids() {
                    if c.effective(s.as_str()) {
                        c.stage_mut(&s).status = EffectStatus::Failed(reason.clone());
                    }
                }
                c.status = EffectStatus::Failed(reason);
                before.changed(c.report())
            }
        };
        if let Some(r) = posted {
            gst::warning!(CAT, imp = self, "effect {}: {}", r.kind, r.status);
            post_status(&self.obj(), &r);
        }
    }

    /// Records a failure of `stage` (of request `generation`, unless
    /// superseded; with `None`, of whatever request still asks for the
    /// stage) and starts a new request, so the worker drops the stage (it
    /// finds the rest loaded, or unloads for `none` without another stage);
    /// posts. The effect's status is not touched. A failed effect starts no
    /// new request: it processes nothing until one comes (a retry or a
    /// change), which reconciles.
    fn fail_stage(&self, stage: &StageId, generation: Option<u64>, reason: String) {
        let posted = {
            let mut c = lock(&self.shared.control);
            c.epoch += 1;
            let before = c.report();
            let reconcile = !matches!(c.status, EffectStatus::Failed(_));
            if c.record_stage_failure(stage, generation, reason, reconcile) && reconcile {
                self.shared.wake.notify_all();
            }
            before.changed(c.report())
        };
        if let Some(r) = posted {
            gst::warning!(CAT, imp = self, "{stage}: {}", r.stage(stage.as_str()));
            post_status(&self.obj(), &r);
        }
    }

    /// Starts a new request so the worker loads `stages` again: the
    /// backend dropped them on a temporal reset (not a failure), so the
    /// chain loaded is without them. Each one still requested and not
    /// failed shows loading until the worker has it ready again; posts.
    fn rebuild_stages(&self, stages: &[StageId]) {
        let posted = {
            let mut c = lock(&self.shared.control);
            c.epoch += 1;
            let before = c.report();
            let mut wanted = false;
            for s in stages {
                if c.effective(s.as_str()) {
                    c.stage_mut(s).status = EffectStatus::Loading;
                    wanted = true;
                }
            }
            if wanted {
                c.generation += 1;
                self.shared.wake.notify_all();
            }
            before.changed(c.report())
        };
        if let Some(r) = posted {
            gst::debug!(
                CAT,
                imp = self,
                "a new source: loading again {stages:?}; stages: {}",
                r.stages_text()
            );
            post_status(&self.obj(), &r);
        }
    }

    /// Creates the CUDA objects from the first input memory: a reference to
    /// its context, a stream, two timing events; and the output pool for
    /// the negotiated caps if there is none.
    fn ensure_gpu(&self, st: &mut StreamState, inbuf: &gst::BufferRef) -> Result<(), String> {
        let lib = cuda::gst_cuda()?;
        let context = input_context(lib, inbuf)?;
        if st.gpu.is_none() {
            // SAFETY: `context` is the live GstCudaContext of the input
            // memory; we take our own reference for the lifetime of
            // `GpuResources`.
            let cu_ctx = unsafe {
                gst::ffi::gst_object_ref(context.cast());
                (lib.gst_cuda_context_get_handle)(context)
            };
            let release_ctx = || unsafe { gst::ffi::gst_object_unref(context.cast()) };
            // SAFETY: as above; the stream is created in that context.
            let stream = unsafe { (lib.gst_cuda_stream_new)(context) };
            if stream.is_null() || cu_ctx.is_null() {
                release_ctx();
                return Err("could not create a CUDA stream in the stream's context".into());
            }
            // SAFETY: `stream` is the live GstCudaStream just created.
            let cu_stream = unsafe { (lib.gst_cuda_stream_get_handle)(stream) };
            // SAFETY: `cu_ctx` is the live context we hold a reference to.
            let events = unsafe { ContextGuard::push(cu_ctx) }
                .and_then(|_g| Ok((Event::new()?, Event::new()?)));
            let (start, end) = match events {
                Ok(e) => e,
                Err(e) => {
                    // SAFETY: our own stream and context references.
                    unsafe { (lib.gst_cuda_stream_unref)(stream) };
                    release_ctx();
                    return Err(format!("CUDA timing events: {e}"));
                }
            };
            st.gpu = Some(GpuResources {
                context,
                cu_ctx,
                stream,
                cu_stream,
                pool: None,
                start: ManuallyDrop::new(start),
                end: ManuallyDrop::new(end),
                fallback: None,
            });
            self.shared.cuda_active.store(true, Ordering::Relaxed);
            let gpu = st.gpu.as_ref().map(GpuResources::gpu_context);
            // SAFETY: `context` is live, and ours for as long as the
            // resources are; the GstContext takes its own reference.
            let share = unsafe { video_background::share_cuda_context(context) };
            let mut c = lock(&self.shared.control);
            c.gpu = gpu;
            c.video.cuda = Some(share);
            self.shared.wake.notify_all();
            gst::info!(
                CAT,
                imp = self,
                "CUDA resources created on the input's context"
            );
        }
        let gpu = st.gpu.as_mut().expect("created above");
        if gpu.context != context {
            return Err(
                "an input frame is in another CUDA context than the stream's first frame".into(),
            );
        }
        if gpu.pool.is_none() {
            let (Some(caps), Some(info)) = (&st.caps, &st.info) else {
                return Err("no caps negotiated".into());
            };
            gpu.pool = Some(make_pool(lib, gpu, caps, info)?);
        }
        Ok(())
    }

    /// Creates the output pool for system-memory caps if there is none: all
    /// a frame the element writes needs there.
    fn ensure_host(&self, st: &mut StreamState) -> Result<(), String> {
        if st.host_pool.is_none() {
            let (Some(caps), Some(info)) = (&st.caps, &st.info) else {
                return Err("no caps negotiated".into());
            };
            st.host_pool = Some(make_host_pool(caps, info)?);
        }
        Ok(())
    }

    /// Releases the backend, then the CUDA objects (see the module docs),
    /// or on system memory the backend and the output pool. The instance is
    /// released under the memory it was created for (`Engine::kind`), not
    /// the current frames': one of host frames with no CUDA context
    /// current, one of CUDA frames with its context pushed (leaked if that
    /// fails). Runs after the worker has been joined.
    fn release_gpu(&self) {
        lock(&self.shared.stage_values).clear();
        let (gpu, cached, host_pool) = {
            let mut st = lock(&self.stream);
            (st.gpu.take(), st.cache.take(), st.host_pool.take())
        };
        // The cached output is memory of our pool and context: gone first.
        drop(cached);
        if let Some(pool) = host_pool {
            let _ = pool.set_active(false);
        }
        let instance = {
            let mut e = lock(&self.shared.engine);
            e.loaded = None;
            // The video frames go before the context reference.
            e.bg = BgState::default();
            // An instance always has its kind; CUDA, the stricter, if not.
            let kind = e.kind.take().unwrap_or(FrameKind::Cuda);
            e.instance.take().map(|i| (i, kind))
        };
        let unloaded = instance.is_some();
        match instance {
            None => {}
            Some((mut instance, FrameKind::Host)) => {
                instance.unload();
                drop(instance);
            }
            Some((mut instance, FrameKind::Cuda)) => match &gpu {
                Some(gpu) => {
                    // SAFETY: the context reference is still ours.
                    match unsafe { ContextGuard::push(gpu.cu_ctx) } {
                        Ok(guard) => {
                            instance.unload();
                            drop(instance);
                            drop(guard);
                        }
                        Err(err) => leak_backend(instance, &err.to_string()),
                    }
                }
                None => leak_backend(instance, "the effect has no CUDA context"),
            },
        }
        let Some(gpu) = gpu else {
            if unloaded {
                let mut c = lock(&self.shared.control);
                c.loaded_kind = None;
                c.video.live = None;
                c.epoch += 1;
                drop(c);
                gst::info!(CAT, imp = self, "backend unloaded");
            }
            return;
        };
        // The pool, the events, the stream and the context reference.
        drop(gpu);
        self.shared.cuda_active.store(false, Ordering::Relaxed);
        let mut c = lock(&self.shared.control);
        c.gpu = None;
        c.video.cuda = None;
        c.video.live = None;
        c.loaded_kind = None;
        c.epoch += 1;
        gst::info!(
            CAT,
            imp = self,
            "backend unloaded and CUDA resources released"
        );
    }

    fn stop_worker(&self) {
        {
            let mut c = lock(&self.shared.control);
            c.stop = true;
            self.shared.wake.notify_all();
        }
        if let Some(h) = lock(&self.worker).take() {
            let _ = h.join();
        }
    }

    /// The stage times of the frame just processed and synchronised (see
    /// "Timings" in the module docs), if the backend has them. Never waits
    /// for the `Engine` lock: if the worker holds it (a load, a switch or an
    /// unload under way), this frame's figures are skipped.
    ///
    /// The status values the backend reports after that frame come with
    /// them; `None` when the lock was not taken (the values are then left
    /// as they were).
    fn stage_figures(&self) -> (Option<StageTimings>, Option<Vec<StageValue>>) {
        let e = match self.shared.engine.try_lock() {
            Ok(e) => e,
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return (None, None),
        };
        match e.instance.as_ref() {
            Some(i) => (i.timings(), Some(i.status_values())),
            None => (None, Some(Vec::new())),
        }
    }

    /// Fills `out` for `action` and waits for it: on the element's CUDA
    /// stream for CUDA caps, on the CPU for system memory.
    fn render(
        &self,
        st: &mut StreamState,
        inbuf: &gst::Buffer,
        outbuf: &mut gst::BufferRef,
    ) -> Result<(), String> {
        // Whatever this frame becomes, the previous output is no longer the
        // last one.
        st.processed = false;
        st.cache = None;
        let info = st.info.clone().ok_or("no caps negotiated")?;
        let format = st.format.clone().ok_or("no caps negotiated")?;
        // What hides the input: the fallback image with `image`, else black.
        let image = match st.fallback {
            EffectFallback::Image => st.fallback_frame.clone(),
            _ => None,
        };
        if !format.cuda {
            return self.render_host(st, inbuf, outbuf, &info, &format, image.as_ref());
        }
        let lib = cuda::gst_cuda()?;
        // Taken for the frame (the decision below borrows the rest of the
        // stream state), and always put back.
        let mut gpu = st.gpu.take().ok_or("no CUDA resources")?;
        let result = self.render_cuda(
            st,
            (&mut gpu, lib),
            (inbuf, outbuf),
            &info,
            &format,
            image.as_ref(),
        );
        st.gpu = Some(gpu);
        result
    }

    /// [`Self::render`] on CUDA frames, with the element's context pushed.
    fn render_cuda(
        &self,
        st: &mut StreamState,
        (gpu, lib): (&mut GpuResources, &'static cuda::GstCuda),
        (inbuf, outbuf): (&gst::Buffer, &mut gst::BufferRef),
        info: &VideoInfo,
        format: &Format,
        image: Option<&Arc<FallbackFrame>>,
    ) -> Result<(), String> {
        // SAFETY: our context reference is live.
        let _guard = unsafe { ContextGuard::push(gpu.cu_ctx) }.map_err(|e| e.to_string())?;
        // SAFETY: the output buffer is ours, writable, from our CUDA pool.
        let out = unsafe {
            CudaFrame::map(
                outbuf.as_mut_ptr(),
                info,
                gst::ffi::GST_MAP_WRITE | MAP_CUDA,
            )
        }?;
        let stream = gpu.cu_stream;
        // The input stays mapped until the stream has finished reading it.
        let mut input_frame: Option<CudaFrame> = None;
        // Every exit from here waits for the stream before the frames are
        // unmapped (this guard drops before them), so an early return never
        // releases the input, or hands out the output, while work queued on
        // them may still run. The normal path disarms it and waits itself.
        let mut sync_on_exit = OnExit::new(move || {
            // SAFETY: our own stream; the context is still pushed.
            let _ = unsafe { obc_gpu::stream_synchronize(stream) };
        });
        let rendered = {
            let mut px = CudaPixels {
                fx: self,
                lib,
                gpu: &mut *gpu,
                info,
                inbuf,
                dst: out.nv12(),
                input: &mut input_frame,
                src: None,
            };
            self.run_frame(st, format, image, &mut px)?
        };
        sync_on_exit.disarm();
        // SAFETY: our own stream. Waiting here also ends this frame's read
        // of the input (see the module docs for why).
        unsafe { obc_gpu::stream_synchronize(stream) }.map_err(|e| e.to_string())?;
        drop(input_frame);
        drop(out);
        let gpu_ms = if rendered.processed {
            Event::elapsed_ms(&gpu.start, &gpu.end).ok()
        } else {
            None
        };
        self.account(st, rendered, gpu_ms);
        Ok(())
    }

    /// [`Self::render`] on system-memory frames: the CPU fills and copies,
    /// and the backend's host methods. No CUDA call.
    fn render_host(
        &self,
        st: &mut StreamState,
        inbuf: &gst::Buffer,
        outbuf: &mut gst::BufferRef,
        info: &VideoInfo,
        format: &Format,
        image: Option<&Arc<FallbackFrame>>,
    ) -> Result<(), String> {
        let out = gst_video::VideoFrameRef::from_buffer_ref_writable(outbuf, info)
            .map_err(|_| "could not map an output frame for writing")?;
        let dst = host_nv12(&out);
        let rendered = {
            let mut px = HostPixels {
                fx: self,
                _out: out,
                dst,
                inbuf,
                info,
                _input: None,
                src: None,
            };
            self.run_frame(st, format, image, &mut px)?
        };
        // No GPU time on the CPU path.
        self.account(st, rendered, None);
        Ok(())
    }

    /// Decides and writes one output frame through `px`, the pixel work of
    /// the negotiated memory: every rule of the module docs, the same for
    /// CUDA and system memory. Returns what the frame got; the caller waits
    /// for the work, then accounts for it ([`Self::account`]).
    fn run_frame<P: FramePixels>(
        &self,
        st: &mut StreamState,
        format: &Format,
        image: Option<&Arc<FallbackFrame>>,
        px: &mut P,
    ) -> Result<Rendered, String> {
        let mut r = Rendered::default();
        match st.action {
            FrameAction::Passthrough => {
                return Err("internal: passthrough frame in transform".into());
            }
            FrameAction::Black => px.fill_hidden(image, format.black)?,
            FrameAction::Process => {
                px.map_input()?;
                let params = Arc::clone(&st.frame);
                // The stages alone hide nothing: an effect frame needs a
                // chain with the effect. A frame of `none` needs a chain
                // with something in it (a stage, or the previous effect
                // while switching).
                let mut alone = st.kind == EffectKind::None;
                // The stages that failed on this frame (the backend has
                // dropped them), whatever the frame then got.
                let mut stage_failures: Vec<(StageId, String)> = Vec::new();
                // The stages the temporal reset dropped, to load again.
                let mut rebuild: Vec<StageId> = Vec::new();
                // A toggle frame never waits for the worker (its cold load
                // may hold the lock for as long as it takes): busy, it is
                // the input, as when nothing is loaded.
                let mut held = if st.toggle {
                    match self.shared.engine.try_lock() {
                        Ok(e) => Some(e),
                        Err(std::sync::TryLockError::Poisoned(p)) => Some(p.into_inner()),
                        Err(std::sync::TryLockError::WouldBlock) => None,
                    }
                } else {
                    Some(lock(&self.shared.engine))
                };
                let mut idle = Engine::default();
                // Why the backend refused a video background frame.
                let mut bg_refused: Option<String> = None;
                let mut bg_given = false;
                let Engine {
                    instance,
                    loaded,
                    bg,
                    ..
                } = match held.as_mut() {
                    Some(e) => &mut **e,
                    None => &mut idle,
                };
                // A toggle frame that finds the effect loaded (the worker
                // installed it after the decision) for frames in this
                // frame's memory is an effect frame: the effect runs on it,
                // and its failure is the effect's. A chain loaded for the
                // other memory (new caps mid-toggle) does not promote it: it
                // stays a toggle frame (the input), as `usable` below would
                // not run that chain either.
                let promoted = st.toggle
                    && loaded
                        .as_ref()
                        .is_some_and(|k| k.kind == px.kind() && k.target.kind != EffectKind::None)
                    && instance.as_ref().is_some_and(|i| i.is_loaded());
                if promoted {
                    alone = false;
                }
                // A backend that lost its chain on its own (a module
                // instance unloaded after a panic) runs nothing: as if
                // unloaded since the decision. Nor does a chain loaded for
                // frames in the other memory (new caps the worker has not
                // loaded for yet).
                let usable = loaded.as_ref().is_some_and(|k| {
                    k.kind == px.kind()
                        && (k.target.kind != EffectKind::None || (alone && k.target.has_stages()))
                }) && instance.as_ref().is_some_and(|i| i.is_loaded());
                let result = match (instance.as_mut(), usable) {
                    (Some(instance), true) => 'frame: {
                        // Before the start event: the reset may wait for
                        // the stream, which is not this frame's GPU time.
                        let reset = if std::mem::take(&mut st.reset_pending) {
                            px.reset_temporal(&mut **instance)
                        } else {
                            Ok(Default::default())
                        };
                        // A stage whose state the backend cannot forget
                        // (Maxine's relighting) is dropped by the reset
                        // rather than carry the previous source's person
                        // into this one. The chain in force is without it
                        // from this frame, and a new request below has the
                        // worker load it again, fresh: until it is ready
                        // the frames run without it (under blur or replace
                        // the effect alone, never the input), and no frame
                        // is relit by the instance that saw the previous
                        // source.
                        let reset = reset.map(|dropped| {
                            if let Some(k) = loaded.as_mut() {
                                for s in dropped.as_slice() {
                                    if k.target.wants(s.as_str()) {
                                        k.target = k.target.without(s.as_str());
                                        rebuild.push(s.clone());
                                    }
                                }
                            }
                        });
                        // The stages alone may have gone with it: nothing
                        // left to run (the input, below).
                        if reset.is_ok()
                            && alone
                            && !loaded.as_ref().is_some_and(|k| k.target.has_stages())
                        {
                            break 'frame None;
                        }
                        // The video background, before the frame is
                        // processed: it advances on fresh camera frames only
                        // (a repeat keeps the frame's background, as it
                        // keeps the frame), and a repeat after a load gets
                        // the last frame again (never the still).
                        if reset.is_ok()
                            && !alone
                            && let Some(k) = loaded
                                .as_ref()
                                .filter(|k| k.target.kind == EffectKind::Replace)
                        {
                            match feed_background(st, format, &k.target, bg, &mut **instance, px) {
                                Ok(given) => bg_given = given,
                                Err(why) => bg_refused = Some(why),
                            }
                        }
                        px.start_timing();
                        let mut res = reset.and_then(|()| px.process(&mut **instance, &params));
                        // A stage failure: the backend has dropped the stage
                        // (and any failure of the stages alone drops them
                        // all), so the chain loaded from now on is without
                        // it. An effect is fine without it: the frame runs
                        // again through the chain without it, so it is still
                        // processed (never the input, nor black, for a
                        // healthy effect), as often as stages fail. Without
                        // an effect the frame is the input.
                        while let Err(err) = &res {
                            let Some(k) = loaded.as_mut() else { break };
                            let failed: Vec<StageId> = match stage_of(err) {
                                Some(s) if k.target.wants(s.as_str()) => vec![s],
                                Some(_) => Vec::new(),
                                None if alone => {
                                    k.target.stages.iter().map(|(s, _)| s.clone()).collect()
                                }
                                None => break,
                            };
                            if failed.is_empty() {
                                break;
                            }
                            for s in failed {
                                k.target = k.target.without(s.as_str());
                                let reason = format!("the {s} stage failed on a frame: {err}");
                                stage_failures.push((s, reason));
                            }
                            if alone {
                                break;
                            }
                            res = px.process(&mut **instance, &params);
                        }
                        if res.is_ok()
                            && let Some(k) = loaded.as_ref()
                        {
                            r.ran = k.target.stages.iter().map(|(s, _)| s.clone()).collect();
                            r.effect_ran = k.target.kind != EffectKind::None;
                        }
                        px.end_timing();
                        Some(res)
                    }
                    _ => None,
                };
                drop(held);
                if let Some(why) = bg_refused {
                    self.video_frame_refused(why);
                } else if bg_given {
                    self.video_frame_given();
                }
                if promoted && r.effect_ran {
                    st.toggle_found = Some((st.generation, st.epoch));
                }
                if !rebuild.is_empty() {
                    self.rebuild_stages(&rebuild);
                }
                // The effect failed (alone, or again without the stages):
                // recorded first, so the stage failures below do not
                // supersede it.
                if !alone && let Some(Err(err)) = &result {
                    self.fail(st.generation, format!("processing a frame failed: {err}"));
                }
                for (stage, reason) in &stage_failures {
                    // Recorded whichever request the frame was decided for:
                    // the stage is gone from the loaded chain, and a request
                    // that still asks for it must learn it now (the first
                    // frame after turning the stage on was decided under the
                    // previous request, and the worker has already handled
                    // the new one).
                    self.fail_stage(stage, None, reason.clone());
                }
                match result {
                    Some(Ok(())) => r.processed = true,
                    // The stages alone failed (recorded above): nothing to
                    // hide, the input.
                    Some(Err(_)) if alone => {
                        px.copy_input()?;
                        r.copied = true;
                    }
                    // The effect failed (recorded above).
                    Some(Err(_)) => match st.fallback {
                        EffectFallback::Black | EffectFallback::Image => {
                            px.fill_hidden(image, format.black)?
                        }
                        EffectFallback::None => {
                            px.copy_input()?;
                            r.copied = true;
                        }
                    },
                    // Unloaded since the decision: the input for `none`
                    // (nothing to hide), black for an effect, never the
                    // input.
                    None if alone => {
                        px.copy_input()?;
                        r.copied = true;
                    }
                    None => px.fill_hidden(image, format.black)?,
                }
            }
        }
        Ok(r)
    }

    /// Records what a frame written by [`Self::run_frame`] got, once its
    /// work is done: the cache flag, what is on screen, the counters, the
    /// stage times and `gpu_ms` (the frame's GPU time, if measured).
    fn account(&self, st: &mut StreamState, r: Rendered, gpu_ms: Option<f32>) {
        // Only a processed frame is cached (`generate_output`); black or
        // the input leaves no cache behind.
        st.processed = r.processed;
        // The room stays on screen after the input or the stages alone;
        // black or an effect frame ends it.
        let output = match (r.processed, r.effect_ran, r.copied) {
            (true, true, _) => FrameOutput::Effect,
            (true, false, _) => FrameOutput::Stages,
            (false, _, true) => FrameOutput::Input,
            (false, _, false) => FrameOutput::Hidden,
        };
        st.raw_on_screen = raw_on_screen_after(st.raw_on_screen, ScreenEvent::Output(output));
        if r.processed {
            self.shared.processed.fetch_add(1, Ordering::Relaxed);
            if !r.ran.is_empty() || r.effect_ran {
                let (times, values) = self.stage_figures();
                update_stage_values(&mut lock(&self.shared.stage_values), values, &r);
                let mut figures = lock(&self.shared.stage_figures);
                for id in r.ran {
                    let i = match figures.iter().position(|(s, _)| *s == id) {
                        Some(i) => i,
                        None => {
                            figures.push((id.clone(), StageFigures::default()));
                            figures.len() - 1
                        }
                    };
                    let f = &mut figures[i].1;
                    f.frames += 1;
                    if let Some(ms) = times.as_ref().and_then(|t| t.step_ms(id.as_str()))
                        && ms > 0.0
                    {
                        push_to(&mut f.ms, ms);
                    }
                }
            }
            if let Some(ms) = gpu_ms {
                push_window(&self.shared.gpu_ms, ms);
            }
        } else if r.copied {
            self.shared.passthrough.fetch_add(1, Ordering::Relaxed);
        } else {
            self.shared.black.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Replaces `current` with the status values `read` after the frame `r`,
/// of the chain loaded for it: the stages that ran on it and, when the
/// effect ran, its `background` stage. `None` (the `Engine` lock was not
/// taken, see "Timings" in the module docs) leaves `current` as it was.
fn update_stage_values(current: &mut Vec<StageValue>, read: Option<Vec<StageValue>>, r: &Rendered) {
    let Some(read) = read else { return };
    *current = read
        .into_iter()
        .filter(|v| r.ran.contains(&v.stage) || (r.effect_ran && v.stage == StageId::BACKGROUND))
        .collect();
}

/// What [`ObcEffect::run_frame`] wrote into a frame.
#[derive(Default)]
struct Rendered {
    /// Processed by the backend (an effect, or the stages alone).
    processed: bool,
    /// The processed frame went through an effect (not the stages alone).
    effect_ran: bool,
    /// The input copied into the output (a failed frame with nothing to
    /// hide, or on-failure=none).
    copied: bool,
    /// The stages that ran on the processed frame.
    ran: Vec<StageId>,
}

/// The pixel work of one output frame in the memory of the negotiated caps:
/// [`CudaPixels`] on the element's stream, [`HostPixels`] on the CPU. What
/// a frame gets is decided once, in [`ObcEffect::run_frame`], for both.
trait FramePixels {
    /// The memory of the frames.
    fn kind(&self) -> FrameKind;
    /// Maps the input for reading (a frame to process only).
    fn map_input(&mut self) -> Result<(), String>;
    /// Fills the output with what hides the input: the fallback image
    /// `image` when there is one of the output's size, else black of
    /// `black`.
    fn fill_hidden(
        &mut self,
        image: Option<&Arc<FallbackFrame>>,
        black: ColourSpec,
    ) -> Result<(), String>;
    /// Copies the mapped input into the output.
    fn copy_input(&mut self) -> Result<(), String>;
    /// The backend's temporal reset, for this memory.
    fn reset_temporal(
        &mut self,
        instance: &mut dyn InferenceBackend,
    ) -> Result<StageList, BackendError>;
    /// Marks the start of the backend's work on the frame (its GPU time).
    fn start_timing(&mut self);
    /// The backend's work on the mapped input into the output.
    fn process(
        &mut self,
        instance: &mut dyn InferenceBackend,
        params: &FrameParams,
    ) -> Result<(), BackendError>;
    /// Marks the end of the backend's work on the frame.
    fn end_timing(&mut self);
    /// Gives the backend `sample`, a video background frame of the output's
    /// size in this memory, mapped for the call
    /// (`InferenceBackend::set_background_frame`).
    fn set_background(
        &mut self,
        instance: &mut dyn InferenceBackend,
        sample: &gst::Sample,
    ) -> Result<(), String>;
}

/// The buffer and video layout of a video background frame.
fn sample_frame(sample: &gst::Sample) -> Result<(&gst::BufferRef, VideoInfo), String> {
    let buffer = sample
        .buffer()
        .ok_or("a video background frame has no buffer")?;
    let caps = sample
        .caps()
        .ok_or("a video background frame has no caps")?;
    let info = VideoInfo::from_caps(caps)
        .map_err(|e| format!("a video background frame has caps {caps}: {e}"))?;
    if info.format() != gst_video::VideoFormat::Nv12 {
        return Err(format!(
            "a video background frame is {:?}, not NV12",
            info.format()
        ));
    }
    Ok((buffer, info))
}

/// Gives the backend the video background's newest frame, if it has not
/// had it (see "The video background" in the module docs): on a frame
/// processed with replace loaded (`loaded`, the target in force), before
/// it is processed. Only a fresh camera frame advances to a new frame of
/// the slot; any frame, a repeat too, gives the last frame again after a
/// load put the load-time still back in force. Without a video wanted, a
/// frame in force goes back to the load-time still once the requested
/// background is the loaded one (until then the video stays in force, and
/// advances). A frame is tried once; whether one was given is returned, or
/// why it was refused.
fn feed_background<P: FramePixels>(
    st: &StreamState,
    format: &Format,
    loaded: &Target,
    bg: &mut BgState,
    instance: &mut dyn InferenceBackend,
    px: &mut P,
) -> Result<bool, String> {
    // The request without the video is not loaded yet: the chain in force
    // keeps the video's frames (its own still is the previous background).
    let holding = !st.bg_wanted
        && st
            .bg_request
            .as_ref()
            .is_some_and(|r| !r.same_background(loaded));
    if !st.bg_wanted && !holding {
        bg.last = None;
        if bg.live {
            bg.live = false;
            bg.sent = None;
            instance
                .set_background_frame(None)
                .map_err(|e| e.to_string())?;
        }
        return Ok(false);
    }
    if holding && !bg.live {
        // A load since: its still is the one to show.
        return Ok(false);
    }
    // The running source's newest frame, on a fresh camera frame; else,
    // after a load (the still back in force), the last frame given before.
    let newest = if st.fresh {
        st.bg_slot.as_ref().and_then(|s| s.latest())
    } else {
        None
    };
    let candidate = match newest {
        Some(f) => Some(f),
        None if !bg.live => bg.last.clone(),
        None => None,
    };
    let Some(frame) = candidate else {
        return Ok(false);
    };
    if bg.sent == Some((frame.slot, frame.seq)) {
        return Ok(false);
    }
    // A frame of other caps (made before new caps) is never given.
    let size = frame
        .sample
        .caps()
        .and_then(|c| VideoInfo::from_caps(c).ok())
        .map(|i| (i.width(), i.height()));
    if size != Some((format.width, format.height)) {
        if bg
            .last
            .as_ref()
            .is_some_and(|l| l.seq == frame.seq && l.slot == frame.slot)
        {
            bg.last = None;
        }
        return Ok(false);
    }
    bg.sent = Some((frame.slot, frame.seq));
    px.set_background(instance, &frame.sample)?;
    bg.live = true;
    bg.last = Some(frame);
    Ok(true)
}

/// A CUDA output frame, written on the element's stream (the context is
/// pushed for as long as it lives).
struct CudaPixels<'a> {
    fx: &'a ObcEffect,
    lib: &'static cuda::GstCuda,
    gpu: &'a mut GpuResources,
    info: &'a VideoInfo,
    inbuf: &'a gst::Buffer,
    /// The mapped output frame.
    dst: DeviceNv12,
    /// The input once mapped, owned by the caller so that it stays mapped
    /// until the stream has finished reading it.
    input: &'a mut Option<CudaFrame>,
    src: Option<DeviceNv12>,
}

impl CudaPixels<'_> {
    fn src(&self) -> Result<DeviceNv12, String> {
        self.src
            .ok_or_else(|| "internal: the input is not mapped".to_owned())
    }
}

impl FramePixels for CudaPixels<'_> {
    fn kind(&self) -> FrameKind {
        FrameKind::Cuda
    }

    fn map_input(&mut self) -> Result<(), String> {
        let mem = self.inbuf.peek_memory(0);
        // SAFETY: `ensure_gpu` checked that this is a GstCudaMemory of our
        // context; the sync only waits for its producer.
        unsafe { (self.lib.gst_cuda_memory_sync)(mem.as_mut_ptr().cast()) };
        // SAFETY: read-only CUDA map of the input; it is never written.
        let input = unsafe {
            CudaFrame::map(
                self.inbuf.as_mut_ptr(),
                self.info,
                gst::ffi::GST_MAP_READ | MAP_CUDA,
            )
        }?;
        self.src = Some(input.nv12());
        // Owned by the caller's scope, so it outlives its stream guard.
        *self.input = Some(input);
        Ok(())
    }

    fn fill_hidden(
        &mut self,
        image: Option<&Arc<FallbackFrame>>,
        black: ColourSpec,
    ) -> Result<(), String> {
        let stream = self.gpu.cu_stream;
        self.fx
            .fill_hidden(self.gpu, image, &self.dst, black, stream)
    }

    fn copy_input(&mut self) -> Result<(), String> {
        let src = self.src()?;
        // SAFETY: both are the mapped CUDA frames of this call, in our
        // context, same size; the input is only read.
        unsafe { obc_gpu::copy_nv12_async(&src, &self.dst, self.gpu.cu_stream) }
            .map_err(|e| e.to_string())
    }

    fn reset_temporal(
        &mut self,
        instance: &mut dyn InferenceBackend,
    ) -> Result<StageList, BackendError> {
        instance.reset_temporal(&self.gpu.gpu_context())
    }

    fn start_timing(&mut self) {
        // SAFETY: our own stream.
        let _ = unsafe { self.gpu.start.record(self.gpu.cu_stream) };
    }

    fn process(
        &mut self,
        instance: &mut dyn InferenceBackend,
        params: &FrameParams,
    ) -> Result<(), BackendError> {
        let src = self.src().map_err(BackendError::InvalidInput)?;
        instance.process(&self.gpu.gpu_context(), params, &src, &self.dst)
    }

    fn end_timing(&mut self) {
        // SAFETY: our own stream.
        let _ = unsafe { self.gpu.end.record(self.gpu.cu_stream) };
    }

    fn set_background(
        &mut self,
        instance: &mut dyn InferenceBackend,
        sample: &gst::Sample,
    ) -> Result<(), String> {
        let (buffer, info) = sample_frame(sample)?;
        // Only memory of the element's own context is read: the video
        // pipeline was given it.
        let context = input_context(self.lib, buffer)
            .map_err(|e| format!("a video background frame: {e}"))?;
        if context != self.gpu.context {
            return Err("a video background frame is in another CUDA context".into());
        }
        let mem = buffer.peek_memory(0);
        // SAFETY: a GstCudaMemory of our context (checked above); the sync
        // only waits for its producer (the converter's stream).
        unsafe { (self.lib.gst_cuda_memory_sync)(mem.as_mut_ptr().cast()) };
        // SAFETY: read-only CUDA map of a live buffer the sample holds; it
        // stays mapped until the call returns, and the buffer alive until
        // the element's stream has finished the frame (`BgState::last`).
        let frame = unsafe {
            CudaFrame::map(
                buffer.as_mut_ptr(),
                &info,
                gst::ffi::GST_MAP_READ | MAP_CUDA,
            )
        }?;
        let r = instance.set_background_frame(Some(BackgroundFrame::Cuda(frame.nv12())));
        drop(frame);
        r.map_err(|e| e.to_string())
    }
}

/// A system-memory output frame, written on the CPU before each call
/// returns. No CUDA call.
struct HostPixels<'a> {
    fx: &'a ObcEffect,
    /// The output frame, mapped for writing while this lives.
    _out: gst_video::VideoFrameRef<&'a mut gst::BufferRef>,
    /// Its planes.
    dst: HostNv12,
    inbuf: &'a gst::Buffer,
    info: &'a VideoInfo,
    /// The input once mapped (for reading only).
    _input: Option<gst_video::VideoFrameRef<&'a gst::BufferRef>>,
    /// Its planes, only ever read.
    src: Option<HostNv12>,
}

impl FramePixels for HostPixels<'_> {
    fn kind(&self) -> FrameKind {
        FrameKind::Host
    }

    fn map_input(&mut self) -> Result<(), String> {
        let input = gst_video::VideoFrameRef::from_buffer_ref_readable(self.inbuf, self.info)
            .map_err(|_| "could not map an input frame for reading")?;
        self.src = Some(host_nv12(&input));
        self._input = Some(input);
        Ok(())
    }

    fn fill_hidden(
        &mut self,
        image: Option<&Arc<FallbackFrame>>,
        black: ColourSpec,
    ) -> Result<(), String> {
        if let Some(img) =
            image.filter(|i| (i.width, i.height) == (self.dst.width, self.dst.height))
        {
            // SAFETY: `dst` is the output frame mapped for writing in
            // `_out`, accessed through nothing else.
            match unsafe { copy_packed_nv12(&img.nv12, &mut self.dst) } {
                Ok(()) => return Ok(()),
                Err(e) => self
                    .fx
                    .fallback_upload_failed(img.serial, format!("cannot copy it: {e}")),
            }
        }
        // SAFETY: as above.
        unsafe { fill_nv12(&mut self.dst, black.black_nv12()) }
    }

    fn copy_input(&mut self) -> Result<(), String> {
        let src = self
            .src
            .as_ref()
            .ok_or("internal: the input is not mapped")?;
        // SAFETY: `src` is the input mapped for reading in `_input`, only
        // read; `dst` the output mapped for writing in `_out`: another
        // buffer, so they do not overlap.
        unsafe { copy_nv12(src, &mut self.dst) }
    }

    fn reset_temporal(
        &mut self,
        instance: &mut dyn InferenceBackend,
    ) -> Result<StageList, BackendError> {
        instance.reset_temporal_host()
    }

    fn start_timing(&mut self) {}

    fn process(
        &mut self,
        instance: &mut dyn InferenceBackend,
        params: &FrameParams,
    ) -> Result<(), BackendError> {
        let src = self
            .src
            .as_ref()
            .ok_or_else(|| BackendError::InvalidInput("the input is not mapped".into()))?;
        instance.process_host(params, src, &mut self.dst)
    }

    fn end_timing(&mut self) {}

    fn set_background(
        &mut self,
        instance: &mut dyn InferenceBackend,
        sample: &gst::Sample,
    ) -> Result<(), String> {
        let (buffer, info) = sample_frame(sample)?;
        let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
            .map_err(|_| "could not map a video background frame for reading")?;
        let plane = |i: u32| {
            frame
                .plane_data(i)
                .map_err(|e| format!("a video background frame's plane {i}: {e}"))
        };
        let strides = gst_video::prelude::VideoFrameExt::plane_stride(&frame);
        let view = HostNv12View {
            y: plane(0)?,
            y_stride: strides[0].max(0) as usize,
            uv: plane(1)?,
            uv_stride: strides[1].max(0) as usize,
            width: info.width(),
            height: info.height(),
        };
        instance
            .set_background_frame(Some(BackgroundFrame::Host(view)))
            .map_err(|e| e.to_string())
    }
}

/// A system-memory buffer pool for `caps`, with video meta (its strides
/// may be padded).
fn make_host_pool(caps: &gst::Caps, info: &VideoInfo) -> Result<gst::BufferPool, String> {
    let pool = gst_video::VideoBufferPool::new();
    let mut config = pool.config();
    config.set_params(Some(caps), info.size() as u32, 2, 0);
    config.add_option(gst_video::BUFFER_POOL_OPTION_VIDEO_META);
    pool.set_config(config)
        .map_err(|e| format!("system-memory buffer pool configuration: {e}"))?;
    pool.set_active(true)
        .map_err(|e| format!("system-memory buffer pool activation: {e}"))?;
    Ok(pool.upcast())
}

/// Makes current the context the calls on an instance created for `kind`
/// frames take: the CUDA context of `gpu`, or none for host frames.
///
/// # Safety
/// `gpu`'s context is alive: the element keeps it alive until it has
/// joined the worker.
unsafe fn push_for(
    kind: FrameKind,
    gpu: Option<GpuContext>,
) -> Result<Option<ContextGuard>, String> {
    match kind {
        FrameKind::Host => Ok(None),
        FrameKind::Cuda => {
            let gpu = gpu.ok_or("the effect has no CUDA context")?;
            // SAFETY: the caller's contract.
            unsafe { ContextGuard::push(gpu.context) }
                .map(Some)
                .map_err(|e| e.to_string())
        }
    }
}

/// A CUDA buffer pool on our context for `caps`, carrying our stream.
fn make_pool(
    lib: &cuda::GstCuda,
    gpu: &GpuResources,
    caps: &gst::Caps,
    info: &VideoInfo,
) -> Result<gst::BufferPool, String> {
    use gst::glib::translate::from_glib_full;
    // SAFETY: our live context; the call returns a new (full) reference.
    let raw = unsafe { (lib.gst_cuda_buffer_pool_new)(gpu.context) };
    if raw.is_null() {
        return Err("could not create a CUDA buffer pool".into());
    }
    // SAFETY: a new GstBufferPool reference, transferred to us.
    let pool: gst::BufferPool = unsafe { from_glib_full(raw) };
    let mut config = pool.config();
    config.set_params(Some(caps), info.size() as u32, 2, 0);
    config.add_option(gst_video::BUFFER_POOL_OPTION_VIDEO_META);
    // SAFETY: `config` is a live pool configuration structure we own;
    // the call stores a reference to our live stream in it.
    unsafe { (lib.gst_buffer_pool_config_set_cuda_stream)(config.as_mut_ptr(), gpu.stream) };
    pool.set_config(config)
        .map_err(|e| format!("CUDA buffer pool configuration: {e}"))?;
    pool.set_active(true)
        .map_err(|e| format!("CUDA buffer pool activation: {e}"))?;
    Ok(pool)
}

/// One unit of work for the worker: the request and what it needs.
struct Job {
    generation: u64,
    /// What to load: the request without the stages that failed
    /// ([`Control::job_target`]).
    target: Target,
    /// The memory of the frames ([`Format::kind`]).
    kind: FrameKind,
    /// The CUDA context and the element's stream, once created: always for
    /// CUDA frames, and for system memory only if the element ran on CUDA
    /// before (to release an instance made for those frames).
    gpu: Option<GpuContext>,
    format: Format,
    factory: Option<Arc<dyn BackendFactory>>,
    /// `Control::revocations` when the job was built.
    revocation: u64,
    /// `Control::refused` when the job was built.
    refused: Option<String>,
}

/// Why a load failed; the backend's error, when it came from the backend,
/// tells a failure of one stage alone from one of the chain.
struct Failure {
    reason: String,
    error: Option<BackendError>,
}

impl Failure {
    fn new(reason: impl Into<String>) -> Failure {
        Failure {
            reason: reason.into(),
            error: None,
        }
    }

    fn stage(&self) -> Option<StageId> {
        self.error.as_ref().and_then(stage_of)
    }
}

/// The stage `err` is the failure of alone, the rest of the chain intact.
fn stage_of(err: &BackendError) -> Option<StageId> {
    match err {
        BackendError::Stage { stage, .. } => Some(stage.clone()),
        _ => None,
    }
}

enum Outcome {
    Unloaded,
    Loaded(LoadReport),
    AlreadyLoaded,
    Failed(Failure),
    Superseded,
}

/// What a job did, and the stage failures it ran into (the job then went
/// on without those stages).
struct JobResult {
    outcome: Outcome,
    stage_failures: Vec<(StageId, String)>,
}

fn worker_loop(shared: Arc<Shared>, element: glib::WeakRef<super::ObcEffect>) {
    let mut handled: Option<u64> = None;
    loop {
        let job = {
            let mut c = lock(&shared.control);
            loop {
                if c.stop {
                    return;
                }
                // CUDA frames need the streaming thread's context first;
                // system memory needs nothing.
                if handled != Some(c.generation)
                    && let Some(format) = c.format.clone()
                    && (c.gpu.is_some() || !format.cuda)
                {
                    break Job {
                        generation: c.generation,
                        target: c.job_target(),
                        kind: format.kind(),
                        gpu: c.gpu,
                        format,
                        factory: c.factory.clone(),
                        revocation: c.revocations,
                        refused: c.refused.clone(),
                    };
                }
                c = shared.wake.wait(c).unwrap_or_else(|e| e.into_inner());
            }
        };
        handled = Some(job.generation);
        let JobResult {
            outcome,
            stage_failures,
        } = run_job(&shared, &job);
        let (loaded_kind, in_force, live) = {
            let e = lock(&shared.engine);
            let k = e.loaded.as_ref();
            let live = k
                .and(e.instance.as_ref())
                .map(|i| (i.name(), i.supports_live_background()));
            (k.map(|k| k.target.kind), k.map(|k| k.target.clone()), live)
        };
        let in_force_target = in_force.clone();
        let in_force_list = in_force
            .as_ref()
            .map(|t| t.stage_list().to_string())
            .unwrap_or_default();
        let in_force = |s: &str| in_force.as_ref().is_some_and(|t| t.wants(s));
        let posted = {
            let mut c = lock(&shared.control);
            let before = c.report();
            c.loaded_kind = published_kind(&c, job.revocation, loaded_kind);
            c.video.live = c.loaded_kind.and(live);
            c.video.in_force = c.loaded_kind.and(in_force_target);
            c.epoch += 1;
            if let Outcome::Loaded(r) = &outcome {
                c.loads += 1;
                c.last_load_ms = Some(r.load_ms + r.warmup_ms.unwrap_or(0.0));
                c.last_load_reused = r.reused_model;
            }
            if c.generation == job.generation {
                for (stage, reason) in &stage_failures {
                    c.record_stage_failure(stage, Some(job.generation), reason.clone(), false);
                }
                let effect = c.target.kind != EffectKind::None;
                match &outcome {
                    Outcome::Unloaded | Outcome::Superseded => {}
                    Outcome::Loaded(_) | Outcome::AlreadyLoaded => {
                        if effect {
                            c.status = EffectStatus::Ready;
                        }
                        for s in c.stage_ids() {
                            if c.effective(s.as_str()) && in_force(s.as_str()) {
                                c.stage_mut(&s).status = EffectStatus::Ready;
                            }
                        }
                    }
                    Outcome::Failed(f) => {
                        if effect {
                            c.status = EffectStatus::Failed(f.reason.clone());
                        }
                        for s in c.stage_ids() {
                            if c.effective(s.as_str()) {
                                c.stage_mut(&s).status = EffectStatus::Failed(f.reason.clone());
                            }
                        }
                    }
                }
                let target = c.target.clone();
                for (s, ctl) in &mut c.stages {
                    if !target.wants(s.as_str()) {
                        ctl.status = EffectStatus::Off;
                    }
                }
            }
            before.changed(c.report())
        };
        if let Some(el) = element.upgrade() {
            match &outcome {
                Outcome::Loaded(r) => gst::info!(
                    CAT,
                    obj = el,
                    "loaded {} with stages {} in {:.1} ms (+{:.1} ms warm-up, model reused: {})",
                    job.target.kind,
                    in_force_list,
                    r.load_ms,
                    r.warmup_ms.unwrap_or(0.0),
                    r.reused_model
                ),
                Outcome::Unloaded => gst::info!(CAT, obj = el, "unloaded"),
                _ => {}
            }
            for (stage, reason) in &stage_failures {
                gst::warning!(CAT, obj = el, "the {stage} stage failed to load: {reason}");
            }
            if let Some(r) = posted {
                post_status(&el, &r);
            }
        }
        // The video background follows what is loaded.
        refresh_video(&shared, &element);
    }
}

/// The decode `serial` of the fallback image at `path` ended with `frame`:
/// kept (or its reason) unless a later decode started; posts when the
/// report changes.
fn fallback_decoded(
    shared: &Shared,
    element: &glib::WeakRef<super::ObcEffect>,
    serial: u64,
    path: &std::path::Path,
    frame: Result<FallbackFrame, String>,
) {
    let posted = {
        let mut c = lock(&shared.control);
        if c.fallback.serial != serial {
            return;
        }
        let before = c.report();
        c.epoch += 1;
        match frame {
            Ok(f) => c.fallback.frame = Some(Arc::new(f)),
            Err(why) => c.fallback.error = Some(fallback_unreadable(path, &why)),
        }
        before.changed(c.report())
    };
    if let Some(el) = element.upgrade() {
        match &posted {
            Some(r) if r.fallback_error.is_some() => {
                gst::warning!(
                    CAT,
                    obj = el,
                    "{}",
                    r.fallback_error.as_deref().unwrap_or("")
                )
            }
            _ => gst::debug!(CAT, obj = el, "fallback image {} decoded", path.display()),
        }
        if let Some(r) = posted {
            post_status(&el, &r);
        }
    }
}

/// What the video background should be for `c` (see "The video
/// background" in the module docs), the file at its path being `file`;
/// `None`: no source and nothing to report.
fn video_want(c: &Control, file: Option<background::FileStamp>) -> Option<VideoWant> {
    let path = c.video.path.as_ref()?;
    if c.settings.kind != EffectKind::Replace || c.loaded_kind != Some(EffectKind::Replace) {
        return None;
    }
    let format = c.format.as_ref()?;
    if let (name, false) = c.video.live? {
        return Some(VideoWant::Still(video_as_still(name)));
    }
    let cuda = if format.cuda {
        match c.video.cuda.as_ref()? {
            Ok(ctx) => Some(ctx.as_ptr() as usize),
            Err(why) => return Some(VideoWant::Still(video_unplayable(path, why))),
        }
    } else {
        None
    };
    Some(VideoWant::Play {
        path: path.clone(),
        file,
        width: format.width,
        height: format.height,
        colorimetry: format.colorimetry.clone(),
        cuda,
    })
}

/// Brings the video background's source in line with what the control
/// state wants ([`video_want`]): stops the source that is no longer
/// wanted, starts the one that is, records why none plays; posts when the
/// report changes. Never holds the control lock while a pipeline stops.
fn refresh_video(shared: &Arc<Shared>, element: &glib::WeakRef<super::ObcEffect>) {
    let mut holder = lock(&shared.video);
    // Stat'ed with the control lock released (see `refresh_fallback`).
    let path = lock(&shared.control).video.path.clone();
    let file = path.as_deref().and_then(background::FileStamp::of);
    let (want, cuda, holding) = {
        let c = lock(&shared.control);
        let want = video_want(&c, file);
        let cuda = c.video.cuda.as_ref().and_then(|r| r.as_ref().ok()).cloned();
        // The request has no video, and the chain in force still shows it:
        // it plays on until the requested background is loaded.
        let holding = want.is_none()
            && c.loaded_kind == Some(EffectKind::Replace)
            && c.video
                .in_force
                .as_ref()
                .is_some_and(|t| !t.same_background(&c.target));
        (want, cuda, holding)
    };
    if want == holder.wanted || (holding && matches!(holder.wanted, Some(VideoWant::Play { .. }))) {
        return;
    }
    // The instance keeps the last frame it was given until the new
    // source's first one.
    let old = holder.source.take();
    shared.video_running.store(false, Ordering::SeqCst);
    holder.wanted = want.clone();
    let slot = match &want {
        Some(VideoWant::Play { .. }) => Some(Slot::new()),
        _ => None,
    };
    {
        let mut c = lock(&shared.control);
        c.video.slot = slot.clone();
    }
    // Never waited for: this may be the streaming thread (new caps).
    if let Some(old) = old {
        old.detach();
    }
    let error = match (want, slot) {
        (
            Some(VideoWant::Play {
                path,
                width,
                height,
                colorimetry,
                cuda: on_cuda,
                ..
            }),
            Some(slot),
        ) => {
            let spec = SourceSpec {
                path: path.clone(),
                width,
                height,
                colorimetry,
                // The context the want was made with (read under the same
                // lock); system memory never gets one.
                cuda: on_cuda.and(cuda),
            };
            let reporter = video_reporter(shared, element, slot.id, path.clone());
            match VideoSource::start(spec, slot, reporter) {
                Ok(source) => {
                    holder.source = Some(source);
                    shared.video_running.store(true, Ordering::SeqCst);
                    None
                }
                Err(why) => Some(video_unplayable(&path, &why)),
            }
        }
        (Some(VideoWant::Still(why)), _) => Some(why),
        _ => None,
    };
    let posted = {
        let mut c = lock(&shared.control);
        let before = c.report();
        if error.is_some() && holder.source.is_none() {
            c.video.slot = None;
        }
        c.video.error = error;
        c.video.refused = false;
        shared.video_refused.store(false, Ordering::SeqCst);
        before.changed(c.report())
    };
    let running = holder.source.is_some();
    drop(holder);
    if let Some(el) = element.upgrade() {
        match posted.as_ref().and_then(|r| r.background_error.as_deref()) {
            Some(why) => gst::warning!(CAT, obj = el, "{why}"),
            None => gst::debug!(CAT, obj = el, "video background running: {running}"),
        }
        if let Some(r) = posted {
            post_status(&el, &r);
        }
    }
}

/// Stops the video background's source, if any, waiting for its
/// pipeline to stop (the element's `stop`: the CUDA context goes next), and
/// forgets why it did not play. A later [`refresh_video`] starts it again.
fn stop_video(shared: &Arc<Shared>, element: &glib::WeakRef<super::ObcEffect>) {
    let mut holder = lock(&shared.video);
    holder.wanted = None;
    let old = holder.source.take();
    shared.video_running.store(false, Ordering::SeqCst);
    let posted = {
        let mut c = lock(&shared.control);
        let before = c.report();
        c.video.slot = None;
        c.video.error = None;
        c.video.refused = false;
        shared.video_refused.store(false, Ordering::SeqCst);
        before.changed(c.report())
    };
    drop(old);
    drop(holder);
    if let (Some(el), Some(r)) = (element.upgrade(), posted) {
        post_status(&el, &r);
    }
}

/// The reporter of the source of slot `id`, playing `path`: records the
/// first failure as the video's error while that slot is the running one
/// (the last frame it gave stays in force), and posts.
fn video_reporter(
    shared: &Arc<Shared>,
    element: &glib::WeakRef<super::ObcEffect>,
    id: u64,
    path: PathBuf,
) -> video_background::Reporter {
    let shared = Arc::downgrade(shared);
    let element = element.clone();
    Arc::new(move |why: String| {
        let Some(shared) = shared.upgrade() else {
            return;
        };
        let posted = {
            let mut c = lock(&shared.control);
            if c.video.slot.as_ref().map(|s| s.id) != Some(id) {
                return;
            }
            let before = c.report();
            c.video.error = Some(video_unplayable(&path, &why));
            c.video.refused = false;
            shared.video_refused.store(false, Ordering::SeqCst);
            before.changed(c.report())
        };
        if let Some(el) = element.upgrade()
            && let Some(r) = posted
        {
            gst::warning!(
                CAT,
                obj = el,
                "{}",
                r.background_error.as_deref().unwrap_or("")
            );
            post_status(&el, &r);
        }
    })
}

fn superseded(shared: &Shared, generation: u64) -> bool {
    let c = lock(&shared.control);
    c.stop || c.generation != generation
}

/// Brings what is loaded in line with `job`. A failure of one stage
/// (`BackendError::Stage` for it), or any failure while the stages are
/// alone (`none`: then of every one of them), is returned as the job's
/// stage failure, and the job goes on without the stage: the rest of the
/// chain, or an unload for `none` without another stage.
fn run_job(shared: &Shared, job: &Job) -> JobResult {
    let mut target = job.target.clone();
    let mut stage_failures = Vec::new();
    loop {
        if target.kind == EffectKind::None && !target.has_stages() {
            unload(shared, job);
            return JobResult {
                outcome: Outcome::Unloaded,
                stage_failures,
            };
        }
        match load(shared, job, &target) {
            Outcome::Failed(f)
                if target.has_stages()
                    && (target.kind == EffectKind::None
                        || f.stage().is_some_and(|s| target.wants(s.as_str()))) =>
            {
                let failed: Vec<StageId> = match f.stage() {
                    Some(s) if target.wants(s.as_str()) => vec![s],
                    _ => target.stages.iter().map(|(s, _)| s.clone()).collect(),
                };
                for s in failed {
                    target = target.without(s.as_str());
                    stage_failures.push((s, f.reason.clone()));
                }
            }
            outcome => {
                return JobResult {
                    outcome,
                    stage_failures,
                };
            }
        }
    }
}

/// Drops the instance of a revoked backend (created before the job's
/// revocation count), or one created for frames in the other memory (new
/// caps): unloaded and dropped with its context pushed (none for host
/// frames), as `release_gpu` does, so no later load reuses it.
fn drop_revoked(shared: &Shared, job: &Job) {
    let stale = {
        let mut e = lock(&shared.engine);
        if e.instance.is_none()
            || (e.revocation >= job.revocation && e.kind.is_none_or(|k| k == job.kind))
        {
            return;
        }
        e.loaded = None;
        e.bg = BgState::default();
        let kind = e.kind.take().unwrap_or(FrameKind::Cuda);
        e.instance.take().map(|i| (i, kind))
    };
    {
        // The streaming thread must not take the dropped chain as loaded.
        let mut c = lock(&shared.control);
        c.loaded_kind = None;
        c.epoch += 1;
    }
    let Some((mut instance, kind)) = stale else {
        return;
    };
    // SAFETY: the element keeps the context alive until it has joined
    // this thread.
    match unsafe { push_for(kind, job.gpu) } {
        Ok(guard) => {
            instance.unload();
            drop(instance);
            drop(guard);
        }
        Err(err) => leak_backend(instance, &err),
    }
}

/// Unloads the backend instance (keeping it for the next load).
fn unload(shared: &Shared, job: &Job) {
    drop_revoked(shared, job);
    {
        let mut c = lock(&shared.control);
        c.loaded_kind = None;
        c.epoch += 1;
    }
    // The instance left is of the job's memory (`drop_revoked`).
    // SAFETY: the element keeps the context alive until it has joined
    // this thread.
    let guard = unsafe { push_for(job.kind, job.gpu) };
    let mut e = lock(&shared.engine);
    match &guard {
        Ok(_) => {
            if let Some(instance) = e.instance.as_mut() {
                instance.unload();
            }
        }
        Err(err) => {
            if let Some(instance) = e.instance.take() {
                e.kind = None;
                leak_backend(instance, err);
            }
        }
    }
    e.loaded = None;
    e.bg = BgState::default();
    drop(e);
    drop(guard);
}

/// Why an effect fails without a backend factory (none set, or revoked).
const NO_BACKEND: &str = "no inference backend is configured for the effects";

/// Loads `target` (an effect, its stages, or the stages alone) for `job`.
/// The instance of a revoked backend is dropped first.
fn load(shared: &Shared, job: &Job, target: &Target) -> Outcome {
    drop_revoked(shared, job);
    let kind = target.kind;
    if kind != EffectKind::None
        && let Some(reason) = &job.refused
    {
        return Outcome::Failed(Failure::new(reason.clone()));
    }
    let colour = match &job.format.colour {
        Ok(c) => *c,
        Err(reason) => return Outcome::Failed(Failure::new(reason.clone())),
    };
    let key = LoadedKey {
        target: target.clone(),
        width: job.format.width,
        height: job.format.height,
        colour,
        kind: job.kind,
    };
    let current = {
        let mut e = lock(&shared.engine);
        // A backend that lost its chain on its own (a module instance
        // unloaded after a panic) has nothing loaded, whatever was last.
        if e.loaded.is_some() && !e.instance.as_ref().is_some_and(|i| i.is_loaded()) {
            e.loaded = None;
        }
        e.loaded.clone()
    };
    if current.as_ref() == Some(&key) {
        return Outcome::AlreadyLoaded;
    }
    let background = match (kind, &target.background) {
        // A video and no still: black behind the person until its first
        // frame (and for good if it cannot play).
        (EffectKind::Replace, None) if target.black_still => Some(Arc::new(black_bgr(&job.format))),
        (EffectKind::Replace, None) => {
            return Outcome::Failed(Failure::new("the replace effect needs a background image"));
        }
        (EffectKind::Replace, Some(path)) => match decode_background(path, &job.format) {
            Ok(b) => Some(Arc::new(b)),
            Err(reason) => return Outcome::Failed(Failure::new(reason)),
        },
        _ => None,
    };
    if superseded(shared, job.generation) {
        return Outcome::Superseded;
    }
    let Some(factory) = job.factory.clone() else {
        return Outcome::Failed(Failure::new(NO_BACKEND));
    };
    if job.kind == FrameKind::Host && !factory.kinds().contains(&FrameKind::Host) {
        return Outcome::Failed(Failure::new(format!(
            "the {} backend takes CUDA frames only, and these frames are in system memory",
            factory.name()
        )));
    }
    // A full load for the backend: nothing loaded, or another size or
    // colour (the instance is left unloaded if it fails).
    let full = current
        .as_ref()
        .is_none_or(|k| (k.width, k.height, k.colour) != (key.width, key.height, key.colour));
    // The effect loads cold after the stages alone.
    let cold_effect = kind != EffectKind::None
        && current
            .as_ref()
            .is_some_and(|k| k.target.kind == EffectKind::None);
    if (full || cold_effect) && current.is_some() {
        // The streaming thread must not wait for this load.
        let mut c = lock(&shared.control);
        c.loaded_kind = None;
        c.epoch += 1;
    }
    // SAFETY: as for the unload above. An instance of the other memory
    // went in `drop_revoked`.
    let _guard = match unsafe { push_for(job.kind, job.gpu) } {
        Ok(g) => g,
        Err(e) => {
            return Outcome::Failed(Failure::new(format!(
                "cannot make the CUDA context current: {e}"
            )));
        }
    };
    let mut e = lock(&shared.engine);
    if e.instance.is_none() {
        match factory.create() {
            Ok(i) => {
                e.instance = Some(i);
                e.revocation = job.revocation;
                e.kind = Some(job.kind);
            }
            Err(err) => {
                return Outcome::Failed(Failure {
                    reason: err.to_string(),
                    error: Some(err),
                });
            }
        }
    }
    let req = LoadRequest {
        effect: kind,
        stages: target.stage_list(),
        width: key.width,
        height: key.height,
        colour,
        background,
        params: target.stage_params(),
    };
    let instance = e.instance.as_mut().expect("created above");
    let result = match (job.kind, &job.gpu) {
        (FrameKind::Cuda, Some(gpu)) => instance.load(gpu, &req),
        // Not reached: the guard above needs the context.
        (FrameKind::Cuda, None) => Err(BackendError::InvalidInput(
            "the effect has no CUDA context".into(),
        )),
        (FrameKind::Host, _) => instance.load_host(&req),
    };
    match result {
        Ok(report) => {
            e.loaded = Some(key);
            // The load-time still is in force again: the last video frame
            // is given again on the next fresh frame.
            e.bg.sent = None;
            e.bg.live = false;
            Outcome::Loaded(report)
        }
        Err(err) => {
            // A switch that fails keeps the previous chain loaded, less the
            // stage of a stage error (a new light the loaded relight stage
            // could not take drops it: the seam's rule), so that the status
            // never says ready for a stage that is not running.
            if full {
                e.loaded = None;
            } else if let (Some(stage), Some(k)) = (stage_of(&err), e.loaded.as_mut()) {
                k.target = k.target.without(stage.as_str());
            }
            Outcome::Failed(Failure {
                reason: err.to_string(),
                error: Some(err),
            })
        }
    }
}

fn decode_background(
    path: &std::path::Path,
    format: &Format,
) -> Result<obc_backend::HostBgr, String> {
    background::decode(path, format.width, format.height)
}

/// A black BGR image of the frame size: the load-time background of a video
/// without a still.
fn black_bgr(format: &Format) -> obc_backend::HostBgr {
    let stride = format.width as usize * 3;
    obc_backend::HostBgr {
        width: format.width,
        height: format.height,
        stride,
        data: vec![0; stride * format.height as usize],
    }
}

impl ObjectImpl for ObcEffect {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPS: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            let rw = glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING;
            vec![
                glib::ParamSpecString::builder("effect")
                    .nick("Effect")
                    .blurb("none | blur | replace")
                    .default_value(Some("none"))
                    .flags(rw)
                    .build(),
                glib::ParamSpecDouble::builder("blur-strength")
                    .nick("Blur strength")
                    .blurb("Background blur strength, 0 to 1 (applies on the next frame)")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(f64::from(EffectSettings::DEFAULT_BLUR_STRENGTH))
                    .flags(rw)
                    .build(),
                glib::ParamSpecString::builder("background")
                    .nick("Background image")
                    .blurb("Absolute path of the replace effect's image (empty: none)")
                    .default_value(Some(""))
                    .flags(rw)
                    .build(),
                glib::ParamSpecString::builder("background-video")
                    .nick("Background video")
                    .blurb(
                        "Absolute path of the replace effect's video (MP4, MOV, MKV, WebM), \
                         played in a loop behind the person; the background image is then its \
                         load-time still (empty: none)",
                    )
                    .default_value(Some(""))
                    .flags(rw)
                    .build(),
                glib::ParamSpecString::builder("on-failure")
                    .nick("On failure")
                    .blurb(
                        "Output after a failure: black | image (the away image) | none \
                         (the unprocessed input)",
                    )
                    .default_value(Some("black"))
                    .flags(rw)
                    .build(),
                glib::ParamSpecString::builder("fallback-image")
                    .nick("Away image")
                    .blurb(
                        "Absolute path of the away image shown in place of black with \
                         on-failure=image (empty: none, black)",
                    )
                    .default_value(Some(""))
                    .flags(rw)
                    .build(),
                glib::ParamSpecString::builder("status")
                    .nick("Status")
                    .blurb("off | loading | ready | failed: <reason>")
                    .read_only()
                    .build(),
            ]
        });
        PROPS.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut s = self.settings();
        let text = || {
            value
                .get::<Option<String>>()
                .ok()
                .flatten()
                .unwrap_or_default()
        };
        let retry = match pspec.name() {
            "effect" => match parse_effect(&text()) {
                Ok(k) => {
                    s.kind = k;
                    true
                }
                Err(e) => {
                    gst::warning!(CAT, imp = self, "ignoring effect: {e}");
                    return;
                }
            },
            "blur-strength" => {
                s.blur_strength = value.get::<f64>().unwrap_or(0.5) as f32;
                false
            }
            "background" => {
                let t = text();
                let p = (!t.is_empty()).then(|| PathBuf::from(t));
                if let Some(p) = &p
                    && !p.is_absolute()
                {
                    gst::warning!(
                        CAT,
                        imp = self,
                        "ignoring background {}: not an absolute path",
                        p.display()
                    );
                    return;
                }
                s.background = p;
                true
            }
            "background-video" => {
                let t = text();
                let p = (!t.is_empty()).then(|| PathBuf::from(t));
                if let Err(e) = self.set_background_video(p) {
                    gst::warning!(CAT, imp = self, "ignoring background-video: {e}");
                }
                return;
            }
            "fallback-image" => {
                let t = text();
                let p = (!t.is_empty()).then(|| PathBuf::from(t));
                if let Some(p) = &p
                    && !p.is_absolute()
                {
                    gst::warning!(
                        CAT,
                        imp = self,
                        "ignoring fallback-image {}: not an absolute path",
                        p.display()
                    );
                    return;
                }
                s.fallback_image = p;
                false
            }
            "on-failure" => match text().parse::<EffectFallback>() {
                Ok(f) => {
                    s.fallback = f;
                    false
                }
                Err(e) => {
                    gst::warning!(CAT, imp = self, "ignoring on-failure: {e}");
                    return;
                }
            },
            other => unreachable!("no property {other}"),
        };
        self.apply(s, retry);
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let s = self.settings();
        match pspec.name() {
            "effect" => s.kind.as_str().to_value(),
            "blur-strength" => f64::from(s.blur_strength).to_value(),
            "background" => s
                .background
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
                .to_value(),
            "background-video" => self
                .background_video()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
                .to_value(),
            "on-failure" => s.fallback.as_str().to_value(),
            "fallback-image" => s
                .fallback_image
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
                .to_value(),
            "status" => self.status().to_string().to_value(),
            other => unreachable!("no property {other}"),
        }
    }
}

impl GstObjectImpl for ObcEffect {}

impl ElementImpl for ObcEffect {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static META: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "omarchy-broadcast effect",
                "Filter/Effect/Video/Hardware",
                "Background blur or replacement, and stackable stages such as video noise \
                 removal, on NV12 frames in CUDA or system memory through an inference \
                 backend; passthrough without either",
                "Raul Dias",
            )
        });
        Some(&*META)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            // CUDA memory first (the preferred path), then system memory.
            // Any size: in system memory a frame the element writes must be
            // of even width and height, as the pipeline's canonical sizes
            // are; an odd one fails the stream (see "System memory" in the
            // module docs).
            let mut caps = gst::Caps::builder("video/x-raw")
                .features([cuda::MEMORY_FEATURE])
                .field("format", "NV12")
                .build();
            caps.merge(
                gst::Caps::builder("video/x-raw")
                    .field("format", "NV12")
                    .build(),
            );
            vec![
                gst::PadTemplate::new(
                    "sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &caps,
                )
                .expect("sink template"),
                gst::PadTemplate::new(
                    "src",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    &caps,
                )
                .expect("src template"),
            ]
        });
        TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for ObcEffect {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        {
            let mut c = lock(&self.shared.control);
            c.stop = false;
        }
        let shared = Arc::clone(&self.shared);
        let element = self.obj().downgrade();
        let handle = std::thread::Builder::new()
            .name("obceffect-load".into())
            .spawn(move || worker_loop(shared, element))
            .map_err(|e| {
                gst::error_msg!(
                    gst::CoreError::Failed,
                    ["cannot start the effect worker thread: {e}"]
                )
            })?;
        *lock(&self.worker) = Some(handle);
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        // The video background first: its frames are of the context the
        // release below lets go of.
        stop_video(&self.shared, &self.obj().downgrade());
        self.stop_worker();
        self.release_gpu();
        let posted = {
            let mut c = lock(&self.shared.control);
            let before = c.report();
            // A restart loads again (and retries a failure of the effect; a
            // stage failure stays recorded until the request changes).
            c.generation += 1;
            c.epoch += 1;
            c.status = c.pending_status();
            c.set_pending_stage_statuses();
            before.changed(c.report())
        };
        if let Some(r) = posted {
            post_status(&self.obj(), &r);
        }
        let mut st = lock(&self.stream);
        st.action = FrameAction::Passthrough;
        st.reset_pending = false;
        st.resend = false;
        st.cache = None;
        st.raw_on_screen = raw_on_screen_after(st.raw_on_screen, ScreenEvent::Reset);
        st.toggle_found = None;
        Ok(())
    }

    fn transform_caps(
        &self,
        _direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        // The same caps on both sides: the element never converts.
        Some(match filter {
            Some(f) => f.intersect_with_mode(caps, gst::CapsIntersectMode::First),
            None => caps.clone(),
        })
    }

    fn set_caps(&self, incaps: &gst::Caps, _outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let info = VideoInfo::from_caps(incaps)
            .map_err(|e| gst::loggable_error!(CAT, "caps {incaps}: {e}"))?;
        let cuda = incaps
            .features(0)
            .is_some_and(|f| f.contains(cuda::MEMORY_FEATURE));
        let format = Format::of(&info, cuda);
        {
            let mut st = lock(&self.stream);
            if st.caps.as_ref() != Some(incaps)
                && let Some(gpu) = st.gpu.as_mut()
                && let Some(pool) = gpu.pool.take()
            {
                let _ = pool.set_active(false);
            }
            if st.caps.as_ref() != Some(incaps)
                && let Some(pool) = st.host_pool.take()
            {
                let _ = pool.set_active(false);
            }
            if st.caps.as_ref() != Some(incaps) {
                st.cache = None;
                st.raw_on_screen = raw_on_screen_after(st.raw_on_screen, ScreenEvent::Reset);
                st.toggle_found = None;
            }
            st.info = Some(info);
            st.caps = Some(incaps.clone());
            st.format = Some(format.clone());
        }
        let posted = {
            let mut c = lock(&self.shared.control);
            c.epoch += 1;
            let before = c.report();
            if c.format.as_ref() != Some(&format) {
                c.format = Some(format);
                // Loading depends on the size and colour: a new request.
                c.generation += 1;
                c.status = c.pending_status();
                c.set_pending_stage_statuses();
                self.shared.wake.notify_all();
            }
            before.changed(c.report())
        };
        if let Some(r) = posted {
            post_status(&self.obj(), &r);
        }
        // The fallback image is decoded for the size and colour, and the
        // video background played for them.
        self.refresh_fallback();
        refresh_video(&self.shared, &self.obj().downgrade());
        Ok(())
    }

    fn decide_allocation(
        &self,
        query: &mut gst::query::Allocation,
    ) -> Result<(), gst::LoggableError> {
        // The element allocates from its own pool, and only once an effect
        // runs: drop downstream's pool so BaseTransform activates nothing
        // (no CUDA allocation in passthrough).
        while query.allocation_pools().next().is_some() {
            query.remove_nth_allocation_pool(0);
        }
        Ok(())
    }

    fn submit_input_buffer(
        &self,
        is_discont: bool,
        inbuf: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        self.prepare_frame(&inbuf);
        self.parent_submit_input_buffer(is_discont, inbuf)
    }

    fn prepare_output_buffer(
        &self,
        inbuf: InputBuffer,
    ) -> Result<PrepareOutputBufferSuccess, gst::FlowError> {
        let inbuf = match inbuf {
            InputBuffer::Readable(b) => b,
            InputBuffer::Writable(b) => &*b,
        };
        let mut st = lock(&self.stream);
        if st.action == FrameAction::Passthrough {
            return Ok(PrepareOutputBufferSuccess::InputBuffer);
        }
        if st.resend {
            match st.cache.as_ref() {
                Some(cached) => {
                    // A new buffer on the cached memory (shared, not
                    // copied), with the repeat's timestamps and flags.
                    let mut out = cached.buffer.copy();
                    drop(st);
                    let outref = out.get_mut().ok_or(gst::FlowError::Error)?;
                    self.parent_copy_metadata(inbuf, outref)
                        .map_err(|_| gst::FlowError::Error)?;
                    return Ok(PrepareOutputBufferSuccess::Buffer(out));
                }
                // Not reachable (the cache goes only between frames); the
                // repeat is processed then.
                None => st.resend = false,
            }
        }
        let (pool, memory) = if st.format.as_ref().is_some_and(|f| !f.cuda) {
            (st.host_pool.as_ref(), "system-memory")
        } else {
            (st.gpu.as_ref().and_then(|g| g.pool.as_ref()), "CUDA")
        };
        let Some(pool) = pool else {
            gst::element_imp_error!(
                self,
                gst::CoreError::Failed,
                ["the effect has no {memory} buffer pool to write black or processed frames"]
            );
            return Err(gst::FlowError::Error);
        };
        let mut out = pool.acquire_buffer(None).inspect_err(|e| {
            gst::error!(
                CAT,
                imp = self,
                "no output buffer from the {memory} pool: {e:?}"
            );
        })?;
        drop(st);
        let outref = out.get_mut().ok_or(gst::FlowError::Error)?;
        self.parent_copy_metadata(inbuf, outref)
            .map_err(|_| gst::FlowError::Error)?;
        Ok(PrepareOutputBufferSuccess::Buffer(out))
    }

    fn transform(
        &self,
        inbuf: &gst::Buffer,
        outbuf: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut st = lock(&self.stream);
        if st.resend {
            // The cached output, already in `outbuf`.
            st.raw_on_screen =
                raw_on_screen_after(st.raw_on_screen, ScreenEvent::Output(FrameOutput::Resent));
            return Ok(gst::FlowSuccess::Ok);
        }
        if let Err(reason) = self.render(&mut st, inbuf, outbuf) {
            drop(st);
            gst::element_imp_error!(
                self,
                gst::CoreError::Failed,
                ["the effect could not write a frame: {reason}"]
            );
            return Err(gst::FlowError::Error);
        }
        Ok(gst::FlowSuccess::Ok)
    }

    fn generate_output(&self) -> Result<GenerateOutputSuccess, gst::FlowError> {
        let out = self.parent_generate_output()?;
        let mut st = lock(&self.stream);
        if std::mem::take(&mut st.processed)
            && let GenerateOutputSuccess::Buffer(buffer) = &out
            && let Some(caps) = st.caps.clone()
        {
            st.cache = Some(Cached {
                buffer: buffer.clone(),
                generation: st.generation,
                epoch: st.epoch,
                frame: Arc::clone(&st.frame),
                caps,
            });
        }
        st.resend = false;
        Ok(out)
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        if event.type_() == gst::EventType::FlushStop {
            let mut st = lock(&self.stream);
            st.cache = None;
            st.resend = false;
            st.raw_on_screen = raw_on_screen_after(st.raw_on_screen, ScreenEvent::Reset);
            st.toggle_found = None;
            // The video background plays on where it is: its pipeline runs
            // on its own clock, apart from this stream, so a flush here
            // gives it no reason to start over (and a restart would wait
            // for its decoder's teardown on the streaming thread).
        }
        self.parent_sink_event(event)
    }
}

impl ObcEffect {
    /// Decides what the frame gets and sets BaseTransform's passthrough to
    /// match, creating the CUDA objects when the frame needs them (or the
    /// stages alone need them loaded: they pass frames through until then).
    fn prepare_frame(&self, inbuf: &gst::BufferRef) {
        if has_placeholder_flag(inbuf) {
            // The output's placeholder image: passed as it came, no state
            // touched (see the module docs).
            let mut st = lock(&self.stream);
            st.action = FrameAction::Passthrough;
            st.resend = false;
            drop(st);
            let obj = self.obj();
            if !obj.is_passthrough() {
                obj.set_passthrough(true);
            }
            return;
        }
        // A new source's room was never shown: the hold applies to it.
        let (raw_on_screen, toggle_found) = {
            let mut st = lock(&self.stream);
            let arrived = ScreenEvent::Arrived {
                new_source: has_new_source_flag(inbuf),
            };
            st.raw_on_screen = raw_on_screen_after(st.raw_on_screen, arrived);
            (st.raw_on_screen, st.toggle_found)
        };
        let (
            kind,
            status,
            fallback,
            loaded,
            any_loaded,
            stages,
            frame,
            generation,
            epoch,
            fallback_frame,
            (bg_slot, bg_wanted, bg_request),
        ) = {
            let c = lock(&self.shared.control);
            (
                c.settings.kind,
                c.status.clone(),
                c.settings.fallback,
                c.usable(),
                c.loaded_kind.is_some(),
                c.any_effective(),
                Arc::clone(&c.frame),
                c.generation,
                c.epoch,
                c.fallback.frame.clone(),
                {
                    let wanted = c.settings.kind == EffectKind::Replace && c.video.path.is_some();
                    // Only needed (and cloned) while a video may be in
                    // force without being wanted.
                    let request = (!wanted && c.loaded_kind == Some(EffectKind::Replace))
                        .then(|| c.target.clone());
                    (c.video.slot.clone(), wanted, request)
                },
            )
        };
        // An effect a toggle frame found loaded and ran counts as loaded
        // until the worker publishes something (then `usable` says it).
        let found = toggle_found == Some((generation, epoch)) && kind != EffectKind::None;
        let loaded = loaded || found;
        // `shown`: what the frame is decided as, `none` for a toggle
        // mid-stream while the effect loads (see the module docs).
        let (mut shown, mut action) = decide_frame(
            kind,
            &status,
            fallback,
            loaded,
            any_loaded,
            stages,
            raw_on_screen,
        );
        // The stages alone need CUDA to load, and a requested effect does
        // (the worker loads once the streaming thread has a context).
        let needs_gpu = action != FrameAction::Passthrough
            || (shown == EffectKind::None && (stages || kind != EffectKind::None));
        let mut st = lock(&self.stream);
        // System memory needs no CUDA: only the output pool, and only for a
        // frame the element writes (the worker loads without anything).
        let host = st.format.as_ref().is_some_and(|f| !f.cuda);
        let ready = if host {
            if action != FrameAction::Passthrough {
                self.ensure_host(&mut st)
            } else {
                Ok(())
            }
        } else if needs_gpu {
            self.ensure_gpu(&mut st, inbuf)
        } else {
            Ok(())
        };
        if let Err(reason) = ready {
            drop(st);
            if kind == EffectKind::None {
                // Only the stages needed CUDA: drop them, pass through.
                let requested: Vec<StageId> = {
                    let c = lock(&self.shared.control);
                    c.target.stages.iter().map(|(s, _)| s.clone()).collect()
                };
                for s in &requested {
                    self.fail_stage(s, Some(generation), reason.clone());
                }
                action = FrameAction::Passthrough;
            } else {
                self.fail(generation, reason.clone());
                shown = kind;
                action = decide(
                    kind,
                    &EffectStatus::Failed(reason),
                    fallback,
                    loaded,
                    stages,
                );
            }
            st = lock(&self.stream);
        }
        if has_new_source_flag(inbuf) {
            st.reset_pending = true;
            st.cache = None;
        }
        // A frame decided black or the input ends the cache; a repeat to
        // process gets the cached output when it is still valid.
        if action != FrameAction::Process {
            st.cache = None;
        }
        let valid = st.cache.as_ref().is_some_and(|c| {
            c.generation == generation
                && c.epoch == epoch
                && Arc::ptr_eq(&c.frame, &frame)
                && st.caps.as_ref() == Some(&c.caps)
        });
        if !valid {
            st.cache = None;
        }
        st.resend = action == FrameAction::Process
            && has_repeat_flag(inbuf)
            && !st.reset_pending
            && st.cache.is_some();
        if action == FrameAction::Passthrough {
            st.raw_on_screen =
                raw_on_screen_after(st.raw_on_screen, ScreenEvent::Output(FrameOutput::Input));
        }
        if !found {
            st.toggle_found = None;
        }
        st.action = action;
        st.generation = generation;
        st.epoch = epoch;
        st.kind = shown;
        st.toggle = shown != kind;
        st.frame = frame;
        st.fallback = fallback;
        st.fallback_frame = fallback_frame;
        st.fresh = !has_repeat_flag(inbuf);
        st.bg_slot = bg_slot;
        st.bg_wanted = bg_wanted;
        st.bg_request = bg_request;
        drop(st);
        let passthrough = action == FrameAction::Passthrough;
        if passthrough {
            self.shared.passthrough.fetch_add(1, Ordering::Relaxed);
        }
        let obj = self.obj();
        if obj.is_passthrough() != passthrough {
            obj.set_passthrough(passthrough);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BackendError, BackendFactory, ColourSpec, Control, EffectFallback, EffectKind,
        EffectSettings, EffectStatus, Format, FrameAction, InferenceBackend, NO_BACKEND, OnExit,
        Refusals, Rendered, StageId, StageValue, background, decide, decide_frame, published_kind,
        refusals, update_stage_values,
    };
    use crate::settings::StageRequest;
    use std::cell::Cell;
    use std::path::Path;
    use std::sync::Arc;

    /// The fallback key `c` wants for the file now at `path`, as
    /// `refresh_fallback` builds it.
    fn key_now(c: &Control, path: &Path) -> Option<super::FallbackKey> {
        c.fallback_wanted(background::FileStamp::of(path))
    }

    /// The values after a frame are those of its loaded chain; a frame
    /// whose figures were skipped (the `Engine` lock not taken) keeps the
    /// last ones.
    #[test]
    fn stage_values_follow_the_loaded_chain_and_survive_a_skipped_frame() {
        let v = |stage: StageId, name: &str| StageValue {
            stage,
            name: name.into(),
            value: 1.0,
        };
        let read = || {
            vec![
                v(StageId::DENOISE, "level"),
                v(StageId::BACKGROUND, "coverage"),
                v(StageId::RELIGHT, "stray"),
            ]
        };
        let stages_alone = Rendered {
            processed: true,
            ran: vec![StageId::DENOISE],
            ..Rendered::default()
        };
        let effect_alone = Rendered {
            processed: true,
            effect_ran: true,
            ..Rendered::default()
        };
        let mut current = Vec::new();
        update_stage_values(&mut current, Some(read()), &stages_alone);
        assert_eq!(current, [v(StageId::DENOISE, "level")], "the stages alone");
        update_stage_values(&mut current, None, &effect_alone);
        assert_eq!(
            current,
            [v(StageId::DENOISE, "level")],
            "a skipped frame keeps the last values"
        );
        update_stage_values(&mut current, Some(read()), &effect_alone);
        assert_eq!(
            current,
            [v(StageId::BACKGROUND, "coverage")],
            "only the effect ran"
        );
        update_stage_values(&mut current, Some(Vec::new()), &effect_alone);
        assert!(current.is_empty(), "none reported");
    }

    #[test]
    fn a_fallback_image_replaced_at_its_path_is_another_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("away.png");
        std::fs::write(&path, b"one").unwrap();
        let mut c = Control::default();
        c.settings.fallback = EffectFallback::Image;
        c.settings.fallback_image = Some(path.clone());
        assert_eq!(key_now(&c, &path), None, "no caps yet");
        c.format = Some(Format {
            width: 64,
            height: 48,
            colour: Ok(ColourSpec::LIMITED_BT601),
            black: ColourSpec::LIMITED_BT601,
            cuda: true,
            colorimetry: "bt601".into(),
        });
        assert_eq!(c.fallback_path(), Some(&path));
        let first = key_now(&c, &path).unwrap();
        assert_eq!(key_now(&c, &path), Some(first.clone()), "unchanged file");
        // Another file renamed onto the path, as an editor saves.
        let next = dir.path().join("next.png");
        std::fs::write(&next, b"two").unwrap();
        std::fs::rename(&next, &path).unwrap();
        let second = key_now(&c, &path).unwrap();
        assert_ne!(second, first, "replaced file");
        assert_eq!(second.path, first.path);
        assert_eq!(key_now(&c, &path), Some(second), "unchanged again");
        // Black wanted: no image, no key.
        c.settings.fallback = EffectFallback::Black;
        assert_eq!(c.fallback_path(), None);
        assert_eq!(key_now(&c, &path), None);
    }

    /// The shape of `render`: an early `?` return after work was queued.
    fn queue_then(fail_early: bool, synced: &Cell<u32>) -> Result<(), String> {
        let mut guard = OnExit::new(|| synced.set(synced.get() + 1));
        if fail_early {
            Err("a fill failed".to_string())?;
        }
        guard.disarm();
        Ok(())
    }

    #[test]
    fn the_exit_guard_runs_on_an_early_return_only() {
        let synced = Cell::new(0);
        assert!(queue_then(true, &synced).is_err());
        assert_eq!(synced.get(), 1, "an early return synchronises");
        queue_then(false, &synced).unwrap();
        assert_eq!(synced.get(), 1, "a disarmed guard does nothing");
    }

    /// A factory the revocation tests hand over (never asked to create).
    struct NoInstance;

    impl BackendFactory for NoInstance {
        fn name(&self) -> &'static str {
            "none"
        }

        fn probe(&self) -> Result<(), BackendError> {
            Ok(())
        }

        fn create(&self) -> Result<Box<dyn InferenceBackend>, BackendError> {
            Err(BackendError::Unavailable("test".into()))
        }
    }

    /// Blur ready on a loaded instance of a factory.
    fn blur_ready() -> Control {
        let mut c = Control::default();
        c.settings = EffectSettings::of(EffectKind::Blur);
        c.target = c.settings.target();
        c.status = EffectStatus::Ready;
        c.loaded_kind = Some(EffectKind::Blur);
        c.factory = Some(Arc::new(NoInstance));
        c
    }

    #[test]
    fn a_revoked_backend_fails_the_effect_at_once() {
        let mut c = blur_ready();
        let (generation, epoch, revocations) = (c.generation, c.epoch, c.revocations);
        assert!(c.revoke());
        assert!(c.factory.is_none());
        assert!(c.revoked);
        assert_eq!(c.generation, generation + 1, "a new request");
        assert_eq!(c.epoch, epoch + 1);
        assert_eq!(c.revocations, revocations + 1, "the instance is stale");
        assert!(!c.usable());
        assert_eq!(c.status, EffectStatus::Failed(NO_BACKEND.into()));
        for fallback in [EffectFallback::Black, EffectFallback::Image] {
            assert_eq!(
                decide(EffectKind::Blur, &c.status, fallback, c.usable(), false),
                FrameAction::Black
            );
            // With the room on screen still no toggle: a failure.
            assert_eq!(
                decide_frame(
                    EffectKind::Blur,
                    &c.status,
                    fallback,
                    c.usable(),
                    false,
                    false,
                    true
                ),
                (EffectKind::Blur, FrameAction::Black)
            );
        }
        assert_eq!(
            decide(
                EffectKind::Blur,
                &c.status,
                EffectFallback::None,
                c.usable(),
                false
            ),
            FrameAction::Passthrough
        );
        // Idempotent: the factory is gone.
        assert!(!c.revoke());
        assert_eq!(c.generation, generation + 1);
    }

    #[test]
    fn a_revocation_without_a_factory_does_nothing() {
        let mut c = Control::default();
        assert!(!c.revoke());
        assert_eq!((c.generation, c.revocations), (0, 0));
        assert!(!c.revoked);
        assert_eq!(c.status, EffectStatus::Off);
    }

    #[test]
    fn a_factory_set_after_a_revocation_starts_a_new_request() {
        let mut c = blur_ready();
        assert!(c.revoke());
        let generation = c.generation;
        assert!(c.rearm(Arc::new(NoInstance), Refusals::default()));
        assert!(c.factory.is_some());
        assert!(!c.revoked);
        assert_eq!(c.generation, generation + 1);
        assert_eq!(c.status, EffectStatus::Loading);
        // Black until ready (the hidden room was never shown again).
        assert_eq!(
            decide_frame(
                EffectKind::Blur,
                &c.status,
                EffectFallback::Black,
                c.usable(),
                false,
                false,
                false
            ),
            (EffectKind::Blur, FrameAction::Black)
        );
        // A second factory starts nothing more.
        assert!(!c.rearm(Arc::new(NoInstance), Refusals::default()));
        assert_eq!(c.generation, generation + 1);
    }

    #[test]
    fn a_factory_set_without_a_revocation_starts_no_request() {
        let mut c = Control::default();
        assert!(!c.rearm(Arc::new(NoInstance), Refusals::default()));
        assert!(c.factory.is_some());
        assert_eq!(c.generation, 0);
        assert_eq!(c.status, EffectStatus::Off);
    }

    #[test]
    fn a_revocation_under_none_fails_the_stages_alone_and_passes_through() {
        let mut c = Control::default();
        c.settings = EffectSettings::default().with_stage(StageRequest::on(StageId::DENOISE));
        c.target = c.settings.target();
        c.stage_mut(&StageId::DENOISE).status = EffectStatus::Ready;
        c.loaded_kind = Some(EffectKind::None);
        c.factory = Some(Arc::new(NoInstance));
        assert!(c.usable());
        assert!(c.revoke());
        assert_eq!(c.status, EffectStatus::Off);
        assert_eq!(
            c.stage_status(StageId::DENOISE.as_str()),
            &EffectStatus::Failed(NO_BACKEND.into())
        );
        assert_eq!(
            decide(
                EffectKind::None,
                &c.status,
                EffectFallback::Black,
                c.usable(),
                c.any_effective()
            ),
            FrameAction::Passthrough
        );
    }

    /// A factory whose check refuses a `direction` past 90 and a
    /// background `mode` other than 1.
    struct Picky;

    impl BackendFactory for Picky {
        fn name(&self) -> &'static str {
            "picky"
        }

        fn probe(&self) -> Result<(), BackendError> {
            Ok(())
        }

        fn create(&self) -> Result<Box<dyn InferenceBackend>, BackendError> {
            Err(BackendError::Unavailable("test".into()))
        }

        fn check_params(
            &self,
            stage: &StageId,
            params: &obc_backend::Params,
            live: &obc_backend::Params,
        ) -> Result<(), String> {
            use obc_backend::Value;
            match live.get("direction").or_else(|| params.get("direction")) {
                Some(Value::Int(d)) if d.abs() > 90 => {
                    return Err(format!("{stage}: direction {d}"));
                }
                _ => {}
            }
            match params.get("mode") {
                Some(Value::Int(m)) if *m != 1 => Err(format!("{stage}: mode {m}")),
                _ => Ok(()),
            }
        }
    }

    #[test]
    fn a_rearm_forgets_the_stage_failures_of_the_revocation() {
        // `none` with noise removal, revoked: the worker's load without a
        // factory records the stage's failure. The re-arm forgets it, so
        // the stage loads again.
        let mut c = Control::default();
        c.settings = EffectSettings::default().with_stage(StageRequest::on(StageId::DENOISE));
        c.target = c.settings.target();
        c.loaded_kind = Some(EffectKind::None);
        c.factory = Some(Arc::new(NoInstance));
        assert!(c.revoke());
        let generation = c.generation;
        assert!(c.record_stage_failure(
            &StageId::DENOISE,
            Some(generation),
            NO_BACKEND.into(),
            false
        ));
        assert!(!c.job_target().wants(StageId::DENOISE.as_str()));
        // Another stage failure is not the revocation's: kept.
        let relight = StageId::RELIGHT;
        c.settings = c
            .settings
            .clone()
            .with_stage(StageRequest::on(relight.clone()));
        c.target = c.settings.target();
        assert!(c.record_stage_failure(&relight, None, "no model".into(), false));
        assert!(c.rearm(Arc::new(NoInstance), Refusals::default()));
        assert_eq!(c.generation, generation + 1);
        assert!(
            c.stage(StageId::DENOISE.as_str())
                .unwrap()
                .failure
                .is_none()
        );
        assert_eq!(
            c.stage_status(StageId::DENOISE.as_str()),
            &EffectStatus::Loading
        );
        assert!(c.job_target().wants(StageId::DENOISE.as_str()));
        assert_eq!(
            c.stage_status(relight.as_str()),
            &EffectStatus::Failed("no model".into())
        );
        assert!(c.refused.is_none());
    }

    #[test]
    fn a_rearm_fails_what_its_factory_refuses_of_the_settings_applied_meanwhile() {
        use obc_backend::{Params, Value};
        let mut c = blur_ready();
        assert!(c.revoke());
        // Applied while revoked (no factory to check them with).
        let mut relight = StageRequest::on(StageId::RELIGHT);
        relight.live = Params::from([("direction".to_string(), Value::Int(120))]);
        let mut s = EffectSettings::of(EffectKind::Blur)
            .with_stage(StageRequest::on(StageId::DENOISE))
            .with_stage(relight);
        s.background_params = Params::from([("mode".to_string(), Value::Int(3))]);
        let refused = refusals(&s, &Picky);
        assert_eq!(
            refused,
            Refusals {
                stages: vec![(StageId::RELIGHT, "relight: direction 120".into())],
                effect: Some("background: mode 3".into()),
            }
        );
        assert_eq!(refusals(&s, &NoInstance), Refusals::default());
        c.settings = s;
        c.target = c.settings.target();
        for st in c.settings.stages.clone() {
            c.stage_mut(&st.id);
        }
        assert!(c.rearm(Arc::new(Picky), refused));
        // The refused stage is dropped as a failed one; the other loads.
        assert_eq!(
            c.stage_status(StageId::RELIGHT.as_str()),
            &EffectStatus::Failed("relight: direction 120".into())
        );
        assert!(!c.job_target().wants(StageId::RELIGHT.as_str()));
        assert!(c.job_target().wants(StageId::DENOISE.as_str()));
        // The effect's loads fail with the refusal (the job carries it).
        assert_eq!(c.refused.as_deref(), Some("background: mode 3"));
        // Under `none` there is no background effect to refuse.
        let mut c = Control {
            factory: Some(Arc::new(NoInstance)),
            ..Control::default()
        };
        assert!(c.revoke());
        let refused = Refusals {
            stages: Vec::new(),
            effect: Some("background: mode 3".into()),
        };
        assert!(c.rearm(Arc::new(Picky), refused));
        assert!(c.refused.is_none());
    }

    #[test]
    fn a_job_built_before_a_revocation_publishes_nothing_loaded() {
        let mut c = blur_ready();
        let revocation = c.revocations;
        // The job finishes before any revocation: what it loaded.
        assert_eq!(
            published_kind(&c, revocation, Some(EffectKind::Blur)),
            Some(EffectKind::Blur)
        );
        // Revoked (and re-armed) while it ran: the instance it left is the
        // revoked backend's, never usable, until the next job drops it.
        assert!(c.revoke());
        assert!(c.rearm(Arc::new(NoInstance), Refusals::default()));
        assert_eq!(published_kind(&c, revocation, Some(EffectKind::Blur)), None);
        c.loaded_kind = published_kind(&c, revocation, Some(EffectKind::Blur));
        assert!(!c.usable());
        // A job of the new count publishes again.
        assert_eq!(
            published_kind(&c, c.revocations, Some(EffectKind::Blur)),
            Some(EffectKind::Blur)
        );
    }
}
