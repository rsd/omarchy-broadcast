//! A recording media backend for the state-machine and D-Bus tests: no
//! device, no pipeline. The test shares the [`World`] with the backend and
//! changes it (cameras present, failures) between steps.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use obc_core::battery_saver::{PowerProfile, PowerSource};
use obc_core::config::VideoMemory;
use obc_core::{EffectMode, EffectOnFailure, ModeRequest, OutputContract, SinkSelector, VideoMode};
use obc_gpu::{GpuError, NothingToPark, ParkError, ParkOutcome, UnparkOutcome};
use obc_ipc::Stats;
use obc_pipeline::control_node::{CaptureFormat, CtrlValue, MenuItem, MenuItemValue, NodeControl};
use obc_pipeline::controls::{
    CTRL_FLAG_INACTIVE, CTRL_TYPE_BOOLEAN, CTRL_TYPE_BUTTON, CTRL_TYPE_INTEGER, CTRL_TYPE_MENU,
    CtrlInfo, DeviceControl, image_control_id,
};
use obc_pipeline::effect::{Availability, Params, StageId};
use obc_pipeline::xu::{FIELD_OF_VIEW, XuRow, XuState};

use super::park::{ParkOps, Parker, StartError, StopFailure, StopSession};
use super::{
    Backends, ControlNode, ControlScan, EffectReport, EffectRun, EffectStatus, Failure,
    MediaBackend, Module, ModuleEntry, OpenedModule, PipeWireGeneration, PreviewRequest,
    SessionError, SessionParams, WatchGeneration, WriterError,
};
use crate::camera::Camera;
use crate::controller::output_mode_string;
use crate::log::Level;
use crate::settings::XuReading;

/// The reason the effect element of a session without an effect module
/// fails an effect with (gst-obc's words).
pub const NO_BACKEND: &str = "no inference backend is configured for the effects";

/// What the running session's output shows ([`World::shown`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shown {
    /// The camera image, unprocessed.
    Camera,
    /// The camera image under the background effect.
    Effect,
    /// Black.
    Black,
    /// The fallback image.
    Image,
    /// The last frame the effect produced (privacy's `freeze` mode); the
    /// fake does not tell it from the away image or black that the
    /// element shows before a first frame.
    Frozen,
}

/// A fake camera with any controls ([`World::devices`]): types, menus and
/// flags as the test builds them, dependencies that flip a control's
/// inactive flag when its master is written, a field of view whose writes
/// can change the capture format.
#[derive(Debug, Clone)]
pub struct FakeDevice {
    /// Its controls, in enumeration order, with their values and flags.
    pub controls: Vec<NodeControl>,
    /// `(dependent, master, values)`: writing the master clears the
    /// dependent's inactive flag when the master's new value is one of
    /// `values`, and sets it otherwise.
    pub links: Vec<(u32, u32, Vec<i32>)>,
    /// Its field of view (the Logitech row), if it has one.
    pub fov: Option<FakeFov>,
    /// Its other Extension Unit rows (the MX Brio's), in scan order after
    /// the field of view.
    pub xu: Vec<FakeXu>,
    /// The capture format `capture_format` reads.
    pub format: CaptureFormat,
    /// Writes to these control ids fail (the device refuses them).
    pub refuse: Vec<u32>,
}

impl Default for FakeDevice {
    fn default() -> Self {
        FakeDevice {
            controls: Vec::new(),
            links: Vec::new(),
            fov: None,
            xu: Vec::new(),
            format: CaptureFormat {
                width: 1920,
                height: 1080,
                pixelformat: u32::from_le_bytes(*b"NV12"),
            },
            refuse: Vec::new(),
        }
    }
}

impl FakeDevice {
    /// The control `id`.
    pub fn control(&self, id: u32) -> &NodeControl {
        self.controls
            .iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("no control {id:#010x}"))
    }

    /// The value of control `id`.
    pub fn value(&self, id: u32) -> i32 {
        self.control(id).value.value().expect("a value")
    }

    /// Sets control `id` as the device would by itself (or another
    /// program): its value and the flags of what depends on it.
    pub fn change(&mut self, id: u32, value: i32) {
        if let Some(c) = self.controls.iter_mut().find(|c| c.id == id) {
            c.value = CtrlValue::Value(value);
        }
        for (dependent, master, values) in self.links.clone() {
            if master != id {
                continue;
            }
            if let Some(d) = self.controls.iter_mut().find(|c| c.id == dependent) {
                if values.contains(&value) {
                    d.info.flags &= !CTRL_FLAG_INACTIVE;
                } else {
                    d.info.flags |= CTRL_FLAG_INACTIVE;
                }
            }
        }
    }
}

/// A call of the backend that changes what the feed shows, in
/// [`World::image_events`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageEvent {
    /// `stop_session`.
    StopSession,
    /// `set_placeholder` (the writer's placeholder), with its image.
    Placeholder(Option<PathBuf>),
    /// `set_output_placeholder` (the session output's clock placeholder),
    /// with its image.
    OutputPlaceholder(Option<PathBuf>),
}

/// A fake field of view (the [`FIELD_OF_VIEW`] row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeFov {
    /// The current byte.
    pub current: u8,
    /// The default byte (`GET_DEF`).
    pub default: u8,
    /// Writing this byte changes the capture format to this one.
    pub changes_format: Option<(u8, CaptureFormat)>,
}

/// A fake Extension Unit row other than the field of view: its row, the
/// current and the default (`GET_DEF`) byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeXu {
    pub row: &'static XuRow,
    pub current: u8,
    pub default: u8,
}

/// What a probe of `row` reads with the bytes `current` and `default`.
fn xu_reading(row: &'static XuRow, current: u8, default: u8) -> XuReading {
    XuReading {
        row,
        state: XuState {
            info: 0x03,
            current,
            current_option: row.option_for_value(current),
            default_raw: Some(default),
            default: row
                .option_for_value(default)
                .unwrap_or_else(|| row.fallback_default()),
        },
    }
}

