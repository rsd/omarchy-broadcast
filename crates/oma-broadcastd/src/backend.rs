//! What the lifecycle state machine asks of the media side.
//!
//! [`MediaBackend`] is the seam between the controller's decisions and the
//! pipelines that carry them out: [`real::GstBackend`] wraps the persistent
//! [`obc_pipeline::LoopbackWriter`] and an [`obc_pipeline::Session`]; the
//! controller's tests use a fake that records the calls.

use std::path::{Path, PathBuf};
use std::time::Duration;

use obc_core::{AspectPolicy, ModeRequest, OutputContract, SinkSelector};
use obc_ipc::Stats;

use crate::camera::Camera;

#[cfg(test)]
pub mod fake;
pub mod real;

/// What a session is built with, from the config at the time it starts.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionParams {
    /// The output contract: always the running writer's.
    pub contract: OutputContract,
    pub aspect: AspectPolicy,
    /// How long a camera switch holds the last frame.
    pub hold_timeout: Duration,
    /// Input age after which the output sends placeholders.
    pub input_gap: Duration,
    /// Pin the camera's exposure so it cannot lower the frame rate.
    pub fixed_framerate: bool,
}

/// Why a running session failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The camera ingest failed or its node disappeared: another camera may
    /// take over.
    Camera(String),
    /// The output pipeline failed: the session has to go.
    Output(String),
}

/// The media operations the controller drives. Every call is made from the
/// controller thread; none may block for long, except starting a session
/// (camera open and CUDA initialisation).
pub trait MediaBackend: Send + 'static {
    /// The colour capture cameras present, in discovery order, each with
    /// the mode chosen for `request`. Loopback devices, IR/greyscale and
    /// metadata nodes are never listed.
    fn scan_cameras(&mut self, request: &ModeRequest) -> Vec<Camera>;

    /// Finds the loopback `sink` names and starts the persistent writer on
    /// it with `contract`. Returns the device node. Fails if the device is
    /// missing or another application holds it in another format.
    fn start_writer(
        &mut self,
        sink: &SinkSelector,
        contract: &OutputContract,
    ) -> Result<PathBuf, String>;

    /// Stops the writer (the device is closed). Idempotent.
    fn stop_writer(&mut self);

    /// Why the running writer has failed, if it has.
    fn writer_failure(&mut self) -> Option<String>;

    /// The number of other processes holding the loopback `node` open,
    /// counted now (the consumer watcher's count can lag a fresh open).
    fn loopback_holders(&mut self, node: &Path) -> u32;

    /// Builds and starts the output (CUDA) forwarding to the writer, with
    /// no ingest yet ([`Self::switch_camera`] adds it). On error nothing of
    /// the session is left.
    fn start_session(&mut self, params: &SessionParams) -> Result<(), String>;

    /// Makes `camera` the running session's ingest, replacing the current
    /// one (the output holds the last frame meanwhile). On error the
    /// session has no ingest.
    fn switch_camera(&mut self, params: &SessionParams, camera: &Camera) -> Result<(), String>;

    /// Stops the session: camera closed, its controls restored, CUDA
    /// context released. Idempotent.
    fn stop_session(&mut self);

    /// Whether the current ingest (since the last start or switch) has
    /// delivered a frame to the output.
    fn first_frame(&mut self) -> bool;

    /// Why the running session has failed, if it has (non-blocking).
    fn session_failure(&mut self) -> Option<Failure>;

    /// The statistics since the previous call (or since the session
    /// started).
    fn take_stats(&mut self) -> Stats;
}
