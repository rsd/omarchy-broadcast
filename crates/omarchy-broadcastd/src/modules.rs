//! The daemon's modules (`obc-module-host`): which are installed, which
//! are enabled, what each offers, and the plan of a session (which module
//! runs each stage, where).
//!
//! At start (and at every reload) the manifests are discovered in the
//! system's and the user's `modules.d` directories, the config's
//! `[modules] dirs` and `OBC_MODULE_PATH`, in that order; the crash ledger
//! (in the state directory) counts the starts that found a module's code
//! running; each enabled module is opened with its configuration (its
//! `[module."<id>"]` table as JSON, the `[effect] maxine_root` alias for
//! the Maxine module, and the GPU's compute capability when it is known
//! without waking the GPU) and probed. Nothing a module does stops the
//! daemon: a missing, broken, mismatched or crashing module is listed
//! with its reason and offers nothing. A reload keeps a module open (and
//! probes it again) when its manifest, its library file and its
//! configuration are unchanged.
//!
//! The runtime choices (enabled modules, stage order, backend profile,
//! pins) come from the state file over the config's `[modules]`
//! ([`obc_core::modules`]). They apply to the next session: a running one
//! keeps the [`Plan`] it started with.
//!
//! The opening of libraries goes through the media backend
//! ([`MediaBackend::open_module`]), so the controller's tests describe
//! modules without a library.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use obc_core::config::{Config, RuntimeState};
use obc_core::modules::{BackendProfile, StoredValue};
use obc_module_host::registry::Profile;
use obc_module_host::settings::{Apply, Role};
use obc_module_host::{
    CrashLedger, ModuleStatus, Registry, SettingDesc, SettingType, SettingsDescription, Slot,
    StageResolution,
};
use obc_pipeline::effect::{Params, StageId, Value};

use crate::backend::{
    Backends, ChainRun, EffectRun, MediaBackend, Module, ModuleChoice, Placement, StageRun,
};
use crate::{debug, info, warning};

/// The module id of the NVIDIA Maxine video effects module, which takes
/// `[effect] maxine_root` as its `sdk_root` when its own table sets none.
pub const MAXINE_MODULE: &str = "br.com.dias.maxine-vfx";

/// The crash ledger's file, in the state file's directory.
pub const LEDGER_FILE: &str = "modules-crash.json";

/// The reason of a module disabled by the user (or the config).
pub const REASON_DISABLED: &str = "Disabled";

/// What the daemon knows of an opened module.
#[derive(Debug, Clone)]
struct Opened {
    module: Option<Module>,
    settings: SettingsDescription,
    /// What it was opened from (`None`: a module the tests describe): a
    /// reload keeps the module while this is unchanged.
    from: Option<OpenedFrom>,
}

/// What a module was opened from: its manifest (file and content), its
/// library file and its configuration.
#[derive(Debug, Clone, PartialEq)]
struct OpenedFrom {
    manifest_path: PathBuf,
    manifest: obc_module_host::Manifest,
    library: Option<FileId>,
    config: serde_json::Value,
}

/// A file as it is on disk: device, inode, size and modification time
/// (a library rebuilt or replaced in place is another file).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileId {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: (i64, i64),
}

impl FileId {
    fn of(path: &Path) -> Option<FileId> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(path).ok()?;
        Some(FileId {
            dev: m.dev(),
            ino: m.ino(),
            len: m.len(),
            mtime: (m.mtime(), m.mtime_nsec()),
        })
    }
}

/// The modules of the daemon. See the module documentation.
pub struct Modules {
    registry: Registry,
    opened: BTreeMap<String, Opened>,
    ledger: Option<CrashLedger>,
    /// The compute capability the modules were opened with.
    compute_capability: Option<(u32, u32)>,
    serial: u32,
    /// What `GetModules` and `GetStageOrder` returned when last counted
    /// (what bumps the serial).
    listed: (Vec<obc_ipc::ModuleInfo>, Vec<StageId>),
}

impl std::fmt::Debug for Modules {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Modules")
            .field("ids", &self.registry.ids())
            .field("serial", &self.serial)
            .finish()
    }
}

/// The configuration a module is opened with: its `[module."<id>"]` table
/// as JSON (`{}` without one); for the Maxine module, `sdk_root` from
/// `[effect] maxine_root` when the table sets none; and
/// `_host.compute_capability` (`[major, minor]`, or null when unknown).
pub fn module_config(config: &Config, id: &str, cc: Option<(u32, u32)>) -> serde_json::Value {
    let mut table = config
        .module
        .get(id)
        .and_then(|t| serde_json::to_value(t).ok())
        .and_then(|v| match v {
            serde_json::Value::Object(o) => Some(o),
            _ => None,
        })
        .unwrap_or_default();
    if id == MAXINE_MODULE
        && !table.contains_key("sdk_root")
        && let Some(root) = &config.effect.maxine_root
    {
        table.insert(
            "sdk_root".into(),
            serde_json::Value::String(root.display().to_string()),
        );
    }
    let cc = match cc {
        Some((major, minor)) => serde_json::json!([major, minor]),
        None => serde_json::Value::Null,
    };
    table.insert(
        "_host".into(),
        serde_json::json!({ "compute_capability": cc }),
    );
    serde_json::Value::Object(table)
}