/// The fake media world.
#[derive(Debug)]
pub struct World {
    /// What a scan returns.
    pub cameras: Vec<Camera>,
    /// The result of finding the sink: the node, or why it cannot be used.
    pub sink: Result<PathBuf, String>,
    /// The running writer: node and contract.
    pub writer: Option<(PathBuf, OutputContract)>,
    pub writer_starts: u32,
    /// The generation of the PipeWire output running next to the writer.
    pub pipewire: Option<PipeWireGeneration>,
    /// The generation of the last PipeWire output started (0: none yet).
    pub pipewire_generation: PipeWireGeneration,
    pub pipewire_starts: u32,
    /// Running PipeWire outputs stopped (alone or with the writer).
    pub pipewire_stops: u32,
    /// `offer_nv12` of the last `start_pipewire`.
    pub pipewire_offer_nv12: Option<bool>,
    /// The preview asked for (`set_preview`); it is encoded while a writer
    /// runs ([`World::previewing`]).
    pub preview: Option<PreviewRequest>,
    /// Every `set_preview`, by its rate (`None`: stopped), in order.
    pub preview_sets: Vec<Option<u32>>,
    /// Reported (once) by `writer_failure` while a writer runs.
    pub writer_failure: Option<String>,
    /// The image the writer's placeholder shows (`None`: black), kept
    /// across writers as the real backend keeps it.
    pub placeholder: Option<PathBuf>,
    /// Every switch of the placeholder (`set_placeholder` that changed
    /// it), in order.
    pub placeholder_sets: Vec<Option<PathBuf>>,
    /// The image given again to the placeholder that already showed it
    /// (the real writer reads it again if the file was replaced), in order.
    pub placeholder_resends: Vec<PathBuf>,
    /// Every `set_output_placeholder` made while a session ran, in order
    /// (the running session's [`SessionParams::placeholder_image`] follows
    /// it, as the real output does).
    pub output_placeholder_sets: Vec<Option<PathBuf>>,
    /// Every `stop_session`, `set_placeholder` and
    /// `set_output_placeholder`, in order (also the calls that change
    /// nothing).
    pub image_events: Vec<ImageEvent>,
    /// The placeholder image is still being decoded: neither shown nor
    /// known to be unreadable (`placeholder_pending`).
    pub placeholder_decoding: bool,
    /// Why the writer cannot decode the placeholder image: reported by
    /// `placeholder_failure` while an image is asked for and a writer runs.
    pub placeholder_unreadable: Option<String>,
    /// Why the running session output cannot decode its clock
    /// placeholder's image: reported by `output_placeholder_failure` while
    /// a session runs with an image asked for.
    pub output_placeholder_unreadable: Option<String>,
    /// The image the writer's crash cover shows (`None`: black), kept
    /// across writers as the real backend keeps it.
    pub cover_image: Option<PathBuf>,
    /// Every switch of the crash cover's image (`set_cover_image` that
    /// changed it), in order.
    pub cover_image_sets: Vec<Option<PathBuf>>,
    /// The cover's image given again (read again if the file was
    /// replaced), in order.
    pub cover_image_resends: Vec<PathBuf>,
    /// The writer's placeholder image and crash cover image each writer
    /// was started with (what the backend kept then), in order.
    pub writer_start_images: Vec<(Option<PathBuf>, Option<PathBuf>)>,
    /// Every `hold_across_restart` made while a writer ran, with whether a
    /// session ran then.
    pub restart_holds: Vec<bool>,
    /// Every `end_restart_hold` made while a writer ran, with whether a
    /// session ran then.
    pub restart_hold_ends: Vec<bool>,
    /// What the effect report says of the effect element's fallback image
    /// (`EffectReport::fallback_error`).
    pub fallback_error: Option<String>,
    /// What the effect report says of the video background
    /// (`EffectReport::background_error`).
    pub background_error: Option<String>,
    /// What the effect report says of a refused switch
    /// (`EffectReport::switch_error`).
    pub switch_error: Option<String>,
    /// What the effect report says of the load in force
    /// (`EffectReport::load_notice`).
    pub load_notice: Option<String>,
    /// What `effect_report` reports as the stages' status values while a
    /// session runs.
    pub stage_values: Vec<obc_pipeline::effect::StageValue>,
    /// What `loopback_holders` counts.
    pub holders: u32,
    pub holder_counts: u32,
    /// What `loopback_holder_names` gives.
    pub holder_names: Vec<String>,
    pub holder_name_reads: u32,
    /// Writers of these contracts fail to start (`true`: as a loopback a
    /// consumer holds in another format).
    pub refused_contracts: Vec<(OutputContract, bool)>,
    /// What a held refusal reports the device held in
    /// (`WriterError::held_as`).
    pub held_as: Option<OutputContract>,
    /// The running session: its params and current camera id (`None`
    /// after a failed switch).
    pub session: Option<(SessionParams, Option<String>)>,
    pub session_starts: u32,
    pub session_stops: u32,
    /// Every camera an ingest was started on, in order.
    pub ingests: Vec<String>,
    /// The capture mode of every ingest switched to, in order.
    pub ingest_modes: Vec<VideoMode>,
    /// The `fixed_framerate` each ingest was started with, in the same
    /// order as `ingests`.
    pub ingest_fixed_framerate: Vec<bool>,
    /// Whether each ingest was built with the enhance stage, in the same
    /// order as `ingests`.
    pub ingest_enhance: Vec<bool>,
    /// The enhance stage's load-time parameters each ingest was built with,
    /// in the same order as `ingests`.
    pub ingest_enhance_params: Vec<Params>,
    /// An ingest built with the enhance stage reports it failed with this
    /// (the stage passing the frames through, as the element does).
    pub enhance_fail: Option<String>,
    /// What `effect_report` reports for the current ingest's enhance
    /// stage.
    pub enhance_status: EffectStatus,
    /// `start_session` fails with this.
    pub fail_start: Option<String>,
    /// `start_session` fails with this when the session has the effect
    /// element or a module stage element (a backend to run).
    pub fail_start_effects: Option<String>,
    /// With `fail_start`: cleaning up the partly built output did not stop
    /// cleanly, with this (`backend::park::StartError::unclean`).
    pub fail_start_cleanup: Option<String>,
    /// With `fail_start`: the failure is the output contract's
    /// (`backend::SessionError::contract`).
    pub fail_start_contract: bool,
    /// A start in CUDA memory fails with this, for want of CUDA
    /// (`backend::SessionError::no_cuda`: a missing `nvcodec` element, no
    /// CUDA context); one in system memory does not. As GStreamer's CUDA
    /// library does, the failed start initialises CUDA all the same
    /// (`cuda_initialised`), so a park after it would reach the driver.
    pub fail_start_no_cuda: Option<String>,
    /// The memory of every session start tried (the unpark refused
    /// included), in order.
    pub start_memories: Vec<VideoMemory>,
    /// What the CUDA presence probe finds (`cuda_unusable`): `None`, CUDA
    /// is usable (the default), else why not.
    pub cuda_unusable: Option<String>,
    /// The CUDA presence probes made.
    pub cuda_probes: u32,
    /// What the power supplies show (`power_source`): AC power by default.
    pub power_source: PowerSource,
    /// power-profiles-daemon's profile: not running by default. Read
    /// (`power_profile`) only while it is followed (`profile_followed`),
    /// as the real cache, which is [`PowerProfile::Unavailable`] otherwise;
    /// a follow is read at once (as the daemon's start waits for it).
    pub power_profile: PowerProfile,
    /// The power reads made (`power_source`).
    pub power_reads: u32,
    /// Whether power-profiles-daemon's profile is followed, as last asked
    /// (`follow_power_profile`).
    pub profile_followed: bool,
    /// Privacy's `freeze` holds a frame (`EffectReport::freeze_held`):
    /// false again at each session start.
    pub freeze_held: bool,
    /// The next session stop is unclean, with this (taken by that stop):
    /// a pipeline left short of NULL.
    pub fail_stop: Option<String>,
    /// The next session stop fails with this, every pipeline in NULL all
    /// the same (taken by that stop): a camera control not restored, say.
    pub fail_stop_at_null: Option<String>,
    /// With `fail_ingest`: the failed switch also left a pipeline outside
    /// NULL, with this (`obc_pipeline::SwitchTiming::unclean_stop`).
    pub unclean_switch: Option<String>,
    /// The running session's first unclean switch: its stop is then
    /// unclean, however it goes (the real backend's `Running::unclean`).
    pub session_unclean: Option<String>,
    /// Run by `start_session`, as if it happened during the output build.
    pub during_start: Option<Hook>,
    /// Ingests on these camera ids fail to start.
    pub fail_ingest: Vec<String>,
    /// Ingests on these camera ids in these modes fail to start.
    pub fail_ingest_modes: Vec<(String, VideoMode)>,
    /// Whether a started ingest delivers frames.
    pub frames: bool,
    /// Reported (once) by `session_failure` while a session runs.
    pub failure: Option<Failure>,
    pub stats: Stats,
    pub stats_taken: u32,
    pub scans: u32,
    /// Scans give every camera the requested size and rate (as a camera
    /// offering every mode would).
    pub follow_request: bool,
    /// `effect_available` fails with this for any effect but `none`, and
    /// so does the load of such an effect (as `effect_fail`).
    pub effect_unavailable: Option<String>,
    pub effect_probes: u32,
    /// An effect applied (at start or by `set_effect`) fails with this.
    pub effect_fail: Option<String>,
    /// `effect_fail` fails the whole chain, as a load of blur or replace
    /// does in the element: a requested noise removal stage that has not
    /// failed itself shows the effect's reason too.
    pub effect_fail_chain: bool,
    /// An effect applied stays `Loading` (the test sets the status) rather
    /// than becoming `Ready` at once.
    pub effect_slow: bool,
    /// What `effect_report` reports while a session runs.
    pub effect_status: EffectStatus,
    /// Noise removal applied (at start or by `set_effect`) fails with this:
    /// the effect is unaffected, as in the element.
    pub denoise_fail: Option<String>,
    /// What `effect_report` reports for the noise removal stage.
    pub denoise_status: EffectStatus,
    /// Relighting applied (at start or by `set_effect`) fails with this,
    /// as `denoise_fail` does for noise removal.
    pub relight_fail: Option<String>,
    /// What `effect_report` reports for the relighting stage.
    pub relight_status: EffectStatus,
    /// The effect the running session applies.
    pub effect: Option<EffectRun>,
    /// Every `set_effect` on a running session, in order.
    pub effect_sets: Vec<EffectRun>,
    /// The effect element cannot drop its backend mid-session, as the
    /// real one until gst-obc can ([`MediaBackend::can_revoke_effect_backend`]).
    pub revoke_unsupported: bool,
    /// The running session's effect element has dropped its backend
    /// ([`MediaBackend::revoke_effect_backend`]): it fails any effect, as
    /// one without a backend does. Ends with the session.
    pub effect_revoked: bool,
    /// Every revocation (`true`) and restoration (`false`) of the running
    /// session's effect backend, in order.
    pub effect_revocations: Vec<bool>,
    /// What `effect_report` reports for the other stages (a module's), by
    /// id; a stage applied on is `Ready` unless set here.
    pub stage_status: std::collections::BTreeMap<String, EffectStatus>,
    /// The fake Maxine module is installed (the default): its manifest and
    /// settings are the real module's, its probe [`World::features`].
    pub maxine: bool,
    /// The settings the fake Maxine module describes in place of
    /// [`maxine_settings`](crate::modules::testing::maxine_settings).
    pub maxine_settings: Option<obc_module_host::SettingsDescription>,
    /// More modules, each with what opening it gives.
    pub modules: Vec<(ModuleEntry, Result<OpenedModule, String>)>,
    /// The modules of the directories are discovered and opened for real
    /// (`obc-module-host`), after the fake ones.
    pub real_modules: bool,
    /// Every module opened, with its configuration, in order.
    pub module_opens: Vec<(String, serde_json::Value)>,
    /// `set_effect` fails with this (once).
    pub fail_set_effect: Option<String>,
    /// What the fake Maxine module's probe reports without a compute
    /// capability (`None`: no root, nothing known: every stage offered).
    pub features: Option<Vec<(StageId, Availability)>>,
    /// What it reports with one, when set (else [`World::features`]): the
    /// models of another GPU.
    pub features_with_cc: Option<Vec<(StageId, Availability)>>,
    /// The next probes of a module by id (opens excepted), in order: each
    /// reports its stages once, before what the module would report
    /// otherwise (a probe that changes within one step).
    pub probe_script: Vec<(String, Vec<(StageId, Availability)>)>,
    /// The compute capability of every probe of the fake Maxine module (at
    /// its opening and after), in order.
    pub feature_probes: Vec<Option<(u32, u32)>>,
    /// What `compute_capability` reports while a session runs.
    pub compute_capability: Option<(u32, u32)>,
    /// `compute_capability` calls, and those made with no session (the GPU
    /// may be parked then: never allowed).
    pub cc_reads: u32,
    pub cc_reads_idle: u32,
    /// The fake GPU driver under the backend's real [`Parker`]
    /// (`backend::park`): CUDA is initialised once a session is built
    /// (as GStreamer's `cuInit` does); before that there is nothing to
    /// park.
    pub cuda_initialised: bool,
    /// The GPU is parked: a stopped session parks it, a start unparks it
    /// before building anything.
    pub parked: bool,
    pub parks: u32,
    pub unparks: u32,
    /// Every park asked of the driver, whatever it answered (with nothing
    /// to park, already parked or failed included).
    pub park_calls: u32,
    /// Parking fails with this (the GPU stays awake), counted in
    /// `park_failures`.
    pub fail_park: Option<String>,
    pub park_failures: u32,
    /// Unparking fails with this: the start fails, the GPU stays parked.
    pub fail_unpark: Option<String>,
    /// What touched the GPU, in order: `park`, `unpark` and `build` (an
    /// output built).
    pub gpu_log: Vec<&'static str>,
    /// What the parker logged.
    pub parker_said: Vec<(Level, String)>,
    /// The fake devices' image controls, by camera id, each with its
    /// current value (what `camera_controls` reports, what
    /// `set_camera_control` changes). A camera without an entry exposes
    /// none; a control left out is one it lacks.
    pub controls: std::collections::HashMap<String, Vec<DeviceControl>>,
    /// Every image control write that reached a fake device, in order:
    /// (camera id, control, value).
    pub control_writes: Vec<(String, String, i32)>,
    /// Writes of these controls fail (on any camera); nothing is changed.
    pub fail_control_writes: Vec<String>,
    /// Opening the node of these camera ids for its image controls fails.
    pub fail_control_query: Vec<String>,
    /// The number of image control queries ([`ControlNode::query`]).
    pub control_queries: u32,
    /// The number of nodes opened for their image controls.
    pub control_opens: u32,
    /// The number of single image control reads ([`ControlNode::get`]).
    pub control_reads: u32,
    /// A query skips these controls (on any camera), as unreadable.
    pub fail_control_reads: Vec<String>,
    /// The next queries' [`World::fail_control_reads`], one per query in
    /// turn (a control whose read fails, then works, or the other way
    /// round); once empty, `fail_control_reads` stays as the last one left
    /// it.
    pub next_read_failures: std::collections::VecDeque<Vec<String>>,
    /// The number of single control queries without a value
    /// ([`ControlNode::query`]).
    pub control_info_reads: u32,
    /// A write of this control leaves this value on the device instead (a
    /// driver that rounds or clamps), by control name.
    pub control_write_quirks: std::collections::HashMap<String, i32>,
    /// Fake devices with any controls, by camera id: a camera with an entry
    /// here is this device, and its `controls` entry is not used.
    pub devices: std::collections::HashMap<String, FakeDevice>,
    /// Every field of view write that reached a fake device, in order:
    /// (camera id, option id).
    pub xu_writes: Vec<(String, String)>,
    /// The number of capture format reads.
    pub format_reads: u32,
    /// The control-event watcher that runs: (camera id, control ids).
    pub watch: Option<(String, Vec<u32>)>,
    /// The generation of the last watcher started (0: none yet).
    pub watch_generation: WatchGeneration,
    pub watch_starts: u32,
    /// Running watchers stopped (alone or replaced).
    pub watch_stops: u32,
    /// Starting a watcher fails with this.
    pub fail_watch: Option<String>,
    /// The orientation watcher that runs: (camera id, its report).
    pub orientation_watch: Option<(String, obc_pipeline::quirks::OrientationReport)>,
    /// The generation of the last orientation watcher started (0: none
    /// yet).
    pub orientation_generation: WatchGeneration,
    pub orientation_starts: u32,
    /// Running orientation watchers stopped (alone or replaced).
    pub orientation_stops: u32,
    /// Starting an orientation watcher fails with this (a node that is
    /// missing or cannot be read).
    pub fail_orientation: Option<String>,
    /// What `ingest_health` reports as the delivered rate while an ingest
    /// runs.
    pub delivered_fps: Option<f64>,
    /// What `ingest_health` reports as the damaged frames dropped while an
    /// ingest runs (the test raises them as the stream goes).
    pub corrupt: obc_pipeline::frame_guard::CorruptFrames,
    /// What `ingest_health` reports of the camera's privacy shutter while
    /// an ingest runs.
    pub shutter: Option<obc_pipeline::ShutterState>,
    /// The writer's mask ([`MediaBackend::set_mask`]), kept across writers
    /// as the real backend keeps it.
    pub mask: bool,
    /// Every `set_mask` that changed it, in order.
    pub mask_sets: Vec<bool>,
    /// Where each camera sits on the USB bus (`usb_placement`), by id; a
    /// camera without an entry is not on USB.
    pub placements: std::collections::HashMap<String, obc_pipeline::UsbPlacement>,
    /// The `usb_placement` reads.
    pub placement_reads: u32,
    /// The time zone `time_zone` gives (`None`: none known).
    pub time_zone: Option<String>,
}

