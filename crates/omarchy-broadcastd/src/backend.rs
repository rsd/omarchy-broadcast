//! What the lifecycle state machine asks of the media side.
//!
//! [`MediaBackend`] is the seam between the controller's decisions and the
//! pipelines that carry them out: [`real::GstBackend`] wraps the persistent
//! [`obc_pipeline::LoopbackWriter`] and an [`obc_pipeline::Session`]; the
//! controller's tests use a fake that records the calls.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use obc_core::{
    AspectPolicy, EffectMode, EffectOnFailure, ModeRequest, OutputContract, SinkSelector,
};
use obc_ipc::Stats;
pub use obc_module_host::{Module, ModuleEntry, SettingsDescription};
use obc_pipeline::control_node::{CaptureFormat, NodeControl};
use obc_pipeline::controls::CtrlInfo;
use obc_pipeline::ctrl_events::CtrlEvent;
use obc_pipeline::effect::{Availability, Params, StageId};
use obc_pipeline::xu::XuRow;

use crate::camera::Camera;
use crate::settings::XuReading;

/// What [`ControlNode::enumerate`] found on a camera's node.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ControlScan {
    /// Every user- and camera-class control of a reported type, in the
    /// order the device reports them, each with its value (or why it could
    /// not be read) and its menu items
    /// ([`obc_pipeline::ControlNode::enumerate`]).
    pub controls: Vec<NodeControl>,
    /// The Extension Unit data rows the camera has and agrees to
    /// ([`obc_pipeline::xu`]), each read now.
    pub xu: Vec<XuReading>,
    /// What could not be read, each as (what, why): the walk stopped before
    /// the end, an Extension Unit row could not be probed. The rest is
    /// kept.
    pub problems: Vec<(String, String)>,
}

/// One camera node opened for its controls
/// ([`MediaBackend::open_camera_controls`]); closed when dropped. Each call
/// is one or a few ioctls.
pub trait ControlNode {
    /// Every control and Extension Unit row the camera exposes, each with
    /// its current value and flags: a control that cannot be read is
    /// reported unreadable, never the whole list.
    fn enumerate(&mut self) -> ControlScan;

    /// Writes control `id`, named `name` (for the error), its value
    /// checked by the caller.
    fn set(&mut self, id: u32, name: &str, value: i32) -> Result<(), String>;

    /// Reads control `id`, named `name` (for the error), back (the driver
    /// may have rounded a value written).
    fn get(&mut self, id: u32, name: &str) -> Result<i32, String>;

    /// Queries control `id`, named `name` (for the error): its range,
    /// default and flags now (`VIDIOC_QUERYCTRL`), without its value. What
    /// a write reads again of the controls that depend on the one written.
    fn query(&mut self, id: u32, name: &str) -> Result<CtrlInfo, String>;

    /// The capture format now (`VIDIOC_G_FMT`): what an Extension Unit
    /// write is checked against.
    fn capture_format(&mut self) -> Result<CaptureFormat, String>;

    /// Writes the option `option` of the Extension Unit row `row`.
    fn xu_set(&mut self, row: &'static XuRow, option: &str) -> Result<(), String>;

    /// Reads the current byte of the Extension Unit row `row`.
    fn xu_current(&mut self, row: &'static XuRow) -> Result<u8, String>;
}

/// Which control-event watcher an event comes from: every
/// [`MediaBackend::watch_controls`] returns a new one, greater than every
/// one before it (the first is 1), so the events of a watcher stopped or
/// replaced are told from the current one's whatever their order.
pub type WatchGeneration = u64;

/// What a control-event watcher reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlEvent {
    /// A watched control changed (its value, flags or range), through
    /// another file descriptor or by the device itself: the event, with
    /// what changed and the control's value, flags and range now.
    Changed(CtrlEvent),
    /// The watcher ended by itself, for this reason (the node went away).
    Ended(String),
}

/// Receives the control events, with the generation of the watcher that
/// sent them, from the watcher's thread (the daemon forwards them to the
/// controller as [`crate::controller::Command::ControlEvent`]).
pub type ControlEventReporter = Arc<dyn Fn(WatchGeneration, ControlEvent) + Send + Sync>;

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
    pub effect: EffectRun,
    /// The modules whose code the session runs, chosen when it starts and
    /// kept until it stops: a backend never changes mid-session.
    pub backends: Backends,
}

/// One stage of a chain the session runs, with its parameters: those read
/// when the chain loads (`params`) and those of each frame (`live`).
#[derive(Debug, Clone, PartialEq)]
pub struct StageRun {
    pub id: StageId,
    /// Requested; a stage that is not runs no code.
    pub enabled: bool,
    pub params: Params,
    pub live: Params,
}

/// Where a chain of module stages runs, apart from the effect element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Before the effect element (stages before the cut-out).
    BeforeEffect,
    /// After it.
    AfterEffect,
}

/// The stages of one module that run in a chain of their own (a module
/// stage element), outside the effect element.
#[derive(Debug, Clone, PartialEq)]
pub struct ChainRun {
    /// The element's name, unique in the session.
    pub name: String,
    /// The module.
    pub module: String,
    pub placement: Placement,
    pub stages: Vec<StageRun>,
}

/// What a session runs: the core background effect (its mode, strength,
/// image and fallback), the stages of the effect element (those of the
/// module that provides the `background` stage, in stage order), the
/// chains of the other modules, and whether the ingest enhances.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectRun {
    /// The background effect: `none`, `blur` or `replace`.
    pub mode: EffectMode,
    /// The blur strength, 0 to 1.
    pub blur_strength: f64,
    /// The image of `replace`.
    pub background_image: Option<PathBuf>,
    /// What shows when the background effect cannot run.
    pub on_failure: EffectOnFailure,
    /// The other stages of the effect element, in order.
    pub stages: Vec<StageRun>,
    /// The chains of the other modules, in order.
    pub chains: Vec<ChainRun>,
    /// The ingest is built with the enhance stage (per camera).
    pub enhance: bool,
}

