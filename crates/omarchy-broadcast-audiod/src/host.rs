//! The worker's modules: discovered exactly as the video daemon discovers
//! them (the system's and the user's `modules.d`, the configuration's
//! `[modules] dirs`, `OBC_MODULE_PATH`), of which only `audio-filter`
//! modules are opened ([`LoadKinds::Audio`]); the crash ledger
//! (`audio-modules-crash.json`, beside the snapshot); and the [`Plan`] of
//! the microphone chain the snapshot asks for.
//!
//! # The chain
//!
//! Every audio stage the enabled modules offer runs in the registry's
//! order (by slot, `clean` before `shape`, then position), when its switch
//! (the module's `role = "enable"` setting of the stage, on when it has
//! none) is on in the snapshot and a module can run it (rank, pin and
//! profile, as for video). A stage that declares a conflict with another
//! one that is on turns that one off (the declaring stage wins; when both
//! declare it, the later one in the order), unless the snapshot's
//! `forced_conflicts` is set. Consecutive stages of one module form one
//! group, loaded as one ordered request, so that the module may run them
//! with one combined model.
//!
//! # The GPU
//!
//! A stage whose provider runs on a GPU (`gpu` or `nvidia-gpu`; an NPU and
//! the CPU always may run) is planned on it only when the [`GpuGate`]
//! allows: the snapshot's `gpu_policy` (`never`; `with-camera` and the
//! camera off when the mic session started) and a real consumer of the
//! microphone (level meters alone do not wake the GPU). Otherwise the
//! stage runs on the best other provider that can run it (a CPU or NPU
//! module, by profile then rank), or not at all, and says why.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use obc_backend::{StageId, Value};
use obc_core::config::Config;
use obc_module_host::settings::{Apply, Condition, Op, Role};
use obc_module_host::{
    CrashLedger, Device, LoadKinds, LogSink, Module, ModuleStatus, Profile, Provider,
    REASON_CRASHED, Registry, SettingDesc, SettingType, SettingsDescription, StageResolution,
};
use serde_json::{Value as Json, json};

use crate::snapshot::Snapshot;
use crate::{debug, info, warning};

/// The reason of a module the crash ledger disabled.
pub const REASON_CRASHED_WORKER: &str = "Disabled after crashing the audio worker twice in a row";

/// The reason of a module the user disabled.
pub const REASON_DISABLED: &str = "Disabled";

/// The `disabled_kind` tokens of the settings document (the settings
/// channel's, revision 28: `obc_ipc::settings::disabled`, which the daemon
/// checks them against).
pub mod disabled_kind {
    /// An `enabled_when` condition does not hold.
    pub const PARENT_OFF: &str = "parent_off";
    /// It cannot apply in this session (the GPU policy rules the stage out).
    pub const NOT_APPLICABLE: &str = "not_applicable";
    /// Another module provides the stage.
    pub const OTHER_PROVIDER: &str = "other_provider";
    /// No module can run the stage, for the reason passed on as it is.
    pub const UNAVAILABLE: &str = "unavailable";
    /// For now: processing is off, or a stage that is on turns this one
    /// off.
    pub const BUSY: &str = "busy";
}

/// The reason of a stage whose switch is off.
pub const REASON_OFF: &str = "Turned off";

/// The reason of every stage while processing is off.
pub const REASON_PROCESSING_OFF: &str = "Microphone processing is off";

/// The reason of a GPU stage under `gpu_policy = "never"`.
pub const REASON_GPU_NEVER: &str = "GPU use for the microphone is turned off";

/// The reason of a GPU stage in a session that started with the camera
/// off, under `gpu_policy = "with-camera"`.
pub const REASON_GPU_CAMERA: &str =
    "The microphone uses the GPU only in a session started while the camera is on";

/// The reason of a GPU stage while only level meters (or nobody) record.
pub const REASON_GPU_NO_RECORDER: &str =
    "The GPU is used only while an application records from the microphone";