impl Default for World {
    fn default() -> Self {
        World {
            cameras: Vec::new(),
            sink: Ok(PathBuf::from("/dev/video11")),
            writer: None,
            writer_starts: 0,
            pipewire: None,
            pipewire_generation: 0,
            pipewire_starts: 0,
            pipewire_stops: 0,
            pipewire_offer_nv12: None,
            preview: None,
            preview_sets: Vec::new(),
            writer_failure: None,
            placeholder: None,
            placeholder_sets: Vec::new(),
            writer_start_images: Vec::new(),
            placeholder_resends: Vec::new(),
            output_placeholder_sets: Vec::new(),
            image_events: Vec::new(),
            placeholder_decoding: false,
            placeholder_unreadable: None,
            output_placeholder_unreadable: None,
            cover_image: None,
            cover_image_sets: Vec::new(),
            cover_image_resends: Vec::new(),
            restart_holds: Vec::new(),
            restart_hold_ends: Vec::new(),
            fallback_error: None,
            background_error: None,
            switch_error: None,
            load_notice: None,
            stage_values: Vec::new(),
            holders: 0,
            holder_counts: 0,
            holder_names: Vec::new(),
            holder_name_reads: 0,
            refused_contracts: Vec::new(),
            held_as: None,
            session: None,
            session_starts: 0,
            session_stops: 0,
            ingests: Vec::new(),
            ingest_modes: Vec::new(),
            ingest_fixed_framerate: Vec::new(),
            ingest_enhance: Vec::new(),
            ingest_enhance_params: Vec::new(),
            enhance_fail: None,
            enhance_status: EffectStatus::Off,
            fail_start: None,
            fail_start_cleanup: None,
            fail_start_contract: false,
            fail_start_no_cuda: None,
            start_memories: Vec::new(),
            cuda_unusable: None,
            cuda_probes: 0,
            power_source: PowerSource::External,
            power_profile: PowerProfile::Unavailable,
            power_reads: 0,
            profile_followed: false,
            freeze_held: false,
            fail_start_effects: None,
            fail_stop: None,
            fail_stop_at_null: None,
            unclean_switch: None,
            session_unclean: None,
            during_start: None,
            fail_ingest: Vec::new(),
            fail_ingest_modes: Vec::new(),
            frames: true,
            failure: None,
            stats: Stats {
                fps: 30.0,
                latency_p50_ms: 12.0,
                cpu_percent: 5.0,
                gpu_percent: Stats::NOT_MEASURABLE,
            },
            stats_taken: 0,
            scans: 0,
            follow_request: false,
            effect_unavailable: None,
            effect_probes: 0,
            effect_fail: None,
            effect_fail_chain: false,
            effect_slow: false,
            effect_status: EffectStatus::Off,
            denoise_fail: None,
            denoise_status: EffectStatus::Off,
            relight_fail: None,
            relight_status: EffectStatus::Off,
            effect: None,
            effect_sets: Vec::new(),
            revoke_unsupported: false,
            effect_revoked: false,
            effect_revocations: Vec::new(),
            stage_status: std::collections::BTreeMap::new(),
            maxine: true,
            maxine_settings: None,
            modules: Vec::new(),
            real_modules: false,
            module_opens: Vec::new(),
            fail_set_effect: None,
            features: None,
            features_with_cc: None,
            probe_script: Vec::new(),
            feature_probes: Vec::new(),
            compute_capability: None,
            cc_reads: 0,
            cc_reads_idle: 0,
            cuda_initialised: false,
            parked: false,
            parks: 0,
            unparks: 0,
            park_calls: 0,
            fail_park: None,
            park_failures: 0,
            fail_unpark: None,
            gpu_log: Vec::new(),
            parker_said: Vec::new(),
            controls: std::collections::HashMap::new(),
            control_writes: Vec::new(),
            fail_control_writes: Vec::new(),
            fail_control_query: Vec::new(),
            control_queries: 0,
            control_opens: 0,
            control_reads: 0,
            fail_control_reads: Vec::new(),
            next_read_failures: std::collections::VecDeque::new(),
            control_info_reads: 0,
            control_write_quirks: std::collections::HashMap::new(),
            devices: std::collections::HashMap::new(),
            xu_writes: Vec::new(),
            format_reads: 0,
            watch: None,
            watch_generation: 0,
            watch_starts: 0,
            watch_stops: 0,
            fail_watch: None,
            orientation_watch: None,
            orientation_generation: 0,
            orientation_starts: 0,
            orientation_stops: 0,
            fail_orientation: None,
            delivered_fps: None,
            corrupt: obc_pipeline::frame_guard::CorruptFrames::default(),
            shutter: None,
            mask: false,
            mask_sets: Vec::new(),
            placements: std::collections::HashMap::new(),
            placement_reads: 0,
            time_zone: None,
        }
    }
}

