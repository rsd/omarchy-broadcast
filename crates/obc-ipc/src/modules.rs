//! The modules of the daemon (revision 9): what `GetModules` returns, read
//! into a typed model ([`ModuleInfo`], [`StageInfo`]) and written back.
//!
//! `GetModules() -> (u serial, aa{sv} modules)`: `serial` is the
//! `ModulesSerial` property's value when the list was made; each module is a
//! dictionary with `id`, `name`, `version`, `licence`, `kind`, `library`,
//! `manifest` (the manifest file), `description`, `enabled` (b), `status`
//! (`available`, `unavailable` or `disabled`), `reason` (why it is not
//! available, empty when it is) and `stages` (aa{sv}): each with `id`,
//! `slot`, `rank` (u), `device`, `available` (b), `reason` and `selected`
//! (b: this module provides the stage at the next pipeline start).
//!
//! Reading is tolerant, as for the settings channel: a key missing or of
//! another type reads as its default, keys this crate does not know are
//! ignored, and an entry without an `id` is dropped.

use crate::settings::{
    Entries, VariantDict, entries_of, entries_of_dict, get_bool, get_int, get_str, list_of, put,
    to_owned,
};
use serde::Serialize;
use zbus::Variant;

/// The first [`crate::INTERFACE_REVISION`] with the modules: `GetModules`,
/// `SetModuleEnabled`, `GetStageOrder`, `SetStageOrder`,
/// `SetBackendProfile`, `SetStagePin`, `ModulesSerial` and
/// `BackendProfile`.
pub const MODULES_REVISION: u32 = 9;

/// The `GetModules` reply on the wire.
pub type ModulesReply = (u32, Vec<VariantDict>);

/// A module's `status`: it can run.
pub const STATUS_AVAILABLE: &str = "available";
/// A module's `status`: it cannot run (`reason` says why).
pub const STATUS_UNAVAILABLE: &str = "unavailable";
/// A module's `status`: it is disabled (`reason` says by whom).
pub const STATUS_DISABLED: &str = "disabled";

/// The backend profiles `SetBackendProfile` takes, in display order.
pub const PROFILES: [&str; 2] = ["quality", "battery"];

/// One stage of a module.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct StageInfo {
    pub id: String,
    /// `pre-matte`, `matte`, `post-matte` or `post-composite`.
    pub slot: String,
    pub rank: u32,
    /// `nvidia-gpu`, `gpu`, `npu` or `cpu`.
    pub device: String,
    pub available: bool,
    pub reason: String,
    /// This module provides the stage at the next pipeline start.
    pub selected: bool,
}

/// One module.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ModuleInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub licence: String,
    pub kind: String,
    pub library: String,
    pub manifest: String,
    pub description: String,
    pub enabled: bool,
    /// [`STATUS_AVAILABLE`], [`STATUS_UNAVAILABLE`] or [`STATUS_DISABLED`].
    pub status: String,
    pub reason: String,
    pub stages: Vec<StageInfo>,
}

fn text(e: &Entries<'_, '_>, key: &str) -> String {
    get_str(e, key).unwrap_or_default().to_string()
}

impl StageInfo {
    /// The stage dictionary.
    pub fn to_dict(&self) -> VariantDict {
        let mut d = VariantDict::new();
        put(&mut d, "id", Variant::from(self.id.as_str()));
        put(&mut d, "slot", Variant::from(self.slot.as_str()));
        put(&mut d, "rank", Variant::from(self.rank));
        put(&mut d, "device", Variant::from(self.device.as_str()));
        put(&mut d, "available", Variant::from(self.available));
        put(&mut d, "reason", Variant::from(self.reason.as_str()));
        put(&mut d, "selected", Variant::from(self.selected));
        d
    }

    fn from_entries(e: &Entries<'_, '_>) -> Option<StageInfo> {
        let id = get_str(e, "id")?.to_string();
        Some(StageInfo {
            id,
            slot: text(e, "slot"),
            rank: get_int(e, "rank")
                .and_then(|r| u32::try_from(r).ok())
                .unwrap_or_default(),
            device: text(e, "device"),
            available: get_bool(e, "available").unwrap_or_default(),
            reason: text(e, "reason"),
            selected: get_bool(e, "selected").unwrap_or_default(),
        })
    }
}

impl ModuleInfo {
    /// The module dictionary.
    pub fn to_dict(&self) -> VariantDict {
        let mut d = VariantDict::new();
        for (k, v) in [
            ("id", &self.id),
            ("name", &self.name),
            ("version", &self.version),
            ("licence", &self.licence),
            ("kind", &self.kind),
            ("library", &self.library),
            ("manifest", &self.manifest),
            ("description", &self.description),
            ("status", &self.status),
            ("reason", &self.reason),
        ] {
            put(&mut d, k, Variant::from(v.as_str()));
        }
        put(&mut d, "enabled", Variant::from(self.enabled));
        let stages: Vec<VariantDict> = self.stages.iter().map(StageInfo::to_dict).collect();
        d.insert("stages".into(), to_owned(&stages));
        d
    }

