//! The `obcmodule` element: one chain of stages of a loadable module
//! (`obc_backend::chain`), run in place on the frames between two other
//! elements of the output.
//!
//! ```text
//! sink, src: video/x-raw(memory:CUDAMemory), format=NV12
//!            video/x-raw, format=NV12                    (same caps both sides)
//! ```
//!
//! * **The chain.** The element hosts one
//!   [`StageChain`](obc_backend::StageChain) created by the
//!   [`StageChainFactory`] it is given ([`ObcModule::set_factory`]), for the
//!   ordered list of stages it is asked for ([`ObcModule::apply`], each a
//!   [`StageRequest`]: the stage, whether it runs, its load-time and its
//!   live parameters). Live parameters apply from the next frame and load
//!   nothing; turning a stage on or off, or a new load-time parameter, is a
//!   new load on the loaded chain.
//! * **Frame memory.** The frames are processed in place, in the memory
//!   the chain takes ([`StageChainFactory::kinds`]): CUDA frames go to a
//!   chain that takes CUDA frames as they are, in the stream's shared CUDA
//!   context, on a CUDA stream the element creates in it; system-memory
//!   frames go to a chain that takes host frames as they are. A chain that
//!   takes host frames only gets CUDA frames through a page-locked host
//!   copy: downloaded, processed, uploaded back into the same frame, on
//!   the element's stream. A chain that takes CUDA frames only cannot run
//!   on system memory: its stages fail with the reason.
//! * **Passthrough.** With no stage running (none requested, all failed, or
//!   not loaded yet) the element is BaseTransform passthrough: the input
//!   buffer goes downstream untouched, with no copy, no chain call and no
//!   CUDA call. A frame that is processed is made writable first, so a
//!   buffer shared with someone else (the output clock repeating a frame)
//!   is copied, never written behind their back.
//! * **Loading** runs on a worker thread of the element (a load may block
//!   for seconds), never on the streaming thread; until the chain is loaded
//!   the frames pass through. While the worker holds the chain (a load or a
//!   switch) frames pass through as well: the stages hide nothing.
//! * **Failures fail open.** A stage of a module hides nothing, so nothing
//!   ever shows black: a stage that fails alone
//!   ([`BackendError::Stage`](obc_backend::BackendError::Stage), on a load
//!   or a frame) is dropped and the others go on (the frame runs again
//!   through the chain without it); any other failure (no factory, a frame
//!   kind the chain does not take, a failed load or frame, a CUDA error)
//!   drops every stage, and the frames pass through unprocessed. Frames are
//!   processed in place, so a frame on which a chain fails as a whole is
//!   what the chain left of it, except through the host copy, where it is
//!   the input. A failure stays until the stage's request changes, the
//!   stages are applied again ([`ObcModule::apply`]) or the element
//!   restarts. Each stage's status ([`ObcModule::stage_status`]) is off,
//!   loading, ready, or failed with the reason; every change is posted on
//!   the bus as an element message named [`STATUS_MESSAGE`], with the
//!   field `module` (the factory's name, empty without one) and, for every
//!   stage the element has been given (on or off), `<id>` (`off`,
//!   `loading`, `ready`, `failed`) and `<id>-reason` (empty unless failed).
//! * **A new source**: a buffer carrying
//!   [`NEW_SOURCE_FLAG`](crate::NEW_SOURCE_FLAG) makes the element call
//!   [`StageChain::reset_temporal`](obc_backend::StageChain::reset_temporal) before the next frame it processes; a
//!   stage the chain drops instead is loaded again by the worker, and the
//!   frames run without it meanwhile.
//! * **A repeat**: a buffer carrying [`REPEAT_FLAG`](crate::REPEAT_FLAG)
//!   is run through
//!   [`StageChain::process_repeat`](obc_backend::StageChain::process_repeat)
//!   instead of `process`; a module of ABI 1.1 sees the flag, one of 1.0
//!   processes the repeat as any frame.
//! * **Stopping** (PAUSED to READY) unloads the chain synchronously (with
//!   the CUDA context current for a CUDA chain), then releases the host
//!   copy, the stream and the context reference.

mod imp;

use std::sync::Arc;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use obc_backend::{FrameKind, StageChainFactory, StageList};

use crate::settings::{EffectStatus, StageId, StageRequest, StageStats};

/// The element factory name.
pub const ELEMENT_NAME: &str = "obcmodule";

/// The name of the element message posted on every status change.
pub const STATUS_MESSAGE: &str = "obc-module-status";