fn lock(world: &Mutex<World>) -> MutexGuard<'_, World> {
    world.lock().unwrap_or_else(|e| e.into_inner())
}

/// The fake GPU driver (the world's GPU fields), as `obc_gpu::park` and
/// `obc_gpu::unpark` would answer. Never called with the world locked.
#[derive(Debug, Clone)]
pub struct WorldGpu(Arc<Mutex<World>>);

impl ParkOps for WorldGpu {
    fn park(&mut self) -> Result<ParkOutcome, ParkError> {
        let mut w = lock(&self.0);
        w.park_calls += 1;
        if !w.cuda_initialised {
            return Ok(ParkOutcome::NothingToPark(NothingToPark::NotInitialised));
        }
        if w.parked {
            return Ok(ParkOutcome::NothingToPark(NothingToPark::AlreadyParked));
        }
        if let Some(e) = w.fail_park.clone() {
            w.park_failures += 1;
            return Err(ParkError::Awake(GpuError::Unavailable(e)));
        }
        w.parked = true;
        w.parks += 1;
        w.gpu_log.push("park");
        Ok(ParkOutcome::Parked {
            lock: Duration::from_millis(1),
            checkpoint: Duration::from_millis(15),
        })
    }

    fn unpark(&mut self) -> Result<UnparkOutcome, GpuError> {
        let mut w = lock(&self.0);
        if !w.parked {
            return Ok(UnparkOutcome::NotParked);
        }
        if let Some(e) = w.fail_unpark.clone() {
            return Err(GpuError::Unavailable(e));
        }
        w.parked = false;
        w.unparks += 1;
        w.gpu_log.push("unpark");
        Ok(UnparkOutcome::Unparked {
            restore: Duration::from_millis(17),
            unlock: Duration::ZERO,
        })
    }

    fn refusals(&self) -> u64 {
        0
    }
}

impl World {
    /// The fake Maxine module's probe with compute capability `cc`: what
    /// [`World::features`] (or [`World::features_with_cc`]) says, every
    /// other stage available.
    fn maxine_probe(&mut self, cc: Option<(u32, u32)>) -> Vec<(StageId, Availability)> {
        self.feature_probes.push(cc);
        let found = match (&self.features_with_cc, cc) {
            (Some(f), Some(_)) => Some(f.clone()),
            _ => self.features.clone(),
        };
        [
            StageId::ENHANCE,
            StageId::DENOISE,
            StageId::BACKGROUND,
            StageId::RELIGHT,
            StageId::new("hdr").expect("a valid stage id"),
        ]
        .into_iter()
        .map(|s| {
            // As the module reports it: the detail goes to its log.
            let a = match found
                .as_ref()
                .and_then(|f| f.iter().find(|(id, _)| *id == s).map(|(_, a)| a.clone()))
            {
                Some(Availability::NotInstalled(_)) => {
                    Availability::NotInstalled("Not installed".into())
                }
                _ => Availability::Installed,
            };
            (s, a)
        })
        .collect()
    }

    /// Stops the running PipeWire output, if any.
    fn stop_pipewire(&mut self) {
        if self.pipewire.take().is_some() {
            self.pipewire_stops += 1;
        }
    }

    /// The rate the preview is encoded at, while one is asked for and a
    /// writer runs.
    pub fn previewing(&self) -> Option<u32> {
        self.writer.as_ref()?;
        self.preview.as_ref().map(|p| p.fps)
    }

    /// What the running session's output shows, as the effect element
    /// decides it (`None` without a session): the camera image for `none`
    /// and for a failed effect with privacy off, black, the fallback image
    /// or the held frame for a failed effect with privacy on (`black`,
    /// `away`, `freeze`) and while one loads, the effect once it is ready.
    pub fn shown(&self) -> Option<Shown> {
        self.session.as_ref()?;
        let effect = self.effect.as_ref()?;
        if effect.mode == EffectMode::None {
            return Some(Shown::Camera);
        }
        let hidden = match effect.on_failure {
            EffectOnFailure::Black => Shown::Black,
            EffectOnFailure::Image => Shown::Image,
            EffectOnFailure::Freeze => Shown::Frozen,
            EffectOnFailure::None => Shown::Camera,
        };
        Some(match &self.effect_status {
            EffectStatus::Ready => Shown::Effect,
            EffectStatus::Failed(_) => hidden,
            // The element holds black (the image) while it loads.
            _ if hidden == Shown::Camera => Shown::Black,
            _ => hidden,
        })
    }

    /// The camera the session streams from.
    pub fn streaming(&self) -> Option<&str> {
        self.session.as_ref().and_then(|(_, c)| c.as_deref())
    }

    /// The running session applies `effect`, as the effect element would:
    /// `none` is off at once, an effect loads (or fails); noise removal
    /// and relighting, apart, are each off unless requested, else ready
    /// (or fail, themselves or with the whole chain).
    fn apply_effect(&mut self, effect: &EffectRun) {
        let stage = |on: bool, fail: Option<&String>| {
            if !on {
                EffectStatus::Off
            } else if let Some(r) = fail {
                EffectStatus::Failed(r.clone())
            } else {
                EffectStatus::Ready
            }
        };
        // A revoked backend runs no stage either.
        let revoked = self.effect_revoked.then(|| NO_BACKEND.to_string());
        self.denoise_status = stage(
            effect.denoise(),
            self.denoise_fail.as_ref().or(revoked.as_ref()),
        );
        self.relight_status = stage(
            effect.relight(),
            self.relight_fail.as_ref().or(revoked.as_ref()),
        );
        self.effect = Some(effect.clone());
        // As the real output: the element of a session without an effect
        // module has no backend, and fails any effect; so does one whose
        // backend was revoked.
        let no_backend = self.effect_revoked
            || self
                .session
                .as_ref()
                .is_some_and(|(p, _)| p.backends.effect.is_none());
        // An effect the probe finds unavailable fails its load for the
        // same reason, as in the element.
        self.effect_status = if effect.mode == EffectMode::None {
            EffectStatus::Off
        } else if no_backend {
            EffectStatus::Failed(NO_BACKEND.into())
        } else if let Some(r) = self
            .effect_fail
            .as_ref()
            .or(self.effect_unavailable.as_ref())
        {
            EffectStatus::Failed(r.clone())
        } else if self.effect_slow {
            EffectStatus::Loading
        } else {
            EffectStatus::Ready
        };
        if let EffectStatus::Failed(r) = &self.effect_status
            && self.effect_fail_chain
        {
            for s in [&mut self.denoise_status, &mut self.relight_status] {
                if *s == EffectStatus::Ready {
                    *s = EffectStatus::Failed(r.clone());
                }
            }
        }
    }
}

/// The running session (the world's `session`), as the parker's
/// lifecycle sees it: its stop is the world's, unclean with `fail_stop` or
/// after an unclean switch (`session_unclean`), failed at NULL with
/// `fail_stop_at_null`. It uses CUDA unless it is in system memory.
struct FakeSession(Arc<Mutex<World>>, VideoMemory);

impl StopSession for FakeSession {
    fn uses_cuda(&self) -> bool {
        self.1 == VideoMemory::Cuda
    }

    fn stop(self) -> Result<(), StopFailure> {
        let mut w = lock(&self.0);
        if w.session.take().is_some() {
            w.session_stops += 1;
        }
        // The models go with the session.
        w.effect = None;
        w.enhance_status = EffectStatus::Off;
        w.effect_status = EffectStatus::Off;
        w.denoise_status = EffectStatus::Off;
        w.relight_status = EffectStatus::Off;
        let at_null = w.fail_stop_at_null.take();
        match (w.fail_stop.take(), w.session_unclean.take()) {
            (Some(e), _) | (None, Some(e)) => Err(StopFailure::unclean(e)),
            (None, None) => at_null.map_or(Ok(()), |e| Err(StopFailure::at_null(e))),
        }
    }
}

/// Something that happens while the fake does something slow.
pub struct Hook(pub Box<dyn FnMut() + Send>);