/// The reason of a GPU stage while the worker learns PipeWire's graph.
pub const REASON_GPU_PENDING: &str = "The GPU is used once the microphone's consumers are known";

/// Whether the microphone may use a GPU now, and why not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GpuGate {
    /// It may.
    #[default]
    Allowed,
    /// `gpu_policy = "never"`.
    Never,
    /// `gpu_policy = "with-camera"`, and the camera was off when the mic
    /// session started.
    CameraOff,
    /// No application records (only level meters, or nobody).
    NoRecorder,
    /// Connected to PipeWire, its graph not known yet: closed until it is.
    Pending,
}

impl GpuGate {
    /// Whether GPU stages may run.
    pub fn allows(self) -> bool {
        self == GpuGate::Allowed
    }

    /// Why not (empty when allowed).
    pub fn reason(self) -> &'static str {
        match self {
            GpuGate::Allowed => "",
            GpuGate::Never => REASON_GPU_NEVER,
            GpuGate::CameraOff => REASON_GPU_CAMERA,
            GpuGate::NoRecorder => REASON_GPU_NO_RECORDER,
            GpuGate::Pending => REASON_GPU_PENDING,
        }
    }
}

/// Whether a stage on `device` wakes a GPU.
pub fn is_gpu_device(device: Device) -> bool {
    matches!(device, Device::NvidiaGpu | Device::Gpu)
}

/// Load-time parameters: `(stage, key, value)`.
pub type LoadParams = Vec<(StageId, String, Value)>;

/// Live parameters: `(stage, key, value)`.
pub type LiveParams = Vec<(String, String, Value)>;

/// What the load of one group depends on: its module, stages and
/// load-time parameters.
pub type GroupKey = (String, Vec<StageId>, LoadParams);

/// What the load of a chain depends on.
pub type LoadKey = Vec<GroupKey>;

/// One group of the chain: consecutive stages of one module.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupPlan {
    /// The module.
    pub module: String,
    /// Its stages, in order.
    pub stages: Vec<StageId>,
    /// The load-time parameters (`apply` `reload` or `restart`).
    pub params: LoadParams,
    /// The per-block parameters (`apply` `live`).
    pub live: LiveParams,
    /// Whether a stage runs off the CPU.
    pub gpu: bool,
}

impl GroupPlan {
    /// The group without its live parameters: what a load depends on.
    pub fn load_key(&self) -> GroupKey {
        (
            self.module.clone(),
            self.stages.clone(),
            self.params.clone(),
        )
    }
}

/// What the plan says of one stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagePlan {
    /// The stage.
    pub id: String,
    /// The module that runs it, or would.
    pub provider: Option<String>,
    /// Whether it is in the chain.
    pub on: bool,
    /// Why not, when it is not.
    pub reason: String,
    /// The `disabled_kind` of the reason of a stage no module runs
    /// ([`disabled_kind`]), empty while one does.
    pub kind: &'static str,
    /// A stage that is on turns it off (a declared conflict).
    pub excluded: bool,
}

/// The chain the snapshot asks for.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Plan {
    /// The groups, in order.
    pub groups: Vec<GroupPlan>,
    /// Every audio stage offered, in order.
    pub stages: Vec<StagePlan>,
    /// Processing is on.
    pub enabled: bool,
}

impl Plan {
    /// Whether a stage runs off the CPU.
    pub fn gpu(&self) -> bool {
        self.groups.iter().any(|g| g.gpu)
    }

    /// What the loads depend on.
    pub fn load_key(&self) -> LoadKey {
        self.groups.iter().map(GroupPlan::load_key).collect()
    }
}

/// The modules of the worker. See the module documentation.
pub struct Host {
    registry: Registry,
    ledger: Option<CrashLedger>,
    config: Config,
    log: LogSink,
}

impl std::fmt::Debug for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Host")
            .field("ids", &self.registry.ids())
            .finish_non_exhaustive()
    }
}

