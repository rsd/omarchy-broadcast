//! The `obceffect` element: the effect of the Process stage
//! (`docs/adr/0012-video-effects-and-maxine-backend.md`), and its optional
//! stackable stages (`docs/adr/0018-video-noise-removal.md`,
//! `docs/adr/0024-stackable-stages-and-effects-in-the-settings-schema.md`),
//! such as video noise removal and relighting of the person.
//!
//! ```text
//! sink, src: video/x-raw(memory:CUDAMemory), format=NV12   (same caps both sides)
//!            video/x-raw, format=NV12                       (system memory)
//! ```
//!
//! * **Properties**, all changeable while playing: `effect` (`none`, `blur`,
//!   `replace`), `blur-strength` (0 to 1, default 0.5; applies on the next
//!   frame), `background` (an absolute path, or empty), `background-video`
//!   (an absolute path, or empty; [`ObcEffect::set_background_video`]),
//!   `on-failure` (`black`, `image`, `none`), `fallback-image` (an absolute
//!   path, or empty); read-only `status`. The stackable stages have no
//!   property: [`ObcEffect::apply`] sets them with the rest
//!   ([`EffectSettings::stages`], each a [`StageId`](obc_backend::StageId)
//!   with its parameters), validated.
//! * **`none`** without a stage is BaseTransform passthrough: the input
//!   buffer goes downstream untouched and the element makes no CUDA call;
//!   it does not even open GStreamer's CUDA library.
//! * **Stages** run in the same backend instance as the effect, in the
//!   order of the settings (the backend runs a stage that consumes the
//!   matte after it, for example relighting of the person), under the
//!   effect or alone with `none`. A stage hides nothing: `none` with stages
//!   passes the input through until they are loaded, and blur and replace
//!   follow their privacy rules below whether or not one is on. A stage is
//!   switched on the loaded chain (a new load-time parameter is a switch
//!   too; its live parameters apply on the next frame and load nothing). If
//!   a stage cannot load or fails on a frame, the element drops it and goes
//!   on without it (the effect keeps running; its status does not turn
//!   failed; a frame under blur or replace is processed again without it,
//!   under `none` the frame is the input), and reports why in the stage's
//!   own status ([`ObcEffect::stage_status`]). The failure is retried when
//!   the stage's request (on or off, or its load-time parameters) changes or
//!   on [`ObcEffect::apply`].
//! * **An effect** writes a new output buffer from a CUDA buffer pool on the
//!   input's `GstCudaContext` (the stream's shared context, taken from the
//!   first input memory); the input is only read, so the clock's repeats of
//!   one buffer are safe. All GPU work runs on one CUDA stream the element
//!   creates in that context, and the stream is synchronised before the
//!   output buffer is pushed (see `imp` for why the input's own stream is
//!   not used).
//! * **Loading** runs on a worker thread of the element, with the context
//!   pushed. Until the requested effect is ready the element outputs black
//!   frames (from `none`, with or without stages) or keeps the previous
//!   effect (from another effect, or when only a stage request changes);
//!   `none` passes the input through at once and unloads on the worker (or
//!   switches to the stages alone).
//! * **Failures** (no backend, a failed load, a frame or a background
//!   image) set [`EffectStatus::Failed`]; the element then outputs black
//!   (`on-failure=black`) or the input (`on-failure=none`). A backend
//!   taken away while running ([`ObcEffect::revoke_backend`]: its module
//!   can no longer run the effect) is such a failure at once; the instance
//!   is dropped, and a factory set after it loads again.
//! * **The fallback image** (`on-failure=image`): every frame the element
//!   would output black (a failure, and the warm-up of an effect) shows the
//!   still image of `fallback-image` instead, cropped to fill the frame. It
//!   is decoded on a thread of its own once per path and caps (as soon as
//!   both are known), uploaded to the element's CUDA context once, and
//!   copied into each such frame. Until it is ready, and when it cannot be
//!   read, the frames are black; the reason is in
//!   [`EffectStats::fallback_error`] and in the status message's
//!   `fallback-image-error` field (empty while there is none).
//! * **The video background** (`background-video`, replace only): a video
//!   played in a loop behind the person
//!   ([`crate::video_background`]: NVDEC into the element's own CUDA
//!   context on CUDA frames, `vp9dec` for VP9 alone on system memory, no
//!   CUDA element there). The settings' `background` is its load-time
//!   still (black when there is none). Its decode pipeline exists only
//!   while replace is loaded on a backend that takes per-frame backgrounds
//!   (`InferenceBackend::supports_live_background`); it stops on a stop,
//!   and on a change to a still or another effect once that is loaded
//!   (until then the video stays behind the person, never the previous
//!   still); a flush leaves it playing; it never runs at idle. Each fresh
//!   camera frame (not a repeat, not the placeholder) hands the newest
//!   decoded frame, if new, to the backend
//!   (`InferenceBackend::set_background_frame`) before it is processed; any
//!   processed frame, a repeat too, gives the last one again after a
//!   reload (which puts the load-time still back in the backend).
//!   Changing the video reloads nothing: the background in force (the
//!   still, or the last frame of the previous video, given again after a
//!   reload) stays until the new video's first frame; a video that cannot
//!   play (no frames, larger than 3840x2160, ...) keeps it for good.
//!   Neither is ever black frames or the camera.
//!   Why the video does not play, or that the backend shows it as a still,
//!   is in [`EffectStats::background_error`] and in the status message's
//!   `background-error` field (empty while there is none);
//!   [`EffectStats::background_video`] says whether the pipeline exists.
//! * Every change of
//!   the effect's or a stage's status is also posted on the bus as an
//!   element message named [`STATUS_MESSAGE`], with the fields `effect`
//!   (string), `status` (`off`, `loading`, `ready`, `failed`), `reason`
//!   (string, empty unless failed), and for every stage the element has
//!   been given (listed in its settings, on or off), `<id>` (the stage's
//!   status, same values) and `<id>-reason` (string, empty unless the
//!   stage's status is failed): `denoise` and `denoise-reason`, for
//!   example. A stage the element was never given has no field: it is
//!   off.
//! * **A new source**: a buffer carrying [`NEW_SOURCE_FLAG`] (the output
//!   clock sets it on the first fresh frame of each source) makes the
//!   element reset the backend's temporal state before the next frame it
//!   processes.
//! * **A repeat**: a buffer carrying [`REPEAT_FLAG`] that is to be
//!   processed gets the element's last processed output again, with the
//!   repeat's timestamps and flags, and runs no effect, as long as that
//!   output was made under the same settings, chain and caps with no
//!   temporal reset pending; otherwise it is processed. The cached memory
//!   is shared, so a downstream element that writes in place writes into a
//!   copy. Black frames and passed-through input leave no cache.
//! * **The placeholder image**: a buffer carrying [`PLACEHOLDER_FLAG`] (the
//!   output clock's away image, no camera picture) goes downstream as it
//!   came, whatever the effect, its status or `on-failure`: no backend
//!   call, no CUDA or pool work, no cache written or dropped, a pending
//!   temporal reset kept for the next camera frame, and what was last on
//!   screen (the start, toggle and new-source rules) left as the last
//!   camera frame made it. It counts in no frame counter of the
//!   statistics.
//! * **Stopping** (PAUSED to READY) unloads the backend synchronously, with
//!   the context pushed, before the element drops its context reference.
//! * **System memory**: on caps without the CUDA memory feature the element
//!   does all of the above on host frames, with no CUDA call at all (no
//!   context, no stream, GStreamer's CUDA library not opened). Black frames
//!   are filled and the input and the fallback image copied on the CPU, the
//!   output buffers come from a system-memory pool, the input is only read,
//!   and the backend is called on host frames (`InferenceBackend::load_host`,
//!   `process_host`, `reset_temporal_host`). A backend factory that does not
//!   list host frames (`BackendFactory::kinds`) fails the effect with the
//!   reason, which then follows `on-failure`. Every rule above is the same in
//!   both memories; only the pixel work differs. Without CUDA there is no GPU
//!   time: [`EffectStats::gpu_ms_p50`] and `gpu_ms_p95` stay empty.