impl std::fmt::Debug for Hook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Hook")
    }
}

/// The backend half; clone it to keep the test's half (the clones share
/// the world and the parker).
#[derive(Debug, Clone)]
pub struct FakeBackend {
    world: Arc<Mutex<World>>,
    /// The daemon's park state machine, over the world's fake GPU. Locked
    /// before the world, never while the world is locked.
    parker: Arc<Mutex<Parker<WorldGpu>>>,
}

impl Default for FakeBackend {
    fn default() -> Self {
        let world: Arc<Mutex<World>> = Arc::default();
        let said = Arc::clone(&world);
        let parker = Parker::with(
            WorldGpu(Arc::clone(&world)),
            Box::new(move |level, msg| lock(&said).parker_said.push((level, msg.to_string()))),
        );
        FakeBackend {
            world,
            parker: Arc::new(Mutex::new(parker)),
        }
    }
}

impl FakeBackend {
    pub fn world(&self) -> MutexGuard<'_, World> {
        lock(&self.world)
    }

    fn parker(&self) -> MutexGuard<'_, Parker<WorldGpu>> {
        self.parker.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The running session, for the parker's lifecycle.
    fn running(&self) -> Option<FakeSession> {
        let memory = self.world().session.as_ref().map(|(p, _)| p.memory);
        memory.map(|m| FakeSession(Arc::clone(&self.world), m))
    }

    /// Whether the parker is tainted (a session did not stop cleanly).
    pub fn parking_tainted(&self) -> bool {
        self.parker().tainted()
    }

    fn start_ingest(w: &mut World, camera: &Camera) -> Result<(), String> {
        let Some((_, current)) = w.session.as_mut() else {
            return Err("no session".into());
        };
        *current = None;
        w.ingests.push(camera.id.clone());
        if w.fail_ingest.contains(&camera.id)
            || w.fail_ingest_modes
                .iter()
                .any(|(id, m)| *id == camera.id && *m == camera.mode)
        {
            if let Some(why) = w.unclean_switch.clone()
                && w.session_unclean.is_none()
            {
                w.session_unclean = Some(format!("switching to {}: {why}", camera.label()));
            }
            return Err(format!("{}: cannot open", camera.label()));
        }
        if let Some((_, current)) = w.session.as_mut() {
            *current = Some(camera.id.clone());
        }
        Ok(())
    }
}

impl MediaBackend for FakeBackend {
    fn scan_cameras(&mut self, request: &ModeRequest) -> Vec<Camera> {
        let mut w = self.world();
        w.scans += 1;
        let mut cams = w.cameras.clone();
        if w.follow_request {
            for c in &mut cams {
                c.mode.width = request.width;
                c.mode.height = request.height;
                c.mode.fps_num = request.fps_num;
                c.mode.fps_den = request.fps_den;
            }
        }
        cams
    }

    fn start_writer(
        &mut self,
        _sink: &SinkSelector,
        contract: &OutputContract,
    ) -> Result<PathBuf, WriterError> {
        let mut w = self.world();
        w.session = None;
        w.writer = None;
        w.stop_pipewire();
        let node = w.sink.clone()?;
        if let Some((_, held)) = w.refused_contracts.iter().find(|(c, _)| c == contract) {
            return Err(WriterError {
                message: format!(
                    "loopback {}: cannot stream {}",
                    node.display(),
                    output_mode_string(contract)
                ),
                held: *held,
                revertible: !*held,
                held_as: if *held { w.held_as } else { None },
            });
        }
        w.writer = Some((node.clone(), *contract));
        w.writer_starts += 1;
        let images = (w.placeholder.clone(), w.cover_image.clone());
        w.writer_start_images.push(images);
        Ok(node)
    }

    fn stop_writer(&mut self) {
        self.stop_session();
        let mut w = self.world();
        w.writer = None;
        w.stop_pipewire();
    }

    fn start_pipewire(&mut self, offer_nv12: bool) -> PipeWireGeneration {
        let mut w = self.world();
        w.pipewire_offer_nv12 = Some(offer_nv12);
        assert!(w.writer.is_some(), "the PipeWire output needs the writer");
        w.stop_pipewire();
        w.pipewire_generation += 1;
        w.pipewire_starts += 1;
        let generation = w.pipewire_generation;
        w.pipewire = Some(generation);
        generation
    }

    fn stop_pipewire(&mut self) {
        self.world().stop_pipewire();
    }

    fn set_preview(&mut self, preview: Option<PreviewRequest>) {
        let mut w = self.world();
        w.preview_sets.push(preview.as_ref().map(|p| p.fps));
        w.preview = preview;
    }

    fn writer_failure(&mut self) -> Option<String> {
        let mut w = self.world();
        if w.writer.is_some() {
            w.writer_failure.take()
        } else {
            None
        }
    }

    fn set_placeholder(&mut self, image: Option<&Path>) {
        let mut w = self.world();
        w.image_events
            .push(ImageEvent::Placeholder(image.map(Path::to_path_buf)));
        if w.placeholder.as_deref() != image {
            let image = image.map(Path::to_path_buf);
            w.placeholder = image.clone();
            w.placeholder_sets.push(image);
        } else if let Some(p) = image {
            w.placeholder_resends.push(p.to_path_buf());
        }
    }

    fn placeholder_pending(&mut self) -> bool {
        let w = self.world();
        w.writer.is_some() && w.placeholder.is_some() && w.placeholder_decoding
    }

    fn placeholder_failure(&mut self) -> Option<String> {
        let w = self.world();
        w.writer.as_ref()?;
        w.placeholder.as_ref()?;
        if w.placeholder_decoding {
            return None;
        }
        w.placeholder_unreadable.clone()
    }

    fn set_cover_image(&mut self, image: Option<&Path>) {
        let mut w = self.world();
        if w.cover_image.as_deref() != image {
            let image = image.map(Path::to_path_buf);
            w.cover_image = image.clone();
            w.cover_image_sets.push(image);
        } else if let Some(p) = image {
            w.cover_image_resends.push(p.to_path_buf());
        }
    }

    fn hold_across_restart(&mut self) {
        let mut w = self.world();
        if w.writer.is_some() {
            let session = w.session.is_some();
            w.restart_holds.push(session);
        }
    }

    fn end_restart_hold(&mut self) {
        let mut w = self.world();
        if w.writer.is_some() {
            let session = w.session.is_some();
            w.restart_hold_ends.push(session);
        }
    }

    fn set_mask(&mut self, on: bool) {
        let mut w = self.world();
        if w.mask != on {
            w.mask = on;
            w.mask_sets.push(on);
        }
    }

    fn output_placeholder_failure(&mut self) -> Option<String> {
        let w = self.world();
        let (params, _) = w.session.as_ref()?;
        params.placeholder_image.as_ref()?;
        w.output_placeholder_unreadable.clone()
    }

    fn set_output_placeholder(&mut self, image: Option<&Path>) {
        let mut w = self.world();
        let image = image.map(Path::to_path_buf);
        w.image_events
            .push(ImageEvent::OutputPlaceholder(image.clone()));
        let w = &mut *w;
        if let Some((params, _)) = w.session.as_mut() {
            params.placeholder_image.clone_from(&image);
            w.output_placeholder_sets.push(image);
        }
    }

    fn loopback_holders(&mut self, _node: &Path) -> u32 {
        let mut w = self.world();
        w.holder_counts += 1;
        w.holders
    }

    fn loopback_holder_names(&mut self, _node: &Path) -> Vec<String> {
        let mut w = self.world();
        w.holder_name_reads += 1;
        w.holder_names.clone()
    }

    fn start_session(&mut self, params: &SessionParams) -> Result<(), SessionError> {
        // Without the world's lock: the test may look at it meanwhile.
        let hook = self.world().during_start.take();
        if let Some(mut hook) = hook {
            (hook.0)();
            self.world().during_start = Some(hook);
        }
        {
            let w = self.world();
            assert!(w.writer.is_some(), "a session needs the writer");
            assert_eq!(
                Some(params.contract),
                w.writer.as_ref().map(|(_, c)| *c),
                "the session contract is the writer's"
            );
        }
        // The real parker's lifecycle: a running session stopped, the GPU
        // unparked before anything touches it, parked again if the start
        // fails cleanly; in system memory neither (`start_session_in`).
        let world = Arc::clone(&self.world);
        let mut running = self.running();
        let memory = params.memory;
        let mut contract = false;
        let mut no_cuda = false;
        lock(&world).start_memories.push(memory);
        self.parker()
            .start_session_in(&mut running, memory, || {
                let mut w = lock(&world);
                let effects =
                    params.backends.effect.is_some() || !params.backends.chains.is_empty();
                if memory == VideoMemory::Cuda
                    && let Some(e) = w.fail_start_no_cuda.clone()
                {
                    no_cuda = true;
                    w.cuda_initialised = true;
                    return Err(StartError::from(e).for_want_of_cuda(true));
                }
                if let Some(e) = w
                    .fail_start
                    .clone()
                    .or_else(|| w.fail_start_effects.clone().filter(|_| effects))
                {
                    contract = w.fail_start_contract;
                    return Err(match w.fail_start_cleanup.clone() {
                        Some(why) => StartError::unclean(e, why),
                        None => StartError::from(e),
                    });
                }
                if memory == VideoMemory::Cuda {
                    w.cuda_initialised = true;
                    w.gpu_log.push("build");
                }
                w.session_unclean = None;
                w.session = Some((params.clone(), None));
                w.session_starts += 1;
                w.freeze_held = false;
                w.effect_revoked = false;
                w.apply_effect(&params.effect);
                Ok(FakeSession(Arc::clone(&world), memory))
            })
            .map_err(|message| SessionError {
                message,
                contract,
                no_cuda,
            })
    }

