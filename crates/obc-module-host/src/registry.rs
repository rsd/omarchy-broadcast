//! The registry of discovered modules: which are enabled, what each can
//! run, who provides each stage, and in what order the stages run.
//!
//! # Status
//!
//! A module is `disabled` (with the reason: by the user, or after crashing
//! the daemon), `unavailable` (with the reason: its manifest, its ABI
//! version, its kind, its library or its probe), or `available`: loaded,
//! probed, and at least one of its stages can run. A disabled module is
//! never opened.
//!
//! # Providers
//!
//! The provider of a stage is chosen among the enabled, available modules
//! whose stage is available: the pinned module when it is one of them;
//! else by the profile: `quality` takes the highest rank, `battery` the
//! device it prefers (NPU, then CPU, then GPU, then NVIDIA GPU), then the
//! highest rank; ties go to the lower module id. A pin that cannot be
//! honoured falls back to the profile, and the stage says why ("Pinned
//! module ... is unavailable: ..."). Phase 1 places a module's stages by
//! the provider of `background`: that module runs the whole cut-out family
//! in the effect element, so a stage that needs the cut-out (a requirement
//! on the `matte` slot or the `background` stage) runs from that module
//! when it can. A pin to another module for such a stage cannot be
//! honoured: the stage falls back to the cut-out's module, and says why
//! ("Pinned module ... is unavailable: Needs the cut-out ..."). With no
//! such stage in the cut-out's module it is unavailable,
//! [`REASON_NEEDS_CUTOUT`].
//!
//! # Order
//!
//! One total order over every stage the enabled modules offer (available
//! or not). The default is by slot, then the declared position, then the
//! stage id. An order is valid when the slots never go back
//! (pre-matte, matte, post-matte, post-composite) and every requirement of
//! a stage is met by a stage earlier in the order (a slot by any stage of
//! that slot); a requirement no enabled module offers is the provider's
//! business, not the order's. Conflicts do not constrain the order: they
//! are checked when stages are turned on ([`Registry::conflict`]).
//!
//! Everything here but [`Registry::open`] is pure: the probe results can be
//! recorded directly ([`Registry::record_probe`]), which the tests do.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use obc_backend::{Availability, StageId};

use crate::discovery::{Discovery, ModuleEntry};
use crate::library::{LogLevel, LogSink};
use crate::manifest::{Device, Manifest, Requirement, Slot, StageDecl};
use crate::module::Module;

/// The reason of a module the user disabled.
pub const REASON_DISABLED: &str = "Disabled";
/// The reason of an enabled module that has not been opened yet.
pub const REASON_NOT_LOADED: &str = "Not loaded yet";
/// The reason of a module none of whose stages can run here, when they
/// give different reasons.
pub const REASON_NO_STAGE: &str = "None of its stages can run here";
/// The reason of a stage no enabled module provides.
pub const REASON_NO_PROVIDER: &str = "No enabled module provides it";
/// The reason of a stage that needs the cut-out of another module than the
/// one that provides `background` (the phase-1 placement rule).
pub const REASON_NEEDS_CUTOUT: &str = "Needs the cut-out of the module that provides it";

/// How the provider of a stage is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Profile {
    /// The highest rank (`quality`).
    #[default]
    Quality,
    /// The most frugal device, then the highest rank (`battery`).
    Battery,
}

impl Profile {
    /// The name (`quality`, `battery`).
    pub fn as_str(self) -> &'static str {
        match self {
            Profile::Quality => "quality",
            Profile::Battery => "battery",
        }
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Profile {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "quality" => Ok(Profile::Quality),
            "battery" => Ok(Profile::Battery),
            _ => Err(format!("unknown profile {s:?} (quality or battery)")),
        }
    }
}

/// The preference of the `battery` profile for a device, lower first.
fn battery_rank(d: Device) -> u8 {
    match d {
        Device::Npu => 0,
        Device::Cpu => 1,
        Device::Gpu => 2,
        Device::NvidiaGpu => 3,
    }
}

/// The status of a module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleStatus {
    /// Loaded and probed, with at least one stage that can run.
    Available,
    /// It cannot run here, for the reason.
    Unavailable(String),
    /// It is disabled, for the reason.
    Disabled(String),
}

impl ModuleStatus {
    /// The name: `available`, `unavailable` or `disabled`.
    pub fn as_str(&self) -> &'static str {
        match self {
            ModuleStatus::Available => "available",
            ModuleStatus::Unavailable(_) => "unavailable",
            ModuleStatus::Disabled(_) => "disabled",
        }
    }

    /// The reason, empty for an available module.
    pub fn reason(&self) -> &str {
        match self {
            ModuleStatus::Available => "",
            ModuleStatus::Unavailable(r) | ModuleStatus::Disabled(r) => r,
        }
    }
}

/// What opening a module gave.
#[derive(Debug, Clone)]
enum Opened {
    /// Not opened (yet, or since it was closed).
    No,
    /// Opening or probing failed, for the reason.
    Failed(String),
    /// Opened and probed: each manifest stage's availability.
    Probed(Vec<(StageId, Availability)>),
}

#[derive(Debug, Clone)]
struct Record {
    entry: ModuleEntry,
    disabled: Option<String>,
    module: Option<Module>,
    opened: Opened,
}

impl Record {
    /// The manifest, when it parsed and can be loaded by this host.
    fn loadable(&self) -> Option<&Manifest> {
        self.entry
            .manifest
            .as_ref()
            .ok()
            .filter(|m| m.unloadable_reason().is_none())
    }

    /// The manifest of a module whose stages count: enabled and loadable.
    fn offering(&self) -> Option<&Manifest> {
        self.loadable().filter(|_| self.disabled.is_none())
    }