    /// The module of a dictionary; `None` without an `id`.
    pub fn from_dict(dict: &VariantDict) -> Option<ModuleInfo> {
        let e = entries_of_dict(dict);
        let id = get_str(&e, "id")?.to_string();
        let stages = e
            .get("stages")
            .and_then(|v| list_of(v))
            .map(|list| {
                list.iter()
                    .filter_map(|v| entries_of(v).and_then(|e| StageInfo::from_entries(&e)))
                    .collect()
            })
            .unwrap_or_default();
        Some(ModuleInfo {
            id,
            name: text(&e, "name"),
            version: text(&e, "version"),
            licence: text(&e, "licence"),
            kind: text(&e, "kind"),
            library: text(&e, "library"),
            manifest: text(&e, "manifest"),
            description: text(&e, "description"),
            enabled: get_bool(&e, "enabled").unwrap_or_default(),
            status: text(&e, "status"),
            reason: text(&e, "reason"),
            stages,
        })
    }

    /// The stage `id` of this module.
    pub fn stage(&self, id: &str) -> Option<&StageInfo> {
        self.stages.iter().find(|s| s.id == id)
    }
}

/// The modules with the serial they were listed at.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ModuleList {
    pub serial: u32,
    pub modules: Vec<ModuleInfo>,
}

impl ModuleList {
    /// The wire form.
    pub fn to_wire(&self) -> ModulesReply {
        (
            self.serial,
            self.modules.iter().map(ModuleInfo::to_dict).collect(),
        )
    }

    /// The list of a reply; entries without an `id` are dropped.
    pub fn from_wire(reply: &ModulesReply) -> ModuleList {
        ModuleList {
            serial: reply.0,
            modules: reply.1.iter().filter_map(ModuleInfo::from_dict).collect(),
        }
    }

    /// The module `id`.
    pub fn find(&self, id: &str) -> Option<&ModuleInfo> {
        self.modules.iter().find(|m| m.id == id)
    }

    /// The module that provides stage `id` at the next pipeline start.
    pub fn provider_of(&self, stage: &str) -> Option<&ModuleInfo> {
        self.modules
            .iter()
            .find(|m| m.stage(stage).is_some_and(|s| s.selected))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::OwnedVariant;
    use zbus::wire::serialized::Context;
    use zbus::wire::{LE, to_bytes};

    fn sample() -> ModuleList {
        ModuleList {
            serial: 3,
            modules: vec![ModuleInfo {
                id: "org.example.hello".into(),
                name: "Hello tint".into(),
                version: "0.1.0".into(),
                licence: "MIT".into(),
                kind: "video-stage".into(),
                library: "/m/libhello.so".into(),
                manifest: "/m/hello.toml".into(),
                description: String::new(),
                enabled: true,
                status: STATUS_AVAILABLE.into(),
                reason: String::new(),
                stages: vec![StageInfo {
                    id: "hello".into(),
                    slot: "post-composite".into(),
                    rank: 100,
                    device: "cpu".into(),
                    available: true,
                    reason: String::new(),
                    selected: true,
                }],
            }],
        }
    }

    #[test]
    fn the_list_round_trips_through_the_wire() {
        let list = sample();
        let wire = list.to_wire();
        // Through the bus encoding, as a client receives it.
        let ctxt = Context::new(LE, 0);
        let bytes = to_bytes(ctxt, &wire).unwrap();
        let (decoded, _): (ModulesReply, _) = bytes.deserialize().unwrap();
        let back = ModuleList::from_wire(&decoded);
        assert_eq!(back, list);
        assert_eq!(back.provider_of("hello").unwrap().id, "org.example.hello");
        assert!(back.provider_of("denoise").is_none());
    }

    #[test]
    fn reading_is_tolerant() {
        let mut d = sample().modules[0].to_dict();
        d.insert("enabled".into(), OwnedVariant::from(7u32));
        d.insert("later".into(), OwnedVariant::from(true));
        d.remove("name");
        let m = ModuleInfo::from_dict(&d).unwrap();
        assert!(!m.enabled);
        assert_eq!(m.name, "");
        assert_eq!(m.stages.len(), 1);
        d.remove("id");
        assert!(ModuleInfo::from_dict(&d).is_none());
        let list = ModuleList::from_wire(&(1, vec![d]));
        assert!(list.modules.is_empty());
    }
}
