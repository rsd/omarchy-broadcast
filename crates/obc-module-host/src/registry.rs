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
//! # Module conflicts and requirements
//!
//! A module's manifest may name modules it conflicts with (`conflicts`)
//! and modules it needs (`[requires] modules`). Both are static rules over
//! the modules that are enabled and whose manifest this host can load,
//! whatever has been opened (or failed to open), in whatever order:
//!
//! - A module that needs a module that is not such a module is refused:
//!   unavailable, "Needs the module <id>". Force does not lift this. It is
//!   one level, and not transitive: a module that needs one refused (by a
//!   conflict, or by its own requirement) is not refused for it, so a
//!   refusal never spreads to the modules that need the one refused.
//! - The parties of the conflicts are the other such modules (a module
//!   refused by a requirement refuses no other). Of two parties one of
//!   which declares a conflict with the other, one is refused:
//!   unavailable, "Conflicts with <name> (<id>)" (the name made one line
//!   and at most [`NAME_MAX`] bytes). The declarer is refused; when both
//!   declare it, the higher module id. The cut-out's module is never the
//!   one refused (the cut-out hides what is behind the person): the party
//!   that would provide `background` by pin and profile, among all the
//!   parties, whatever their probes say; the other side is refused
//!   instead, also when it has the lower id or declares nothing. A
//!   conflict is not enforced when either module is forced
//!   ([`Registry::set_forced`], the configuration's `modules.forced.<id>`,
//!   for debugging and testing).
//!
//! A refused module offers nothing and its library is never opened
//! ([`Registry::open`]); one opened before it was refused offers nothing
//! either ([`Registry::probed`] still tells what its probe said, for a
//! session that keeps it until it ends).
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
//! on the `matte` slot or the `background` stage) or that sits in the
//! `matte` or `post-matte` slot runs from that module when it can
//! (`background` itself excepted). A pin to another module for such a
//! stage cannot be honoured: the stage falls back to the cut-out's module,
//! and says why ("Pinned module ... is unavailable: Needs the cut-out
//! ..."). With no such stage in the cut-out's module it is unavailable,
//! [`REASON_NEEDS_CUTOUT`].
//!
//! A provider's own requirements then apply: every stage it `requires`
//! must have a provider ("Needs <stage>" otherwise), and every slot it
//! `requires` some offered stage of that slot with a provider ("Needs a
//! <slot> stage"); unmet, the stage has no provider. (A requirement on the
//! cut-out is the placement rule's, above.)
//!
//! [`Registry::modules`] lists a stage that no module provides, which the
//! module's probe says can run but the placement rule or its requirements
//! refuse there, as unavailable with that reason; a stage another module
//! provides is only not `selected` in this one.
//!
//! # Order
//!
//! One total order over every stage the enabled modules offer (available
//! or not). The default is by slot, then the declared position, then the
//! stage id. An order is valid when the slots never go back
//! (pre-matte, matte, post-matte, post-composite) and every requirement of
//! a stage is met by a stage earlier in the order (a slot by any stage of
//! that slot); a requirement no enabled module offers is no error of the
//! order (the stage then has no provider, above). Conflicts do not
//! constrain the order.
//!
//! # Stage conflicts
//!
//! Of two stages that are both on and conflict (either one's declaration
//! names the other), one is turned off ([`Registry::excluded`]): the one
//! that declares the conflict wins; when both declare it, the later one in
//! the order wins; `background` never loses (the cut-out hides what is
//! behind the person: the other stage is the one dropped). A conflict is
//! not enforced when either stage's module is forced.
//!
//! # Kinds
//!
//! A registry loads the module kinds of its [`LoadKinds`]: the video
//! daemon's registry ([`Registry::new`], [`LoadKinds::Video`]) offers
//! video stages only and lists an audio module as unavailable
//! ([`crate::manifest::REASON_AUDIO_IN_WORKER`]) without ever opening its
//! library; the audio worker's ([`Registry::with_kinds`] of
//! [`LoadKinds::Audio`]) offers the `mic-*` stages of audio modules only,
//! in slot order (`clean`, then `shape`). Rank, pins and profile apply to
//! both alike.
//!
//! Everything here but [`Registry::open`] is pure: the probe results can be
//! recorded directly ([`Registry::record_probe`]), which the tests do.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use obc_backend::{Availability, StageId};

use crate::discovery::{Discovery, ModuleEntry};
use crate::library::{LogLevel, LogSink};
use crate::manifest::{Device, LoadKinds, Manifest, Requirement, Slot, StageDecl};
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

/// The longest module name a reason gives ("Conflicts with <name> (<id>)"),
/// in bytes.
pub const NAME_MAX: usize = 64;

/// A module's `name` as a reason shows it: control, bidirectional and
/// zero-width characters removed, whitespace runs collapsed to one space
/// and trimmed, at most [`NAME_MAX`] bytes (cut on a character boundary).
fn shown_name(name: &str) -> String {
    let invisible = |c: char| {
        matches!(
            c,
            '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2060}'..='\u{2064}'
                | '\u{061c}'
                | '\u{feff}'
        )
    };
    let mut out = String::new();
    for word in name
        .split(char::is_whitespace)
        .map(|w| w.chars().filter(|c| !c.is_control() && !invisible(*c)))
    {
        let word: String = word.collect();
        if word.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&word);
    }
    if out.len() > NAME_MAX {
        let mut end = NAME_MAX;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        out.truncate(out.trim_end().len());
    }
    out
}