    fn status(&self) -> ModuleStatus {
        if let Some(r) = &self.disabled {
            return ModuleStatus::Disabled(r.clone());
        }
        let manifest = match &self.entry.manifest {
            Ok(m) => m,
            Err(e) => return ModuleStatus::Unavailable(e.clone()),
        };
        if let Some(r) = manifest.unloadable_reason() {
            return ModuleStatus::Unavailable(r);
        }
        match &self.opened {
            Opened::No => ModuleStatus::Unavailable(REASON_NOT_LOADED.into()),
            Opened::Failed(e) => ModuleStatus::Unavailable(e.clone()),
            Opened::Probed(stages) => {
                if stages.iter().any(|(_, a)| a.is_installed()) {
                    return ModuleStatus::Available;
                }
                let mut reasons: Vec<&str> =
                    stages.iter().filter_map(|(_, a)| a.reason()).collect();
                reasons.dedup();
                match reasons.as_slice() {
                    [one] => ModuleStatus::Unavailable((*one).to_string()),
                    _ => ModuleStatus::Unavailable(REASON_NO_STAGE.into()),
                }
            }
        }
    }

    /// Whether stage `stage` can run, and why not.
    fn stage_availability(&self, stage: &str) -> Availability {
        if let Opened::Probed(stages) = &self.opened
            && self.disabled.is_none()
        {
            return stages
                .iter()
                .find(|(s, _)| s == stage)
                .map(|(_, a)| a.clone())
                .unwrap_or_else(|| Availability::NotInstalled(crate::module::NOT_REPORTED.into()));
        }
        Availability::NotInstalled(self.status().reason().to_string())
    }
}

/// One stage of a module, as [`Registry::modules`] lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageInfo {
    /// The stage id.
    pub id: StageId,
    /// Its slot.
    pub slot: Slot,
    /// Its rank.
    pub rank: u32,
    /// Its device.
    pub device: Device,
    /// Whether it can run.
    pub available: bool,
    /// Why not, empty when it can.
    pub reason: String,
    /// Whether this module is the stage's provider.
    pub selected: bool,
}

/// One module, as [`Registry::modules`] lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleInfo {
    /// The module id.
    pub id: String,
    /// The manifest file.
    pub manifest_path: PathBuf,
    /// The manifest, when it parsed.
    pub manifest: Option<Manifest>,
    /// Whether it is enabled.
    pub enabled: bool,
    /// Its status.
    pub status: ModuleStatus,
    /// Its stages (none when the manifest did not parse).
    pub stages: Vec<StageInfo>,
}

/// A module that offers a stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    /// The module id.
    pub module: String,
    /// The module's declaration of the stage.
    pub decl: StageDecl,
    /// Whether the stage can run from this module.
    pub availability: Availability,
}

/// Who provides a stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageResolution {
    /// The stage; `None` when the text asked about is not a valid stage
    /// id (no provider then, and the reason says so).
    pub stage: Option<StageId>,
    /// The provider, when one can run it.
    pub provider: Option<String>,
    /// Why no module can run it, or, with a provider, why the pin was not
    /// honoured.
    pub reason: Option<String>,
}

/// The registry. See the module documentation.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    records: BTreeMap<String, Record>,
    profile: Profile,
    pins: BTreeMap<StageId, String>,
    order: Option<Vec<StageId>>,
}

impl Registry {
    /// A registry of the discovered modules, all enabled and none opened.
    pub fn new(discovery: Discovery) -> Registry {
        Registry::from_entries(discovery.entries)
    }

    /// A registry of `entries` (a later entry of the same id replaces an
    /// earlier one).
    pub fn from_entries(entries: impl IntoIterator<Item = ModuleEntry>) -> Registry {
        let records = entries
            .into_iter()
            .map(|entry| {
                (
                    entry.id.clone(),
                    Record {
                        entry,
                        disabled: None,
                        module: None,
                        opened: Opened::No,
                    },
                )
            })
            .collect();
        Registry {
            records,
            ..Registry::default()
        }
    }

    fn record_mut(&mut self, id: &str) -> Result<&mut Record, String> {
        self.records
            .get_mut(id)
            .ok_or_else(|| format!("No module {id} is installed"))
    }

    /// The ids of every module, in order.
    pub fn ids(&self) -> Vec<String> {
        self.records.keys().cloned().collect()
    }

    /// The discovery entry of module `id`.
    pub fn entry(&self, id: &str) -> Option<&ModuleEntry> {
        self.records.get(id).map(|r| &r.entry)
    }

    /// Enables or disables module `id`; disabling closes it. Whether that
    /// changed anything.
    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<bool, String> {
        if enabled {
            let r = self.record_mut(id)?;
            Ok(r.disabled.take().is_some())
        } else {
            self.disable(id, REASON_DISABLED)
        }
    }

    /// Disables module `id` for `reason` (shown as its status), closing it.
    /// Whether that changed anything.
    pub fn disable(&mut self, id: &str, reason: impl Into<String>) -> Result<bool, String> {
        let r = self.record_mut(id)?;
        let reason = reason.into();
        let changed = r.disabled.as_deref() != Some(reason.as_str());
        r.disabled = Some(reason);
        r.module = None;
        r.opened = Opened::No;
        Ok(changed)
    }

    /// Whether module `id` is enabled.
    pub fn is_enabled(&self, id: &str) -> bool {
        self.records.get(id).is_some_and(|r| r.disabled.is_none())
    }

