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
use obc_pipeline::controls::DeviceControl;

use crate::camera::Camera;

/// What [`ControlNode::query`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ControlQuery {
    /// The [`obc_pipeline::controls::IMAGE_CONTROLS`] the device exposes
    /// as adjustable, in that order, each with its value.
    pub controls: Vec<DeviceControl>,
    /// Each control left out because its query or its read failed: (name,
    /// why).
    pub skipped: Vec<(String, String)>,
}

/// One camera node opened for its image controls
/// ([`MediaBackend::open_camera_controls`]); closed when dropped. Each
/// call is one or a few ioctls.
pub trait ControlNode {
    /// The image controls the device exposes, each with its current value
    /// ([`obc_pipeline::controls::ImageControls::query`]): a control that
    /// cannot be read is skipped, never the whole list.
    fn query(&mut self) -> ControlQuery;

    /// Writes the image control `name` (its value checked by the caller).
    fn set(&mut self, name: &str, value: i32) -> Result<(), String>;

    /// Reads the image control `name` back (the driver may have rounded a
    /// value written).
    fn get(&mut self, name: &str) -> Result<i32, String>;
}

#[cfg(test)]
pub mod fake;
pub mod park;
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
    /// The noise removal stage, apart from the effect: `Off` unless
    /// requested. Its failure never changes what the effect shows: the
    /// output streams on without the stage.
    pub denoise: EffectStatus,
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

/// The live preview the backend is asked to encode
/// ([`MediaBackend::set_preview`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewRequest {
    /// Frames per second written: the highest rate of the leases held.
    pub fps: u32,
    /// The private directory the frame file is written to (prepared by the
    /// caller, see [`crate::preview::PreviewDir`]).
    pub dir: PathBuf,
}

/// Receives the sequence number of every preview frame written, from the
/// encoder's own thread (the daemon emits `PreviewFrame` with it, without
/// the controller). The numbers increase strictly over the backend's life,
/// across encoders, from 1.
pub type PreviewReporter = Arc<dyn Fn(u32) + Send + Sync>;

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
    /// no PipeWire output: [`Self::start_pipewire`] does. A preview asked
    /// for ([`Self::set_preview`]) is encoded from the new writer's frames,
    /// with its contract.
    fn start_writer(
        &mut self,
        sink: &SinkSelector,
        contract: &OutputContract,
    ) -> Result<PathBuf, String>;

    /// Stops the writer (the device is closed), the PipeWire output and the
    /// preview encoder (which removes its frame files); a preview still
    /// asked for ([`Self::set_preview`]) resumes with the next writer.
    /// Idempotent.
    fn stop_writer(&mut self);

    /// Starts the PipeWire output next to the running writer, with the
    /// writer's contract and the same frames, replacing the one that runs
    /// (stopped first). The writer is not touched. `offer_nv12` is
    /// `[pipewire] offer_nv12`: whether the node offers NV12 next to YUY2 and
    /// I420. Returns the new output's generation, which tags every event it
    /// reports.
    ///
    /// Never fails: what happens is reported through its
    /// [`PipeWireEvent`]s, the first of which are
    /// [`PipeWireState::Connecting`] and `Consumers(0)`; an output that
    /// cannot start (no writer runs, PipeWire unusable, or a build without
    /// PipeWire support: [`PipeWireState::Unsupported`]) reports its state
    /// and `Consumers(0)` instead.
    fn start_pipewire(&mut self, offer_nv12: bool) -> PipeWireGeneration;

    /// Stops the PipeWire output alone, if one runs: its node disappears;
    /// the writer, the preview and the session keep running. Reports
    /// nothing: the caller resets its view of the output. Idempotent.
    fn stop_pipewire(&mut self);

    /// Asks for the live preview (`Some`), or for none. While one is asked
    /// for and a writer runs, the writer's frames (the same buffers the
    /// loopback and the PipeWire output get: the writer's one tap feeds
    /// both) are decimated to `fps`, scaled and encoded as JPEG into
    /// `dir`, each frame reported to the [`PreviewReporter`] the backend
    /// was built with. A new rate applies to the running encoder; a new
    /// directory restarts it; `None` stops it and removes its frame files.
    /// The PipeWire output is not touched, and neither is the writer.
    /// Never fails and never blocks for long: a preview that cannot be
    /// encoded writes no frame. Idempotent.
    fn set_preview(&mut self, preview: Option<PreviewRequest>);

    /// Why the running writer has failed, if it has.
    fn writer_failure(&mut self) -> Option<String>;

    /// The number of other processes holding the loopback `node` open,
    /// counted now (the consumer watcher's count can lag a fresh open).
    fn loopback_holders(&mut self, node: &Path) -> u32;

    /// Builds and starts the output (CUDA) forwarding to the writer, with
    /// no ingest yet ([`Self::switch_camera`] adds it). The GPU is unparked
    /// first (see [`park`]); an error doing so is this call's error. On
    /// error nothing of the session is left, and the GPU is parked again.
    fn start_session(&mut self, params: &SessionParams) -> Result<(), String>;

    /// Makes `camera` the running session's ingest, replacing the current
    /// one (the output holds the last frame meanwhile). On error the
    /// session has no ingest.
    fn switch_camera(&mut self, params: &SessionParams, camera: &Camera) -> Result<(), String>;

    /// Stops the session: camera closed, its controls restored, CUDA
    /// context released, then the GPU parked if the session stopped cleanly
    /// (see [`park`]; a session that did not, or a park that fails, leaves
    /// it awake). Idempotent.
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
    /// always available. Called before [`Self::start_session`], while the
    /// GPU may be parked: it must make no CUDA call.
    fn effect_available(&mut self, effect: &EffectConfig) -> Result<(), String>;

    /// Applies `effect` to the running session at once, without restarting
    /// it (a load continues in the background, reported by
    /// [`Self::effect_report`]); after a failure it retries even when
    /// nothing changed. Without a session it does nothing.
    fn set_effect(&mut self, effect: &EffectConfig) -> Result<(), String>;

    /// The running session's effect (`None`: no session).
    fn effect_report(&mut self) -> Option<EffectReport>;

    /// Opens `camera`'s node for its image controls (for writing as well
    /// when `write`): one open for the whole operation, its queries, writes
    /// and reads ([`ControlNode`]). Opening starts no stream, so it may be
    /// done in any state, whoever uses the camera. `Err` when the node
    /// cannot be opened. The caller writes only to the camera the session
    /// has open.
    fn open_camera_controls(
        &mut self,
        camera: &Camera,
        write: bool,
    ) -> Result<Box<dyn ControlNode>, String>;

    /// The image controls `camera` exposes, each with its value on the
    /// device: [`Self::open_camera_controls`], then one query.
    fn camera_controls(&mut self, camera: &Camera) -> Result<ControlQuery, String> {
        Ok(self.open_camera_controls(camera, false)?.query())
    }
}
