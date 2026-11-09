//! What the lifecycle state machine asks of the media side.
//!
//! [`MediaBackend`] is the seam between the controller's decisions and the
//! pipelines that carry them out: [`real::GstBackend`] wraps the persistent
//! [`obc_pipeline::LoopbackWriter`] and an [`obc_pipeline::Session`]; the
//! controller's tests use a fake that records the calls.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use obc_core::config::EffectConfig;
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
    /// The effect the output starts with (the effective settings: the
    /// state file's over the config's). The output can always change it
    /// later ([`MediaBackend::set_effect`]).
    pub effect: EffectConfig,
}

/// The state of the running session's effect.
#[derive(Debug, Clone, PartialEq)]
pub enum EffectStatus {
    /// No effect is requested: frames pass through.
    Off,
    /// The effect's models (or its background image) are loading; the
    /// output shows black frames, or the previous effect, meanwhile.
    Loading,
    /// The effect processes every frame.
    Ready,
    /// The effect failed; the output shows what `effect.on_failure` says.
    Failed(String),
}

/// What the running session's effect reports when polled.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectReport {
    pub status: EffectStatus,
    /// Duration of the last load (models and first run), in milliseconds.
    pub load_ms: Option<f64>,
    /// The last load reused the models already loaded (a switch between
    /// blur and replace).
    pub reused: bool,
    /// Median GPU time of one processed frame, in milliseconds.
    pub gpu_ms_p50: Option<f64>,
}

/// The state of the optional PipeWire output (`[pipewire] enabled`), the
/// `PipeWireOutput` property.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipeWireState {
    /// Not configured, or no writer runs.
    Off,
    /// The daemon was built without PipeWire support.
    Unsupported,
    /// The node is being created (PipeWire is not reachable yet, or the
    /// node is not registered yet).
    Connecting,
    /// The node is registered; nothing is linked to it.
    Ready,
    /// A consumer is linked and frames flow to it.
    Streaming,
    /// The output failed, with the reason. The loopback is unaffected.
    Failed(String),
}

impl PipeWireState {
    /// The wire form: `off`, `unsupported`, `connecting`, `ready`,
    /// `streaming`, `failed`.
    pub fn as_str(&self) -> &'static str {
        match self {
            PipeWireState::Off => "off",
            PipeWireState::Unsupported => "unsupported",
            PipeWireState::Connecting => "connecting",
            PipeWireState::Ready => "ready",
            PipeWireState::Streaming => "streaming",
            PipeWireState::Failed(_) => "failed",
        }
    }
}

/// What the PipeWire output reports, from its own thread, through the
/// [`PipeWireReporter`] the backend was built with, tagged with the
/// output's [`PipeWireGeneration`]. A newly started output reports
/// [`PipeWireState::Connecting`] and `Consumers(0)` first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipeWireEvent {
    /// The number of links to the node.
    Consumers(u32),
    /// The output's state changed.
    State(PipeWireState),
}

/// Which of the PipeWire outputs a backend started an event comes from:
/// every [`MediaBackend::start_pipewire`] returns a new one, greater than
/// every one before it (the first is 1). The events of an output still
/// queued when it was stopped or replaced carry its generation, so the
/// controller tells them from the current output's whatever the order in
/// which they arrive.
pub type PipeWireGeneration = u64;

/// Receives the PipeWire output's events, each with the generation of the
/// output that sent it (the daemon forwards them to the controller as
/// [`crate::controller::Command::PipeWire`]). Shared: the backend keeps it
/// and hands every PipeWire output it starts a closure that calls it with
/// that output's generation, from the output's own thread.
pub type PipeWireReporter = Arc<dyn Fn(PipeWireGeneration, PipeWireEvent) + Send + Sync>;

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
    /// it with `contract`, stopping the running one (and its PipeWire
    /// output) first. Returns the device node. Fails if the device is
    /// missing or another application holds it in another format. Starts
    /// no PipeWire output: [`Self::start_pipewire`] does.
    fn start_writer(
        &mut self,
        sink: &SinkSelector,
        contract: &OutputContract,
    ) -> Result<PathBuf, String>;

    /// Stops the writer (the device is closed) and the PipeWire output.
    /// Idempotent.
    fn stop_writer(&mut self);

    /// Starts the PipeWire output next to the running writer, with the
    /// writer's contract and the same frames, replacing the one that runs
    /// (stopped first). The writer is not touched. Returns the new output's
    /// generation, which tags every event it reports.
    ///
    /// Never fails: what happens is reported through its
    /// [`PipeWireEvent`]s, the first of which are
    /// [`PipeWireState::Connecting`] and `Consumers(0)`; an output that
    /// cannot start (no writer runs, PipeWire unusable, or a build without
    /// PipeWire support: [`PipeWireState::Unsupported`]) reports its state
    /// and `Consumers(0)` instead.
    fn start_pipewire(&mut self) -> PipeWireGeneration;

    /// Stops the PipeWire output alone, if one runs: its node disappears;
    /// the writer and the session keep running. Reports nothing: the caller
    /// resets its view of the output. Idempotent.
    fn stop_pipewire(&mut self);

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

    /// Whether `effect` could run now, with the reason if not: the
    /// inference backend is present (no model is loaded and no CUDA
    /// context created to find out) and, for `replace`, the background
    /// image is a readable regular file within the size limit. `none` is
    /// always available.
    fn effect_available(&mut self, effect: &EffectConfig) -> Result<(), String>;

    /// Applies `effect` to the running session at once, without restarting
    /// it (a load continues in the background, reported by
    /// [`Self::effect_report`]); after a failure it retries even when
    /// nothing changed. Without a session it does nothing.
    fn set_effect(&mut self, effect: &EffectConfig) -> Result<(), String>;

    /// The running session's effect (`None`: no session).
    fn effect_report(&mut self) -> Option<EffectReport>;
}