    fn switch_camera(&mut self, params: &SessionParams, camera: &Camera) -> Result<(), String> {
        let mut w = self.world();
        w.ingest_fixed_framerate.push(params.fixed_framerate);
        w.ingest_enhance.push(params.effect.enhance);
        w.ingest_enhance_params
            .push(params.effect.enhance_params.clone());
        w.ingest_modes.push(camera.mode);
        let res = Self::start_ingest(&mut w, camera);
        w.enhance_status = match (&res, params.effect.enhance, &w.enhance_fail) {
            (Ok(()), true, Some(r)) => EffectStatus::Failed(r.clone()),
            (Ok(()), true, None) => EffectStatus::Ready,
            _ => EffectStatus::Off,
        };
        res
    }

    fn stop_session(&mut self) {
        self.world().image_events.push(ImageEvent::StopSession);
        // All of it stopped, then the real parker parks if it stopped
        // cleanly.
        let mut running = self.running();
        self.parker().stop_session(&mut running);
    }

    fn first_frame(&mut self) -> bool {
        let w = self.world();
        w.frames && w.streaming().is_some()
    }

    fn session_failure(&mut self) -> Option<Failure> {
        let mut w = self.world();
        if w.session.is_some() {
            w.failure.take()
        } else {
            None
        }
    }

    fn take_stats(&mut self) -> Stats {
        let mut w = self.world();
        w.stats_taken += 1;
        w.stats
    }

    fn effect_available(&mut self, effect: &EffectRun, _: &Backends) -> Result<(), String> {
        let mut w = self.world();
        w.effect_probes += 1;
        match &w.effect_unavailable {
            Some(r) if effect.mode != EffectMode::None => Err(r.clone()),
            _ => Ok(()),
        }
    }

    fn set_effect(&mut self, effect: &EffectRun) -> Result<(), String> {
        let mut w = self.world();
        if w.session.is_none() {
            return Ok(());
        }
        w.effect_sets.push(effect.clone());
        if let Some(e) = w.fail_set_effect.take() {
            return Err(e);
        }
        w.apply_effect(effect);
        Ok(())
    }

    fn can_revoke_effect_backend(&self) -> bool {
        !self.world().revoke_unsupported
    }

    fn revoke_effect_backend(&mut self) -> Result<(), String> {
        let mut w = self.world();
        if w.session.is_none() {
            return Ok(());
        }
        assert!(!w.revoke_unsupported, "revoked although unsupported");
        let w = &mut *w;
        w.effect_revocations.push(true);
        w.effect_revoked = true;
        // As the element: the effect (unless `none`) and every stage it
        // runs fail at once, the stages alone too.
        if let Some(effect) = w.effect.as_ref() {
            if effect.mode != EffectMode::None {
                w.effect_status = EffectStatus::Failed(NO_BACKEND.into());
            }
            for s in [&mut w.denoise_status, &mut w.relight_status] {
                if *s != EffectStatus::Off {
                    *s = EffectStatus::Failed(NO_BACKEND.into());
                }
            }
        }
        Ok(())
    }

    fn restore_effect_backend(&mut self) -> Result<(), String> {
        let mut w = self.world();
        if w.session.is_none() {
            return Ok(());
        }
        w.effect_revocations.push(false);
        w.effect_revoked = false;
        // The element loads what it is asked for again.
        if let Some(effect) = w.effect.clone() {
            w.apply_effect(&effect);
        }
        Ok(())
    }

    fn effect_report(&mut self) -> Option<EffectReport> {
        let w = self.world();
        w.session.as_ref()?;
        let stages = w
            .effect
            .iter()
            .flat_map(|e| {
                e.stages
                    .iter()
                    .chain(e.chains.iter().flat_map(|c| c.stages.iter()))
            })
            .map(|s| {
                let status = match s.id.as_str() {
                    "denoise" => w.denoise_status.clone(),
                    "relight" => w.relight_status.clone(),
                    other => match (s.enabled, w.stage_status.get(other)) {
                        (false, _) => EffectStatus::Off,
                        (true, Some(st)) => st.clone(),
                        (true, None) => EffectStatus::Ready,
                    },
                };
                (s.id.clone(), status)
            })
            .collect();
        Some(EffectReport {
            status: w.effect_status.clone(),
            stages,
            enhance: w.enhance_status.clone(),
            load_ms: Some(120.0),
            reused: false,
            gpu_ms_p50: Some(1.2),
            freeze_held: w.freeze_held,
            fallback_error: w.fallback_error.clone(),
            background_error: w.background_error.clone(),
            switch_error: w.switch_error.clone(),
            load_notice: w.load_notice.clone(),
            stage_values: w.stage_values.clone(),
        })
    }

    fn discover_modules(&mut self, dirs: &[PathBuf]) -> Vec<ModuleEntry> {
        let w = self.world();
        let mut entries: Vec<ModuleEntry> = Vec::new();
        if w.maxine {
            entries.push(crate::modules::testing::maxine_entry());
        }
        entries.extend(w.modules.iter().map(|(e, _)| e.clone()));
        if w.real_modules {
            entries.extend(obc_module_host::scan(dirs).entries);
        }
        entries
    }

    fn open_module(
        &mut self,
        entry: &ModuleEntry,
        config: &serde_json::Value,
    ) -> Result<OpenedModule, String> {
        let mut w = self.world();
        w.module_opens.push((entry.id.clone(), config.clone()));
        if w.maxine && entry.id == crate::modules::MAXINE_MODULE {
            let cc = config["_host"]["compute_capability"]
                .as_array()
                .and_then(|a| Some((a.first()?.as_u64()? as u32, a.get(1)?.as_u64()? as u32)));
            return Ok(OpenedModule {
                module: None,
                stages: w.maxine_probe(cc),
                settings: w
                    .maxine_settings
                    .clone()
                    .unwrap_or_else(crate::modules::testing::maxine_settings),
            });
        }
        if let Some((_, opened)) = w.modules.iter().find(|(e, _)| e.id == entry.id) {
            return opened.clone();
        }
        drop(w);
        let manifest = entry.manifest.as_ref().map_err(Clone::clone)?;
        let module = Module::open(manifest, config, obc_module_host::stderr_log())?;
        let stages = module.probe().map_err(|e| e.message)?;
        let settings = module.settings().map_err(|e| e.message)?;
        Ok(OpenedModule {
            module: Some(module),
            stages,
            settings,
        })
    }

    fn probe_module(
        &mut self,
        id: &str,
        module: Option<&Module>,
    ) -> Result<Vec<(StageId, Availability)>, String> {
        let mut w = self.world();
        if let Some(i) = w.probe_script.iter().position(|(m, _)| m == id) {
            return Ok(w.probe_script.remove(i).1);
        }
        if w.maxine && id == crate::modules::MAXINE_MODULE {
            let cc = w
                .module_opens
                .iter()
                .rev()
                .find(|(m, _)| m == id)
                .and_then(|(_, c)| c["_host"]["compute_capability"].as_array().cloned())
                .and_then(|a| Some((a.first()?.as_u64()? as u32, a.get(1)?.as_u64()? as u32)));
            return Ok(w.maxine_probe(cc));
        }
        if let Some((_, opened)) = w.modules.iter().find(|(e, _)| e.id == id) {
            return opened.clone().map(|o| o.stages);
        }
        match module {
            Some(m) => m.probe().map_err(|e| e.message),
            None => Err("the module is not loaded".into()),
        }
    }

    fn cuda_unusable(&mut self) -> Option<String> {
        let mut w = self.world();
        w.cuda_probes += 1;
        w.cuda_unusable.clone()
    }

    fn power_source(&mut self) -> PowerSource {
        let mut w = self.world();
        w.power_reads += 1;
        w.power_source.clone()
    }

    fn power_profile(&mut self) -> PowerProfile {
        let w = self.world();
        if w.profile_followed {
            w.power_profile.clone()
        } else {
            PowerProfile::Unavailable
        }
    }

    fn follow_power_profile(&mut self, on: bool) {
        self.world().profile_followed = on;
    }

    fn compute_capability(&mut self) -> Option<(u32, u32)> {
        let mut w = self.world();
        w.cc_reads += 1;
        if w.session.is_none() {
            w.cc_reads_idle += 1;
            return None;
        }
        w.compute_capability
    }

    fn open_camera_controls(
        &mut self,
        camera: &Camera,
        write: bool,
    ) -> Result<Box<dyn ControlNode>, String> {
        let mut w = self.world();
        w.control_opens += 1;
        if w.fail_control_query.contains(&camera.id) {
            return Err(format!(
                "opening {} for its controls: Input/output error",
                camera.path.display()
            ));
        }
        Ok(Box::new(FakeControlNode {
            world: Arc::clone(&self.world),
            camera: camera.clone(),
            write,
        }))
    }

