//! The `obceffect` element: the effect of the Process stage
//! (`docs/adr/0012-video-effects-and-maxine-backend.md`), and its optional
//! stages: video noise removal (`docs/adr/0018-video-noise-removal.md`)
//! and relighting of the person.
//!
//! ```text
//! sink, src: video/x-raw(memory:CUDAMemory), format=NV12   (same caps both sides)
//! ```
//!
//! * **Properties**, all changeable while playing: `effect` (`none`, `blur`,
//!   `replace`), `blur-strength` (0 to 1, default 0.5; applies on the next
//!   frame), `background` (an absolute path, or empty), `on-failure`
//!   (`black`, `none`), `denoise` (boolean, default false), `relight`
//!   (boolean, default false), `relight-light` (`warm`, `neutral`,
//!   `cool`; default `neutral`), `relight-pan` (degrees, -180 to 180,
//!   default 0) and `relight-gain` (0.5 to 2, default 1; both apply on the
//!   next frame); read-only `status`. [`ObcEffect::apply`] sets them
//!   together, validated.
//! * **`none`** without denoise is BaseTransform passthrough: the input
//!   buffer goes downstream untouched and the element makes no CUDA call;
//!   it does not even open GStreamer's CUDA library.
//! * **Denoise** runs before the effect in the same backend instance, or
//!   alone with `none`. It hides nothing: `none` with denoise passes the
//!   input through until the stage is loaded, and blur and replace follow
//!   their privacy rules below whether or not it is on. If the stage cannot
//!   load or fails on a frame, the element drops it and goes on without it
//!   (the effect keeps running; its status does not turn failed), and
//!   reports why in the separate denoise status
//!   ([`ObcEffect::denoise_status`]). The failure is retried when the
//!   `denoise` value changes or on [`ObcEffect::apply`].
//! * **Relight** runs after denoise and the green-screen matte, on the
//!   person only, in the same backend instance, under an effect or alone
//!   with `none`. It follows the rules of denoise: it hides nothing, it is
//!   switched on the loaded chain, and a failure of it alone drops it (a
//!   frame under blur or replace is processed again without it, under
//!   `none` the frame is the input), reported in the separate relight
//!   status ([`ObcEffect::relight_status`]) and retried when `relight` or
//!   `relight-light` changes or on [`ObcEffect::apply`]. A new light is a
//!   switch that loads no model.
//! * **An effect** writes a new output buffer from a CUDA buffer pool on the
//!   input's `GstCudaContext` (the stream's shared context, taken from the
//!   first input memory); the input is only read, so the clock's repeats of
//!   one buffer are safe. All GPU work runs on one CUDA stream the element
//!   creates in that context, and the stream is synchronised before the
//!   output buffer is pushed (see `imp` for why the input's own stream is
//!   not used).
//! * **Loading** runs on a worker thread of the element, with the context
//!   pushed. Until the requested effect is ready the element outputs black
//!   frames (from `none`, with or without denoise) or keeps the previous
//!   effect (from another effect, or when only the denoise request
//!   changes); `none` passes the input through at once and unloads on the
//!   worker (or switches to the denoise stage alone).
//! * **Failures** (no backend, a failed load, a frame or a background
//!   image) set [`EffectStatus::Failed`]; the element then outputs black
//!   (`on-failure=black`) or the input (`on-failure=none`). Every change of
//!   the effect's or the denoise status is also posted on the bus as an
//!   element message named [`STATUS_MESSAGE`], with the fields `effect`
//!   (string), `status` (`off`, `loading`, `ready`, `failed`), `reason`
//!   (string, empty unless failed), `denoise` (the denoise status, same
//!   values), `denoise-reason` (string, empty unless the denoise status
//!   is failed), `relight` and `relight-reason` (the same for the
//!   relighting stage).
//! * **A new source**: a buffer carrying [`NEW_SOURCE_FLAG`] (the output
//!   clock sets it on the first fresh frame of each source) makes the
//!   element reset the backend's temporal state before the next frame it
//!   processes.
//! * **Stopping** (PAUSED to READY) unloads the backend synchronously, with
//!   the context pushed, before the element drops its context reference.

mod imp;

use std::sync::Arc;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use obc_backend::{BackendFactory, Stage};

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
    // SAFETY: `buffer` is a live GstBuffer for the borrow; this reads its
    // flag word, as GST_BUFFER_FLAG_IS_SET does.
    let flags = unsafe { (*buffer.as_ptr()).mini_object.flags };
    flags & NEW_SOURCE_FLAG.bits() != 0
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

    /// Validates `settings` ([`EffectSettings::validate`]) and applies them
    /// at once, as one change: one reload at most, and after a failure a
    /// new attempt even when nothing changed. A change of the per-frame
    /// parameters alone ([`EffectSettings::changes_frame_params_only`])
    /// makes no new attempt.
    pub fn apply(&self, settings: &EffectSettings) -> Result<(), String> {
        settings.validate()?;
        self.imp().apply(settings.clone(), true);
        Ok(())
    }

    /// The current settings.
    pub fn settings(&self) -> EffectSettings {
        self.imp().settings()
    }

    /// The state of the requested effect ([`EffectStatus::Off`] for
    /// `none`, with or without denoise).
    pub fn status(&self) -> EffectStatus {
        self.imp().status()
    }

    /// The state of the denoise stage: [`EffectStatus::Off`] unless
    /// requested, then loading, ready, or failed with the reason (the stage
    /// was dropped, or the chain it runs in failed).
    pub fn denoise_status(&self) -> EffectStatus {
        self.imp().stage_status(Stage::Denoise)
    }

    /// The state of the relighting stage, as [`ObcEffect::denoise_status`].
    pub fn relight_status(&self) -> EffectStatus {
        self.imp().stage_status(Stage::Relight)
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
