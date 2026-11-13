//! Device discovery, pipeline building and measurement for omarchy-broadcast.
//!
//! Selects the camera (and its capture mode, from the capabilities it
//! enumerates) and the v4l2loopback sink, and runs the stream in three
//! stages (`docs/adr/0009-ingest-process-output.md`):
//!
//! * **Ingest** ([`ingest`]), one pipeline per source: camera, CPU ingest
//!   adapter where the format needs one, upload to CUDA memory, then crop
//!   and scale to the canonical frame (NV12 in CUDA memory at the output
//!   size, with the contract's colour label), written into a latest-frame
//!   slot ([`slot`]).
//! * **Process** ([`process`], [`effect`]): the effect on canonical frames,
//!   at the head of the output pipeline: nothing by default, or the
//!   `obceffect` element when [`OutputConfig::effect`] is set, whose effect
//!   (`none`, `blur`, `replace`) changes while the output streams
//!   ([`Output::set_effect`]; `docs/adr/0012-video-effects-and-maxine-backend.md`),
//!   and around it the stages of loadable modules ([`module`]), one
//!   `obcmodule` element per [`OutputConfig::module_stages`] entry.
//! * **Output** ([`output`]), one long-lived pipeline per sink with a fixed
//!   contract ([`obc_core::OutputContract`]): a clock thread ([`clock`])
//!   emits one frame per period (fresh, repeated, or a black placeholder),
//!   converts to the contract format, downloads and writes to the loopback.
//!
//! Canonical frames live in CUDA memory by default. An output built with
//! [`Output::build_in`] and [`obc_core::config::VideoMemory::System`] runs
//! the same three stages in system memory instead, on the CPU, with no CUDA
//! element and no CUDA context: the ingest scales with `videoconvertscale`,
//! the effect element and the module stages take host frames, and the
//! output has no download ([`frame`]).
//!
//! [`session::Session`] owns an output and its current ingest and switches
//! sources without touching the output. In the daemon, the output forwards
//! its frames to the persistent [`loopback::LoopbackWriter`], which owns the
//! loopback device for the daemon's whole life and sends black placeholders
//! while no output runs; [`live::LiveStats`] measures a running stream for
//! the daemon's statistics. At each start on a loopback the writer also
//! puts the crash cover in place ([`loopback_cover`]): a black timeout
//! image the driver serves while no writer streams, so a consumer survives
//! the daemon's crash and restart. The measurement modules
//! (`instrument`, `stats`, `gpu`, `stamp`) serve `obc-bench`. With the
//! cargo feature `pipewire` (off by default), the `pipewire` module also
//! offers the writer's frames as a native PipeWire camera node, fed by the
//! writer's tap ([`loopback::FrameTap`]). The [`preview`] encoder, fed by
//! the same tap, writes small JPEG snapshots of them for a live preview.
//!
//! Camera settings: [`control_node`] enumerates, reads and writes every
//! user and camera control of an opened node and reads its capture format
//! back; [`ctrl_events`] watches their changes (`V4L2_EVENT_CTRL`) on a
//! thread; [`xu`] reaches the vendor settings of UVC Extension Units listed
//! as data rows (the Logitech field of view). [`controls`] keeps the fixed
//! image-control allowlist and the frame-rate guard.

pub mod capability;
pub mod clock;
pub mod colour_range;
pub mod control_node;
pub mod controls;
pub mod ctrl_events;
pub mod device;
pub mod discovery;
pub mod effect;
pub mod frame;
pub mod frame_guard;
pub mod geometry;
pub mod gpu;
pub mod ingest;
pub mod instrument;
pub mod live;
pub mod loopback;
pub mod loopback_cover;
pub mod module;
pub mod output;
pub mod pipeline;
#[cfg(feature = "pipewire")]
pub mod pipewire;
pub mod preview;
pub mod process;
pub mod quirks;
pub mod session;
pub mod slot;
pub mod stamp;
pub mod stats;
#[cfg(test)]
mod test_support;
pub mod xu;

pub use capability::{Capabilities, IngestPath, ModeChoice, choose_mode};
pub use clock::{PacerCounts, TickKind};
pub use control_node::{CaptureFormat, ControlNode, CtrlValue, Enumeration, NodeControl};
pub use ctrl_events::{CtrlEvent, CtrlNotice, CtrlWatcher};
pub use device::{
    CameraChoice, DeviceInfo, DiscoverySource, SelectError, select_camera, select_sink,
};
pub use discovery::{DiscoveryRoute, list_cameras, scan_sysfs};
pub use effect::{
    EffectBackend, EffectFallback, EffectKind, EffectSettings, EffectStats, EffectStatus,
    effect_backend_available_with,
};
pub use frame::{Canonical, Frame, FrameWriter};
pub use frame_guard::CorruptFrames;
pub use geometry::{Crop, crop_for};
pub use ingest::{ControlReport, Ingest, IngestConfig};
pub use live::{LiveStats, LiveWindow};
pub use loopback::{
    FrameTap, LoopbackWriter, PlaceholderState, WriterAttachment, WriterConfig, WriterCounts,
    WriterHandle, WriterSink, image_contract_frame, writer_caps_string,
};
pub use loopback_cover::{ConsumerCover, CoverReport, CoverShows};
pub use module::{ModulePlacement, ModuleStageSpec, ModuleStats};
pub use output::{
    EmitInfo, EmitObserver, FrameOrigin, Output, OutputConfig, OutputCounts, OutputSink,
};
pub use pipeline::{
    LinkCaps, LoopbackMismatch, MediaPipeline, PipelineError, StopError, all_cuda_between,
};
pub use preview::{PreviewConfig, PreviewCounts, PreviewEncoder};
pub use process::GpuStage;
pub use session::{Session, SwitchTiming};
pub use xu::{XuControl, XuRow, XuState};
