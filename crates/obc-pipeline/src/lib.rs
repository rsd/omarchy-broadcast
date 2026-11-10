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
//!   ([`Output::set_effect`]; `docs/adr/0012-video-effects-and-maxine-backend.md`).
//! * **Output** ([`output`]), one long-lived pipeline per sink with a fixed
//!   contract ([`obc_core::OutputContract`]): a clock thread ([`clock`])
//!   emits one frame per period (fresh, repeated, or a black placeholder),
//!   converts to the contract format, downloads and writes to the loopback.
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

pub mod capability;
pub mod clock;
pub mod controls;
pub mod device;
pub mod discovery;
pub mod effect;
pub mod frame;
pub mod geometry;
pub mod gpu;
pub mod ingest;
pub mod instrument;
pub mod live;
pub mod loopback;
pub mod loopback_cover;
pub mod output;
pub mod pipeline;
#[cfg(feature = "pipewire")]
pub mod pipewire;
pub mod preview;
pub mod process;
pub mod session;
pub mod slot;
pub mod stamp;
pub mod stats;
#[cfg(test)]
mod test_support;

pub use capability::{Capabilities, IngestPath, ModeChoice, choose_mode};
pub use clock::{PacerCounts, TickKind};
pub use device::{
    CameraChoice, DeviceInfo, DiscoverySource, SelectError, select_camera, select_sink,
};
pub use discovery::{DiscoveryRoute, list_cameras, scan_sysfs};
pub use effect::{
    EffectBackend, EffectFallback, EffectKind, EffectSettings, EffectStats, EffectStatus,
    effect_backend_available,
};
pub use frame::{Canonical, Frame, FrameWriter};
pub use geometry::{Crop, crop_for};
pub use ingest::{ControlReport, Ingest, IngestConfig};
pub use live::{LiveStats, LiveWindow};
pub use loopback::{
    FrameTap, LoopbackWriter, WriterConfig, WriterCounts, WriterHandle, WriterSink,
    writer_caps_string,
};
pub use loopback_cover::{ConsumerCover, CoverReport};
pub use output::{
    EmitInfo, EmitObserver, FrameOrigin, Output, OutputConfig, OutputCounts, OutputSink,
};
pub use pipeline::{
    LinkCaps, LoopbackMismatch, MediaPipeline, PipelineError, StopError, all_cuda_between,
};
pub use preview::{PreviewConfig, PreviewCounts, PreviewEncoder};
pub use process::GpuStage;
pub use session::{Session, SwitchTiming};