/// The manifest directories: the system's, the user's, the config's
/// `[modules] dirs`, then `OBC_MODULE_PATH`.
pub fn module_dirs(config: &Config) -> Vec<PathBuf> {
    obc_module_host::default_dirs(&config.modules.dirs)
}

fn profile_of(p: BackendProfile) -> Profile {
    match p {
        BackendProfile::Quality => Profile::Quality,
        BackendProfile::Battery => Profile::Battery,
    }
}

/// `v` as the backend seam's value.
pub fn backend_value(v: &StoredValue) -> Value {
    match v {
        StoredValue::Bool(b) => Value::Bool(*b),
        StoredValue::Int(i) => Value::Int(*i),
        StoredValue::Str(s) => Value::Str(s.clone()),
    }
}

/// Whether `v` is a valid value of a setting of type `kind`: of its type,
/// an option of an enum, within an int's range and on its step.
pub fn fits(kind: &SettingType, v: &StoredValue) -> bool {
    match (kind, v) {
        (SettingType::Bool { .. }, StoredValue::Bool(_)) => true,
        (SettingType::Int { min, max, step, .. }, StoredValue::Int(i)) => {
            (min..=max).contains(&i) && (i - min) % step == 0
        }
        (SettingType::Enum { options, .. }, StoredValue::Str(s)) => {
            options.iter().any(|(o, _)| o == s)
        }
        _ => false,
    }
}

/// The default of a setting of type `kind` (`None` for an action).
pub fn default_of(kind: &SettingType) -> Option<StoredValue> {
    match kind {
        SettingType::Bool { default } => Some(StoredValue::Bool(*default)),
        SettingType::Int { default, .. } => Some(StoredValue::Int(*default)),
        SettingType::Enum { default, .. } => Some(StoredValue::Str(default.clone())),
        SettingType::Action => None,
    }
}

/// The default value of `desc` the daemon shows: the configuration's (the
/// typed `[effect]` keys of revision 8) when it fits, else the module's.
pub fn configured_default(config: &Config, desc: &SettingDesc) -> Option<StoredValue> {
    config
        .effect
        .legacy_settings()
        .remove(&desc.id)
        .filter(|v| fits(&desc.kind, v))
        .or_else(|| default_of(&desc.kind))
}

/// The value of `desc` in force: the state file's when it fits, else the
/// configured default.
pub fn value_of(
    config: &Config,
    runtime: &RuntimeState,
    desc: &SettingDesc,
) -> Option<StoredValue> {
    runtime
        .settings
        .get(&desc.id)
        .filter(|v| fits(&desc.kind, v))
        .cloned()
        .or_else(|| configured_default(config, desc))
}

/// A module the session plan chose: its id, its module and its settings.
#[derive(Debug, Clone)]
pub struct Planned {
    pub id: String,
    pub module: Option<Module>,
    pub settings: SettingsDescription,
}

impl Planned {
    fn choice(&self) -> ModuleChoice {
        ModuleChoice {
            id: self.id.clone(),
            module: self.module.clone(),
        }
    }
}

/// A chain of the plan.
#[derive(Debug, Clone)]
pub struct PlannedChain {
    pub name: String,
    pub module: Planned,
    pub placement: Placement,
    pub stages: Vec<StageId>,
}

/// Which module runs each stage of a session, and where: fixed when the
/// session starts ([`Modules::plan`]).
#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// The effect element's backend: the provider of `background` (or,
    /// when none can run it now, the enabled module that declares it, so
    /// that the effect fails with its reason).
    pub effect: Option<Planned>,
    /// The effect module's other stages, in stage order (without
    /// `background` and `enhance`), whether they can run now or not.
    pub effect_stages: Vec<StageId>,
    /// Why there is no effect module.
    pub effect_reason: String,
    /// The provider of `enhance`.
    pub enhance: Option<Planned>,
    /// The other modules' stages, by module and placement.
    pub chains: Vec<PlannedChain>,
}

impl Plan {
    /// The backends of the plan.
    pub fn backends(&self) -> Backends {
        Backends {
            effect: self.effect.as_ref().map(Planned::choice),
            enhance: self.enhance.as_ref().map(Planned::choice),
            chains: self
                .chains
                .iter()
                .map(|c| (c.name.clone(), c.module.choice()))
                .collect(),
        }
    }