mod imp;

use std::sync::Arc;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use obc_backend::{BackendFactory, Params, StageId};

use crate::settings::{EffectSettings, EffectStats, EffectStatus};

/// The element factory name.
pub const ELEMENT_NAME: &str = "obceffect";

/// The name of the element message posted on every status change.
pub const STATUS_MESSAGE: &str = "obc-effect-status";

/// The buffer flag that marks the first frame of a new source: the output
/// clock sets it on the first fresh frame it emits from each source and
/// clears it on every other buffer; `obceffect` then resets the backend's
/// temporal state (`InferenceBackend::reset_temporal`) before processing
/// it. `DISCONT` cannot serve: the clock also sets it after every skipped
/// tick.
///
/// It is the first flag GStreamer leaves free for video buffers
/// (`GST_VIDEO_BUFFER_FLAG_LAST`). `GST_BUFFER_FLAG_LAST` itself is taken
/// on video buffers (it is `GST_VIDEO_BUFFER_FLAG_INTERLACED`).
///
/// Set and clear it with `set_flags` / `unset_flags`, and test it with
/// [`has_new_source_flag`]: `BufferRef::flags` drops the bits it has no name
/// for, this one included.
pub const NEW_SOURCE_FLAG: gst::BufferFlags =
    gst::BufferFlags::from_bits_retain(gst_video::ffi::GST_VIDEO_BUFFER_FLAG_LAST);