    /// Opens and probes module `id`, with `config` (its configuration
    /// table, `{}` when none) and its log lines to `log`, unless it is
    /// disabled, cannot be loaded, or is open already. A failure becomes
    /// its status.
    pub fn open(
        &mut self,
        id: &str,
        config: &serde_json::Value,
        log: &LogSink,
    ) -> Result<(), String> {
        let r = self.record_mut(id)?;
        if r.disabled.is_some() || !matches!(r.opened, Opened::No) {
            return Ok(());
        }
        let Some(manifest) = r.loadable().cloned() else {
            return Ok(());
        };
        match Module::open(&manifest, config, log.clone()) {
            Err(e) => {
                log(id, LogLevel::Warn, &format!("unavailable: {e}"));
                r.opened = Opened::Failed(e);
            }
            Ok(module) => match module.probe() {
                Ok(stages) => {
                    r.opened = Opened::Probed(stages);
                    r.module = Some(module);
                }
                Err(e) => {
                    log(id, LogLevel::Warn, &format!("unavailable: {e}"));
                    r.opened = Opened::Failed(e.message);
                }
            },
        }
        Ok(())
    }

    /// Opens every enabled module ([`Registry::open`]), each with the
    /// configuration `config_for` gives for its id.
    pub fn open_all(&mut self, config_for: impl Fn(&str) -> serde_json::Value, log: &LogSink) {
        for id in self.ids() {
            let _ = self.open(&id, &config_for(&id), log);
        }
    }

    /// Probes the open module `id` again (what is installed may have
    /// changed); a failure becomes its status and closes it.
    pub fn reprobe(&mut self, id: &str) -> Result<(), String> {
        let r = self.record_mut(id)?;
        let Some(module) = &r.module else {
            return Ok(());
        };
        match module.probe() {
            Ok(stages) => r.opened = Opened::Probed(stages),
            Err(e) => {
                r.opened = Opened::Failed(e.message);
                r.module = None;
            }
        }
        Ok(())
    }

    /// Closes module `id` (to open it again with another configuration).
    pub fn close(&mut self, id: &str) -> Result<(), String> {
        let r = self.record_mut(id)?;
        r.module = None;
        r.opened = Opened::No;
        Ok(())
    }

    /// Records what opening and probing module `id` gave, without opening
    /// it: the availability of its stages, or why it could not be opened.
    /// For tests, and for a host that probes by other means.
    pub fn record_probe(
        &mut self,
        id: &str,
        result: Result<Vec<(StageId, Availability)>, String>,
    ) -> Result<(), String> {
        let r = self.record_mut(id)?;
        r.opened = match result {
            Ok(stages) => Opened::Probed(stages),
            Err(e) => Opened::Failed(e),
        };
        Ok(())
    }

    /// The open module `id`: enabled, loaded and probed.
    pub fn module(&self, id: &str) -> Option<&Module> {
        self.records.get(id).and_then(|r| r.module.as_ref())
    }

    /// The status of module `id`.
    pub fn status(&self, id: &str) -> Option<ModuleStatus> {
        self.records.get(id).map(Record::status)
    }