    /// The module ids whose code a session of this plan runs with
    /// `effect`: the effect module when the background effect is on or
    /// one of its stages is requested, the enhancer when the ingest has the
    /// enhance stage, and each chain's module when one of its stages is
    /// requested.
    pub fn running_ids(&self, effect: &EffectRun) -> Vec<String> {
        let effect_runs =
            effect.mode != obc_core::EffectMode::None || effect.stages.iter().any(|s| s.enabled);
        let chain_runs = |c: &PlannedChain| {
            effect
                .chains
                .iter()
                .any(|r| r.name == c.name && r.stages.iter().any(|s| s.enabled))
        };
        let mut ids: Vec<String> = self
            .effect
            .iter()
            .filter(|_| effect_runs)
            .chain(self.enhance.iter().filter(|_| effect.enhance))
            .chain(
                self.chains
                    .iter()
                    .filter(|c| chain_runs(c))
                    .map(|c| &c.module),
            )
            .map(|p| p.id.clone())
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// The settings of the module that runs `stage` in this plan.
    fn settings_of(&self, stage: &StageId) -> Option<&SettingsDescription> {
        if self.effect_stages.contains(stage) {
            return self.effect.as_ref().map(|p| &p.settings);
        }
        if *stage == StageId::ENHANCE {
            return self.enhance.as_ref().map(|p| &p.settings);
        }
        self.chains
            .iter()
            .find(|c| c.stages.contains(stage))
            .map(|c| &c.module.settings)
    }
}

/// The name of the chain of `module` at `placement`: a valid, unique
/// element name.
fn chain_name(module: &str, placement: Placement) -> String {
    let suffix = match placement {
        Placement::BeforeEffect => "pre",
        Placement::AfterEffect => "post",
    };
    let max = obc_pipeline::module::MAX_NAME_LEN - suffix.len() - 1;
    let head: String = module.chars().take(max).collect();
    format!("{head}-{suffix}")
}

impl Modules {
    /// No module at all.
    pub fn empty() -> Modules {
        Modules {
            registry: Registry::from_entries(Vec::new()),
            opened: BTreeMap::new(),
            ledger: None,
            compute_capability: None,
            serial: 1,
            listed: (Vec::new(), Vec::new()),
        }
    }

    /// Discovers, opens and probes the modules (see the module
    /// documentation), with the crash ledger at `ledger` (`None`: none) and
    /// the compute capability `cc` when known.
    pub fn load<B: MediaBackend>(
        backend: &mut B,
        config: &Config,
        runtime: &RuntimeState,
        ledger: Option<&Path>,
        cc: Option<(u32, u32)>,
    ) -> Modules {
        let mut m = Modules::empty();
        m.ledger = ledger.map(|path| {
            let mut l = CrashLedger::open(path);
            if let Some(e) = l.load_error() {
                warning!("{e}");
            }
            match l.on_start() {
                Ok(newly) => {
                    for id in newly {
                        warning!("module {id}: {}", obc_module_host::REASON_CRASHED);
                    }
                }
                Err(e) => warning!("the crash ledger {} cannot be written: {e}", path.display()),
            }
            l
        });
        m.reload(backend, config, runtime, cc);
        // The first list is serial 1.
        m.serial = 1;
        m.listed = m.listing();
        m
    }

    /// Discovers the modules again, as at start (the ledger is kept, and
    /// not counted again). A module already open whose manifest (its file
    /// and content), library file and configuration are unchanged is kept
    /// open and probed again; the others are opened anew.
    pub fn reload<B: MediaBackend>(
        &mut self,
        backend: &mut B,
        config: &Config,
        runtime: &RuntimeState,
        cc: Option<(u32, u32)>,
    ) {
        let dirs = module_dirs(config);
        let entries = backend.discover_modules(&dirs);
        for e in &entries {
            if let Err(why) = &e.manifest {
                warning!("module {}: {why}", e.id);
            }
        }
        self.registry = Registry::from_entries(entries);
        let mut before = std::mem::take(&mut self.opened);
        self.compute_capability = cc;
        self.apply_enabled(config, runtime);
        for id in self.registry.ids() {
            match before.remove(&id) {
                Some(o) if self.unchanged(config, &id, &o) => self.keep(backend, &id, o),
                _ => self.open(backend, config, &id),
            }
        }
        // What is not kept is closed here (a session that runs it holds
        // its own reference).
        drop(before);
        self.apply_choices(config, runtime);
        let ids = self.registry.ids();
        if ids.is_empty() {
            info!("no effect modules installed");
        } else {
            info!("modules: {}", ids.join(", "));
        }
        self.bump();
    }

    /// Sets each module enabled or disabled: by the crash ledger, then the
    /// state file's choice over the config's `[modules] disabled`.
    fn apply_enabled(&mut self, config: &Config, runtime: &RuntimeState) {
        for id in self.registry.ids() {
            let crashed = self
                .ledger
                .as_ref()
                .and_then(|l| l.disabled(&id))
                .map(str::to_string);
            let res = match crashed {
                Some(why) => self.registry.disable(&id, why),
                None if !runtime.modules.enabled(&config.modules, &id) => {
                    self.registry.disable(&id, REASON_DISABLED)
                }
                None => self.registry.set_enabled(&id, true),
            };
            if let Err(e) = res {
                debug!("{e}");
            }
        }
    }