/// The JSON of a setting value.
fn json_of(v: &Value) -> Json {
    match v {
        Value::Bool(b) => json!(b),
        Value::Int(i) => json!(i),
        Value::Float(f) => json!(f),
        Value::Str(s) => json!(s),
    }
}

/// A snapshot value as a value of `kind`, when it is a valid one: of its
/// type, an option of an enum, within an int's range and on its step.
fn fit(kind: &SettingType, v: &Json) -> Option<Value> {
    match kind {
        SettingType::Bool { .. } => v.as_bool().map(Value::Bool),
        SettingType::Int { min, max, step, .. } => v
            .as_i64()
            .filter(|i| (min..=max).contains(&i) && (i - min) % step.max(&1) == 0)
            .map(Value::Int),
        SettingType::Enum { options, .. } => v
            .as_str()
            .filter(|s| options.iter().any(|(o, _)| o == s))
            .map(|s| Value::Str(s.to_string())),
    }
}

/// The default of a setting of type `kind`.
fn default_of(kind: &SettingType) -> Option<Value> {
    match kind {
        SettingType::Bool { default } => Some(Value::Bool(*default)),
        SettingType::Int { default, .. } => Some(Value::Int(*default)),
        SettingType::Enum { default, .. } => Some(Value::Str(default.clone())),
    }
}

/// The value of `desc` in force: the snapshot's when it fits, else the
/// module's default.
pub fn value_of(snapshot: &Snapshot, desc: &SettingDesc) -> Option<Value> {
    snapshot
        .values
        .get(&desc.id)
        .and_then(|v| fit(&desc.kind, v))
        .or_else(|| default_of(&desc.kind))
}

/// Whether condition `c` holds with the values in force of `settings`.
fn holds(snapshot: &Snapshot, settings: &SettingsDescription, c: &Condition) -> bool {
    let Some(v) = settings
        .setting(&c.id)
        .and_then(|d| value_of(snapshot, d))
        .map(|v| json_of(&v))
    else {
        return false;
    };
    match c.op {
        Op::Eq => v == c.value,
        Op::Ne => v != c.value,
        Op::In => c.value.as_array().is_some_and(|a| a.contains(&v)),
    }
}

fn is_gpu(device: Device) -> bool {
    device != Device::Cpu
}

impl Host {
    /// Discovers the modules in `dirs` (normally
    /// [`obc_module_host::default_dirs`] of `config`), counts the
    /// crash ledger at `ledger` (`None`: none) and applies `snapshot`
    /// ([`Host::apply`]).
    pub fn load(
        config: Config,
        dirs: &[PathBuf],
        ledger: Option<&Path>,
        snapshot: &Snapshot,
        log: LogSink,
    ) -> Host {
        let discovery = obc_module_host::scan(dirs);
        for e in &discovery.entries {
            if let Err(why) = &e.manifest {
                warning!("module {}: {why}", e.id);
            }
        }
        let registry = Registry::new(discovery).with_kinds(LoadKinds::Audio);
        let ledger = ledger.map(|path| {
            let mut l = CrashLedger::open(path);
            if let Some(e) = l.load_error() {
                warning!("{e}");
            }
            match l.on_start() {
                Ok(newly) => {
                    for id in newly {
                        warning!("module {id}: {REASON_CRASHED_WORKER}");
                    }
                }
                Err(e) => warning!("the crash ledger {} cannot be written: {e}", path.display()),
            }
            l
        });
        let mut host = Host {
            registry,
            ledger,
            config,
            log,
        };
        host.apply(snapshot);
        let audio: Vec<String> = host
            .registry
            .ids()
            .into_iter()
            .filter(|id| host.is_audio(id))
            .collect();
        if audio.is_empty() {
            info!("no audio modules installed");
        } else {
            info!("audio modules: {}", audio.join(", "));
        }
        host
    }

    /// Whether module `id` is an audio filter (its manifest parsed).
    fn is_audio(&self, id: &str) -> bool {
        self.registry
            .entry(id)
            .and_then(|e| e.manifest.as_ref().ok())
            .is_some_and(|m| m.kind.is_audio())
    }