    /// Every module, by id, with its stages and which of them it provides.
    pub fn modules(&self) -> Vec<ModuleInfo> {
        let providers: BTreeMap<StageId, Option<String>> = self
            .stage_ids()
            .into_iter()
            .map(|s| {
                let p = self.resolve(s.as_str()).provider;
                (s, p)
            })
            .collect();
        self.records
            .values()
            .map(|r| {
                let manifest = r.entry.manifest.as_ref().ok();
                let stages = manifest
                    .map(|m| {
                        m.stages
                            .iter()
                            .map(|d| {
                                let a = r.stage_availability(d.id.as_str());
                                StageInfo {
                                    id: d.id.clone(),
                                    slot: d.slot,
                                    rank: d.rank,
                                    device: d.device,
                                    available: a.is_installed(),
                                    reason: a.reason().unwrap_or_default().to_string(),
                                    selected: providers
                                        .get(&d.id)
                                        .is_some_and(|p| p.as_deref() == Some(r.entry.id.as_str())),
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                ModuleInfo {
                    id: r.entry.id.clone(),
                    manifest_path: r.entry.manifest_path.clone(),
                    manifest: manifest.cloned(),
                    enabled: r.disabled.is_none(),
                    status: r.status(),
                    stages,
                }
            })
            .collect()
    }

    /// The modules that offer `stage` (enabled, with a manifest this host
    /// can load), by module id.
    pub fn providers(&self, stage: &str) -> Vec<Provider> {
        self.records
            .values()
            .filter_map(|r| {
                let decl = r.offering()?.stage(stage)?;
                Some(Provider {
                    module: r.entry.id.clone(),
                    decl: decl.clone(),
                    availability: r.stage_availability(stage),
                })
            })
            .collect()
    }

    /// Every stage the enabled modules offer, by id.
    pub fn stage_ids(&self) -> Vec<StageId> {
        let mut ids: Vec<StageId> = self
            .records
            .values()
            .filter_map(Record::offering)
            .flat_map(|m| m.stages.iter().map(|s| s.id.clone()))
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// The profile.
    pub fn profile(&self) -> Profile {
        self.profile
    }

    /// Sets the profile.
    pub fn set_profile(&mut self, profile: Profile) {
        self.profile = profile;
    }

    /// The pins, by stage.
    pub fn pins(&self) -> &BTreeMap<StageId, String> {
        &self.pins
    }

    /// Pins `stage` to module `module`, or clears its pin (`None`). A pin
    /// may name a module that is not installed: it is reported as not
    /// honoured.
    pub fn set_pin(&mut self, stage: StageId, module: Option<String>) {
        match module {
            Some(m) => {
                self.pins.insert(stage, m);
            }
            None => {
                self.pins.remove(&stage);
            }
        }
    }

    /// The order of `a` before `b` under the profile.
    fn prefer(&self, a: &Provider, b: &Provider) -> Ordering {
        let by_rank = b.decl.rank.cmp(&a.decl.rank);
        let primary = match self.profile {
            Profile::Quality => by_rank,
            Profile::Battery => battery_rank(a.decl.device)
                .cmp(&battery_rank(b.decl.device))
                .then(by_rank),
        };
        primary.then_with(|| a.module.cmp(&b.module))
    }

    /// The provider of `stage` by pin and profile, before the placement
    /// rule.
    fn resolve_base(&self, stage: &str) -> StageResolution {
        let stage_id = match StageId::new(stage) {
            Ok(id) => Some(id),
            Err(e) => {
                return StageResolution {
                    stage: None,
                    provider: None,
                    reason: Some(e.to_string()),
                };
            }
        };
        let mut providers = self.providers(stage);
        providers.sort_by(|a, b| self.prefer(a, b));
        let can_run = |p: &Provider| self.can_run(p);
        let mut pin_reason = None;
        if let Some(pin) = self.pins.get(stage) {
            match providers.iter().find(|p| &p.module == pin) {
                Some(p) if can_run(p) => {
                    return StageResolution {
                        stage: stage_id,
                        provider: Some(pin.clone()),
                        reason: None,
                    };
                }
                Some(p) => {
                    let why = match self.records.get(pin).map(Record::status) {
                        Some(ModuleStatus::Available) | None => {
                            p.availability.reason().unwrap_or_default().to_string()
                        }
                        Some(s) => s.reason().to_string(),
                    };
                    pin_reason = Some(format!("Pinned module {pin} is unavailable: {why}"));
                }
                None => {
                    let why = match self.records.get(pin) {
                        None => "it is not installed".to_string(),
                        Some(r) if r.disabled.is_some() => r.status().reason().to_string(),
                        Some(r) if r.offering().is_none() => r.status().reason().to_string(),
                        Some(_) => format!("it does not provide {stage}"),
                    };
                    pin_reason = Some(format!("Pinned module {pin} is unavailable: {why}"));
                }
            }
        }
        if let Some(p) = providers.iter().find(|p| can_run(p)) {
            return StageResolution {
                stage: stage_id,
                provider: Some(p.module.clone()),
                reason: pin_reason,
            };
        }
        let reason = pin_reason.or_else(|| {
            providers.first().map(|p| {
                let status = self.records.get(&p.module).map(Record::status);
                match status {
                    Some(ModuleStatus::Available) | None => {
                        p.availability.reason().unwrap_or_default().to_string()
                    }
                    Some(s) => s.reason().to_string(),
                }
            })
        });
        StageResolution {
            stage: stage_id,
            provider: None,
            reason: Some(reason.unwrap_or_else(|| REASON_NO_PROVIDER.into())),
        }
    }

    /// Who provides `stage` (see the module documentation), with the
    /// phase-1 placement rule applied. A text that is not a valid stage id
    /// has no provider, and its reason says why.
    pub fn resolve(&self, stage: &str) -> StageResolution {
        let base = self.resolve_base(stage);
        if stage == StageId::BACKGROUND.as_str() {
            return base;
        }
        let Some(provider) = &base.provider else {
            return base;
        };
        let needs_matte = self
            .records
            .get(provider)
            .and_then(Record::offering)
            .and_then(|m| m.stage(stage))
            .is_some_and(StageDecl::needs_matte);
        if !needs_matte {
            return base;
        }
        let cutout = self.resolve_base(StageId::BACKGROUND.as_str()).provider;
        if cutout.as_ref() == Some(provider) {
            return base;
        }
        // A pin to this provider cannot be honoured: it cannot get the
        // cut-out. As for any pin that cannot be, the stage falls back (to
        // the cut-out's provider, the only one that can run it), and says
        // why.
        let pinned = self.pins.get(stage) == Some(provider);
        let pin_reason =
            || format!("Pinned module {provider} is unavailable: {REASON_NEEDS_CUTOUT}");
        if let Some(cutout) = cutout
            && self
                .providers(stage)
                .iter()
                .any(|p| p.module == cutout && self.can_run(p))
        {
            return StageResolution {
                stage: base.stage,
                provider: Some(cutout),
                reason: if pinned {
                    Some(pin_reason())
                } else {
                    base.reason
                },
            };
        }
        StageResolution {
            stage: base.stage,
            provider: None,
            reason: Some(if pinned {
                pin_reason()
            } else {
                REASON_NEEDS_CUTOUT.to_string()
            }),
        }
    }

    /// Whether `p` can run its stage: its module is available and the
    /// stage too.
    fn can_run(&self, p: &Provider) -> bool {
        p.availability.is_installed()
            && self.records.get(&p.module).map(Record::status) == Some(ModuleStatus::Available)
    }

    /// [`Registry::resolve`] for every offered stage, in the order
    /// ([`Registry::order`]).
    pub fn resolve_all(&self) -> Vec<StageResolution> {
        self.order()
            .iter()
            .map(|s| self.resolve(s.as_str()))
            .collect()
    }

    /// The declaration of `stage` that the order uses: its provider's, or
    /// the first offering module's.
    pub fn decl(&self, stage: &str) -> Option<StageDecl> {
        let providers = self.providers(stage);
        let provider = self.resolve_base(stage).provider;
        providers
            .iter()
            .find(|p| Some(&p.module) == provider.as_ref())
            .or(providers.first())
            .map(|p| p.decl.clone())
    }

    fn order_key(&self, stage: &StageId) -> (Slot, i32, StageId) {
        let d = self.decl(stage.as_str());
        (
            d.as_ref().map_or(Slot::PostComposite, |d| d.slot),
            d.as_ref()
                .map_or(crate::manifest::DEFAULT_POSITION, |d| d.position),
            stage.clone(),
        )
    }

    /// The default order: by slot, position, then id.
    pub fn default_order(&self) -> Vec<StageId> {
        let mut ids = self.stage_ids();
        ids.sort_by_cached_key(|s| self.order_key(s));
        ids
    }

    /// The order in force: the one set ([`Registry::set_order`]) with the
    /// stages it does not name added at their default places, when that is
    /// valid; else the default.
    pub fn order(&self) -> Vec<StageId> {
        let Some(set) = &self.order else {
            return self.default_order();
        };
        let known = self.stage_ids();
        let mut order: Vec<StageId> = set.iter().filter(|s| known.contains(s)).cloned().collect();
        for missing in self.default_order() {
            if order.contains(&missing) {
                continue;
            }
            let key = self.order_key(&missing);
            let at = order
                .iter()
                .position(|s| self.order_key(s) > key)
                .unwrap_or(order.len());
            order.insert(at, missing);
        }
        if self.check_constraints(&order).is_ok() {
            order
        } else {
            self.default_order()
        }
    }

    /// The order set, as set (it may name stages no module offers now).
    pub fn order_set(&self) -> Option<&[StageId]> {
        self.order.as_deref()
    }

    /// Sets the order, after [`Registry::validate_order`].
    pub fn set_order(&mut self, order: Vec<StageId>) -> Result<(), String> {
        self.validate_order(&order)?;
        self.order = Some(order);
        Ok(())
    }

    /// Forgets the order set: the default applies.
    pub fn reset_order(&mut self) {
        self.order = None;
    }

    /// Restores an order set before (from the state file) without checking
    /// it: [`Registry::order`] falls back to the default while it is
    /// invalid.
    pub fn restore_order(&mut self, order: Option<Vec<StageId>>) {
        self.order = order;
    }

    /// Whether `order` is a valid order of every offered stage (see the
    /// module documentation); the reason names the offending stages.
    pub fn validate_order(&self, order: &[StageId]) -> Result<(), String> {
        let known = self.stage_ids();
        for (i, s) in order.iter().enumerate() {
            if order[..i].contains(s) {
                return Err(format!("The order names {s} twice"));
            }
            if !known.contains(s) {
                return Err(format!("No enabled module offers the stage {s}"));
            }
        }
        let missing: Vec<&str> = known
            .iter()
            .filter(|s| !order.contains(s))
            .map(StageId::as_str)
            .collect();
        if !missing.is_empty() {
            return Err(format!("The order leaves out {}", missing.join(", ")));
        }
        self.check_constraints(order)
    }

    /// The slot and requirement rules over `order`.
    fn check_constraints(&self, order: &[StageId]) -> Result<(), String> {
        let decls: Vec<Option<StageDecl>> = order.iter().map(|s| self.decl(s.as_str())).collect();
        let slot_of = |i: usize| decls[i].as_ref().map(|d| d.slot);
        let offered_slots: Vec<Slot> = decls.iter().flatten().map(|d| d.slot).collect();
        for i in 0..order.len() {
            let Some(d) = &decls[i] else { continue };
            for j in 0..i {
                if let Some(earlier) = slot_of(j)
                    && earlier > d.slot
                {
                    return Err(format!(
                        "{} ({}) cannot come after {} ({})",
                        order[i], d.slot, order[j], earlier
                    ));
                }
            }
            for req in &d.requires {
                let met = match req {
                    Requirement::Stage(s) => {
                        if !order.contains(s) {
                            continue;
                        }
                        order[..i].contains(s)
                    }
                    Requirement::Slot(slot) => {
                        if !offered_slots.contains(slot) {
                            continue;
                        }
                        (0..i).any(|j| slot_of(j) == Some(*slot))
                    }
                };
                if !met {
                    let what = match req {
                        Requirement::Stage(s) => s.to_string(),
                        Requirement::Slot(slot) => format!("a {slot} stage"),
                    };
                    return Err(format!("{} needs {what} before it", order[i]));
                }
            }
        }
        Ok(())
    }

    /// Why `stage` cannot be turned on while `active` are on: a conflict
    /// either declares. `None` when it can.
    pub fn conflict(&self, stage: &StageId, active: &[StageId]) -> Option<String> {
        let mine = self.decl(stage.as_str());
        active.iter().filter(|a| *a != stage).find_map(|other| {
            let theirs = self.decl(other.as_str());
            let clash = mine.as_ref().is_some_and(|d| d.conflicts.contains(other))
                || theirs.as_ref().is_some_and(|d| d.conflicts.contains(stage));
            clash.then(|| format!("Cannot run together with {other}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A manifest of module `id` with `stages`: `(id, slot, rank, device,
    /// extra lines)`.
    fn entry(id: &str, stages: &[(&str, &str, u32, &str, &str)]) -> ModuleEntry {
        let mut text = format!(
            "abi = 1\nid = \"{id}\"\nname = \"N\"\nversion = \"1\"\nlicence = \"MIT\"\n\
             kind = \"video-stage\"\nlibrary = \"/l/{id}.so\"\n"
        );
        for (sid, slot, rank, device, extra) in stages {
            text.push_str(&format!(
                "[[stage]]\nid = \"{sid}\"\nslot = \"{slot}\"\nrank = {rank}\n\
                 device = \"{device}\"\nframes = [\"cuda\"]\nformats = [\"nv12\"]\n{extra}\n"
            ));
        }
        let path = Path::new("/m").join(format!("{id}.toml"));
        let manifest = Manifest::parse(&text, &path);
        ModuleEntry {
            id: id.into(),
            manifest_path: path,
            manifest,
        }
    }

    fn s(id: &str) -> StageId {
        StageId::new(id).unwrap()
    }

    const MAXINE: &str = "br.com.dias.maxine-vfx";
    const ONNX: &str = "org.example.onnx";
    const HELLO: &str = "org.example.hello";

    fn maxine() -> ModuleEntry {
        entry(
            MAXINE,
            &[
                ("enhance", "pre-matte", 300, "nvidia-gpu", "position = 0"),
                ("denoise", "pre-matte", 300, "nvidia-gpu", ""),
                ("background", "matte", 300, "nvidia-gpu", ""),
                (
                    "relight",
                    "post-matte",
                    300,
                    "nvidia-gpu",
                    "requires = [\"matte\"]",
                ),
            ],
        )
    }

    fn onnx() -> ModuleEntry {
        entry(
            ONNX,
            &[
                ("background", "matte", 200, "npu", ""),
                (
                    "relight",
                    "post-matte",
                    400,
                    "cpu",
                    "requires = [\"background\"]",
                ),
            ],
        )
    }

    fn hello() -> ModuleEntry {
        entry(
            HELLO,
            &[(
                "hello",
                "post-composite",
                100,
                "cpu",
                "conflicts = [\"relight\"]",
            )],
        )
    }

    fn all_installed(r: &mut Registry, id: &str) {
        let stages = r
            .entry(id)
            .unwrap()
            .manifest
            .as_ref()
            .unwrap()
            .stages
            .clone();
        r.record_probe(
            id,
            Ok(stages
                .iter()
                .map(|d| (d.id.clone(), Availability::Installed))
                .collect()),
        )
        .unwrap();
    }

    fn registry() -> Registry {
        let mut r = Registry::from_entries([maxine(), onnx(), hello()]);
        for id in [MAXINE, ONNX, HELLO] {
            all_installed(&mut r, id);
        }
        r
    }

    #[test]
    fn statuses_say_why() {
        let mut r = Registry::from_entries([
            maxine(),
            entry("org.x.bad", &[("x", "late", 1, "cpu", "")]),
            ModuleEntry {
                manifest: Manifest::parse(
                    &std::fs::read_to_string("/dev/null").unwrap(),
                    Path::new("/m/empty.toml"),
                ),
                id: "empty".into(),
                manifest_path: "/m/empty.toml".into(),
            },
            {
                let mut e = onnx();
                if let Ok(m) = &mut e.manifest {
                    m.abi = 2;
                }
                e
            },
        ]);
        assert_eq!(
            r.status(MAXINE),
            Some(ModuleStatus::Unavailable(REASON_NOT_LOADED.into()))
        );
        assert!(
            r.status("org.x.bad")
                .unwrap()
                .reason()
                .contains("unknown slot")
        );
        assert_eq!(r.status("empty").unwrap().as_str(), "unavailable");
        assert!(r.status(ONNX).unwrap().reason().contains("module ABI 2"));
        assert_eq!(r.status("nope"), None);

        r.record_probe(
            MAXINE,
            Ok(vec![
                (
                    s("enhance"),
                    Availability::NotInstalled("Not installed".into()),
                ),
                (
                    s("denoise"),
                    Availability::NotInstalled("Not installed".into()),
                ),
                (
                    s("background"),
                    Availability::NotInstalled("Not installed".into()),
                ),
                (
                    s("relight"),
                    Availability::NotInstalled("Not installed".into()),
                ),
            ]),
        )
        .unwrap();
        assert_eq!(
            r.status(MAXINE),
            Some(ModuleStatus::Unavailable("Not installed".into()))
        );
        r.record_probe(
            MAXINE,
            Ok(vec![
                (s("enhance"), Availability::NotInstalled("A".into())),
                (s("denoise"), Availability::NotInstalled("B".into())),
            ]),
        )
        .unwrap();
        assert_eq!(
            r.status(MAXINE),
            Some(ModuleStatus::Unavailable(REASON_NO_STAGE.into()))
        );
        r.record_probe(MAXINE, Err("The probe failed: boom".into()))
            .unwrap();
        assert_eq!(r.status(MAXINE).unwrap().reason(), "The probe failed: boom");
        all_installed(&mut r, MAXINE);
        assert_eq!(r.status(MAXINE), Some(ModuleStatus::Available));

        assert_eq!(r.set_enabled(MAXINE, false), Ok(true));
        assert_eq!(r.set_enabled(MAXINE, false), Ok(false));
        assert_eq!(
            r.status(MAXINE),
            Some(ModuleStatus::Disabled(REASON_DISABLED.into()))
        );
        assert!(!r.is_enabled(MAXINE));
        assert_eq!(r.set_enabled(MAXINE, true), Ok(true));
        // Disabling closed it: it must be opened again.
        assert_eq!(
            r.status(MAXINE),
            Some(ModuleStatus::Unavailable(REASON_NOT_LOADED.into()))
        );
        assert!(
            r.set_enabled("nope", true)
                .unwrap_err()
                .contains("No module nope")
        );
        r.disable(MAXINE, "Disabled after crashing the daemon twice in a row")
            .unwrap();
        assert_eq!(
            r.status(MAXINE).unwrap().reason(),
            "Disabled after crashing the daemon twice in a row"
        );
    }

    #[test]
    fn the_quality_profile_takes_the_highest_rank_and_battery_the_frugal_device() {
        let mut r = registry();
        // Maxine 300 over ONNX 200 for the cut-out.
        assert_eq!(r.resolve("background").provider.as_deref(), Some(MAXINE));
        // ONNX's relight ranks 400 but needs its own cut-out: the cut-out's
        // provider runs the stage instead.
        assert_eq!(r.resolve("relight").provider.as_deref(), Some(MAXINE));
        // Pinned to ONNX, it cannot run with Maxine's cut-out: the pin is
        // not honoured, and the stage falls back to the cut-out's provider.
        r.set_pin(s("relight"), Some(ONNX.into()));
        let relight = r.resolve("relight");
        assert_eq!(relight.provider.as_deref(), Some(MAXINE));
        assert_eq!(
            relight.reason,
            Some(format!(
                "Pinned module {ONNX} is unavailable: {REASON_NEEDS_CUTOUT}"
            ))
        );
        r.set_pin(s("relight"), None);
        // Without a cut-out from its own module, nothing can run it.
        let mut lone = Registry::from_entries([onnx(), maxine()]);
        all_installed(&mut lone, ONNX);
        lone.record_probe(
            MAXINE,
            Ok(vec![
                (s("background"), Availability::Installed),
                (
                    s("relight"),
                    Availability::NotInstalled("Not installed".into()),
                ),
            ]),
        )
        .unwrap();
        assert_eq!(
            lone.resolve("relight").reason.as_deref(),
            Some(REASON_NEEDS_CUTOUT)
        );

        r.set_profile(Profile::Battery);
        assert_eq!(r.resolve("background").provider.as_deref(), Some(ONNX));
        assert_eq!(r.resolve("relight").provider.as_deref(), Some(ONNX));
        assert_eq!(r.resolve("denoise").provider.as_deref(), Some(MAXINE));
        // A text that is no stage id is never taken for another stage.
        let bad = r.resolve("Background");
        assert_eq!((&bad.stage, &bad.provider), (&None, &None));
        assert!(
            bad.reason.as_deref().unwrap().contains("invalid stage id"),
            "{bad:?}"
        );
        assert_eq!(r.resolve("status").provider, None, "a reserved name");
        assert_eq!(
            r.resolve("missing"),
            StageResolution {
                stage: Some(s("missing")),
                provider: None,
                reason: Some(REASON_NO_PROVIDER.into())
            }
        );
    }

    #[test]
    fn ties_go_to_the_lower_module_id() {
        let mut r = Registry::from_entries([
            entry("org.b.m", &[("x", "pre-matte", 5, "cpu", "")]),
            entry("org.a.m", &[("x", "pre-matte", 5, "cpu", "")]),
        ]);
        all_installed(&mut r, "org.a.m");
        all_installed(&mut r, "org.b.m");
        assert_eq!(r.resolve("x").provider.as_deref(), Some("org.a.m"));
    }

    #[test]
    fn pins_win_when_they_can_and_say_why_when_not() {
        let mut r = registry();
        r.set_pin(s("background"), Some(ONNX.into()));
        assert_eq!(r.resolve("background").provider.as_deref(), Some(ONNX));
        assert_eq!(r.resolve("background").reason, None);
        // With ONNX's cut-out, Maxine's relight is the one placed wrong: the
        // pin falls back to ONNX's, and says why.
        r.set_pin(s("relight"), Some(MAXINE.into()));
        let relight = r.resolve("relight");
        assert_eq!(relight.provider.as_deref(), Some(ONNX));
        assert_eq!(
            relight.reason,
            Some(format!(
                "Pinned module {MAXINE} is unavailable: {REASON_NEEDS_CUTOUT}"
            ))
        );
        r.set_pin(s("relight"), None);
        assert_eq!(r.resolve("relight").provider.as_deref(), Some(ONNX));

        r.set_enabled(ONNX, false).unwrap();
        let bg = r.resolve("background");
        assert_eq!(
            bg.provider.as_deref(),
            Some(MAXINE),
            "falls back by profile"
        );
        assert_eq!(
            bg.reason.as_deref(),
            Some("Pinned module org.example.onnx is unavailable: Disabled")
        );
        r.set_enabled(ONNX, true).unwrap();
        r.record_probe(
            ONNX,
            Ok(vec![
                (s("background"), Availability::NotInstalled("No NPU".into())),
                (s("relight"), Availability::Installed),
            ]),
        )
        .unwrap();
        assert_eq!(
            r.resolve("background").reason.as_deref(),
            Some("Pinned module org.example.onnx is unavailable: No NPU")
        );
        r.set_pin(s("background"), Some(HELLO.into()));
        assert_eq!(
            r.resolve("background").reason.as_deref(),
            Some("Pinned module org.example.hello is unavailable: it does not provide background")
        );
        r.set_pin(s("background"), Some("org.gone.x".into()));
        assert_eq!(
            r.resolve("background").reason.as_deref(),
            Some("Pinned module org.gone.x is unavailable: it is not installed")
        );
        assert_eq!(r.resolve("background").provider.as_deref(), Some(MAXINE));
    }

    #[test]
    fn an_unavailable_stage_reports_its_best_providers_reason() {
        let mut r = Registry::from_entries([maxine()]);
        r.record_probe(
            MAXINE,
            Ok(vec![
                (s("enhance"), Availability::Installed),
                (
                    s("denoise"),
                    Availability::NotInstalled("Not installed".into()),
                ),
                (s("background"), Availability::Installed),
                (s("relight"), Availability::Installed),
            ]),
        )
        .unwrap();
        assert_eq!(
            r.resolve("denoise"),
            StageResolution {
                stage: Some(s("denoise")),
                provider: None,
                reason: Some("Not installed".into())
            }
        );
        // Maxine alone: its relight has its own cut-out.
        assert_eq!(r.resolve("relight").provider.as_deref(), Some(MAXINE));
        let info = r.modules();
        let stages = &info[0].stages;
        assert_eq!(stages.len(), 4);
        let denoise = stages.iter().find(|st| st.id == "denoise").unwrap();
        assert_eq!(
            (denoise.available, denoise.reason.as_str(), denoise.selected),
            (false, "Not installed", false)
        );
        let relight = stages.iter().find(|st| st.id == "relight").unwrap();
        assert!(relight.available && relight.selected);
        r.record_probe(MAXINE, Err("The probe failed: x".into()))
            .unwrap();
        assert_eq!(
            r.resolve("background").reason.as_deref(),
            Some("The probe failed: x")
        );
    }

    #[test]
    fn the_default_order_is_by_slot_position_and_id() {
        let r = registry();
        let order: Vec<String> = r.default_order().iter().map(|s| s.to_string()).collect();
        assert_eq!(
            order,
            ["enhance", "denoise", "background", "relight", "hello"]
        );
        assert_eq!(r.order(), r.default_order());
        assert!(r.validate_order(&r.default_order()).is_ok());
    }

    #[test]
    fn orders_are_validated_with_the_offending_stages() {
        let mut r = registry();
        let o = |ids: &[&str]| ids.iter().map(|i| s(i)).collect::<Vec<_>>();
        // denoise before enhance: same slot, any order.
        assert!(
            r.validate_order(&o(&[
                "denoise",
                "enhance",
                "background",
                "relight",
                "hello"
            ]))
            .is_ok()
        );
        let e = r
            .validate_order(&o(&[
                "enhance",
                "background",
                "denoise",
                "relight",
                "hello",
            ]))
            .unwrap_err();
        assert_eq!(
            e,
            "denoise (pre-matte) cannot come after background (matte)"
        );
        let e = r
            .validate_order(&o(&[
                "enhance",
                "denoise",
                "background",
                "hello",
                "relight",
            ]))
            .unwrap_err();
        assert!(
            e.contains("relight (post-matte) cannot come after hello (post-composite)"),
            "{e}"
        );
        let e = r
            .validate_order(&o(&["enhance", "denoise", "background"]))
            .unwrap_err();
        assert_eq!(e, "The order leaves out hello, relight");
        let e = r
            .validate_order(&o(&[
                "enhance",
                "enhance",
                "denoise",
                "background",
                "relight",
                "hello",
            ]))
            .unwrap_err();
        assert_eq!(e, "The order names enhance twice");
        let e = r
            .validate_order(&o(&[
                "enhance",
                "denoise",
                "background",
                "relight",
                "hello",
                "zoom",
            ]))
            .unwrap_err();
        assert_eq!(e, "No enabled module offers the stage zoom");

        // Requirements: a post-matte stage needing a pre-matte one after it.
        let mut q = Registry::from_entries([entry(
            "org.x.q",
            &[
                ("a", "pre-matte", 1, "cpu", ""),
                ("b", "pre-matte", 1, "cpu", "requires = [\"a\"]"),
                (
                    "c",
                    "post-composite",
                    1,
                    "cpu",
                    "requires = [\"matte\", \"gone\"]",
                ),
            ],
        )]);
        all_installed(&mut q, "org.x.q");
        assert!(
            q.validate_order(&o(&["a", "b", "c"])).is_ok(),
            "no matte stage, no stage gone: ignored"
        );
        assert_eq!(
            q.validate_order(&o(&["b", "a", "c"])).unwrap_err(),
            "b needs a before it"
        );

        let mut m = Registry::from_entries([entry(
            "org.x.m",
            &[
                ("m", "matte", 1, "cpu", ""),
                ("p", "matte", 1, "cpu", "requires = [\"matte\"]"),
            ],
        )]);
        all_installed(&mut m, "org.x.m");
        assert!(m.validate_order(&o(&["m", "p"])).is_ok());
        assert_eq!(
            m.validate_order(&o(&["p", "m"])).unwrap_err(),
            "p needs a matte stage before it"
        );

        r.set_order(o(&["denoise", "enhance", "background", "relight", "hello"]))
            .unwrap();
        assert_eq!(r.order()[0], s("denoise"));
        assert!(r.set_order(o(&["hello"])).is_err());
        assert_eq!(
            r.order()[0],
            s("denoise"),
            "a refused order changes nothing"
        );
        r.reset_order();
        assert_eq!(r.order()[0], s("enhance"));
    }

    #[test]
    fn a_stored_order_follows_the_modules_installed() {
        let mut r = Registry::from_entries([maxine(), hello()]);
        all_installed(&mut r, MAXINE);
        all_installed(&mut r, HELLO);
        r.set_order(
            ["denoise", "enhance", "background", "relight", "hello"]
                .iter()
                .map(|i| s(i))
                .collect(),
        )
        .unwrap();
        r.set_enabled(HELLO, false).unwrap();
        let order: Vec<String> = r.order().iter().map(|s| s.to_string()).collect();
        assert_eq!(
            order,
            ["denoise", "enhance", "background", "relight"],
            "hello gone"
        );
        // A new module's stages go to their default places.
        let mut r2 = Registry::from_entries([maxine(), hello()]);
        all_installed(&mut r2, MAXINE);
        r2.restore_order(Some(
            ["denoise", "enhance", "background", "relight"]
                .iter()
                .map(|i| s(i))
                .collect(),
        ));
        let order: Vec<String> = r2.order().iter().map(|s| s.to_string()).collect();
        assert_eq!(
            order,
            ["denoise", "enhance", "background", "relight", "hello"]
        );
        // An invalid stored order falls back to the default.
        r2.restore_order(Some(vec![s("relight"), s("denoise")]));
        assert_eq!(r2.order(), r2.default_order());
    }

    #[test]
    fn conflicts_are_reported_by_either_side() {
        let r = registry();
        assert_eq!(
            r.conflict(&s("hello"), &[s("denoise"), s("relight")])
                .as_deref(),
            Some("Cannot run together with relight")
        );
        assert_eq!(
            r.conflict(&s("relight"), &[s("hello")]).as_deref(),
            Some("Cannot run together with hello")
        );
        assert_eq!(r.conflict(&s("denoise"), &[s("hello")]), None);
    }

    #[test]
    fn disabled_modules_offer_nothing() {
        let mut r = registry();
        r.set_enabled(ONNX, false).unwrap();
        r.set_enabled(HELLO, false).unwrap();
        assert_eq!(
            r.stage_ids().iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            ["background", "denoise", "enhance", "relight"]
        );
        assert_eq!(r.providers("background").len(), 1);
        let info = r.modules();
        let hello = info.iter().find(|m| m.id == HELLO).unwrap();
        assert!(!hello.enabled);
        assert_eq!(hello.stages[0].reason, REASON_DISABLED);
        assert!(!hello.stages[0].available);
    }

    #[test]
    fn profiles_parse() {
        assert_eq!("battery".parse::<Profile>(), Ok(Profile::Battery));
        assert_eq!(Profile::default().to_string(), "quality");
        assert!("eco".parse::<Profile>().is_err());
    }
}
