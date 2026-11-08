//! Device discovery, pipeline building and measurement for oma-broadcast.
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
//! * **Process** ([`process`]): the effect chain on canonical frames, at the
//!   head of the output pipeline; empty by default.
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
//! the daemon's statistics. The measurement modules
//! (`instrument`, `stats`, `gpu`, `stamp`) serve `obc-bench`.

pub mod capability;
pub mod clock;
pub mod controls;
pub mod device;
pub mod discovery;
pub mod frame;
pub mod geometry;
pub mod gpu;
pub mod ingest;
pub mod instrument;
pub mod live;
pub mod loopback;
pub mod output;
pub mod pipeline;
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
pub use frame::{Canonical, Frame, FrameWriter};
pub use geometry::{Crop, crop_for};
pub use ingest::{ControlReport, Ingest, IngestConfig};
pub use live::{LiveStats, LiveWindow};
pub use loopback::{
    LoopbackWriter, WriterConfig, WriterCounts, WriterHandle, WriterSink, writer_caps_string,
};
pub use output::{
    EmitInfo, EmitObserver, FrameOrigin, Output, OutputConfig, OutputCounts, OutputSink,
};
pub use pipeline::{LinkCaps, LoopbackMismatch, MediaPipeline, PipelineError, all_cuda_between};
pub use process::GpuStage;
pub use session::{Session, SwitchTiming};
