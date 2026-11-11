//! The `obceffect` element: the effect of the Process stage
//! (`docs/adr/0012-video-effects-and-maxine-backend.md`), and its optional
//! stackable stages (`docs/adr/0018-video-noise-removal.md`,
//! `docs/adr/0024-stackable-stages-and-effects-in-the-settings-schema.md`),
//! such as video noise removal and relighting of the person.
//!
//! ```text
//! sink, src: video/x-raw(memory:CUDAMemory), format=NV12   (same caps both sides)
//! ```
//!
//! * **Properties**, all changeable while playing: `effect` (`none`, `blur`,
//!   `replace`), `blur-strength` (0 to 1, default 0.5; applies on the next
//!   frame), `background` (an absolute path, or empty), `on-failure`
//!   (`black`, `image`, `none`), `fallback-image` (an absolute path, or
//!   empty); read-only `status`. The stackable stages have no
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
//!   (`on-failure=black`) or the input (`on-failure=none`).
//! * **The fallback image** (`on-failure=image`): every frame the element
//!   would output black (a failure, and the warm-up of an effect) shows the
//!   still image of `fallback-image` instead, cropped to fill the frame. It
//!   is decoded on a thread of its own once per path and caps (as soon as
//!   both are known), uploaded to the element's CUDA context once, and
//!   copied into each such frame. Until it is ready, and when it cannot be
//!   read, the frames are black; the reason is in
//!   [`EffectStats::fallback_error`] and in the status message's
//!   `fallback-image-error` field (empty while there is none).
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
//! * **Stopping** (PAUSED to READY) unloads the backend synchronously, with
//!   the context pushed, before the element drops its context reference.

mod imp;

use std::sync::Arc;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use obc_backend::BackendFactory;

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
    /// "no inference backend is configured".
    pub fn set_backend_factory(&self, factory: Option<Arc<dyn BackendFactory>>) {
        self.imp().set_backend_factory(factory);
    }

    /// The current backend factory.
    pub fn backend_factory(&self) -> Option<Arc<dyn BackendFactory>> {
        self.imp().backend_factory()
    }

    /// Validates `settings` ([`EffectSettings::validate`], and each listed
    /// stage's parameters with the backend factory's
    /// `BackendFactory::check_params` when the element has a factory) and
    /// applies them at once, as one change: one reload at most, and after a
    /// failure a new attempt even when nothing changed. A change of the
    /// per-frame parameters alone
    /// ([`EffectSettings::changes_frame_params_only`]) makes no new attempt.
    pub fn apply(&self, settings: &EffectSettings) -> Result<(), String> {
        settings.validate()?;
        if let Some(factory) = self.backend_factory() {
            for s in &settings.stages {
                factory.check_params(&s.id, &s.params, &s.live)?;
            }
        }
        self.imp().apply(settings.clone(), true);
        Ok(())
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