/// Whether `buffer` carries [`NEW_SOURCE_FLAG`].
pub fn has_new_source_flag(buffer: &gst::BufferRef) -> bool {
    has_flag(buffer, NEW_SOURCE_FLAG)
}

/// The buffer flag that marks a **repeat**: the output clock sets it on a
/// tick that emits the last camera frame again (no new one arrived in
/// time) and clears it on every other buffer. `obcmodule` hands such a
/// frame to its chain as a repeat (`StageChain::process_repeat`), which a
/// module of ABI 1.1 sees as `OBC_FRAME_FLAG_REPEAT`; `obceffect` sends
/// its last processed output again instead of running its effect, when
/// that output is still valid (see the module docs).
///
/// It is the flag after [`NEW_SOURCE_FLAG`] (`GST_VIDEO_BUFFER_FLAG_LAST
/// << 1`), also free for video buffers. Set, clear and test it as that one
/// ([`has_repeat_flag`]).
pub const REPEAT_FLAG: gst::BufferFlags =
    gst::BufferFlags::from_bits_retain(gst_video::ffi::GST_VIDEO_BUFFER_FLAG_LAST << 1);

/// Whether `buffer` carries [`REPEAT_FLAG`].
pub fn has_repeat_flag(buffer: &gst::BufferRef) -> bool {
    has_flag(buffer, REPEAT_FLAG)
}

/// The buffer flag that marks the output clock's **placeholder image**: the
/// clock sets it on a placeholder tick that pushes the image the user chose
/// to be seen without a camera picture (the "away image"), and clears it on
/// every other buffer, the black placeholder included. Such a frame is no
/// camera picture: `obceffect` and `obcmodule` pass it downstream as it
/// came, with no backend or chain call, and leave every state of theirs as
/// it was (the repeat cache, a pending temporal reset, what was last on
/// screen; see their module docs).
///
/// It is the flag after [`REPEAT_FLAG`] (`GST_VIDEO_BUFFER_FLAG_LAST <<
/// 2`), also free for video buffers. Set, clear and test it as that one
/// ([`has_placeholder_flag`]).
pub const PLACEHOLDER_FLAG: gst::BufferFlags =
    gst::BufferFlags::from_bits_retain(gst_video::ffi::GST_VIDEO_BUFFER_FLAG_LAST << 2);

/// Whether `buffer` carries [`PLACEHOLDER_FLAG`].
pub fn has_placeholder_flag(buffer: &gst::BufferRef) -> bool {
    has_flag(buffer, PLACEHOLDER_FLAG)
}

fn has_flag(buffer: &gst::BufferRef, flag: gst::BufferFlags) -> bool {
    // SAFETY: `buffer` is a live GstBuffer for the borrow; this reads its
    // flag word, as GST_BUFFER_FLAG_IS_SET does.
    let flags = unsafe { (*buffer.as_ptr()).mini_object.flags };
    flags & flag.bits() != 0
}