    /// What module `id` would be opened from now, if it is enabled and
    /// loadable.
    fn opened_from(&self, config: &Config, id: &str) -> Option<OpenedFrom> {
        if !self.registry.is_enabled(id) {
            return None;
        }
        let entry = self.registry.entry(id)?;
        let manifest = entry.manifest.as_ref().ok()?;
        if manifest.unloadable_reason().is_some() {
            return None;
        }
        Some(OpenedFrom {
            manifest_path: entry.manifest_path.clone(),
            manifest: manifest.clone(),
            library: FileId::of(&manifest.library),
            config: module_config(config, id, self.compute_capability),
        })
    }

    /// Whether `opened` (module `id` as opened before a reload) is what
    /// would be opened now.
    fn unchanged(&self, config: &Config, id: &str, opened: &Opened) -> bool {
        opened.from.is_some() && opened.from == self.opened_from(config, id)
    }

    /// Keeps the open module `id` across a reload: probed again, under the
    /// crash ledger's marker (an error makes it unavailable, as at open).
    fn keep<B: MediaBackend>(&mut self, backend: &mut B, id: &str, opened: Opened) {
        self.ledger_begin(id);
        let res = backend.probe_module(id, opened.module.as_ref());
        self.ledger_end_probe(id);
        match res {
            Ok(stages) => {
                debug!("module {id} unchanged: kept open");
                let _ = self.registry.record_probe(id, Ok(stages));
                self.opened.insert(id.to_string(), opened);
            }
            Err(e) => {
                warning!("module {id} unavailable: {e}");
                let _ = self.registry.record_probe(id, Err(e));
            }
        }
    }

    /// Opens and probes module `id` if it is enabled, loadable and not open
    /// yet. Its code runs under the crash ledger's marker.
    fn open<B: MediaBackend>(&mut self, backend: &mut B, config: &Config, id: &str) {
        if !self.registry.is_enabled(id) || self.opened.contains_key(id) {
            return;
        }
        let Some(entry) = self.registry.entry(id).cloned() else {
            return;
        };
        let Ok(manifest) = &entry.manifest else {
            return;
        };
        if let Some(why) = manifest.unloadable_reason() {
            info!("module {id} unavailable: {why}");
            return;
        }
        let from = self.opened_from(config, id);
        let json = module_config(config, id, self.compute_capability);
        self.ledger_begin(id);
        let res = backend.open_module(&entry, &json);
        self.ledger_end_probe(id);
        match res {
            Ok(o) => {
                debug!("module {id} ({}) opened", manifest.library.display());
                let _ = self.registry.record_probe(id, Ok(o.stages));
                self.opened.insert(
                    id.to_string(),
                    Opened {
                        module: o.module,
                        settings: o.settings,
                        from,
                    },
                );
            }
            Err(e) => {
                warning!("module {id} unavailable: {e}");
                let _ = self.registry.record_probe(id, Err(e));
            }
        }
    }

    /// The profile, the pins and the stage order in force; a stored order
    /// that is not valid for the modules found is replaced by the default
    /// one, with a warning. The serial moves when what `GetModules` lists
    /// (the stages `selected`, among them) or the order changed.
    pub fn apply_choices(&mut self, config: &Config, runtime: &RuntimeState) {
        self.registry
            .set_profile(profile_of(runtime.modules.profile(&config.modules)));
        for stage in self.registry.pins().keys().cloned().collect::<Vec<_>>() {
            self.registry.set_pin(stage, None);
        }
        for (stage, module) in runtime.modules.pins(&config.modules) {
            if let Ok(s) = StageId::new(stage) {
                self.registry.set_pin(s, Some(module));
            }
        }
        self.registry.reset_order();
        let stored = runtime.modules.order(&config.modules);
        if !stored.is_empty() {
            let order: Result<Vec<StageId>, String> = stored
                .iter()
                .map(|s| StageId::new(s.clone()).map_err(|e| e.to_string()))
                .collect();
            if let Err(e) = order.and_then(|o| self.registry.set_order(o)) {
                warning!("the stored stage order is not valid ({e}): using the default order");
            }
        }
        self.bump();
    }

    /// Probes every opened module again, after reopening them all when the
    /// compute capability is now `cc` and they were opened with another.
    /// Whether what `GetModules` lists changed.
    pub fn refresh<B: MediaBackend>(
        &mut self,
        backend: &mut B,
        config: &Config,
        runtime: &RuntimeState,
        cc: Option<(u32, u32)>,
    ) {
        if cc != self.compute_capability {
            self.compute_capability = cc;
            let ids: Vec<String> = self.opened.keys().cloned().collect();
            for id in ids {
                self.opened.remove(&id);
                let _ = self.registry.close(&id);
                self.open(backend, config, &id);
            }
        } else {
            let ids: Vec<String> = self.opened.keys().cloned().collect();
            for id in ids {
                let module = self.opened.get(&id).and_then(|o| o.module.clone());
                self.ledger_begin(&id);
                let res = backend.probe_module(&id, module.as_ref());
                self.ledger_end_probe(&id);
                if let Err(e) = &res {
                    warning!("module {id} unavailable: {e}");
                    self.opened.remove(&id);
                }
                let _ = self.registry.record_probe(&id, res);
            }
        }
        self.apply_choices(config, runtime);
        self.bump();
    }