impl EffectRun {
    /// The stage `id` of the effect element or of a chain.
    pub fn stage(&self, id: &str) -> Option<&StageRun> {
        self.stages
            .iter()
            .chain(self.chains.iter().flat_map(|c| c.stages.iter()))
            .find(|s| s.id == id)
    }

    /// Nothing for the effect element to do: no background effect and no
    /// stage requested.
    pub fn effect_idle(&self) -> bool {
        self.mode == EffectMode::None && self.stages.iter().all(|s| !s.enabled)
    }
}

/// A module chosen for a session: its id, with the loaded module (`None`
/// only for modules a test describes without a library).
#[derive(Clone)]
pub struct ModuleChoice {
    pub id: String,
    pub module: Option<Module>,
}

impl std::fmt::Debug for ModuleChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.id)
    }
}

impl PartialEq for ModuleChoice {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

/// The module code a session runs: the effect element's backend (the
/// provider of `background`), the ingest's enhancer (the provider of
/// `enhance`) and each chain's module, by chain name. Absent ones run
/// nothing: no effect element, no enhance stage, no chain.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Backends {
    pub effect: Option<ModuleChoice>,
    pub enhance: Option<ModuleChoice>,
    pub chains: Vec<(String, ModuleChoice)>,
}

/// A module opened by [`MediaBackend::open_module`]: the module, what its
/// probe says of each stage, and its settings.
#[derive(Debug, Clone)]
pub struct OpenedModule {
    /// The module (`None` for a module a test describes without a library).
    pub module: Option<Module>,
    pub stages: Vec<(StageId, Availability)>,
    pub settings: SettingsDescription,
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
    /// Each stage of the effect element and of the chains, apart from the
    /// effect, in [`EffectRun`] order: `Off` unless requested. A stage's
    /// failure never changes what the effect shows: the output streams on
    /// without the stage.
    pub stages: Vec<(StageId, EffectStatus)>,
    /// The enhance stage of the current ingest: `Off` unless the ingest was
    /// built with it. Its failure never changes what the effect shows: the
    /// stage passes the camera frames through.
    pub enhance: EffectStatus,
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

    /// Whether `effect` could run now on the effect backend of `backends`,
    /// with the reason if not: the backend is present and its probe answers
    /// (no model is loaded and no CUDA context created to find out) and,
    /// for `replace`, the background image is a readable regular file
    /// within the size limit. `none` is always available. Called before
    /// [`Self::start_session`], while the GPU may be parked: it must make no
    /// CUDA call.
    fn effect_available(&mut self, effect: &EffectRun, backends: &Backends) -> Result<(), String>;

    /// Applies `effect` to the running session at once, without restarting
    /// it (a load continues in the background, reported by
    /// [`Self::effect_report`]); after a failure it retries even when
    /// nothing changed. Without a session it does nothing.
    fn set_effect(&mut self, effect: &EffectRun) -> Result<(), String>;

    /// The running session's effect (`None`: no session).
    fn effect_report(&mut self) -> Option<EffectReport>;

    /// The module manifests of `dirs`, in order, a later one overriding an
    /// earlier one by module id ([`obc_module_host::scan`]). Reads files
    /// only: safe at idle.
    fn discover_modules(&mut self, dirs: &[PathBuf]) -> Vec<ModuleEntry>;

    /// Opens the library of module `entry` with its configuration `config`
    /// (JSON), and probes it: its stages' availability (the filesystem
    /// only, ADR 0006: no CUDA call) and its settings. `Err` is the reason
    /// the module is unavailable.
    fn open_module(
        &mut self,
        entry: &ModuleEntry,
        config: &serde_json::Value,
    ) -> Result<OpenedModule, String>;

    /// Probes the opened module `id` again (filesystem only, as
    /// [`Self::open_module`]).
    fn probe_module(
        &mut self,
        id: &str,
        module: Option<&Module>,
    ) -> Result<Vec<(StageId, Availability)>, String>;

    /// The compute capability `(major, minor)` of the GPU the running
    /// session uses, if the session's GPU side reports it (`None` without a
    /// session). Called only while a session runs, when the GPU is awake
    /// anyway: it may ask the driver.
    fn compute_capability(&mut self) -> Option<(u32, u32)>;

    /// Opens `camera`'s node for its controls (for writing as well when
    /// `write`): one open for the whole operation, its enumerations,
    /// writes and reads ([`ControlNode`]). Opening starts no stream, so it
    /// may be done in any state, whoever uses the camera. `Err` when the
    /// node cannot be opened. The caller writes only to the camera the
    /// session has open.
    fn open_camera_controls(
        &mut self,
        camera: &Camera,
        write: bool,
    ) -> Result<Box<dyn ControlNode>, String>;

    /// Starts watching the controls `ids` of `camera` for changes
    /// (`V4L2_EVENT_CTRL`) on a read-only descriptor of its own, replacing
    /// the watcher that runs (stopped first). Each event goes to the
    /// backend's [`ControlEventReporter`] with the returned generation.
    /// Called only while the session has the camera open: an open
    /// descriptor keeps a UVC camera out of autosuspend.
    fn watch_controls(&mut self, camera: &Camera, ids: &[u32]) -> Result<WatchGeneration, String>;

    /// Stops the control-event watcher, if one runs; no event of it is
    /// reported afterwards. Idempotent.
    fn unwatch_controls(&mut self);
}
