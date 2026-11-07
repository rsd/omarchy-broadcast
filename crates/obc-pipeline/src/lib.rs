//! Device discovery, pipeline building and measurement for oma-broadcast.
//!
//! Selects the camera (and its capture mode, from the capabilities it
//! enumerates) and the v4l2loopback sink, builds the
//! `v4l2src -> [CPU ingest adapter] -> CUDA memory -> v4l2sink` pipeline, and
//! measures it (fps, added latency, CPU, GPU, caps). Discovery, selection and
//! the builder are meant for the daemon as well; the measurement modules
//! serve `obc-bench`.

pub mod capability;
pub mod controls;
pub mod device;
pub mod discovery;
pub mod gpu;
pub mod instrument;
pub mod pipeline;
pub mod stamp;
pub mod stats;

pub use capability::{Capabilities, IngestPath, ModeChoice, choose_mode};
pub use device::{
    CameraChoice, DeviceInfo, DiscoverySource, SelectError, select_camera, select_sink,
};
pub use discovery::{DiscoveryRoute, list_cameras, scan_sysfs};
pub use pipeline::{
    GpuStage, LinkCaps, MediaPipeline, PipelineConfig, PipelineError, all_cuda_between,
};
