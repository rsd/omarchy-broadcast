//! The `obceffect` element: the effect of the Process stage
//! (`docs/adr/0012-video-effects-and-maxine-backend.md`).
//!
//! ```text
//! sink, src: video/x-raw(memory:CUDAMemory), format=NV12   (same caps both sides)
//! ```
//!
//! * **Properties**, all changeable while playing: `effect` (`none`, `blur`,
//!   `replace`), `blur-strength` (0 to 1, default 0.5; applies on the next
//!   frame), `background` (an absolute path, or empty), `on-failure`
//!   (`black`, `none`); read-only `status`. [`ObcEffect::apply`] sets them
//!   together, validated.
//! * **`none`** is BaseTransform passthrough: the input buffer goes
//!   downstream untouched and the element makes no CUDA call; it does not
//!   even open GStreamer's CUDA library.
//! * **An effect** writes a new output buffer from a CUDA buffer pool on the
//!   input's `GstCudaContext` (the stream's shared context, taken from the
//!   first input memory); the input is only read, so the clock's repeats of
//!   one buffer are safe. All GPU work runs on one CUDA stream the element
//!   creates in that context, and the stream is synchronised before the
//!   output buffer is pushed (see `imp` for why the input's own stream is
//!   not used).
//! * **Loading** runs on a worker thread of the element, with the context
//!   pushed. Until the requested effect is ready the element outputs black
//!   frames (from `none`) or keeps the previous effect (from another effect);
//!   `none` passes the input through at once and unloads on the worker.
//! * **Failures** (no backend, a failed load, a frame or a background
//!   image) set [`EffectStatus::Failed`]; the element then outputs black
//!   (`on-failure=black`) or the input (`on-failure=none`). Every status
//!   change is also posted on the bus as an element message named
//!   [`STATUS_MESSAGE`], with the fields `effect` (string), `status`
//!   (`off`, `loading`, `ready`, `failed`) and `reason` (string, empty unless
//!   failed).
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
    /// new attempt even when nothing changed.
    pub fn apply(&self, settings: &EffectSettings) -> Result<(), String> {
        settings.validate()?;
        self.imp().apply(settings.clone(), true);
        Ok(())
    }

    /// The current settings.
    pub fn settings(&self) -> EffectSettings {
        self.imp().settings()
    }

    /// The state of the requested effect.
    pub fn status(&self) -> EffectStatus {
        self.imp().status()
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