    /// The configuration module `id` is opened with: its `[module."<id>"]`
    /// table as JSON (`{}` without one), and `_host.compute_capability`,
    /// null (the worker does not wake the GPU to learn it).
    fn module_config(&self, id: &str) -> Json {
        let mut table = self
            .config
            .module
            .get(id)
            .and_then(|t| serde_json::to_value(t).ok())
            .and_then(|v| match v {
                Json::Object(o) => Some(o),
                _ => None,
            })
            .unwrap_or_default();
        table.insert("_host".into(), json!({ "compute_capability": null }));
        Json::Object(table)
    }

    /// Applies the module choices of `snapshot`: each module enabled or
    /// disabled (by the crash ledger, then the snapshot's
    /// `disabled_modules`), every enabled audio module opened and probed
    /// (under the crash ledger's marker), then the profile and the pins.
    pub fn apply(&mut self, snapshot: &Snapshot) {
        for id in self.registry.ids() {
            let crashed = self.ledger.as_ref().and_then(|l| l.disabled(&id)).map(|r| {
                if r == REASON_CRASHED {
                    REASON_CRASHED_WORKER.to_string()
                } else {
                    r.to_string()
                }
            });
            let res = match crashed {
                Some(why) => self.registry.disable(&id, why),
                None if snapshot.disabled_modules.contains(&id) => {
                    self.registry.disable(&id, REASON_DISABLED)
                }
                None => self.registry.set_enabled(&id, true),
            };
            if let Err(e) = res {
                debug!("{e}");
            }
            if self.registry.is_enabled(&id)
                && self.is_audio(&id)
                && self.registry.module(&id).is_none()
            {
                self.open(&id);
            }
        }
        self.registry.set_profile(snapshot.profile());
        for stage in self.registry.pins().keys().cloned().collect::<Vec<_>>() {
            self.registry.set_pin(stage, None);
        }
        for (stage, module) in &snapshot.pins {
            if let Ok(s) = StageId::new(stage.clone()) {
                self.registry.set_pin(s, Some(module.clone()));
            }
        }
    }

    /// Opens and probes module `id`, its code under the ledger's marker.
    fn open(&mut self, id: &str) {
        let config = self.module_config(id);
        self.ledger_begin(id);
        let res = self.registry.open(id, &config, &self.log);
        self.ledger_end_probe(id);
        if let Err(e) = res {
            debug!("{e}");
        }
        if self.registry.module(id).is_some() {
            debug!("module {id} opened");
        }
    }

    /// Before module `id`'s code runs (a load, then its blocks): sets its
    /// marker until [`Host::ledger_end`].
    pub fn ledger_begin(&mut self, id: &str) {
        if let Some(l) = &mut self.ledger
            && let Err(e) = l.begin(id)
        {
            debug!("the crash ledger cannot be written: {e}");
        }
    }

    /// The user enables module `id` again: the crash ledger forgets its
    /// strikes and its disabling, and the snapshot's choices apply again
    /// (it is opened when enabled there). Whether the ledger had disabled
    /// it. An unknown module is an error.
    pub fn reset_module(&mut self, id: &str, snapshot: &Snapshot) -> Result<bool, String> {
        if self.registry.entry(id).is_none() {
            return Err(format!("No module {id} is installed"));
        }
        let was = match &mut self.ledger {
            Some(l) => l
                .reenable(id)
                .map_err(|e| format!("the crash ledger cannot be written: {e}"))?,
            None => false,
        };
        if was {
            info!("module {id}: enabled again");
        }
        self.apply(snapshot);
        Ok(was)
    }

    /// Module `id`'s chain was unloaded cleanly.
    pub fn ledger_end(&mut self, id: &str) {
        if let Some(l) = &mut self.ledger
            && let Err(e) = l.end(id)
        {
            debug!("the crash ledger cannot be written: {e}");
        }
    }