    fn watch_controls(&mut self, camera: &Camera, ids: &[u32]) -> Result<WatchGeneration, String> {
        self.unwatch_controls();
        let mut w = self.world();
        if let Some(e) = w.fail_watch.clone() {
            return Err(e);
        }
        w.watch_generation += 1;
        w.watch_starts += 1;
        w.watch = Some((camera.id.clone(), ids.to_vec()));
        Ok(w.watch_generation)
    }

    fn unwatch_controls(&mut self) {
        let mut w = self.world();
        if w.watch.take().is_some() {
            w.watch_stops += 1;
        }
    }

    fn watch_orientation(
        &mut self,
        camera: &Camera,
        report: obc_pipeline::quirks::OrientationReport,
    ) -> Result<WatchGeneration, String> {
        self.unwatch_orientation();
        let mut w = self.world();
        w.orientation_starts += 1;
        if let Some(e) = w.fail_orientation.clone() {
            return Err(e);
        }
        w.orientation_generation += 1;
        w.orientation_watch = Some((camera.id.clone(), report));
        Ok(w.orientation_generation)
    }

    fn unwatch_orientation(&mut self) {
        let mut w = self.world();
        if w.orientation_watch.take().is_some() {
            w.orientation_stops += 1;
        }
    }

    fn ingest_health(&mut self) -> Option<super::IngestHealth> {
        let w = self.world();
        // An ingest runs while the session has a camera.
        w.session.as_ref().and_then(|(_, cam)| cam.as_ref())?;
        Some(super::IngestHealth {
            delivered_fps: w.delivered_fps,
            corrupt: w.corrupt,
            shutter: w.shutter,
        })
    }

    fn usb_placement(&mut self, camera: &Camera) -> Option<obc_pipeline::UsbPlacement> {
        let mut w = self.world();
        w.placement_reads += 1;
        w.placements.get(&camera.id).cloned()
    }

    fn time_zone(&mut self) -> Option<String> {
        self.world().time_zone.clone()
    }
}

/// The synthetic id of a simple control ([`World::controls`]) that is not
/// one of the five image controls.
const SIMPLE_ID_BASE: u32 = 0x0098_0f00;

/// A simple control of [`World::controls`] as the device would report it:
/// an integer of the user class (the image control's id), no flag.
fn simple_control(i: usize, c: &DeviceControl, unreadable: bool) -> NodeControl {
    NodeControl {
        id: image_control_id(&c.name).unwrap_or(SIMPLE_ID_BASE + i as u32),
        name: c.name.clone(),
        info: CtrlInfo {
            name: c.name.clone(),
            minimum: c.min,
            maximum: c.max,
            step: c.step,
            default_value: c.default,
            flags: 0,
            type_: CTRL_TYPE_INTEGER,
        },
        value: if unreadable {
            CtrlValue::Unreadable {
                errno: Some(libc::EPIPE),
                message: "Broken pipe".into(),
            }
        } else {
            CtrlValue::Value(c.value)
        },
        menu: Vec::new(),
    }
}

/// A fake device node opened for its controls.
struct FakeControlNode {
    world: Arc<Mutex<World>>,
    camera: Camera,
    /// Opened for writing: a write through a node that was not fails.
    write: bool,
}

impl FakeControlNode {
    /// The name of control `id` on the camera, if it has it.
    fn name_of(w: &World, camera: &str, id: u32) -> Option<String> {
        if let Some(d) = w.devices.get(camera) {
            return d
                .controls
                .iter()
                .find(|c| c.id == id)
                .map(|c| c.name.clone());
        }
        w.controls.get(camera).and_then(|cs| {
            cs.iter()
                .enumerate()
                .find(|(i, c)| simple_control(*i, c, false).id == id)
                .map(|(_, c)| c.name.clone())
        })
    }

    fn fov_of<'a>(&self, w: &'a mut World, row: &'static XuRow) -> Result<&'a mut FakeFov, String> {
        let path = self.camera.path.display().to_string();
        w.devices
            .get_mut(&self.camera.id)
            .and_then(|d| d.fov.as_mut())
            .filter(|_| row.name == FIELD_OF_VIEW.name)
            .ok_or_else(|| format!("{path}: no {}", row.name))
    }

    fn xu_of<'a>(&self, w: &'a mut World, row: &'static XuRow) -> Result<&'a mut FakeXu, String> {
        let path = self.camera.path.display().to_string();
        w.devices
            .get_mut(&self.camera.id)
            .and_then(|d| d.xu.iter_mut().find(|x| x.row.name == row.name))
            .ok_or_else(|| format!("{path}: no {}", row.name))
    }
}

impl ControlNode for FakeControlNode {
    fn enumerate(&mut self) -> ControlScan {
        let mut w = lock(&self.world);
        w.control_queries += 1;
        if let Some(next) = w.next_read_failures.pop_front() {
            w.fail_control_reads = next;
        }
        let unreadable = w.fail_control_reads.clone();
        if let Some(d) = w.devices.get(&self.camera.id) {
            let controls = d
                .controls
                .iter()
                .cloned()
                .map(|mut c| {
                    if unreadable.contains(&c.name) {
                        c.value = CtrlValue::Unreadable {
                            errno: Some(libc::EPIPE),
                            message: "Broken pipe".into(),
                        };
                    }
                    c
                })
                .collect();
            let xu = d
                .fov
                .iter()
                .map(|f| xu_reading(&FIELD_OF_VIEW, f.current, f.default))
                .chain(d.xu.iter().map(|x| xu_reading(x.row, x.current, x.default)))
                .collect();
            return ControlScan {
                controls,
                xu,
                problems: Vec::new(),
            };
        }
        let controls = w
            .controls
            .get(&self.camera.id)
            .cloned()
            .unwrap_or_default()
            .iter()
            .enumerate()
            .map(|(i, c)| simple_control(i, c, unreadable.contains(&c.name)))
            .collect();
        ControlScan {
            controls,
            xu: Vec::new(),
            problems: Vec::new(),
        }
    }

    fn set(&mut self, id: u32, _name: &str, value: i32) -> Result<(), String> {
        let mut w = lock(&self.world);
        let path = self.camera.path.display().to_string();
        let Some(name) = Self::name_of(&w, &self.camera.id, id) else {
            return Err(format!("{path}: setting {id:#010x}: Invalid argument"));
        };
        if !self.write {
            return Err(format!(
                "{path}: setting {name}: opened read-only (a controller bug)"
            ));
        }
        if w.fail_control_writes.contains(&name) {
            return Err(format!("{path}: setting {name}: Input/output error"));
        }
        let stored = w.control_write_quirks.get(&name).copied().unwrap_or(value);
        if let Some(d) = w.devices.get_mut(&self.camera.id) {
            let inactive = d.control(id).info.flags & CTRL_FLAG_INACTIVE != 0;
            if d.refuse.contains(&id) || inactive {
                return Err(format!("{path}: setting {name}: Permission denied"));
            }
            d.change(id, stored);
        } else if let Some(c) = w
            .controls
            .get_mut(&self.camera.id)
            .and_then(|cs| cs.iter_mut().find(|c| c.name == name))
        {
            c.value = stored;
        }
        w.control_writes.push((self.camera.id.clone(), name, value));
        Ok(())
    }

    fn get(&mut self, id: u32, _name: &str) -> Result<i32, String> {
        let mut w = lock(&self.world);
        w.control_reads += 1;
        let path = self.camera.path.display();
        if let Some(d) = w.devices.get(&self.camera.id) {
            return d
                .controls
                .iter()
                .find(|c| c.id == id)
                .and_then(|c| c.value.value())
                .ok_or_else(|| format!("{path}: reading {id:#010x}: Invalid argument"));
        }
        let name = Self::name_of(&w, &self.camera.id, id);
        w.controls
            .get(&self.camera.id)
            .and_then(|cs| cs.iter().find(|c| Some(&c.name) == name.as_ref()))
            .map(|c| c.value)
            .ok_or_else(|| format!("{path}: reading {id:#010x}: Invalid argument"))
    }

    fn query(&mut self, id: u32, _name: &str) -> Result<CtrlInfo, String> {
        let mut w = lock(&self.world);
        w.control_info_reads += 1;
        let path = self.camera.path.display();
        if let Some(d) = w.devices.get(&self.camera.id) {
            return d
                .controls
                .iter()
                .find(|c| c.id == id)
                .map(|c| c.info.clone())
                .ok_or_else(|| format!("{path}: querying {id:#010x}: Invalid argument"));
        }
        w.controls
            .get(&self.camera.id)
            .and_then(|cs| {
                cs.iter()
                    .enumerate()
                    .map(|(i, c)| simple_control(i, c, false))
                    .find(|c| c.id == id)
            })
            .map(|c| c.info)
            .ok_or_else(|| format!("{path}: querying {id:#010x}: Invalid argument"))
    }

    fn capture_format(&mut self) -> Result<CaptureFormat, String> {
        let mut w = lock(&self.world);
        w.format_reads += 1;
        Ok(w.devices
            .get(&self.camera.id)
            .map(|d| d.format)
            .unwrap_or_else(|| FakeDevice::default().format))
    }