/// A snapshot of the element's counters.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ModuleStats {
    /// The factory's name, empty without one.
    pub module: String,
    /// The frame memory of the loaded chain; `None` while nothing is
    /// loaded.
    pub kind: Option<FrameKind>,
    /// Every stage the element has been given, in the order it first saw
    /// them, with its state and the frames it ran on (no timings:
    /// `ms_p50` and `ms_p95` are `None`).
    pub stages: Vec<StageStats>,
    /// Frames the chain processed.
    pub frames_processed: u64,
    /// Of those, frames processed through the host copy (CUDA frames for a
    /// chain that takes host frames only).
    pub frames_converted: u64,
    /// Frames passed through unprocessed.
    pub frames_passthrough: u64,
    /// Successful loads since the element was created.
    pub loads: u64,
    /// Wall time of the last successful load, warm-up included, and
    /// whether it reused what was loaded.
    pub last_load_ms: Option<f64>,
    pub last_load_reused: bool,
    /// The last failure, of any stage or of the chain, with its reason.
    pub last_error: Option<String>,
    /// Whether the element holds CUDA resources (its stream and a context
    /// reference): only on CUDA frames with a stage requested.
    pub cuda_active: bool,
}

impl ModuleStats {
    /// The figures of stage `id`: those the element has, else (a stage it
    /// was never given) off with nothing counted.
    pub fn stage(&self, id: &StageId) -> StageStats {
        self.stages
            .iter()
            .find(|s| s.id == *id)
            .cloned()
            .unwrap_or_else(|| StageStats::new(id.clone()))
    }
}

glib::wrapper! {
    /// The module stage element; see the module docs.
    pub struct ObcModule(ObjectSubclass<imp::ObcModule>)
        @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

impl ObcModule {
    /// A new element named `name` (or a generated name).
    pub fn new(name: Option<&str>) -> ObcModule {
        let mut b = glib::Object::builder::<ObcModule>();
        if let Some(name) = name {
            b = b.property("name", name);
        }
        b.build()
    }

    /// Sets the factory the element creates its chain with. Set it before
    /// the element starts; a change applies to the next chain the element
    /// creates. Without one, every requested stage fails with "no module is
    /// configured for these stages".
    pub fn set_factory(&self, factory: Option<Arc<dyn StageChainFactory>>) {
        self.imp().set_factory(factory);
    }

    /// The current factory.
    pub fn factory(&self) -> Option<Arc<dyn StageChainFactory>> {
        self.imp().factory()
    }

    /// Checks `stages` (each id listed once) and applies them at once, in
    /// their order, as one change: one load at most, and a new attempt for
    /// the stages that failed even when nothing changed. A change of the
    /// live parameters alone makes no new attempt and loads nothing.
    pub fn apply(&self, stages: &[StageRequest]) -> Result<(), String> {
        StageList::new(stages.iter().map(|s| s.id.clone())).map_err(|e| e.to_string())?;
        self.imp().apply(stages.to_vec(), true);
        Ok(())
    }

    /// The stages as last applied.
    pub fn stages(&self) -> Vec<StageRequest> {
        self.imp().stages()
    }

    /// The state of stage `id`: [`EffectStatus::Off`] unless requested,
    /// then loading, ready, or failed with the reason.
    pub fn stage_status(&self, id: &str) -> EffectStatus {
        self.imp().stage_status(id)
    }

    /// Counters.
    pub fn stats(&self) -> ModuleStats {
        self.imp().stats()
    }
}

/// Registers `obcmodule` with GStreamer for this process (a static element,
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
                ObcModule::static_type(),
            )
            .map_err(|e| e.to_string())
        })
        .clone()
        .map_err(|e| glib::bool_error!("registering {ELEMENT_NAME}: {e}"))
}

/// Downcasts an element made from the `obcmodule` factory.
pub fn from_element(element: &gst::Element) -> Option<&ObcModule> {
    element.downcast_ref::<ObcModule>()
}

/// The memory a chain of `factory` runs on for frames in CUDA memory
/// (`cuda`) or in system memory: the frames' own kind when the chain takes
/// it, else host frames for CUDA frames (through a host copy); a chain that
/// takes CUDA frames only cannot run on system memory.
pub fn chain_kind(factory: &dyn StageChainFactory, cuda: bool) -> Result<FrameKind, String> {
    let kinds = factory.kinds();
    let own = if cuda {
        FrameKind::Cuda
    } else {
        FrameKind::Host
    };
    if kinds.contains(&own) {
        return Ok(own);
    }
    if cuda && kinds.contains(&FrameKind::Host) {
        return Ok(FrameKind::Host);
    }
    let name = factory.name();
    if kinds.is_empty() {
        return Err(format!("the {name} module takes no kind of frame"));
    }
    let list = kinds
        .iter()
        .map(|k| k.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "the {name} module's stages take {list} frames only, and these frames are in system \
         memory"
    ))
}