    /// Enables or disables module `id` (the state file's choice is the
    /// caller's to keep). An enabled module is opened now; one the crash
    /// ledger disabled is forgiven. Whether anything changed.
    pub fn set_enabled<B: MediaBackend>(
        &mut self,
        backend: &mut B,
        config: &Config,
        runtime: &RuntimeState,
        id: &str,
        on: bool,
    ) -> Result<bool, String> {
        if self.registry.entry(id).is_none() {
            return Err(format!("No module {id} is installed"));
        }
        let changed = if on {
            let was = self.registry.set_enabled(id, true)?;
            if let Some(l) = &mut self.ledger
                && let Err(e) = l.reenable(id)
            {
                warning!("the crash ledger cannot be written: {e}");
            }
            self.open(backend, config, id);
            was
        } else {
            let was = self.registry.disable(id, REASON_DISABLED)?;
            self.opened.remove(id);
            was
        };
        self.apply_choices(config, runtime);
        self.bump();
        Ok(changed)
    }

    fn ledger_begin(&mut self, id: &str) {
        if let Some(l) = &mut self.ledger
            && let Err(e) = l.begin(id)
        {
            debug!("the crash ledger cannot be written: {e}");
        }
    }

    /// The end of a probe: the marker cleared, the strikes kept.
    fn ledger_end_probe(&mut self, id: &str) {
        if let Some(l) = &mut self.ledger
            && let Err(e) = l.end_probe(id)
        {
            debug!("the crash ledger cannot be written: {e}");
        }
    }

    fn ledger_end(&mut self, id: &str) {
        if let Some(l) = &mut self.ledger
            && let Err(e) = l.end(id)
        {
            debug!("the crash ledger cannot be written: {e}");
        }
    }

    /// Module `id`'s code is about to run a short check (its probe, before
    /// a session starts): its marker is set until [`Self::probe_ended`].
    pub fn probe_started(&mut self, id: &str) {
        self.ledger_begin(id);
    }

    /// The check of [`Self::probe_started`] returned.
    pub fn probe_ended(&mut self, id: &str) {
        self.ledger_end_probe(id);
    }

    /// A session runs the code of `ids`: their markers are set until
    /// [`Self::session_ended`].
    pub fn session_started(&mut self, ids: &[String]) {
        for id in ids {
            self.ledger_begin(id);
        }
    }

    /// The session that ran `ids` stopped cleanly.
    pub fn session_ended(&mut self, ids: &[String]) {
        for id in ids {
            self.ledger_end(id);
        }
    }

    /// What `GetModules` and `GetStageOrder` return now.
    fn listing(&self) -> (Vec<obc_ipc::ModuleInfo>, Vec<StageId>) {
        (self.info(), self.order())
    }

    /// Bumps the serial when what `GetModules` or `GetStageOrder` returns
    /// changed.
    fn bump(&mut self) {
        let now = self.listing();
        if now != self.listed {
            self.listed = now;
            self.serial = self.serial.wrapping_add(1).max(1);
        }
    }

    /// Carries on the serial of `previous`, the modules this one replaces: a
    /// list that differs from what `previous` listed moves the serial, so a
    /// client that saw the earlier list (the empty one, before the modules
    /// load) is told of the change.
    pub fn follow(&mut self, previous: &Modules) {
        self.serial = previous.serial;
        self.listed = previous.listed.clone();
        self.bump();
    }

    /// `ModulesSerial`.
    pub fn serial(&self) -> u32 {
        self.serial
    }

    /// Whether any module is installed (discovered, loadable or not).
    pub fn any_installed(&self) -> bool {
        !self.registry.ids().is_empty()
    }

    /// Whether module `id` is installed.
    pub fn installed(&self, id: &str) -> bool {
        self.registry.entry(id).is_some()
    }

    /// The `GetModules` list.
    pub fn info(&self) -> Vec<obc_ipc::ModuleInfo> {
        self.registry
            .modules()
            .into_iter()
            .map(|m| {
                let manifest = m.manifest.as_ref();
                let text = |f: fn(&obc_module_host::Manifest) -> String| {
                    manifest.map(f).unwrap_or_default()
                };
                obc_ipc::ModuleInfo {
                    id: m.id.clone(),
                    name: text(|m| m.name.clone()),
                    version: text(|m| m.version.clone()),
                    licence: text(|m| m.licence.clone()),
                    kind: text(|m| m.kind.as_str().into()),
                    library: text(|m| m.library.display().to_string()),
                    manifest: m.manifest_path.display().to_string(),
                    description: text(|m| m.description.clone().unwrap_or_default()),
                    enabled: m.enabled,
                    status: m.status.as_str().into(),
                    reason: m.status.reason().into(),
                    stages: m
                        .stages
                        .iter()
                        .map(|s| obc_ipc::StageInfo {
                            id: s.id.to_string(),
                            slot: s.slot.as_str().into(),
                            rank: s.rank,
                            device: s.device.as_str().into(),
                            available: s.available,
                            reason: s.reason.clone(),
                            selected: s.selected,
                        })
                        .collect(),
                }
            })
            .collect()
    }