/// The stage id of frame generation (vendor neutral). The daemon adds it
/// to a session by itself, after every other stage, when the output rate is
/// twice the camera's: it is in no order ([`Registry::order`],
/// [`Registry::validate_order`]). Its provider is chosen as any stage's.
pub const FRAME_GENERATION: &str = "frame-generation";

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
    /// The kinds of the registry.
    kinds: LoadKinds,
    disabled: Option<String>,
    /// Why the module is refused by a module conflict or requirement
    /// ([`Registry::refresh_refusals`]), kept up to date by the registry.
    refused: Option<String>,
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
            .filter(|m| m.unloadable_reason_for(self.kinds).is_none())
    }

    /// The manifest of a module the module rules are over: enabled and
    /// loadable.
    fn candidate(&self) -> Option<&Manifest> {
        self.loadable().filter(|_| self.disabled.is_none())
    }

    /// The manifest of a module whose stages count: enabled, loadable and
    /// not refused.
    fn offering(&self) -> Option<&Manifest> {
        self.candidate().filter(|_| self.refused.is_none())
    }

    fn status(&self) -> ModuleStatus {
        if let Some(r) = &self.disabled {
            return ModuleStatus::Disabled(r.clone());
        }
        let manifest = match &self.entry.manifest {
            Ok(m) => m,
            Err(e) => return ModuleStatus::Unavailable(e.clone()),
        };
        if let Some(r) = manifest.unloadable_reason_for(self.kinds) {
            return ModuleStatus::Unavailable(r);
        }
        if let Some(r) = &self.refused {
            return ModuleStatus::Unavailable(r.clone());
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
            && self.refused.is_none()
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
    /// Whether it is forced: its module conflicts are not enforced
    /// ([`Registry::set_forced`]). Never for a module of a kind this host
    /// does not load.
    pub forced: bool,
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
    kinds: LoadKinds,
    profile: Profile,
    pins: BTreeMap<StageId, String>,
    order: Option<Vec<StageId>>,
    /// The modules whose conflicts are not enforced.
    forced: BTreeSet<String>,
}

impl Registry {
    /// A registry of the discovered modules, all enabled and none opened,
    /// that loads video modules ([`LoadKinds::Video`]; see
    /// [`Registry::with_kinds`]).
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
                        kinds: LoadKinds::default(),
                        disabled: None,
                        refused: None,
                        module: None,
                        opened: Opened::No,
                    },
                )
            })
            .collect();
        let mut r = Registry {
            records,
            ..Registry::default()
        };
        r.refresh_refusals();
        r
    }

    /// This registry, loading the modules of `kinds` instead (the audio
    /// worker's: [`LoadKinds::Audio`]). Modules opened before are closed.
    pub fn with_kinds(mut self, kinds: LoadKinds) -> Registry {
        self.kinds = kinds;
        for r in self.records.values_mut() {
            if r.kinds != kinds {
                r.kinds = kinds;
                r.module = None;
                r.opened = Opened::No;
            }
        }
        self.refresh_refusals();
        self
    }

    /// The kinds of module this registry loads.
    pub fn kinds(&self) -> LoadKinds {
        self.kinds
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
            let changed = r.disabled.take().is_some();
            self.refresh_refusals();
            Ok(changed)
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
        self.refresh_refusals();
        Ok(changed)
    }

    /// Whether module `id` is enabled.
    pub fn is_enabled(&self, id: &str) -> bool {
        self.records.get(id).is_some_and(|r| r.disabled.is_none())
    }

    /// Sets the modules that are forced: a module conflict
    /// ([`Self::refresh_refusals`]) or a stage conflict ([`Self::excluded`]) is
    /// not enforced when either side is one of them. For debugging and
    /// testing. A module requirement is enforced whatever is forced.
    pub fn set_forced(&mut self, forced: impl IntoIterator<Item = String>) {
        self.forced = forced.into_iter().collect();
        self.refresh_refusals();
    }

    /// Whether module `id` is forced ([`Self::set_forced`]).
    pub fn is_forced(&self, id: &str) -> bool {
        self.forced.contains(id)
    }

    /// The first module `id` needs (`[requires] modules`, in its
    /// manifest's order) that is not enabled and loadable, `None` when
    /// there is none (or `id` is not enabled and loadable itself).
    fn missing_module(&self, id: &str) -> Option<String> {
        let m = self.records.get(id)?.candidate()?;
        m.requires_modules
            .iter()
            .find(|req| {
                self.records
                    .get(req.as_str())
                    .and_then(Record::candidate)
                    .is_none()
            })
            .cloned()
    }

    /// The module the module conflicts keep for the cut-out: among the
    /// `parties` (enabled, loadable, no module requirement unmet) that
    /// declare `background`, the one that would provide it by pin and
    /// profile (whatever their probes say: the rules are static).
    fn background_keeper(&self, parties: &BTreeSet<String>) -> Option<String> {
        let bg = StageId::BACKGROUND.as_str();
        let offers: Vec<Provider> = parties
            .iter()
            .filter_map(|id| {
                let decl = self.records.get(id)?.candidate()?.stage(bg)?;
                Some(Provider {
                    module: id.clone(),
                    decl: decl.clone(),
                    availability: Availability::Installed,
                })
            })
            .collect();
        if let Some(pin) = self.pins.get(bg)
            && offers.iter().any(|p| &p.module == pin)
        {
            return Some(pin.clone());
        }
        offers
            .into_iter()
            .min_by(|a, b| self.prefer(a, b))
            .map(|p| p.module)
    }

    /// Why party `id` is refused by a module conflict (see the module
    /// documentation), `None` when it is not: the first conflict it loses,
    /// its own declarations first (in its manifest's order), then the
    /// parties that declare one with it (by id).
    fn conflict_of(
        &self,
        id: &str,
        parties: &BTreeSet<String>,
        keeper: Option<&str>,
    ) -> Option<String> {
        if !parties.contains(id) || keeper == Some(id) {
            return None;
        }
        let mine = self.records.get(id)?.candidate()?;
        let declaring = parties.iter().filter(|other| {
            self.records
                .get(other.as_str())
                .and_then(Record::candidate)
                .is_some_and(|m| m.conflicts.iter().any(|c| c == id))
        });
        for other in mine.conflicts.iter().chain(declaring) {
            if !parties.contains(other) || self.is_forced(id) || self.is_forced(other) {
                continue;
            }
            let Some(theirs) = self.records.get(other).and_then(Record::candidate) else {
                continue;
            };
            let i_declare = mine.conflicts.contains(other);
            let they_declare = theirs.conflicts.iter().any(|c| c == id);
            let loses = if keeper == Some(other.as_str()) {
                // The cut-out's module never loses.
                true
            } else if i_declare && they_declare {
                id > other.as_str()
            } else {
                i_declare
            };
            if loses {
                return Some(format!(
                    "Conflicts with {} ({other})",
                    shown_name(&theirs.name)
                ));
            }
        }
        None
    }

    /// Computes every module's refusal again (after what it depends on
    /// changed: the modules enabled, the kinds, the forced set, the pins,
    /// the profile): a module requirement first, then a conflict among the
    /// parties.
    fn refresh_refusals(&mut self) {
        let needs: BTreeMap<String, String> = self
            .records
            .keys()
            .filter_map(|id| Some((id.clone(), self.missing_module(id)?)))
            .collect();
        let parties: BTreeSet<String> = self
            .records
            .iter()
            .filter(|(id, r)| r.candidate().is_some() && !needs.contains_key(*id))
            .map(|(id, _)| id.clone())
            .collect();
        let keeper = self.background_keeper(&parties);
        let refused: Vec<(String, Option<String>)> = self
            .records
            .keys()
            .map(|id| {
                let why = match needs.get(id) {
                    Some(req) => Some(format!("Needs the module {req}")),
                    None => self.conflict_of(id, &parties, keeper.as_deref()),
                };
                (id.clone(), why)
            })
            .collect();
        for (id, why) in refused {
            if let Some(r) = self.records.get_mut(&id) {
                r.refused = why;
            }
        }
    }

    /// Whether modules `a` and `b` are in a module conflict that is
    /// enforced: either declares one with the other (`conflicts`), both
    /// are enabled and loadable, and neither is forced. Whichever side the
    /// rules refuse, the two never run in one process.
    pub fn conflict_between(&self, a: &str, b: &str) -> bool {
        if a == b || self.is_forced(a) || self.is_forced(b) {
            return false;
        }
        let candidate = |id: &str| self.records.get(id).and_then(Record::candidate);
        let (Some(ma), Some(mb)) = (candidate(a), candidate(b)) else {
            return false;
        };
        ma.conflicts.iter().any(|c| c == b) || mb.conflicts.iter().any(|c| c == a)
    }

    /// Whether module `id` is refused by a module conflict or requirement
    /// (its status says why).
    pub fn is_refused(&self, id: &str) -> bool {
        self.records.get(id).is_some_and(|r| r.refused.is_some())
    }

    /// Whether opening module `id` was tried since it was last closed (it
    /// is open, or opening or probing it failed): [`Self::open`] does
    /// nothing then.
    pub fn tried(&self, id: &str) -> bool {
        self.records
            .get(id)
            .is_some_and(|r| !matches!(r.opened, Opened::No))
    }

    /// What the probe of the enabled module `id` said of `stage`, whether
    /// or not the module is refused since (a running session may keep a
    /// module it started with until it ends); `None` when it is disabled
    /// or not probed, or did not report the stage.
    pub fn probed(&self, id: &str, stage: &str) -> Option<Availability> {
        let r = self.records.get(id)?;
        if r.disabled.is_some() {
            return None;
        }
        match &r.opened {
            Opened::Probed(stages) => stages
                .iter()
                .find(|(s, _)| s == stage)
                .map(|(_, a)| a.clone()),
            _ => None,
        }
    }

    /// The module conflicts that are not enforced because a side is
    /// forced, while that lets both run: each pair of available modules
    /// (enabled, not refused, opened and probed with a stage that can run)
    /// one of which declares a conflict with the other, `(lower id, higher
    /// id)`, by id. A pair one side of which is refused otherwise (a
    /// requirement, another conflict), failed to open or is unavailable is
    /// none.
    pub fn forced_module_conflicts(&self) -> Vec<(String, String)> {
        let loaded = |r: &Record| {
            r.offering()
                .filter(|_| r.status() == ModuleStatus::Available)
                .cloned()
        };
        let mut out = Vec::new();
        for (a, ra) in &self.records {
            let Some(ma) = loaded(ra) else { continue };
            for (b, rb) in self.records.range::<str, _>((
                std::ops::Bound::Excluded(a.as_str()),
                std::ops::Bound::Unbounded,
            )) {
                let Some(mb) = loaded(rb) else { continue };
                let clash = ma.conflicts.contains(b) || mb.conflicts.contains(a);
                if clash && (self.is_forced(a) || self.is_forced(b)) {
                    out.push((a.clone(), b.clone()));
                }
            }
        }
        out
    }

    /// Opens and probes module `id`, with `config` (its configuration
    /// table, `{}` when none) and its log lines to `log`, unless it is
    /// disabled, cannot be loaded, is refused (a module conflict or
    /// requirement), or is open already. A failure becomes
    /// its status. A probed module is asked for its settings description
    /// once, which [`Module::cached_settings`] then holds (the backend
    /// factory checks parameters against it); a description that fails is
    /// logged, the module kept.
    pub fn open(
        &mut self,
        id: &str,
        config: &serde_json::Value,
        log: &LogSink,
    ) -> Result<(), String> {
        let r = self.record_mut(id)?;
        if r.disabled.is_some() || r.refused.is_some() || !matches!(r.opened, Opened::No) {
            return Ok(());
        }
        let Some(manifest) = r.loadable().cloned() else {
            return Ok(());
        };
        match Module::open_for(&manifest, config, log.clone(), r.kinds) {
            Err(e) => {
                log(id, LogLevel::Warn, &format!("unavailable: {e}"));
                r.opened = Opened::Failed(e);
            }
            Ok(module) => match module.probe() {
                Ok(stages) => {
                    if let Err(e) = module.settings() {
                        log(id, LogLevel::Warn, &format!("no settings description: {e}"));
                    }
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
                                let id = r.entry.id.as_str();
                                let a = r.stage_availability(d.id.as_str());
                                let provider = providers.get(&d.id).cloned().flatten();
                                // A stage the probe says can run that the
                                // resolution refuses, and no module
                                // provides, says why.
                                let refused = (a.is_installed()
                                    && provider.is_none()
                                    && r.status() == ModuleStatus::Available)
                                    .then(|| self.resolution_refusal(id, d.id.as_str(), d))
                                    .flatten();
                                StageInfo {
                                    id: d.id.clone(),
                                    slot: d.slot,
                                    rank: d.rank,
                                    device: d.device,
                                    available: a.is_installed() && refused.is_none(),
                                    reason: refused.unwrap_or_else(|| {
                                        a.reason().unwrap_or_default().to_string()
                                    }),
                                    selected: provider.as_deref() == Some(id),
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
                    // Forced only where the force is read: a module of a
                    // kind this host loads (the audio worker has its own
                    // `[mic] force_conflicts`).
                    forced: self.is_forced(&r.entry.id)
                        && manifest.is_some_and(|m| r.kinds.admits(m.kind)),
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
        // The cut-out's module the conflicts keep may be another now.
        self.refresh_refusals();
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
        // The cut-out's module the conflicts keep may be another now.
        self.refresh_refusals();
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
        // No module offers it: a refused module that would is why.
        let refused = || {
            self.records
                .values()
                .filter(|r| r.refused.is_some())
                .find(|r| r.candidate().is_some_and(|m| m.stage(stage).is_some()))
                .and_then(|r| r.refused.clone())
        };
        let reason = pin_reason.or_else(|| {
            if providers.is_empty() {
                return refused();
            }
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
    /// phase-1 placement rule and the provider's requirements applied. A
    /// text that is not a valid stage id has no provider, and its reason
    /// says why.
    pub fn resolve(&self, stage: &str) -> StageResolution {
        let res = self.resolve_placed(stage);
        let Some(provider) = &res.provider else {
            return res;
        };
        let Some(decl) = self
            .records
            .get(provider)
            .and_then(Record::offering)
            .and_then(|m| m.stage(stage))
        else {
            return res;
        };
        if let Some(why) = self.unmet_requirement(stage, decl) {
            return StageResolution {
                stage: res.stage,
                provider: None,
                reason: Some(why),
            };
        }
        res
    }

    /// Why a provider whose declaration of `stage` is `decl` cannot run
    /// it for its requirements ("Needs <stage>", "Needs a <slot> stage"),
    /// `None` when they are met. Without recursion: a requirement is met
    /// by a stage that has a provider by pin and profile
    /// ([`Self::resolve_base`]). A requirement on the cut-out is the
    /// placement rule's.
    fn unmet_requirement(&self, stage: &str, decl: &StageDecl) -> Option<String> {
        let has_provider = |s: &str| self.resolve_base(s).provider.is_some();
        for req in &decl.requires {
            if req.is_matte() {
                continue;
            }
            let (met, what) = match req {
                Requirement::Stage(s) => (has_provider(s.as_str()), s.to_string()),
                Requirement::Slot(slot) => (
                    self.stage_ids().iter().any(|t| {
                        t.as_str() != stage
                            && self.decl(t.as_str()).is_some_and(|d| d.slot == *slot)
                            && has_provider(t.as_str())
                    }),
                    format!("a {slot} stage"),
                ),
            };
            if !met {
                return Some(format!("Needs {what}"));
            }
        }
        None
    }

    /// Whether the declaration `decl` of a stage is of the cut-out family
    /// (it requires the cut-out, or sits in the `matte` or `post-matte`
    /// slot; `background` itself excepted).
    fn in_cutout_family(stage: &str, decl: &StageDecl) -> bool {
        stage != StageId::BACKGROUND.as_str()
            && (decl.needs_matte() || matches!(decl.slot, Slot::Matte | Slot::PostMatte))
    }

    /// Why module `module`, whose probe says it can run `stage` (its
    /// declaration `decl`), cannot run it by the resolution: the placement
    /// rule ([`REASON_NEEDS_CUTOUT`]) or its requirements
    /// ([`Self::unmet_requirement`]); `None` when it can. Asked only of a
    /// stage no module provides: one another module provides is merely
    /// not this one's.
    fn resolution_refusal(&self, module: &str, stage: &str, decl: &StageDecl) -> Option<String> {
        if Self::in_cutout_family(stage, decl)
            && self
                .resolve_base(StageId::BACKGROUND.as_str())
                .provider
                .as_deref()
                != Some(module)
        {
            return Some(REASON_NEEDS_CUTOUT.to_string());
        }
        self.unmet_requirement(stage, decl)
    }

    /// The provider of `stage` by pin and profile, with the phase-1
    /// placement rule applied: a stage of the cut-out family (it requires
    /// the cut-out, or sits in the `matte` or `post-matte` slot) runs from
    /// the provider of `background` only.
    fn resolve_placed(&self, stage: &str) -> StageResolution {
        let base = self.resolve_base(stage);
        if stage == StageId::BACKGROUND.as_str() {
            return base;
        }
        let Some(provider) = &base.provider else {
            return base;
        };
        let in_cutout = self
            .records
            .get(provider)
            .and_then(Record::offering)
            .and_then(|m| m.stage(stage))
            .is_some_and(|d| Self::in_cutout_family(stage, d));
        if !in_cutout {
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

    /// The stages an order is over: every stage offered but
    /// [`FRAME_GENERATION`].
    fn ordered_ids(&self) -> Vec<StageId> {
        let mut ids = self.stage_ids();
        ids.retain(|s| s.as_str() != FRAME_GENERATION);
        ids
    }

    /// The default order: by slot, position, then id
    /// ([`FRAME_GENERATION`] left out).
    pub fn default_order(&self) -> Vec<StageId> {
        let mut ids = self.ordered_ids();
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
        let known = self.ordered_ids();
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
        let known = self.ordered_ids();
        for (i, s) in order.iter().enumerate() {
            if order[..i].contains(s) {
                return Err(format!("The order names {s} twice"));
            }
            if s.as_str() == FRAME_GENERATION {
                return Err(format!(
                    "The order cannot name {s}: it always runs after the other stages"
                ));
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

    /// Whether the declaration of `a` names `b` among its conflicts.
    fn declares_conflict(&self, a: &StageId, b: &StageId) -> bool {
        self.decl(a.as_str())
            .is_some_and(|d| d.conflicts.contains(b))
    }

    /// The stages of `on` (the stages turned on, in the stage order) that
    /// a conflict turns off, each with the stage that wins over it (see the
    /// module documentation): the declarer wins; when both declare it, the
    /// later one in `on`; `background` never loses. A conflict is not
    /// enforced when `forced` holds for either stage (its module is
    /// forced). Each stage's declaration is its provider's
    /// ([`Self::decl`]).
    pub fn excluded(
        &self,
        on: &[StageId],
        forced: impl Fn(&StageId) -> bool,
    ) -> BTreeMap<StageId, StageId> {
        let mut out = BTreeMap::new();
        for (i, s) in on.iter().enumerate() {
            if *s == StageId::BACKGROUND || forced(s) {
                continue;
            }
            for (j, t) in on.iter().enumerate() {
                if i == j || t == s || forced(t) {
                    continue;
                }
                let theirs = self.declares_conflict(t, s);
                let mine = self.declares_conflict(s, t);
                let loses = if *t == StageId::BACKGROUND {
                    theirs || mine
                } else if theirs && mine {
                    j > i
                } else {
                    theirs
                };
                if loses {
                    out.insert(s.clone(), t.clone());
                    break;
                }
            }
        }
        out
    }

    /// The stage conflicts among `on` that are not enforced because
    /// `forced` holds for a side ([`Self::excluded`]): each pair that
    /// conflicts, in the order of `on` (the earlier first).
    pub fn forced_stage_conflicts(
        &self,
        on: &[StageId],
        forced: impl Fn(&StageId) -> bool,
    ) -> Vec<(StageId, StageId)> {
        let mut out = Vec::new();
        for (i, a) in on.iter().enumerate() {
            for b in &on[i + 1..] {
                if a == b {
                    continue;
                }
                let clash = self.declares_conflict(a, b) || self.declares_conflict(b, a);
                if clash && (forced(a) || forced(b)) {
                    out.push((a.clone(), b.clone()));
                }
            }
        }
        out
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

    /// An audio module of `id` with `stages`: `(id, slot, rank, device)`.
    fn audio_entry(id: &str, stages: &[(&str, &str, u32, &str)]) -> ModuleEntry {
        let mut text = format!(
            "abi = 1\nid = \"{id}\"\nname = \"N\"\nversion = \"1\"\nlicence = \"MIT\"\n\
             kind = \"audio-filter\"\nlibrary = \"/l/{id}.so\"\n"
        );
        for (sid, slot, rank, device) in stages {
            text.push_str(&format!(
                "[[stage]]\nid = \"{sid}\"\nslot = \"{slot}\"\nrank = {rank}\n\
                 device = \"{device}\"\nframes = [\"host\"]\nrates = [48000]\n\
                 frame_samples = [480]\n"
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

    const AFX: &str = "br.com.dias.maxine-afx";
    const VOICE: &str = "br.com.dias.voice-cpu";

    fn audio_entries() -> [ModuleEntry; 3] {
        [
            audio_entry(
                AFX,
                &[
                    ("mic-enhance", "shape", 300, "nvidia-gpu"),
                    ("mic-denoise", "clean", 300, "nvidia-gpu"),
                    ("mic-dereverb", "clean", 300, "nvidia-gpu"),
                ],
            ),
            audio_entry(VOICE, &[("mic-denoise", "clean", 200, "cpu")]),
            hello(),
        ]
    }

    #[test]
    fn the_video_registry_lists_audio_modules_and_never_offers_their_stages() {
        let mut r = Registry::from_entries(audio_entries());
        assert_eq!(r.kinds(), LoadKinds::Video);
        all_installed(&mut r, HELLO);
        assert_eq!(
            r.status(AFX),
            Some(ModuleStatus::Unavailable(
                crate::manifest::REASON_AUDIO_IN_WORKER.into()
            ))
        );
        assert_eq!(r.stage_ids(), [s("hello")]);
        assert_eq!(r.order(), [s("hello")]);
        assert_eq!(r.resolve("mic-denoise").provider, None);
        // Opening an audio module in the video registry never opens its
        // library (which does not exist here).
        let log: LogSink = std::sync::Arc::new(|_, _, _| {});
        r.open(AFX, &serde_json::json!({}), &log).unwrap();
        assert_eq!(
            r.status(AFX).unwrap().reason(),
            crate::manifest::REASON_AUDIO_IN_WORKER
        );
    }

    #[test]
    fn the_audio_registry_offers_mic_stages_by_rank_pin_and_profile() {
        let mut r = Registry::from_entries(audio_entries()).with_kinds(LoadKinds::Audio);
        assert_eq!(r.kinds(), LoadKinds::Audio);
        for id in [AFX, VOICE] {
            all_installed(&mut r, id);
        }
        assert_eq!(
            r.status(HELLO),
            Some(ModuleStatus::Unavailable(
                crate::manifest::REASON_VIDEO_IN_DAEMON.into()
            ))
        );
        assert_eq!(r.status(AFX), Some(ModuleStatus::Available));
        // Slot order: clean before shape, then the stage id.
        assert_eq!(
            r.order(),
            [s("mic-denoise"), s("mic-dereverb"), s("mic-enhance")]
        );
        assert_eq!(r.resolve("mic-denoise").provider.as_deref(), Some(AFX));
        r.set_profile(Profile::Battery);
        assert_eq!(r.resolve("mic-denoise").provider.as_deref(), Some(VOICE));
        r.set_profile(Profile::Quality);
        r.set_pin(s("mic-denoise"), Some(VOICE.into()));
        assert_eq!(r.resolve("mic-denoise").provider.as_deref(), Some(VOICE));
        assert!(r.resolve("hello").provider.is_none());
        // A shape stage cannot come before a clean one.
        let e = r
            .validate_order(&[s("mic-enhance"), s("mic-denoise"), s("mic-dereverb")])
            .unwrap_err();
        assert!(e.contains("cannot come after"), "{e}");
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
    fn frame_generation_is_in_no_order_but_has_a_provider() {
        let fg = entry(
            "org.example.fg",
            &[(FRAME_GENERATION, "post-composite", 100, "cpu", "")],
        );
        let mut r = Registry::from_entries([maxine(), onnx(), hello(), fg]);
        for id in [MAXINE, ONNX, HELLO, "org.example.fg"] {
            all_installed(&mut r, id);
        }
        assert!(r.stage_ids().contains(&s(FRAME_GENERATION)));
        let order: Vec<String> = r.default_order().iter().map(|s| s.to_string()).collect();
        assert_eq!(
            order,
            ["enhance", "denoise", "background", "relight", "hello"]
        );
        assert_eq!(r.order(), r.default_order());
        // An order without it is complete; one naming it is refused.
        assert!(r.validate_order(&r.default_order()).is_ok());
        let mut with = r.default_order();
        with.push(s(FRAME_GENERATION));
        let e = r.validate_order(&with).unwrap_err();
        assert!(e.contains("cannot name frame-generation"), "{e}");
        // A stored order naming it is kept without it.
        r.restore_order(Some(with));
        assert_eq!(r.order(), r.default_order());
        assert_eq!(
            r.resolve(FRAME_GENERATION).provider.as_deref(),
            Some("org.example.fg")
        );
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

    /// [`entry`] of module `id` with `head` (top-level keys and the
    /// `[requires]` table) before its stages.
    fn entry_with(id: &str, head: &str, stages: &[(&str, &str, u32, &str, &str)]) -> ModuleEntry {
        let mut e = entry(id, stages);
        let path = e.manifest_path.clone();
        let mut text = format!(
            "abi = 1\nid = \"{id}\"\nname = \"Name of {id}\"\nversion = \"1\"\n\
             licence = \"MIT\"\nkind = \"video-stage\"\nlibrary = \"/l/{id}.so\"\n{head}\n"
        );
        for (sid, slot, rank, device, extra) in stages {
            text.push_str(&format!(
                "[[stage]]\nid = \"{sid}\"\nslot = \"{slot}\"\nrank = {rank}\n\
                 device = \"{device}\"\nframes = [\"cuda\"]\nformats = [\"nv12\"]\n{extra}\n"
            ));
        }
        e.manifest = Manifest::parse(&text, &path);
        assert!(e.manifest.is_ok(), "{:?}", e.manifest);
        e
    }

    const A: &str = "org.x.a";
    const B: &str = "org.x.b";

    /// Modules `A` (stage `alpha`) and `B` (stage `beta`), with `head_a`
    /// and `head_b`, both probed with every stage installed.
    fn pair(head_a: &str, head_b: &str) -> Registry {
        let mut r = Registry::from_entries([
            entry_with(A, head_a, &[("alpha", "pre-matte", 100, "cpu", "")]),
            entry_with(B, head_b, &[("beta", "pre-matte", 100, "cpu", "")]),
        ]);
        all_installed(&mut r, A);
        all_installed(&mut r, B);
        r
    }

    fn unavailable(why: &str) -> Option<ModuleStatus> {
        Some(ModuleStatus::Unavailable(why.into()))
    }

    #[test]
    fn a_module_that_declares_a_conflict_with_an_enabled_one_is_refused() {
        let mut r = pair("conflicts = [\"org.x.b\"]", "");
        let why = "Conflicts with Name of org.x.b (org.x.b)";
        assert_eq!(r.status(A), unavailable(why));
        assert_eq!(r.status(B), Some(ModuleStatus::Available));
        assert!(r.is_refused(A) && !r.is_refused(B));
        // It offers nothing, and its stage says why.
        assert_eq!(r.stage_ids(), [s("beta")]);
        assert!(r.providers("alpha").is_empty());
        let res = r.resolve("alpha");
        assert_eq!((res.provider, res.reason.as_deref()), (None, Some(why)));
        let info = r.modules();
        let a = info.iter().find(|m| m.id == A).unwrap();
        assert_eq!(a.status, ModuleStatus::Unavailable(why.into()));
        assert!(!a.stages[0].available && a.stages[0].reason == why);
        assert!(!a.forced);
        // A pin to it says why it cannot be honoured.
        r.set_pin(s("alpha"), Some(A.into()));
        assert_eq!(
            r.resolve("alpha").reason.unwrap(),
            format!("Pinned module {A} is unavailable: {why}")
        );
        r.set_pin(s("alpha"), None);
        // The other one disabled: no conflict any more, whatever the order
        // things were opened in.
        r.set_enabled(B, false).unwrap();
        assert!(!r.is_refused(A));
        assert_eq!(r.status(A), Some(ModuleStatus::Available));
        assert_eq!(r.resolve("alpha").provider.as_deref(), Some(A));
        r.set_enabled(B, true).unwrap();
        assert_eq!(r.status(A), unavailable(why));
        // The other one's manifest not loadable: no conflict either.
        let mut r = Registry::from_entries([
            entry_with(
                A,
                "conflicts = [\"org.x.b\"]",
                &[("alpha", "pre-matte", 1, "cpu", "")],
            ),
            ModuleEntry {
                id: B.into(),
                manifest_path: "/m/b.toml".into(),
                manifest: Err("the manifest is malformed".into()),
            },
        ]);
        all_installed(&mut r, A);
        assert_eq!(r.status(A), Some(ModuleStatus::Available));
        // Not installed at all: no conflict.
        let mut r = Registry::from_entries([entry_with(
            A,
            "conflicts = [\"org.x.b\"]",
            &[("alpha", "pre-matte", 1, "cpu", "")],
        )]);
        all_installed(&mut r, A);
        assert_eq!(r.status(A), Some(ModuleStatus::Available));
    }

    #[test]
    fn a_mutual_module_conflict_refuses_only_the_higher_id() {
        let r = pair("conflicts = [\"org.x.b\"]", "conflicts = [\"org.x.a\"]");
        assert_eq!(r.status(A), Some(ModuleStatus::Available));
        assert_eq!(
            r.status(B),
            unavailable("Conflicts with Name of org.x.a (org.x.a)")
        );
        assert_eq!(r.stage_ids(), [s("alpha")]);
        // Only the declarer is refused, also when it is the lower id.
        let r = pair("", "conflicts = [\"org.x.a\"]");
        assert_eq!(r.status(A), Some(ModuleStatus::Available));
        assert!(r.is_refused(B));
    }

    /// Two modules that both offer `background`: `A` (rank `rank_a`) and
    /// `B` (rank `rank_b`), with `head_a` and `head_b`, all installed.
    fn cutouts(head_a: &str, rank_a: u32, head_b: &str, rank_b: u32) -> Registry {
        let mut r = Registry::from_entries([
            entry_with(A, head_a, &[("background", "matte", rank_a, "cpu", "")]),
            entry_with(B, head_b, &[("background", "matte", rank_b, "npu", "")]),
        ]);
        all_installed(&mut r, A);
        all_installed(&mut r, B);
        r
    }

    #[test]
    fn the_cut_out_module_never_loses_a_module_conflict() {
        let from = |m: &str| format!("Conflicts with Name of {m} ({m})");
        // Both declare it: the higher id (B) would be refused, but B gives
        // the cut-out by rank: A is.
        let mut r = cutouts(
            "conflicts = [\"org.x.b\"]",
            100,
            "conflicts = [\"org.x.a\"]",
            200,
        );
        assert_eq!(r.status(B), Some(ModuleStatus::Available));
        assert_eq!(r.status(A), unavailable(&from(B)));
        assert_eq!(r.resolve("background").provider.as_deref(), Some(B));
        // Ranked the other way: the usual rule (B refused).
        let r2 = cutouts(
            "conflicts = [\"org.x.b\"]",
            300,
            "conflicts = [\"org.x.a\"]",
            200,
        );
        assert_eq!(r2.status(B), unavailable(&from(A)));
        // Only B declares it, and B gives the cut-out: A is refused, though
        // it declares nothing and has the lower id.
        let r3 = cutouts("", 100, "conflicts = [\"org.x.a\"]", 200);
        assert_eq!(r3.status(A), unavailable(&from(B)));
        assert_eq!(r3.status(B), Some(ModuleStatus::Available));
        // The profile decides too: battery prefers B's NPU.
        let mut r4 = cutouts(
            "conflicts = [\"org.x.b\"]",
            300,
            "conflicts = [\"org.x.a\"]",
            200,
        );
        assert!(r4.is_refused(B));
        r4.set_profile(Profile::Battery);
        assert!(r4.is_refused(A) && !r4.is_refused(B));
        // And the pin, over the profile.
        r.set_pin(s("background"), Some(A.into()));
        assert!(r.is_refused(B) && !r.is_refused(A));
        assert_eq!(r.resolve("background").provider.as_deref(), Some(A));
        r.set_pin(s("background"), None);
        assert!(r.is_refused(A));
        // Whatever the probes say: the rule is static.
        r.record_probe(
            B,
            Ok(vec![(
                s("background"),
                Availability::NotInstalled("x".into()),
            )]),
        )
        .unwrap();
        assert!(r.is_refused(A));
        // Forced: neither is refused.
        r.set_forced([A.to_string()]);
        assert!(!r.is_refused(A) && !r.is_refused(B));
        // A module that offers no cut-out, in conflict with the cut-out's:
        // it is refused, whoever declares it.
        let mut r = Registry::from_entries([
            maxine(),
            entry_with(
                "aa.x.tint",
                "conflicts = [\"br.com.dias.maxine-vfx\"]",
                &[("tint", "post-composite", 1, "cpu", "")],
            ),
        ]);
        all_installed(&mut r, MAXINE);
        all_installed(&mut r, "aa.x.tint");
        assert!(r.is_refused("aa.x.tint") && !r.is_refused(MAXINE));
        let mut r = Registry::from_entries([
            entry_with(
                "zz.x.vfx",
                "conflicts = [\"aa.x.tint\"]",
                &[("background", "matte", 1, "cpu", "")],
            ),
            entry_with("aa.x.tint", "", &[("tint", "post-composite", 1, "cpu", "")]),
        ]);
        all_installed(&mut r, "zz.x.vfx");
        all_installed(&mut r, "aa.x.tint");
        assert!(r.is_refused("aa.x.tint") && !r.is_refused("zz.x.vfx"));
        assert_eq!(
            r.status("aa.x.tint"),
            unavailable("Conflicts with Name of zz.x.vfx (zz.x.vfx)")
        );
    }

    #[test]
    fn a_conflict_reason_bounds_the_name() {
        let long = "W".repeat(NAME_MAX + 40);
        let mut b = entry_with(B, "", &[("beta", "pre-matte", 1, "cpu", "")]);
        if let Ok(m) = &mut b.manifest {
            m.name = format!("Evil\u{202e}\n  {long}");
        }
        let mut r = Registry::from_entries([
            entry_with(
                A,
                "conflicts = [\"org.x.b\"]",
                &[("alpha", "pre-matte", 1, "cpu", "")],
            ),
            b,
        ]);
        all_installed(&mut r, A);
        let why = r.status(A).unwrap().reason().to_string();
        let shown = format!("Evil {}", "W".repeat(NAME_MAX - 5));
        assert_eq!(why, format!("Conflicts with {shown} (org.x.b)"));
        assert_eq!(shown_name("  a\tb \u{200b}c  "), "a b c");
        assert_eq!(shown_name(&"é".repeat(40)).len(), NAME_MAX);
    }

    #[test]
    fn a_forced_conflict_is_reported_only_while_both_run() {
        let mut r = pair("conflicts = [\"org.x.b\"]", "");
        r.set_forced([A.to_string()]);
        assert_eq!(
            r.forced_module_conflicts(),
            [(A.to_string(), B.to_string())]
        );
        // The other side failed to open.
        r.record_probe(B, Err("no library".into())).unwrap();
        assert!(r.forced_module_conflicts().is_empty());
        // Not opened yet.
        r.close(B).unwrap();
        assert!(r.forced_module_conflicts().is_empty());
        // Opened, but none of its stages can run.
        r.record_probe(
            B,
            Ok(vec![(s("beta"), Availability::NotInstalled("x".into()))]),
        )
        .unwrap();
        assert!(r.forced_module_conflicts().is_empty());
        all_installed(&mut r, B);
        assert_eq!(r.forced_module_conflicts().len(), 1);
        // The other side refused by a requirement.
        let mut r = Registry::from_entries([
            entry_with(
                A,
                "conflicts = [\"org.x.b\"]",
                &[("alpha", "pre-matte", 1, "cpu", "")],
            ),
            entry_with(
                B,
                "[requires]\nmodules = [\"org.x.none\"]",
                &[("beta", "pre-matte", 1, "cpu", "")],
            ),
        ]);
        all_installed(&mut r, A);
        all_installed(&mut r, B);
        r.set_forced([A.to_string()]);
        assert!(r.is_refused(B));
        assert!(r.forced_module_conflicts().is_empty());
        // The other side lost another conflict, not forced.
        let mut r = Registry::from_entries([
            entry_with(
                A,
                "conflicts = [\"org.x.b\"]",
                &[("alpha", "pre-matte", 1, "cpu", "")],
            ),
            entry_with(
                B,
                "conflicts = [\"org.x.c\"]",
                &[("beta", "pre-matte", 1, "cpu", "")],
            ),
            entry_with("org.x.c", "", &[("gamma", "pre-matte", 1, "cpu", "")]),
        ]);
        for id in [A, B, "org.x.c"] {
            all_installed(&mut r, id);
        }
        r.set_forced([A.to_string()]);
        assert!(r.is_refused(B) && !r.is_refused(A));
        assert!(r.forced_module_conflicts().is_empty());
    }

    #[test]
    fn tried_and_probed_tell_what_opening_gave() {
        let mut r = pair("conflicts = [\"org.x.b\"]", "");
        assert!(r.tried(A) && r.tried(B));
        assert!(r.is_refused(A));
        // Refused, its probe is still known.
        assert_eq!(r.probed(A, "alpha"), Some(Availability::Installed));
        assert_eq!(r.probed(A, "beta"), None);
        r.close(A).unwrap();
        assert!(!r.tried(A));
        assert_eq!(r.probed(A, "alpha"), None);
        r.record_probe(A, Err("broken".into())).unwrap();
        assert!(r.tried(A));
        assert_eq!(r.probed(A, "alpha"), None);
        // Disabled: closed, nothing known.
        r.set_enabled(B, false).unwrap();
        assert!(!r.tried(B));
        assert_eq!(r.probed(B, "beta"), None);
    }

    #[test]
    fn a_forced_side_lifts_a_module_conflict_and_is_reported() {
        for forced in [A, B] {
            let mut r = pair("conflicts = [\"org.x.b\"]", "conflicts = [\"org.x.a\"]");
            assert!(r.forced_module_conflicts().is_empty());
            r.set_forced([forced.to_string()]);
            assert!(r.is_forced(forced));
            assert_eq!(r.status(A), Some(ModuleStatus::Available), "{forced}");
            assert_eq!(r.status(B), Some(ModuleStatus::Available), "{forced}");
            assert_eq!(r.stage_ids(), [s("alpha"), s("beta")]);
            assert_eq!(
                r.forced_module_conflicts(),
                [(A.to_string(), B.to_string())]
            );
            assert!(r.modules().iter().any(|m| m.id == forced && m.forced));
            r.set_forced([]);
            assert!(r.is_refused(B));
        }
        // Forced with no conflict: nothing is overridden.
        let mut r = pair("", "");
        r.set_forced([A.to_string()]);
        assert!(r.forced_module_conflicts().is_empty());
        // A disabled other: nothing is overridden either.
        let mut r = pair("conflicts = [\"org.x.b\"]", "");
        r.set_forced([A.to_string()]);
        r.set_enabled(B, false).unwrap();
        assert!(r.forced_module_conflicts().is_empty());
    }

    #[test]
    fn a_conflict_between_two_modules_is_either_way_and_lifted_by_a_force() {
        let mut r = pair("conflicts = [\"org.x.b\"]", "");
        assert!(r.conflict_between(A, B) && r.conflict_between(B, A));
        assert!(!r.conflict_between(A, A));
        assert!(!r.conflict_between(A, "org.x.none"));
        r.set_forced([B.to_string()]);
        assert!(!r.conflict_between(A, B), "forced: not enforced");
        r.set_forced([]);
        r.set_enabled(B, false).unwrap();
        assert!(!r.conflict_between(A, B), "a disabled side is in none");
        assert!(!pair("", "").conflict_between(A, B));
    }

    #[test]
    fn only_a_module_of_a_kind_the_host_loads_is_listed_forced() {
        let mut r = Registry::from_entries([
            hello(),
            audio_entry(AFX, &[("mic-denoise", "clean", 300, "nvidia-gpu")]),
        ]);
        r.set_forced([HELLO.to_string(), AFX.to_string()]);
        let forced = |r: &Registry, id: &str| r.modules().iter().any(|m| m.id == id && m.forced);
        assert!(forced(&r, HELLO));
        assert!(!forced(&r, AFX), "an audio module in the video host");
        let mut audio = Registry::from_entries([
            hello(),
            audio_entry(AFX, &[("mic-denoise", "clean", 300, "nvidia-gpu")]),
        ])
        .with_kinds(LoadKinds::Audio);
        audio.set_forced([HELLO.to_string(), AFX.to_string()]);
        assert!(forced(&audio, AFX));
        assert!(!forced(&audio, HELLO), "a video module in the audio host");
    }

    #[test]
    fn a_refused_module_is_never_opened() {
        // Its library does not exist: opening it would make its status a
        // load failure.
        let mut r = Registry::from_entries([
            entry_with(
                A,
                "conflicts = [\"org.x.b\"]",
                &[("alpha", "pre-matte", 1, "cpu", "")],
            ),
            entry_with(B, "", &[("beta", "pre-matte", 1, "cpu", "")]),
            entry_with(
                "org.x.c",
                "[requires]\nmodules = [\"org.x.none\"]",
                &[("gamma", "pre-matte", 1, "cpu", "")],
            ),
        ]);
        let log: LogSink = std::sync::Arc::new(|_, _, _| {});
        r.open(A, &serde_json::json!({}), &log).unwrap();
        r.open("org.x.c", &serde_json::json!({}), &log).unwrap();
        assert!(r.module(A).is_none());
        assert_eq!(
            r.status(A),
            unavailable("Conflicts with Name of org.x.b (org.x.b)")
        );
        assert_eq!(
            r.status("org.x.c"),
            unavailable("Needs the module org.x.none")
        );
        // Not refused any more: not opened yet either.
        r.set_enabled(B, false).unwrap();
        assert_eq!(r.status(A), unavailable(REASON_NOT_LOADED));
        // Opened, then refused: it offers nothing.
        all_installed(&mut r, A);
        assert_eq!(r.resolve("alpha").provider.as_deref(), Some(A));
        r.set_enabled(B, true).unwrap();
        assert!(r.resolve("alpha").provider.is_none());
        assert!(r.stage_ids().iter().all(|x| x.as_str() != "alpha"));
    }

    #[test]
    fn a_module_that_needs_a_module_not_enabled_is_refused_even_forced() {
        let mut r = pair("[requires]\nmodules = [\"org.x.b\"]", "");
        assert_eq!(r.status(A), Some(ModuleStatus::Available));
        r.set_enabled(B, false).unwrap();
        let why = "Needs the module org.x.b";
        assert_eq!(r.status(A), unavailable(why));
        assert_eq!(r.resolve("alpha").reason.as_deref(), Some(why));
        r.set_forced([A.to_string(), B.to_string()]);
        assert_eq!(r.status(A), unavailable(why));
        r.set_enabled(B, true).unwrap();
        assert_eq!(r.status(A), Some(ModuleStatus::Available));
        // Not installed at all.
        let mut r = Registry::from_entries([entry_with(
            A,
            "[requires]\nmodules = [\"org.x.b\"]",
            &[("alpha", "pre-matte", 1, "cpu", "")],
        )]);
        all_installed(&mut r, A);
        assert_eq!(r.status(A), unavailable(why));
        // Required and in conflict with a third: the requirement is first,
        // and a module refused by one is no party to a conflict (the third
        // is not refused for it, even though it declares one).
        let mut r = Registry::from_entries([
            entry_with(
                A,
                "conflicts = [\"org.x.c\"]\n[requires]\nmodules = [\"org.x.b\"]",
                &[("alpha", "pre-matte", 1, "cpu", "")],
            ),
            entry_with(
                "org.x.c",
                "conflicts = [\"org.x.a\"]",
                &[("gamma", "pre-matte", 1, "cpu", "")],
            ),
        ]);
        all_installed(&mut r, A);
        all_installed(&mut r, "org.x.c");
        assert_eq!(r.status(A), unavailable(why));
        assert_eq!(r.status("org.x.c"), Some(ModuleStatus::Available));
    }

    #[test]
    fn a_module_that_needs_one_refused_by_a_conflict_is_not_refused_for_it() {
        // C needs A; A loses its conflict with B: C stays (one level).
        let mut r = Registry::from_entries([
            entry_with(
                A,
                "conflicts = [\"org.x.b\"]",
                &[("alpha", "pre-matte", 1, "cpu", "")],
            ),
            entry_with(B, "", &[("beta", "pre-matte", 1, "cpu", "")]),
            entry_with(
                "org.x.c",
                "[requires]\nmodules = [\"org.x.a\"]",
                &[("gamma", "pre-matte", 1, "cpu", "")],
            ),
        ]);
        for id in [A, B, "org.x.c"] {
            all_installed(&mut r, id);
        }
        assert!(r.is_refused(A));
        assert!(!r.is_refused("org.x.c"));
        assert_eq!(r.status("org.x.c"), Some(ModuleStatus::Available));
    }

    #[test]
    fn a_provider_whose_required_stage_or_slot_has_no_provider_cannot_run_it() {
        let stages: &[(&str, &str, u32, &str, &str)] = &[
            ("pre", "pre-matte", 1, "cpu", ""),
            ("tone", "post-composite", 1, "cpu", "requires = [\"pre\"]"),
            (
                "grade",
                "post-composite",
                1,
                "cpu",
                "requires = [\"pre-matte\"]",
            ),
            (
                "lut",
                "post-composite",
                1,
                "cpu",
                "requires = [\"sharpen\"]",
            ),
        ];
        let mut r = Registry::from_entries([entry_with(A, "", stages)]);
        all_installed(&mut r, A);
        for st in ["tone", "grade", "pre"] {
            assert_eq!(r.resolve(st).provider.as_deref(), Some(A), "{st}");
        }
        // A required stage no module offers.
        let res = r.resolve("lut");
        assert_eq!(res.provider, None);
        assert_eq!(res.reason.as_deref(), Some("Needs sharpen"));
        // No order error for it.
        assert!(r.validate_order(&r.default_order()).is_ok());
        // The required stage offered but unable to run.
        r.record_probe(
            A,
            Ok(vec![
                (s("pre"), Availability::NotInstalled("no model".into())),
                (s("tone"), Availability::Installed),
                (s("grade"), Availability::Installed),
                (s("lut"), Availability::Installed),
            ]),
        )
        .unwrap();
        let res = r.resolve("tone");
        assert_eq!(
            (res.provider, res.reason.as_deref()),
            (None, Some("Needs pre"))
        );
        let res = r.resolve("grade");
        assert_eq!(
            (res.provider, res.reason.as_deref()),
            (None, Some("Needs a pre-matte stage"))
        );
        // The modules list says why the resolution refuses them, though
        // the probe says they can run.
        let info = r.modules();
        let stage = |id: &str| info[0].stages.iter().find(|x| x.id == s(id)).unwrap();
        assert!(!stage("tone").available);
        assert_eq!(stage("tone").reason, "Needs pre");
        assert!(!stage("grade").available);
        assert_eq!(stage("grade").reason, "Needs a pre-matte stage");
        assert!(!stage("lut").available);
        assert_eq!(stage("lut").reason, "Needs sharpen");
        // The probe's own reason stays the probe's.
        assert!(!stage("pre").available);
        assert_eq!(stage("pre").reason, "no model");
    }

    #[test]
    fn a_matte_or_post_matte_stage_runs_only_from_the_cut_out_module() {
        let mut r = Registry::from_entries([
            maxine(),
            entry_with(
                A,
                "",
                &[
                    ("tint", "post-matte", 900, "cpu", ""),
                    ("edge", "matte", 900, "cpu", ""),
                    ("warm", "pre-matte", 900, "cpu", ""),
                    ("glow", "post-composite", 900, "cpu", ""),
                ],
            ),
        ]);
        all_installed(&mut r, MAXINE);
        all_installed(&mut r, A);
        assert_eq!(r.resolve("background").provider.as_deref(), Some(MAXINE));
        for st in ["tint", "edge"] {
            let res = r.resolve(st);
            assert_eq!(res.provider, None, "{st}");
            assert_eq!(res.reason.as_deref(), Some(REASON_NEEDS_CUTOUT), "{st}");
        }
        for st in ["warm", "glow"] {
            assert_eq!(r.resolve(st).provider.as_deref(), Some(A), "{st}");
        }
        // The modules list says why A cannot run them; a stage the cut-out's
        // module provides is merely not A's.
        let info = r.modules();
        let a = info.iter().find(|m| m.id == A).unwrap();
        for st in ["tint", "edge"] {
            let x = a.stages.iter().find(|x| x.id == s(st)).unwrap();
            assert!(!x.available && !x.selected, "{st}");
            assert_eq!(x.reason, REASON_NEEDS_CUTOUT, "{st}");
        }
        let warm = a.stages.iter().find(|x| x.id == s("warm")).unwrap();
        assert!(warm.available && warm.selected);
        // No cut-out at all: no module can run them either.
        r.set_enabled(MAXINE, false).unwrap();
        assert_eq!(r.resolve("background").provider, None);
        for st in ["tint", "edge"] {
            let res = r.resolve(st);
            assert_eq!(res.provider, None, "{st}");
            assert_eq!(res.reason.as_deref(), Some(REASON_NEEDS_CUTOUT), "{st}");
        }
        assert_eq!(r.resolve("warm").provider.as_deref(), Some(A));
        // The cut-out's own module runs them.
        let mut r = Registry::from_entries([entry_with(
            A,
            "",
            &[
                ("background", "matte", 1, "cpu", ""),
                ("tint", "post-matte", 1, "cpu", ""),
            ],
        )]);
        all_installed(&mut r, A);
        assert_eq!(r.resolve("tint").provider.as_deref(), Some(A));
    }

    #[test]
    fn a_stage_conflict_turns_off_the_loser() {
        let mut r = Registry::from_entries([
            maxine(),
            entry_with(
                A,
                "",
                &[
                    ("alpha", "pre-matte", 1, "cpu", "conflicts = [\"beta\"]"),
                    ("beta", "post-composite", 1, "cpu", ""),
                    (
                        "gamma",
                        "post-composite",
                        1,
                        "cpu",
                        "conflicts = [\"delta\"]",
                    ),
                    (
                        "delta",
                        "post-composite",
                        1,
                        "cpu",
                        "conflicts = [\"gamma\"]",
                    ),
                    (
                        "eps",
                        "post-composite",
                        1,
                        "cpu",
                        "conflicts = [\"background\"]",
                    ),
                    ("zeta", "post-composite", 1, "cpu", ""),
                ],
            ),
        ]);
        all_installed(&mut r, MAXINE);
        all_installed(&mut r, A);
        let none = |_: &StageId| false;
        // The declarer wins, whichever comes first.
        let want: BTreeMap<StageId, StageId> = [(s("beta"), s("alpha"))].into();
        assert_eq!(r.excluded(&[s("alpha"), s("beta")], none), want);
        assert_eq!(r.excluded(&[s("beta"), s("alpha")], none), want);
        // Both declare: the later one wins.
        assert_eq!(
            r.excluded(&[s("gamma"), s("delta")], none),
            [(s("gamma"), s("delta"))].into()
        );
        assert_eq!(
            r.excluded(&[s("delta"), s("gamma")], none),
            [(s("delta"), s("gamma"))].into()
        );
        // The background never loses, even to the declarer.
        assert_eq!(
            r.excluded(&[s("background"), s("eps")], none),
            [(s("eps"), s("background"))].into()
        );
        // No conflict, or one side not on: nothing.
        assert!(
            r.excluded(&[s("alpha"), s("zeta"), s("gamma")], none)
                .is_empty()
        );
        assert!(
            r.forced_stage_conflicts(&[s("alpha"), s("beta")], none)
                .is_empty()
        );
        // Forced on either side: not enforced, and reported.
        for f in ["alpha", "beta"] {
            let forced = |x: &StageId| x.as_str() == f;
            assert!(
                r.excluded(&[s("alpha"), s("beta")], forced).is_empty(),
                "{f}"
            );
            assert_eq!(
                r.forced_stage_conflicts(&[s("alpha"), s("beta"), s("zeta")], forced),
                [(s("alpha"), s("beta"))]
            );
        }
        // Forced: the background does not win either.
        let forced = |x: &StageId| x.as_str() == "eps";
        assert!(r.excluded(&[s("background"), s("eps")], forced).is_empty());
        // The in-tree example: hello declares a conflict with relight.
        let r = registry();
        assert_eq!(
            r.excluded(&[s("denoise"), s("relight"), s("hello")], none),
            [(s("relight"), s("hello"))].into()
        );
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