glib::wrapper! {
    /// The effect element; see the module docs.
    pub struct ObcEffect(ObjectSubclass<imp::ObcEffect>)
        @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

impl ObcEffect {
    /// A new element named `name` (or a generated name).
    pub fn new(name: Option<&str>) -> ObcEffect {
        let mut b = glib::Object::builder::<ObcEffect>();
        if let Some(name) = name {
            b = b.property("name", name);
        }
        b.build()
    }

    /// Sets the backend factory the element creates its instance with. Set
    /// it before an effect is requested; a change applies to the next
    /// instance the element creates. Without one, every effect fails with
    /// "no inference backend is configured". A factory set after
    /// [`Self::revoke_backend`] re-arms the element: the effect and its
    /// stages load again with it, with no restart. The settings applied
    /// while revoked were checked without a factory: they are checked with
    /// it then, as [`Self::apply`] does, and what it refuses fails (a
    /// refused stage is dropped, a refused background effect fails until
    /// settings it accepts are applied).
    pub fn set_backend_factory(&self, factory: Option<Arc<dyn BackendFactory>>) {
        self.imp().set_backend_factory(factory);
    }

    /// Takes the backend away while the element runs: the factory is
    /// cleared, and the effect and every effective stage fail at once with
    /// the no-backend reason, so the next frames follow `on-failure`
    /// (black, the fallback image, or with `none` the input). The worker
    /// drops the instance (unloaded with the context pushed); no later
    /// load reuses it. A frame already decided before the call may still
    /// be processed by it. Never waits for a load under way. Without a
    /// factory it does nothing.
    pub fn revoke_backend(&self) {
        self.imp().revoke_backend();
    }

    /// The current backend factory.
    pub fn backend_factory(&self) -> Option<Arc<dyn BackendFactory>> {
        self.imp().backend_factory()
    }

    /// Validates `settings` ([`EffectSettings::validate`], and each listed
    /// stage's parameters, and the background stage's, with the backend factory's
    /// `BackendFactory::check_params` when the element has a factory) and
    /// applies them at once, as one change: one reload at most, and after a
    /// failure a new attempt even when nothing changed. A change of the
    /// per-frame parameters alone
    /// ([`EffectSettings::changes_frame_params_only`]) makes no new attempt.
    ///
    /// With a video background ([`Self::set_background_video`]) replace
    /// needs no image ([`EffectSettings::validate_with_video`]).
    pub fn apply(&self, settings: &EffectSettings) -> Result<(), String> {
        settings.validate_with_video(self.imp().has_background_video())?;
        if let Some(factory) = self.backend_factory() {
            for s in &settings.stages {
                factory.check_params(&s.id, &s.params, &s.live)?;
            }
            if !settings.background_params.is_empty() {
                factory.check_params(
                    &StageId::BACKGROUND,
                    &settings.background_params,
                    &Params::new(),
                )?;
            }
        }
        self.imp().apply_checked(settings.clone());
        Ok(())
    }

    /// Sets the video background of replace (`background-video`): an
    /// absolute path to an MP4, MOV, MKV or WebM file, or `None`. The
    /// settings' `background` is then its load-time still (black when there
    /// is none). Changing the video alone reloads nothing: the background
    /// in force stays until the new video's first frame. Refuses a relative
    /// path. Set it before applying replace without an image.
    pub fn set_background_video(&self, path: Option<&std::path::Path>) -> Result<(), String> {
        self.imp()
            .set_background_video(path.map(std::path::Path::to_path_buf))
    }

    /// The video background of replace, if one is set.
    pub fn background_video(&self) -> Option<std::path::PathBuf> {
        self.imp().background_video()
    }

    /// The current settings.
    pub fn settings(&self) -> EffectSettings {
        self.imp().settings()
    }

    /// The state of the requested effect ([`EffectStatus::Off`] for
    /// `none`, with or without stages).
    pub fn status(&self) -> EffectStatus {
        self.imp().status()
    }

    /// The state of the stage `id`: [`EffectStatus::Off`] unless requested,
    /// then loading, ready, or failed with the reason (the stage was
    /// dropped, or the chain it runs in failed).
    pub fn stage_status(&self, id: &str) -> EffectStatus {
        self.imp().stage_status(id)
    }

    /// Counters and timings.
    pub fn stats(&self) -> EffectStats {
        self.imp().stats()
    }
}

/// Registers `obceffect` with GStreamer for this process (a static element,
/// no plugin file). Idempotent; initialises GStreamer if needed.
pub fn register() -> Result<(), glib::BoolError> {
    use std::sync::OnceLock;
    static REGISTERED: OnceLock<Result<(), String>> = OnceLock::new();
    REGISTERED
        .get_or_init(|| {
            gst::init().map_err(|e| e.to_string())?;
            gst::Element::register(
                None,
                ELEMENT_NAME,
                gst::Rank::NONE,
                ObcEffect::static_type(),
            )
            .map_err(|e| e.to_string())
        })
        .clone()
        .map_err(|e| glib::bool_error!("registering {ELEMENT_NAME}: {e}"))
}

/// Downcasts an element made from the `obceffect` factory.
pub fn from_element(element: &gst::Element) -> Option<&ObcEffect> {
    element.downcast_ref::<ObcEffect>()
}