    /// The stage order in force: over every stage the enabled modules
    /// offer.
    pub fn order(&self) -> Vec<StageId> {
        self.registry.order()
    }

    /// The default stage order.
    pub fn default_order(&self) -> Vec<StageId> {
        self.registry.default_order()
    }

    /// Checks `order` against the stages offered (each once, all of them,
    /// slots and requirements kept): the reason when it is not valid.
    pub fn validate_order(&self, order: &[String]) -> Result<Vec<StageId>, String> {
        let ids = order
            .iter()
            .map(|s| StageId::new(s.clone()).map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        self.registry.validate_order(&ids)?;
        Ok(ids)
    }

    /// Who provides `stage` at the next session.
    pub fn resolve(&self, stage: &str) -> StageResolution {
        self.registry.resolve(stage)
    }

    /// The modules whose settings are shown: enabled, opened, in module id
    /// order, each with its settings.
    pub fn described(&self) -> impl Iterator<Item = (&str, &SettingsDescription)> {
        self.opened
            .iter()
            .filter(|(id, _)| self.registry.is_enabled(id))
            .map(|(id, o)| (id.as_str(), &o.settings))
    }

    /// The setting `id` of an enabled module, with the module.
    pub fn setting(&self, id: &str) -> Option<(&str, &SettingDesc)> {
        self.described()
            .find_map(|(m, d)| d.setting(id).map(|s| (m, s)))
    }

    /// Whether an enabled, opened module declares `stage`.
    pub fn declares(&self, stage: &str) -> Option<&str> {
        self.described().map(|(id, _)| id).find(|id| {
            self.registry
                .entry(id)
                .and_then(|e| e.manifest.as_ref().ok())
                .is_some_and(|m| m.stage(stage).is_some())
        })
    }

    /// Why `module` cannot run `stage` at the next session, `None` when it
    /// can: the stage's reason, or that another module provides it.
    pub fn stage_reason(&self, module: &str, stage: &str) -> Option<String> {
        let res = self.registry.resolve(stage);
        match &res.provider {
            Some(p) if p == module => None,
            Some(p) => Some(format!("Provided by {}", self.name_of(p))),
            None => Some(
                res.reason
                    .unwrap_or_else(|| obc_module_host::registry::REASON_NO_PROVIDER.into()),
            ),
        }
    }

    /// Whether `module` can run `stage` now.
    pub fn runs(&self, module: &str, stage: &str) -> bool {
        self.stage_reason(module, stage).is_none()
    }

    /// The name of module `id` (its id when it has none).
    pub fn name_of(&self, id: &str) -> String {
        self.registry
            .entry(id)
            .and_then(|e| e.manifest.as_ref().ok())
            .map_or_else(|| id.to_string(), |m| m.name.clone())
    }

    /// The label of `stage` in messages: its switch's label, lower-cased
    /// (`noise removal`), else the stage id.
    pub fn stage_label(&self, stage: &str) -> String {
        self.described()
            .find_map(|(_, d)| d.enable_of(stage))
            .map_or_else(|| stage.to_string(), |s| s.label.to_lowercase())
    }

    /// The session plan: who runs each stage, and where (see [`Plan`]).
    pub fn plan(&self) -> Plan {
        let planned = |id: &str| {
            self.opened.get(id).map(|o| Planned {
                id: id.to_string(),
                module: o.module.clone(),
                settings: o.settings.clone(),
            })
        };
        let background = self.registry.resolve(StageId::BACKGROUND.as_str());
        let effect_id = background.provider.clone().or_else(|| {
            self.declares(StageId::BACKGROUND.as_str())
                .map(String::from)
        });
        let effect = effect_id.as_deref().and_then(planned);
        let effect_reason = background
            .reason
            .unwrap_or_else(|| obc_module_host::registry::REASON_NO_PROVIDER.into());
        // As for the effect: the module that declares it when none can run
        // it now, so that it runs once it can.
        let enhance = self
            .registry
            .resolve(StageId::ENHANCE.as_str())
            .provider
            .or_else(|| self.declares(StageId::ENHANCE.as_str()).map(String::from))
            .as_deref()
            .and_then(planned);
        let mut effect_stages = Vec::new();
        let mut chains: Vec<PlannedChain> = Vec::new();
        for stage in self.order() {
            if stage == StageId::BACKGROUND || stage == StageId::ENHANCE {
                continue;
            }
            let provider = self.registry.resolve(stage.as_str()).provider;
            let declared_by_effect = effect_id.as_deref().is_some_and(|e| {
                self.registry
                    .entry(e)
                    .and_then(|x| x.manifest.as_ref().ok())
                    .is_some_and(|m| m.stage(stage.as_str()).is_some())
            });
            match provider {
                Some(p) if Some(p.as_str()) == effect_id.as_deref() => effect_stages.push(stage),
                None if declared_by_effect => effect_stages.push(stage),
                Some(p) => {
                    let slot = self
                        .registry
                        .entry(&p)
                        .and_then(|x| x.manifest.as_ref().ok())
                        .and_then(|m| m.stage(stage.as_str()))
                        .map_or(Slot::PostComposite, |d| d.slot);
                    let placement = if slot < Slot::Matte {
                        Placement::BeforeEffect
                    } else {
                        Placement::AfterEffect
                    };
                    if let Some(c) = chains
                        .iter_mut()
                        .find(|c| c.module.id == p && c.placement == placement)
                    {
                        c.stages.push(stage);
                    } else if let Some(module) = planned(&p) {
                        chains.push(PlannedChain {
                            name: chain_name(&p, placement),
                            module,
                            placement,
                            stages: vec![stage],
                        });
                    }
                }
                None => {}
            }
        }
        Plan {
            effect,
            effect_stages,
            effect_reason,
            enhance,
            chains,
        }
    }

    /// Whether the session's `stage` is requested: its switch's value (on
    /// when the module gives it none), and the stage can run now.
    pub fn stage_on(
        &self,
        plan: &Plan,
        config: &Config,
        runtime: &RuntimeState,
        stage: &StageId,
    ) -> bool {
        let Some(settings) = plan.settings_of(stage) else {
            return false;
        };
        let module = self.provider_in(plan, stage);
        let on = settings
            .enable_of(stage.as_str())
            .and_then(|s| value_of(config, runtime, s))
            .is_none_or(|v| v == StoredValue::Bool(true));
        // `Some(false)` only when it is known not to run: a module gone
        // from the registry (disabled mid-session) keeps what it had.
        on && module.is_none_or(|m| !self.installed(m) || self.runs_or_unknown(m, stage.as_str()))
    }

    /// Whether `module` runs `stage` now, or the registry cannot say (the
    /// module is not enabled any more).
    fn runs_or_unknown(&self, module: &str, stage: &str) -> bool {
        !self.registry.is_enabled(module) || self.runs(module, stage)
    }

    fn provider_in<'a>(&self, plan: &'a Plan, stage: &StageId) -> Option<&'a str> {
        if plan.effect_stages.contains(stage) {
            return plan.effect.as_ref().map(|p| p.id.as_str());
        }
        if *stage == StageId::ENHANCE {
            return plan.enhance.as_ref().map(|p| p.id.as_str());
        }
        plan.chains
            .iter()
            .find(|c| c.stages.contains(stage))
            .map(|c| c.module.id.as_str())
    }