    fn xu_set(&mut self, row: &'static XuRow, option: &str) -> Result<(), String> {
        let mut w = lock(&self.world);
        if !self.write {
            return Err("opened read-only (a controller bug)".into());
        }
        let byte = row
            .option(option)
            .ok_or_else(|| format!("{option:?} is not an option of {}", row.name))?
            .value;
        if row.name == FIELD_OF_VIEW.name {
            let fov = self.fov_of(&mut w, row)?;
            fov.current = byte;
            let change = fov.changes_format.filter(|(b, _)| *b == byte);
            if let (Some((_, format)), Some(d)) = (change, w.devices.get_mut(&self.camera.id)) {
                d.format = format;
            }
        } else {
            self.xu_of(&mut w, row)?.current = byte;
        }
        w.xu_writes
            .push((self.camera.id.clone(), option.to_string()));
        Ok(())
    }

    fn xu_current(&mut self, row: &'static XuRow) -> Result<u8, String> {
        let mut w = lock(&self.world);
        if row.name == FIELD_OF_VIEW.name {
            return Ok(self.fov_of(&mut w, row)?.current);
        }
        Ok(self.xu_of(&mut w, row)?.current)
    }
}

/// The control ids of [`brio_device`].
pub mod cid {
    pub const SHARPNESS: u32 = 0x0098_091b;
    pub const GAIN: u32 = 0x0098_0913;
    pub const WB_AUTO: u32 = 0x0098_090c;
    pub const WB_TEMP: u32 = 0x0098_091a;
    pub const AUTO_EXPOSURE: u32 = 0x009a_0901;
    pub const EXPOSURE: u32 = 0x009a_0902;
    pub const DYNAMIC_FPS: u32 = 0x009a_0903;
    pub const PAN_RESET: u32 = 0x009a_0906;
}

use cid::{AUTO_EXPOSURE, DYNAMIC_FPS, EXPOSURE, GAIN, PAN_RESET, SHARPNESS, WB_AUTO, WB_TEMP};

/// A control as a device would report it.
pub fn node_control(
    id: u32,
    name: &str,
    type_: u32,
    range: (i32, i32, i32),
    default: i32,
    value: i32,
    flags: u32,
) -> NodeControl {
    NodeControl {
        id,
        name: obc_pipeline::control_node::normalise_control_name(name),
        info: CtrlInfo {
            name: name.into(),
            minimum: range.0,
            maximum: range.1,
            step: range.2,
            default_value: default,
            flags,
            type_,
        },
        value: if type_ == CTRL_TYPE_BUTTON {
            CtrlValue::None
        } else {
            CtrlValue::Value(value)
        },
        menu: Vec::new(),
    }
}

/// A fake MX Brio: its controls with the values a real one reports
/// (exposure, temperature and focus inactive under their automatic modes),
/// the dependencies the driver has, its field of view at 65°.
pub fn brio_device() -> FakeDevice {
    let int = |id, name, r, d, v, f| node_control(id, name, CTRL_TYPE_INTEGER, r, d, v, f);
    let boolean = |id, name, d, v| node_control(id, name, CTRL_TYPE_BOOLEAN, (0, 1, 1), d, v, 0);
    let mut mode = node_control(
        AUTO_EXPOSURE,
        "Auto Exposure",
        CTRL_TYPE_MENU,
        (0, 3, 1),
        3,
        3,
        0,
    );
    mode.menu = vec![
        MenuItem {
            index: 1,
            value: MenuItemValue::Name("Manual Mode".into()),
        },
        MenuItem {
            index: 3,
            value: MenuItemValue::Name("Aperture Priority Mode".into()),
        },
    ];
    FakeDevice {
        controls: vec![
            int(0x0098_0900, "Brightness", (0, 255, 1), 128, 128, 0),
            int(0x0098_0901, "Contrast", (0, 255, 1), 128, 128, 0),
            boolean(WB_AUTO, "White Balance, Automatic", 1, 1),
            int(GAIN, "Gain", (0, 255, 1), 0, 0, 0),
            int(
                WB_TEMP,
                "White Balance Temperature",
                (2000, 7500, 1),
                5000,
                2975,
                CTRL_FLAG_INACTIVE,
            ),
            int(SHARPNESS, "Sharpness", (0, 255, 1), 128, 128, 0),
            mode,
            int(
                EXPOSURE,
                "Exposure Time, Absolute",
                (3, 2047, 1),
                156,
                1250,
                CTRL_FLAG_INACTIVE,
            ),
            boolean(DYNAMIC_FPS, "Exposure, Dynamic Framerate", 0, 1),
            node_control(
                PAN_RESET,
                "Pan, Reset",
                CTRL_TYPE_BUTTON,
                (0, 0, 0),
                0,
                0,
                0,
            ),
        ],
        links: vec![
            (WB_TEMP, WB_AUTO, vec![0]),
            (EXPOSURE, AUTO_EXPOSURE, vec![1]),
        ],
        fov: Some(FakeFov {
            current: 2,
            default: 0,
            changes_format: None,
        }),
        ..FakeDevice::default()
    }
}

/// The stages of the fake Maxine module as a test reads them (what the
/// settings of revision 8 were): switches and parameters by stage id.
impl EffectRun {
    fn stage_param(&self, stage: &str, key: &str) -> Option<obc_pipeline::effect::Value> {
        let s = self.stage(stage)?;
        s.params.get(key).or_else(|| s.live.get(key)).cloned()
    }

    fn stage_mut(&mut self, id: &str) -> &mut super::StageRun {
        self.stages
            .iter_mut()
            .chain(self.chains.iter_mut().flat_map(|c| c.stages.iter_mut()))
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("no stage {id}"))
    }

    /// Turns stage `id` on or off, as a test expects it.
    pub fn set_enabled(&mut self, id: &str, on: bool) {
        self.stage_mut(id).enabled = on;
    }

    /// Sets the load-time parameter `key` of `stage`, as a test expects it.
    pub fn set_param(&mut self, stage: &str, key: &str, value: &str) {
        self.stage_mut(stage)
            .params
            .insert(key.into(), obc_pipeline::effect::Value::from(value));
    }

    /// Noise removal requested.
    pub fn denoise(&self) -> bool {
        self.stage("denoise").is_some_and(|s| s.enabled)
    }

    /// Relighting requested.
    pub fn relight(&self) -> bool {
        self.stage("relight").is_some_and(|s| s.enabled)
    }

    /// The relighting light.
    pub fn relight_light(&self) -> obc_core::RelightLight {
        match self.stage_param("relight", "light") {
            Some(obc_pipeline::effect::Value::Str(s)) => s.parse().unwrap(),
            other => panic!("no relighting light: {other:?}"),
        }
    }

    /// The relighting direction, in degrees.
    pub fn relight_direction(&self) -> i32 {
        match self.stage_param("relight", "direction") {
            Some(obc_pipeline::effect::Value::Int(i)) => i as i32,
            other => panic!("no relighting direction: {other:?}"),
        }
    }

    /// The relighting brightness, in percent.
    pub fn relight_brightness(&self) -> u32 {
        match self.stage_param("relight", "brightness") {
            Some(obc_pipeline::effect::Value::Int(i)) => i as u32,
            other => panic!("no relighting brightness: {other:?}"),
        }
    }
}

#[test]
fn the_fake_offers_and_writes_other_extension_unit_rows() {
    use obc_pipeline::xu::{SHOW_MODE, TEMPERATURE_COMPENSATION, VIBRANCE};
    let mut backend = FakeBackend::default();
    let camera = crate::camera::tests::cam("usb-mx", "MX Brio", Some("046d:0944"), "/dev/video0");
    let mut device = brio_device();
    device.xu = vec![
        FakeXu {
            row: &VIBRANCE,
            current: 128,
            default: 128,
        },
        FakeXu {
            row: &SHOW_MODE,
            current: 0,
            default: 0,
        },
    ];
    backend.world().devices.insert("usb-mx".into(), device);
    let mut node = backend.open_camera_controls(&camera, true).unwrap();
    let scan = node.enumerate();
    let rows: Vec<(&str, u8, Option<&str>)> = scan
        .xu
        .iter()
        .map(|x| {
            (
                x.row.name,
                x.state.current,
                x.state.current_option.map(|o| o.id),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("field_of_view", 2, Some("65")),
            ("vibrance", 128, Some("128")),
            ("show_mode", 0, Some("off")),
        ]
    );
    node.xu_set(&VIBRANCE, "200").unwrap();
    node.xu_set(&SHOW_MODE, "on").unwrap();
    assert_eq!(node.xu_current(&VIBRANCE), Ok(200));
    assert_eq!(node.xu_current(&SHOW_MODE), Ok(1));
    assert!(node.xu_set(&VIBRANCE, "256").is_err());
    assert!(node.xu_set(&TEMPERATURE_COMPENSATION, "60").is_err());
    assert_eq!(
        backend.world().xu_writes,
        [
            ("usb-mx".to_string(), "200".to_string()),
            ("usb-mx".to_string(), "on".to_string()),
        ]
    );
}