    fn ledger_end_probe(&mut self, id: &str) {
        if let Some(l) = &mut self.ledger
            && let Err(e) = l.end_probe(id)
        {
            debug!("the crash ledger cannot be written: {e}");
        }
    }

    /// The open module `id`.
    pub fn module(&self, id: &str) -> Option<Module> {
        self.registry.module(id).cloned()
    }

    /// The settings of module `id`, as it described them.
    fn settings_of(&self, id: &str) -> Option<SettingsDescription> {
        self.registry.module(id).and_then(Module::cached_settings)
    }

    /// The name of module `id` (its id when it has none).
    fn name_of(&self, id: &str) -> String {
        self.registry
            .entry(id)
            .and_then(|e| e.manifest.as_ref().ok())
            .map_or_else(|| id.to_string(), |m| m.name.clone())
    }

    /// The label of `stage` in messages: its switch's label, else its id.
    fn stage_label(&self, stage: &str, provider: Option<&str>) -> String {
        provider
            .and_then(|p| self.settings_of(p))
            .and_then(|s| s.enable_of(stage).map(|d| d.label.clone()))
            .unwrap_or_else(|| stage.to_string())
    }

    /// The audio stages offered, in the registry's order.
    fn audio_order(&self) -> Vec<StageId> {
        self.registry
            .order()
            .into_iter()
            .filter(|s| {
                self.registry
                    .decl(s.as_str())
                    .is_some_and(|d| d.audio.is_some())
            })
            .collect()
    }

    /// Whether `a`'s declaration names `b` among its conflicts.
    fn declares_conflict(&self, a: &StageId, b: &StageId) -> bool {
        self.registry
            .decl(a.as_str())
            .is_some_and(|d| d.conflicts.contains(b))
    }

    /// The provider of `stage` with the GPU gate applied (see the module
    /// documentation): the registry's choice, unless it runs on a GPU the
    /// gate closes; then the best provider that does not, by profile, rank
    /// and id.
    fn resolve(&self, stage: &StageId, gate: GpuGate) -> StageResolution {
        let res = self.registry.resolve(stage.as_str());
        if gate.allows() {
            return res;
        }
        let Some(chosen) = res.provider.clone() else {
            return res;
        };
        let providers = self.registry.providers(stage.as_str());
        let on_gpu = |m: &str| {
            providers
                .iter()
                .find(|p| p.module == m)
                .is_some_and(|p| is_gpu_device(p.decl.device))
        };
        if !on_gpu(&chosen) {
            return res;
        }
        let mut others: Vec<&Provider> = providers
            .iter()
            .filter(|p| {
                !is_gpu_device(p.decl.device)
                    && p.availability.is_installed()
                    && self.registry.status(&p.module) == Some(ModuleStatus::Available)
            })
            .collect();
        let battery = self.registry.profile() == Profile::Battery;
        others.sort_by(|a, b| {
            let npu_first = |p: &Provider| u8::from(p.decl.device != Device::Npu);
            let by_device = if battery {
                npu_first(a).cmp(&npu_first(b))
            } else {
                std::cmp::Ordering::Equal
            };
            by_device
                .then(b.decl.rank.cmp(&a.decl.rank))
                .then_with(|| a.module.cmp(&b.module))
        });
        let pinned = self.registry.pins().get(stage) == Some(&chosen);
        let pin_reason =
            pinned.then(|| format!("Pinned module {chosen} is unavailable: {}", gate.reason()));
        match others.first() {
            Some(p) => StageResolution {
                stage: res.stage,
                provider: Some(p.module.clone()),
                reason: pin_reason.or(res.reason),
            },
            None => StageResolution {
                stage: res.stage,
                provider: None,
                reason: Some(pin_reason.unwrap_or_else(|| gate.reason().to_string())),
            },
        }
    }

    /// Whether `module` runs `stage` on a GPU or NPU (anything but the
    /// CPU: the deeper pipeline).
    fn off_cpu(&self, stage: &StageId, module: &str) -> bool {
        self.registry
            .providers(stage.as_str())
            .iter()
            .find(|p| p.module == module)
            .is_some_and(|p| is_gpu(p.decl.device))
    }

