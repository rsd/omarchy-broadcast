//! The `obcenhance` element: the enhance stage of the ingest, which
//! upscales a low-resolution camera frame through an enhancer
//! ([`obc_backend::enhance`]) before the ingest scales it to the output
//! contract.
//!
//! ```text
//! sink: video/x-raw(memory:CUDAMemory), format={NV12,I420}, width=W, height=H
//! src:  video/x-raw(memory:CUDAMemory), format=NV12, width=k*W, height=k*H
//! ```
//!
//! * **Properties**: `scale` (`k`, 2 to 4, default 2; set before the
//!   element starts), read-only `status`. The enhancer factory and the
//!   stage's load-time parameters are set from Rust
//!   ([`ObcEnhance::set_factory`], [`ObcEnhance::set_params`]).
//! * **Loading** happens on the streaming thread, with the first frame: the
//!   enhancer is created and loaded in the input's CUDA context, on a CUDA
//!   stream the element creates in it. The ingest has not delivered a frame
//!   yet, so the output keeps showing what it showed before. The request
//!   is the caps, the factor and the parameters: when any of them differs
//!   from what is loaded, the next frame loads again.
//! * **The crop of the ingest.** The element reads the crop region
//!   `videocrop` attaches (`GstVideoCropMeta`) and enhances that region
//!   only; its output carries no crop.
//! * **Failure opens.** The stage hides nothing: if no factory is set, the
//!   enhancer cannot be created or loaded, or a frame fails, the element
//!   sets [`EffectStatus::Failed`] with the reason, releases the enhancer,
//!   drops that one frame and renegotiates its source pad to the input's
//!   caps. From the next frame on it passes the camera frames through
//!   untouched (the ingest's scaler sizes them as it does without the
//!   stage), until it is stopped. Every status change is posted on the bus
//!   as an element message named [`STATUS_MESSAGE`] with the fields
//!   `status` (`loading`, `ready`, `failed`), `reason` (empty unless
//!   failed) and `scale`.
//! * **Stopping** (PAUSED to READY) unloads the enhancer and releases the
//!   pool, the stream and the context reference, in that order: nothing of
//!   the stage outlives its ingest (`docs/adr/0017-cuda-checkpoint-parking.md`).

mod imp;

use std::sync::Arc;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use obc_backend::Params;
use obc_backend::enhance::EnhancerFactory;

use crate::settings::EffectStatus;

/// The element factory name.
pub const ELEMENT_NAME: &str = "obcenhance";

/// The name of the element message posted on every status change.
pub const STATUS_MESSAGE: &str = "obc-enhance-status";

/// What the element has done since it was created.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EnhanceStats {
    /// Frames enhanced.
    pub enhanced: u64,
    /// Frames passed through untouched after a failure.
    pub passthrough: u64,
    /// Frames dropped: the one frame of each failure.
    pub dropped: u64,
    /// Wall time of the last load, in milliseconds.
    pub load_ms: Option<f64>,
    /// GPU time per enhanced frame (conversions included), over the recent
    /// frames, in milliseconds.
    pub gpu_ms_p50: Option<f64>,
    pub gpu_ms_p95: Option<f64>,
}

glib::wrapper! {
    /// The enhance element; see the module docs.
    pub struct ObcEnhance(ObjectSubclass<imp::ObcEnhance>)
        @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

impl ObcEnhance {
    /// A new element named `name` (or a generated name) with the factor
    /// `scale` (clamped to 2 to 4).
    pub fn new(name: Option<&str>, scale: u32) -> ObcEnhance {
        let mut b = glib::Object::builder::<ObcEnhance>().property("scale", scale.clamp(2, 4));
        if let Some(name) = name {
            b = b.property("name", name);
        }
        b.build()
    }

    /// Sets the factory the element creates its enhancer with; set it
    /// before the element starts. Without one the stage fails open with
    /// "no enhancer is configured".
    pub fn set_factory(&self, factory: Option<Arc<dyn EnhancerFactory>>) {
        self.imp().set_factory(factory);
    }

    /// Sets the `enhance` stage's load-time parameters the enhancer is
    /// loaded with ([`EnhanceRequest::params`](obc_backend::enhance::EnhanceRequest::params)).
    /// Set them before the element starts; a later change loads the
    /// enhancer again with the next frame.
    pub fn set_params(&self, params: Params) {
        self.imp().set_params(params);
    }

    /// The `enhance` stage's load-time parameters.
    pub fn params(&self) -> Params {
        self.imp().params()
    }

    /// The scale factor.
    pub fn scale(&self) -> u32 {
        self.property::<u32>("scale")
    }

    /// The state of the stage: loading until the first frame is enhanced,
    /// then ready, or failed with the reason.
    pub fn status(&self) -> EffectStatus {
        self.imp().status()
    }

    /// Counters and timings.
    pub fn stats(&self) -> EnhanceStats {
        self.imp().stats()
    }
}

/// Registers `obcenhance` with GStreamer for this process (a static
/// element, no plugin file). Idempotent; initialises GStreamer if needed.
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
                ObcEnhance::static_type(),
            )
            .map_err(|e| e.to_string())
        })
        .clone()
        .map_err(|e| glib::bool_error!("registering {ELEMENT_NAME}: {e}"))
}

/// Downcasts an element made from the `obcenhance` factory.
pub fn from_element(element: &gst::Element) -> Option<&ObcEnhance> {
    element.downcast_ref::<ObcEnhance>()
}