    /// The request of the session's `stage`: on or off, with its parameters
    /// (load-time: `reload` and `restart`; per frame: `live`) from the
    /// settings in force.
    pub fn stage_run(
        &self,
        plan: &Plan,
        config: &Config,
        runtime: &RuntimeState,
        stage: &StageId,
    ) -> StageRun {
        let mut params = Params::new();
        let mut live = Params::new();
        if let Some(settings) = plan.settings_of(stage) {
            for s in settings.settings() {
                if s.stage.as_ref() != Some(stage) || s.role == Some(Role::Enable) {
                    continue;
                }
                let (Some(key), Some(v)) = (&s.key, value_of(config, runtime, s)) else {
                    continue;
                };
                let into = match s.apply {
                    Some(Apply::Live) => &mut live,
                    _ => &mut params,
                };
                into.insert(key.clone(), backend_value(&v));
            }
        }
        StageRun {
            id: stage.clone(),
            enabled: self.stage_on(plan, config, runtime, stage),
            params,
            live,
        }
    }

    /// The chains of `plan` with the settings in force.
    pub fn chain_runs(
        &self,
        plan: &Plan,
        config: &Config,
        runtime: &RuntimeState,
    ) -> Vec<ChainRun> {
        plan.chains
            .iter()
            .map(|c| ChainRun {
                name: c.name.clone(),
                module: c.module.id.clone(),
                placement: c.placement,
                stages: c
                    .stages
                    .iter()
                    .map(|s| self.stage_run(plan, config, runtime, s))
                    .collect(),
            })
            .collect()
    }

    /// Whether module `id` is enabled.
    pub fn is_enabled(&self, id: &str) -> bool {
        self.registry.is_enabled(id)
    }

    /// The status of module `id`.
    pub fn status(&self, id: &str) -> Option<ModuleStatus> {
        self.registry.status(id)
    }
}

/// Modules for the tests: the Maxine module as its manifest and settings
/// describe it (no library), and a way to make a [`Modules`] of any.
#[cfg(test)]
pub mod testing {
    use std::path::Path;

    use obc_module_host::discovery::ModuleEntry;
    use obc_module_host::{Manifest, SettingsDescription};
    use obc_pipeline::effect::{Availability, StageId};