    /// The chain `snapshot` asks for (see the module documentation), with
    /// GPU stages only when `gate` allows.
    pub fn plan(&self, snapshot: &Snapshot, gate: GpuGate) -> Plan {
        let order = self.audio_order();
        let mut stages = Vec::new();
        // (stage, provider), the stages whose switch is on and that a
        // module can run.
        let mut on: Vec<(StageId, String)> = Vec::new();
        for s in &order {
            let res = self.resolve(s, gate);
            let provider = res.provider.clone();
            // No provider under the gate, one without it: the GPU policy
            // rules the stage out of this session.
            let gated = provider.is_none()
                && !gate.allows()
                && self.registry.resolve(s.as_str()).provider.is_some();
            let kind = match &provider {
                Some(_) => "",
                None if !snapshot.enabled => disabled_kind::BUSY,
                None if gated => disabled_kind::NOT_APPLICABLE,
                None => disabled_kind::UNAVAILABLE,
            };
            let (is_on, reason) = match &provider {
                _ if !snapshot.enabled => (false, REASON_PROCESSING_OFF.to_string()),
                None => (
                    false,
                    res.reason
                        .unwrap_or_else(|| obc_module_host::registry::REASON_NO_PROVIDER.into()),
                ),
                Some(p) => {
                    let switch = self
                        .settings_of(p)
                        .and_then(|d| d.enable_of(s.as_str()).cloned());
                    let wanted = switch
                        .as_ref()
                        .and_then(|d| value_of(snapshot, d))
                        .is_none_or(|v| v == Value::Bool(true));
                    if wanted {
                        (true, String::new())
                    } else {
                        (false, REASON_OFF.to_string())
                    }
                }
            };
            if is_on && let Some(p) = &provider {
                on.push((s.clone(), p.clone()));
            }
            stages.push(StagePlan {
                id: s.to_string(),
                provider,
                on: is_on,
                reason,
                kind,
                excluded: false,
            });
        }
        if !snapshot.forced_conflicts {
            let excluded = self.excluded(&on);
            for (s, by) in &excluded {
                if let Some(sp) = stages.iter_mut().find(|x| x.id == s.as_str()) {
                    sp.on = false;
                    sp.excluded = true;
                    let provider = on.iter().find(|(x, _)| x == by).map(|(_, p)| p.as_str());
                    sp.reason = format!(
                        "Cannot run together with {}",
                        self.stage_label(by.as_str(), provider).to_lowercase()
                    );
                }
            }
            on.retain(|(s, _)| !excluded.contains_key(s));
        }
        let mut groups: Vec<GroupPlan> = Vec::new();
        for (s, p) in on {
            let gpu = self.off_cpu(&s, &p);
            let (params, live) = self.params_of(snapshot, &p, &s);
            match groups.last_mut() {
                Some(g) if g.module == p => {
                    g.stages.push(s);
                    g.params.extend(params);
                    g.live.extend(live);
                    g.gpu |= gpu;
                }
                _ => groups.push(GroupPlan {
                    module: p,
                    stages: vec![s],
                    params,
                    live,
                    gpu,
                }),
            }
        }
        Plan {
            groups,
            stages,
            enabled: snapshot.enabled,
        }
    }

    /// The stages of `on` another one turns off, with the one that does.
    fn excluded(&self, on: &[(StageId, String)]) -> BTreeMap<StageId, StageId> {
        let mut out = BTreeMap::new();
        for (i, (s, _)) in on.iter().enumerate() {
            for (j, (t, _)) in on.iter().enumerate() {
                if i == j || !self.declares_conflict(t, s) {
                    continue;
                }
                // `t` names `s`: `t` wins, unless `s` names `t` as well
                // and comes later.
                if !self.declares_conflict(s, t) || j > i {
                    out.insert(s.clone(), t.clone());
                    break;
                }
            }
        }
        out
    }

    /// The load-time and live parameters of `stage` run by `module`.
    fn params_of(
        &self,
        snapshot: &Snapshot,
        module: &str,
        stage: &StageId,
    ) -> (LoadParams, LiveParams) {
        let mut params = Vec::new();
        let mut live = Vec::new();
        let Some(settings) = self.settings_of(module) else {
            return (params, live);
        };
        for s in settings.settings() {
            if s.stage.as_ref() != Some(stage) || s.role == Some(Role::Enable) {
                continue;
            }
            let (Some(key), Some(v)) = (&s.key, value_of(snapshot, s)) else {
                continue;
            };
            match s.apply {
                Some(Apply::Live) => live.push((stage.to_string(), key.clone(), v)),
                _ => params.push((stage.clone(), key.clone(), v)),
            }
        }
        (params, live)
    }

    /// The `GetModules` document: every module discovered (video ones
    /// listed unavailable), with its stages.
    pub fn modules_json(&self) -> Json {
        let list: Vec<Json> = self
            .registry
            .modules()
            .into_iter()
            .map(|m| {
                let manifest = m.manifest.as_ref();
                json!({
                    "id": m.id,
                    "name": manifest.map(|x| x.name.clone()).unwrap_or_default(),
                    "version": manifest.map(|x| x.version.clone()).unwrap_or_default(),
                    "licence": manifest.map(|x| x.licence.clone()).unwrap_or_default(),
                    "kind": manifest.map(|x| x.kind.as_str()).unwrap_or_default(),
                    "description": manifest.and_then(|x| x.description.clone()).unwrap_or_default(),
                    "library": manifest.map(|x| x.library.display().to_string()).unwrap_or_default(),
                    "manifest": m.manifest_path.display().to_string(),
                    "enabled": m.enabled,
                    "status": m.status.as_str(),
                    "reason": m.status.reason(),
                    "stages": m.stages.iter().map(|s| json!({
                        "id": s.id.as_str(),
                        "slot": s.slot.as_str(),
                        "rank": s.rank,
                        "device": s.device.as_str(),
                        "available": s.available,
                        "reason": s.reason,
                        "selected": s.selected,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        json!({ "modules": list })
    }

    /// The `GetSettings` document: the `mic.*` groups of the enabled, open
    /// audio modules, in module id order, in the keys of the obc-ipc
    /// setting dictionaries (with `stage`, `key`, `apply` and `role` too);
    /// `enabled` computed from the conditions, the stage's provider and the
    /// conflicts in force, with its `disabled_kind` ([`disabled_kind`]):
    /// `other_provider` (`Handled by <name> instead`), `not_applicable`
    /// (the GPU policy), `busy` (processing off, or a stage that is on
    /// turns this one off), `unavailable` (no module can run the stage),
    /// `parent_off` (a condition that does not hold); `""` while enabled.
    ///
    /// It describes what the user may configure, so only the policy gates
    /// ([`GpuGate::Never`], [`GpuGate::CameraOff`]: the stage cannot run in
    /// this session) grey a GPU stage's settings; the transient ones
    /// ([`GpuGate::NoRecorder`], [`GpuGate::Pending`]) are planned as
    /// allowed, as when an application records. The status keeps the
    /// stage's reason for those.
    pub fn settings_json(&self, snapshot: &Snapshot, gate: GpuGate) -> Json {
        let gate = match gate {
            GpuGate::NoRecorder | GpuGate::Pending => GpuGate::Allowed,
            g => g,
        };
        let plan = self.plan(snapshot, gate);
        let mut groups = Vec::new();
        for id in self.registry.ids() {
            if !self.registry.is_enabled(&id) || !self.is_audio(&id) {
                continue;
            }
            let Some(desc) = self.settings_of(&id) else {
                continue;
            };
            for g in &desc.groups {
                let settings: Vec<Json> = g
                    .settings
                    .iter()
                    .map(|s| self.setting_json(snapshot, &plan, &id, &desc, s))
                    .collect();
                let mut group = json!({
                    "id": g.id,
                    "label": g.label,
                    "module": id,
                    "settings": settings,
                });
                if let Some(surface) = &g.surface {
                    group["surface"] = json!(surface);
                }
                groups.push(group);
            }
        }
        json!({ "groups": groups })
    }

    fn setting_json(
        &self,
        snapshot: &Snapshot,
        plan: &Plan,
        module: &str,
        desc: &SettingsDescription,
        s: &SettingDesc,
    ) -> Json {
        let value = value_of(snapshot, s);
        let mut enabled = true;
        let mut reason = String::new();
        let mut kind = "";
        if let Some(stage) = &s.stage {
            let sp = plan.stages.iter().find(|p| p.id == stage.as_str());
            match sp.map(|p| p.provider.as_deref()) {
                Some(Some(p)) if p != module => {
                    enabled = false;
                    reason = format!("Handled by {} instead", self.name_of(p));
                    kind = disabled_kind::OTHER_PROVIDER;
                }
                Some(Some(_)) => {}
                _ => {
                    enabled = false;
                    reason = sp
                        .map(|p| p.reason.clone())
                        .unwrap_or_else(|| obc_module_host::registry::REASON_NO_PROVIDER.into());
                    kind = sp.map_or(disabled_kind::UNAVAILABLE, |p| p.kind);
                }
            }
            // The switch of a stage another one turns off.
            if enabled
                && s.role == Some(Role::Enable)
                && let Some(p) = sp
                && p.excluded
            {
                enabled = false;
                reason = p.reason.clone();
                kind = disabled_kind::BUSY;
            }
        }
        if enabled && !s.enabled_when.iter().all(|c| holds(snapshot, desc, c)) {
            enabled = false;
            reason = s.disabled_reason.clone().unwrap_or_default();
            kind = disabled_kind::PARENT_OFF;
        }
        let conditions: Vec<Json> = s
            .enabled_when
            .iter()
            .map(|c| {
                let op = match c.op {
                    Op::Eq => "eq",
                    Op::Ne => "ne",
                    Op::In => "in",
                };
                json!([c.id, op, c.value])
            })
            .collect();
        let mut d = json!({
            "id": s.id,
            "label": s.label,
            "type": s.kind.as_str(),
            "scope": "global",
            "stored": snapshot.values.contains_key(&s.id),
            "enabled": enabled,
            "disabled_reason": reason,
            "disabled_kind": kind,
            "enabled_when": conditions,
        });
        match &s.kind {
            SettingType::Int {
                min,
                max,
                step,
                unit,
                ..
            } => {
                d["min"] = json!(min);
                d["max"] = json!(max);
                d["step"] = json!(step);
                if let Some(u) = unit {
                    d["unit"] = json!(u);
                }
            }
            SettingType::Enum { options, .. } => {
                d["options"] = json!(
                    options
                        .iter()
                        .map(|(v, l)| json!([v, l]))
                        .collect::<Vec<_>>()
                );
            }
            SettingType::Bool { .. } => {}
        }
        if let Some(def) = default_of(&s.kind) {
            d["default"] = json_of(&def);
        }
        if let Some(v) = &value {
            d["value"] = json_of(v);
        }
        if let Some(h) = &s.hint {
            d["hint"] = json!(h);
        }
        if let Some(stage) = &s.stage {
            d["stage"] = json!(stage.as_str());
        }
        if let Some(k) = &s.key {
            d["key"] = json!(k);
        }
        if let Some(a) = s.apply {
            d["apply"] = json!(match a {
                Apply::Live => "live",
                Apply::Reload => "reload",
                Apply::Restart => "restart",
            });
        }
        if s.role == Some(Role::Enable) {
            d["role"] = json!("enable");
        }
        d
    }

    /// The modules whose code the plan runs.
    pub fn module_ids(plan: &Plan) -> BTreeSet<String> {
        plan.groups.iter().map(|g| g.module.clone()).collect()
    }
}