    use super::Modules;
    use crate::backend::OpenedModule;

    /// The Maxine module's manifest, as installed.
    pub const MAXINE_MANIFEST: &str = include_str!("../../obc-module-maxine/maxine-vfx.toml");

    /// The settings the Maxine module describes (its tests pin the same
    /// document).
    pub const MAXINE_SETTINGS: &str = r#"{"groups":[
        {"id":"video","label":"Video","settings":[
            {"id":"effect.video.denoise","label":"Noise removal","type":"bool","default":false,
             "stage":"denoise","role":"enable","hint":"switch"},
            {"id":"effect.video.enhance","label":"Enhance low-resolution camera","type":"bool",
             "default":false,"stage":"enhance","role":"enable","hint":"switch"}]},
        {"id":"lighting","label":"Lighting","settings":[
            {"id":"effect.lighting.relight","label":"Relighting","type":"bool","default":false,
             "stage":"relight","role":"enable","hint":"switch"},
            {"id":"effect.lighting.light","label":"Light","type":"enum",
             "options":[["warm","Warm"],["neutral","Neutral"],["cool","Cool"]],
             "default":"neutral","stage":"relight","key":"light","apply":"reload",
             "enabled_when":[["effect.lighting.relight","eq",true]],
             "disabled_reason":"Turn on relighting to change this","hint":"segmented"},
            {"id":"effect.lighting.direction","label":"Light direction","type":"int",
             "min":-90,"max":90,"step":5,"default":0,"unit":"°","stage":"relight",
             "key":"direction","apply":"live",
             "enabled_when":[["effect.lighting.relight","eq",true]],
             "disabled_reason":"Turn on relighting to change this","hint":"slider"},
            {"id":"effect.lighting.brightness","label":"Brightness","type":"int",
             "min":50,"max":200,"step":5,"default":100,"unit":"%","stage":"relight",
             "key":"brightness","apply":"live",
             "enabled_when":[["effect.lighting.relight","eq",true]],
             "disabled_reason":"Turn on relighting to change this","hint":"slider"}]}
    ]}"#;

    /// An entry of `manifest`, as discovered at `path`.
    pub fn entry(manifest: &str, path: &str) -> ModuleEntry {
        let path = Path::new(path);
        let m = Manifest::parse(manifest, path);
        ModuleEntry {
            id: m
                .as_ref()
                .map_or_else(|_| "broken".into(), |m| m.id.clone()),
            manifest_path: path.to_path_buf(),
            manifest: m,
        }
    }

    /// The Maxine module's entry.
    pub fn maxine_entry() -> ModuleEntry {
        entry(
            MAXINE_MANIFEST,
            "/usr/lib/omarchy-broadcast/modules.d/maxine-vfx.toml",
        )
    }

    /// The settings of `json` for a module of `stages`.
    pub fn settings(json: &str, stages: &[&str]) -> SettingsDescription {
        let stages: Vec<StageId> = stages.iter().map(|s| StageId::new(*s).unwrap()).collect();
        obc_module_host::settings::parse_settings(json, &stages, |line| panic!("{line}")).unwrap()
    }

    /// The Maxine module's settings.
    pub fn maxine_settings() -> SettingsDescription {
        settings(
            MAXINE_SETTINGS,
            &["enhance", "denoise", "background", "relight"],
        )
    }

    /// The Maxine module opened, its stages available but those of
    /// `unavailable` (stage, reason).
    pub fn maxine_opened(unavailable: &[(&str, &str)]) -> OpenedModule {
        let stages = ["enhance", "denoise", "background", "relight"]
            .into_iter()
            .map(|s| {
                let a = unavailable
                    .iter()
                    .find(|(u, _)| *u == s)
                    .map_or(Availability::Installed, |(_, r)| {
                        Availability::NotInstalled((*r).into())
                    });
                (StageId::new(s).unwrap(), a)
            })
            .collect();
        OpenedModule {
            module: None,
            stages,
            settings: maxine_settings(),
        }
    }

    /// The modules of `list`, each opened as it says, all enabled.
    pub fn modules(list: Vec<(ModuleEntry, Result<OpenedModule, String>)>) -> Modules {
        let mut m = Modules::empty();
        m.registry = obc_module_host::Registry::from_entries(list.iter().map(|(e, _)| e.clone()));
        for (e, opened) in list {
            match opened {
                Ok(o) => {
                    let _ = m.registry.record_probe(&e.id, Ok(o.stages));
                    m.opened.insert(
                        e.id.clone(),
                        super::Opened {
                            module: o.module,
                            settings: o.settings,
                            from: None,
                        },
                    );
                }
                Err(why) => {
                    let _ = m.registry.record_probe(&e.id, Err(why));
                }
            }
        }
        m.listed = m.listing();
        m
    }

    /// The Maxine module alone, its stages available but `unavailable`.
    pub fn maxine(unavailable: &[(&str, &str)]) -> Modules {
        modules(vec![(maxine_entry(), Ok(maxine_opened(unavailable)))])
    }
}
