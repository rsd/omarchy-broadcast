//! `modules` and `order`: the effect modules the daemon found, their
//! stages, the backend profile, the pins and the stage order (interface
//! revision 9). From a daemon of revision 10, the choices a later
//! configuration file sets (which the daemon refuses to change) are shown
//! with that file, and `order` shows the moves of each stage the daemon
//! would accept.
//!
//! The reads (`modules`, `modules list`, `modules info`, `modules profile`
//! without a profile, `order`, `order show`) never start the daemon; the
//! changes may, through D-Bus activation. A change takes effect when the
//! camera next starts; it is printed as read back.

use std::collections::BTreeMap;

use clap::Subcommand;
use obc_ipc::{ConfigSchema, IpcError, ModuleInfo, ModuleList};
use serde::Serialize;
use zbus::Connection;

use crate::bus::{self, EXIT_OK, Failure, READ_TIMEOUT};
use crate::human::printable;
use crate::json::{
    self, MAX_NAME_BYTES, MAX_PATH_BYTES, MAX_REASON_BYTES, VERSION, clip, clip_start,
};
use crate::{CALL_TIMEOUT, out, report, within};

/// The text when the daemon found no module.
pub const NO_MODULES: &str = "No effect modules installed";

/// The longest module id in JSON, in bytes.
const MAX_MODULE_ID_BYTES: usize = 64;
/// The longest stage id, slot or device in JSON, in bytes.
const MAX_STAGE_FIELD_BYTES: usize = 32;
/// The longest version, kind or licence in JSON, in bytes.
const MAX_TEXT_BYTES: usize = 96;
/// The longest description in JSON, in bytes.
const MAX_DESCRIPTION_BYTES: usize = 160;
/// At most this many stages of a module, and stage ids of an order, are
/// listed.
const MAX_STAGES: usize = 32;

/// The subcommands of `modules`.
#[derive(Subcommand, Clone)]
pub enum ModulesAction {
    /// List the modules, their status and stages, and the backend profile.
    List {
        /// Print JSON lines (`"type":"modules"`, then `"type":"module"`).
        #[arg(long)]
        json: bool,
    },
    /// Show one module in full.
    Info {
        /// The module id, e.g. `br.com.dias.maxine-vfx`.
        id: String,
        /// Print one JSON line (`"type":"module"`).
        #[arg(long)]
        json: bool,
    },
    /// Enable a module (from the next camera start; its settings appear at
    /// once). Enabling a module the daemon disabled after crashes forgives
    /// it.
    Enable {
        /// The module id.
        id: String,
    },
    /// Disable a module (from the next camera start).
    Disable {
        /// The module id.
        id: String,
    },
    /// Show or set the backend profile: `quality` (the highest-ranked
    /// module) or `battery` (an NPU or the CPU first).
    Profile {
        /// The profile to set; omitted, the current one is printed.
        #[arg(value_parser = obc_ipc::modules::PROFILES)]
        profile: Option<String>,
    },
    /// Pin the module that provides a stage, whatever the profile says
    /// (`--clear` removes the pin).
    Pin {
        /// The stage id, e.g. `background`.
        stage: String,
        /// The module id.
        #[arg(required_unless_present = "clear", conflicts_with = "clear")]
        module: Option<String>,
        /// Remove the stage's pin.
        #[arg(long)]
        clear: bool,
    },
}

/// The subcommands of `order`.
#[derive(Subcommand, Clone)]
pub enum OrderAction {
    /// Print the stage order.
    Show {
        /// Print one JSON line (`"type":"order"`).
        #[arg(long)]
        json: bool,
    },
    /// Set the stage order: every stage the enabled modules offer, once
    /// each, comma-separated, e.g. `enhance,denoise,background,relight`.
    /// The daemon refuses an order the stages cannot run in, with the
    /// reason.
    Set {
        /// The stage ids, comma-separated.
        #[arg(value_name = "ID,...", value_delimiter = ',', num_args = 1)]
        order: Vec<String>,
    },
    /// Go back to the default order (by slot, then each stage's preferred
    /// position).
    Reset,
}

// ---------------------------------------------------------------------------
// Human output
// ---------------------------------------------------------------------------

fn status_text(m: &ModuleInfo) -> String {
    if m.reason.is_empty() {
        m.status.clone()
    } else {
        format!("{}: {}", m.status, printable(&m.reason))
    }
}

fn stage_lines(m: &ModuleInfo, out: &mut String) {
    for s in &m.stages {
        let mut line = format!(
            "    {} ({}, {}, rank {})",
            printable(&s.id),
            printable(&s.slot),
            printable(&s.device),
            s.rank
        );
        if s.selected {
            line.push_str(" selected");
        } else if !s.available {
            line.push_str(&format!(" unavailable: {}", printable(&s.reason)));
        }
        out.push_str(&line);
        out.push('\n');
    }
}

/// What the configuration files after `config.d/50-panel.toml` set of the
/// module choices (the daemon refuses to change those): each the file, `""`
/// when none does (always, from a daemon before revision 10).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    /// `modules.profile`.
    pub profile: String,
    /// `modules.order`.
    pub order: String,
    /// `modules.enabled.<id>`, by module id.
    pub enabled: BTreeMap<String, String>,
}

impl Overrides {
    /// The overrides the config schema's rows name.
    pub fn of(schema: &ConfigSchema) -> Overrides {
        let mut o = Overrides::default();
        for r in schema.groups.iter().flat_map(|g| &g.rows) {
            if r.overridden_by.is_empty() {
                continue;
            }
            if r.id == "modules.profile" {
                o.profile = r.overridden_by.clone();
            } else if r.id == "modules.order" {
                o.order = r.overridden_by.clone();
            } else if let Some(id) = r.id.strip_prefix("modules.enabled.") {
                o.enabled.insert(id.to_string(), r.overridden_by.clone());
            }
        }
        o
    }

    /// The file that sets module `id`'s `modules.enabled`, `""` if none.
    pub fn enabled_of(&self, id: &str) -> &str {
        self.enabled.get(id).map_or("", String::as_str)
    }
}

/// ` (set by <file>)`, or nothing.
fn set_by(file: &str) -> String {
    if file.is_empty() {
        String::new()
    } else {
        format!(" (set by {})", printable(file))
    }
}

/// `modules list`.
pub fn list_text(list: &ModuleList, profile: &str, over: &Overrides) -> String {
    if list.modules.is_empty() {
        return format!("{NO_MODULES}\n");
    }
    let mut out = format!("Profile: {}{}\n", printable(profile), set_by(&over.profile));
    for m in &list.modules {
        out.push_str(&format!(
            "{}  {} {}  {}{}{}\n",
            printable(&m.id),
            printable(&m.name),
            printable(&m.version),
            status_text(m),
            if m.enabled { "" } else { " (disabled)" },
            set_by(over.enabled_of(&m.id))
        ));
        stage_lines(m, &mut out);
    }
    out
}

/// `modules info`.
pub fn info_text(m: &ModuleInfo, over: &Overrides) -> String {
    let mut out = String::new();
    let mut row = |k: &str, v: &str| {
        if !v.is_empty() {
            out.push_str(&format!("{k:<12} {}\n", printable(v)));
        }
    };
    row("Module:", &m.id);
    row("Name:", &m.name);
    row("Version:", &m.version);
    row("Licence:", &m.licence);
    row("Kind:", &m.kind);
    row("Description:", &m.description);
    row("Library:", &m.library);
    row("Manifest:", &m.manifest);
    row("Enabled:", if m.enabled { "yes" } else { "no" });
    row("Set by:", over.enabled_of(&m.id));
    row("Status:", &status_text(m));
    out.push_str("Stages:\n");
    stage_lines(m, &mut out);
    out
}

/// One stage of the order: its slot, and whether the daemon would accept
/// it swapped with the stage before (`up`) or after (`down`) it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrderStage {
    pub id: String,
    pub slot: String,
    pub up: bool,
    pub down: bool,
}

/// `order show`: the stages in order, then the file that sets the order
/// when a later file does.
pub fn order_text(order: &[OrderStage], overridden_by: &str) -> String {
    if order.is_empty() {
        return format!("{NO_MODULES}\n");
    }
    let mut out = String::new();
    for (i, s) in order.iter().enumerate() {
        out.push_str(&format!("{}. {}\n", i + 1, printable(&s.id)));
    }
    if !overridden_by.is_empty() {
        out.push_str(&format!(
            "The order is set by {}: change it there.\n",
            printable(overridden_by)
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// JSON output
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ModulesLine<'a> {
    v: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    serial: u32,
    profile: std::borrow::Cow<'a, str>,
    count: usize,
    profile_overridden_by: std::borrow::Cow<'a, str>,
}

#[derive(Serialize)]
struct StageJson<'a> {
    id: std::borrow::Cow<'a, str>,
    slot: std::borrow::Cow<'a, str>,
    rank: u32,
    device: std::borrow::Cow<'a, str>,
    available: bool,
    reason: std::borrow::Cow<'a, str>,
    selected: bool,
}

#[derive(Serialize)]
struct ModuleJson<'a> {
    id: std::borrow::Cow<'a, str>,
    name: std::borrow::Cow<'a, str>,
    version: std::borrow::Cow<'a, str>,
    licence: std::borrow::Cow<'a, str>,
    kind: std::borrow::Cow<'a, str>,
    description: std::borrow::Cow<'a, str>,
    library: std::borrow::Cow<'a, str>,
    manifest: std::borrow::Cow<'a, str>,
    enabled: bool,
    status: std::borrow::Cow<'a, str>,
    reason: std::borrow::Cow<'a, str>,
    stages: Vec<StageJson<'a>>,
    stages_truncated: bool,
    overridden_by: std::borrow::Cow<'a, str>,
}

#[derive(Serialize)]
struct ModuleLine<'a> {
    v: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    serial: u32,
    index: usize,
    count: usize,
    module: ModuleJson<'a>,
}

#[derive(Serialize)]
struct OrderStageJson<'a> {
    id: std::borrow::Cow<'a, str>,
    slot: std::borrow::Cow<'a, str>,
    up: bool,
    down: bool,
}

#[derive(Serialize)]
struct OrderLine<'a> {
    v: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    order: Vec<std::borrow::Cow<'a, str>>,
    order_truncated: bool,
    stages: Vec<OrderStageJson<'a>>,
    overridden_by: std::borrow::Cow<'a, str>,
}

fn to_line<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).expect("a line always serializes")
}

fn module_json<'a>(m: &'a ModuleInfo, overridden_by: &'a str) -> ModuleJson<'a> {
    ModuleJson {
        id: clip(&m.id, MAX_MODULE_ID_BYTES),
        name: clip(&m.name, MAX_NAME_BYTES),
        version: clip(&m.version, MAX_TEXT_BYTES),
        licence: clip(&m.licence, MAX_TEXT_BYTES),
        kind: clip(&m.kind, MAX_STAGE_FIELD_BYTES),
        description: clip(&m.description, MAX_DESCRIPTION_BYTES),
        library: clip_start(&m.library, MAX_PATH_BYTES),
        manifest: clip_start(&m.manifest, MAX_PATH_BYTES),
        enabled: m.enabled,
        status: clip(&m.status, MAX_STAGE_FIELD_BYTES),
        reason: clip(&m.reason, MAX_REASON_BYTES),
        stages: m
            .stages
            .iter()
            .take(MAX_STAGES)
            .map(|s| StageJson {
                id: clip(&s.id, MAX_STAGE_FIELD_BYTES),
                slot: clip(&s.slot, MAX_STAGE_FIELD_BYTES),
                rank: s.rank,
                device: clip(&s.device, MAX_STAGE_FIELD_BYTES),
                available: s.available,
                reason: clip(&s.reason, MAX_REASON_BYTES),
                selected: s.selected,
            })
            .collect(),
        stages_truncated: m.stages.len() > MAX_STAGES,
        overridden_by: clip_start(overridden_by, MAX_PATH_BYTES),
    }
}

/// `{"v":1,"type":"modules","serial":S,"profile":P,"count":N,"profile_overridden_by":F}`:
/// the header of a module list; `N` `module` lines follow.
pub fn modules_line(list: &ModuleList, profile: &str, over: &Overrides) -> String {
    to_line(&ModulesLine {
        v: VERSION,
        kind: "modules",
        serial: list.serial,
        profile: clip(profile, MAX_STAGE_FIELD_BYTES),
        count: list.modules.len(),
        profile_overridden_by: clip_start(&over.profile, MAX_PATH_BYTES),
    })
}

/// `{"v":1,"type":"module","serial":S,"index":I,"count":N,"module":{...}}`:
/// one module. A line over [`json::MAX_LINE_BYTES`] gives up stages from
/// the end (`stages_truncated` is then `true`).
pub fn module_line(
    serial: u32,
    index: usize,
    count: usize,
    m: &ModuleInfo,
    overridden_by: &str,
) -> String {
    let mut module = module_json(m, overridden_by);
    loop {
        let line = to_line(&ModuleLine {
            v: VERSION,
            kind: "module",
            serial,
            index,
            count,
            module,
        });
        if line.len() <= json::MAX_LINE_BYTES {
            return line;
        }
        let keep = serde_json::from_str::<serde_json::Value>(&line)
            .ok()
            .and_then(|v| v["module"]["stages"].as_array().map(Vec::len))
            .unwrap_or(0);
        module = module_json(m, overridden_by);
        let n = keep.saturating_sub(1);
        module.stages.truncate(n);
        module.stages_truncated = true;
        if n == 0 {
            return to_line(&ModuleLine {
                v: VERSION,
                kind: "module",
                serial,
                index,
                count,
                module,
            });
        }
    }
}

/// `{"v":1,"type":"order","order":[...],"order_truncated":false,
/// "stages":[{"id","slot","up","down"},...],"overridden_by":""}`: `up` /
/// `down` whether the daemon would accept the stage swapped with the one
/// before / after it (always false from a daemon before revision 10).
pub fn order_line(order: &[OrderStage], overridden_by: &str) -> String {
    let shown = || order.iter().take(MAX_STAGES);
    to_line(&OrderLine {
        v: VERSION,
        kind: "order",
        order: shown()
            .map(|s| clip(&s.id, MAX_STAGE_FIELD_BYTES))
            .collect(),
        order_truncated: order.len() > MAX_STAGES,
        stages: shown()
            .map(|s| OrderStageJson {
                id: clip(&s.id, MAX_STAGE_FIELD_BYTES),
                slot: clip(&s.slot, MAX_STAGE_FIELD_BYTES),
                up: s.up,
                down: s.down,
            })
            .collect(),
        overridden_by: clip_start(overridden_by, MAX_PATH_BYTES),
    })
}

// ---------------------------------------------------------------------------
// Running the commands
// ---------------------------------------------------------------------------

/// What later files set of the module choices, from a daemon of
/// `revision` (nothing before revision 10), without starting it.
async fn overrides(conn: &Connection, revision: u32) -> Result<Overrides, Failure> {
    if revision < obc_ipc::CONFIG_REVISION {
        return Ok(Overrides::default());
    }
    bus::config_schema(conn).await.map(|s| Overrides::of(&s))
}

/// The modules, the profile and the overrides, without starting the
/// daemon.
async fn read() -> Result<(ModuleList, String, Overrides), Failure> {
    let conn = bus::connect().await?;
    let snap = bus::fetch(&conn).await?;
    bus::require_modules(snap.interface_revision)?;
    let list = bus::modules(&conn).await?;
    let over = overrides(&conn, snap.interface_revision).await?;
    Ok((list, snap.backend_profile.unwrap_or_default(), over))
}

/// The stage order and the file that sets it, without starting the
/// daemon: from a daemon of revision 10 with the moves it accepts; from
/// one of revision 9 with the slots of the module list and no move.
async fn read_order() -> Result<(Vec<OrderStage>, String), Failure> {
    let conn = bus::connect().await?;
    let snap = bus::fetch(&conn).await?;
    bus::require_modules(snap.interface_revision)?;
    if snap.interface_revision >= obc_ipc::CONFIG_REVISION {
        let moves = bus::stage_moves(&conn).await?;
        let over = overrides(&conn, snap.interface_revision).await?;
        let order = moves
            .into_iter()
            .map(|(id, slot, up, down)| OrderStage { id, slot, up, down })
            .collect();
        return Ok((order, over.order));
    }
    let order = bus::stage_order(&conn).await?;
    let list = bus::modules(&conn).await?;
    let slot_of = |id: &str| {
        list.modules
            .iter()
            .flat_map(|m| m.stages.iter())
            .find(|s| s.id == id)
            .map(|s| s.slot.clone())
            .unwrap_or_default()
    };
    let order = order
        .into_iter()
        .map(|id| OrderStage {
            slot: slot_of(&id),
            id,
            up: false,
            down: false,
        })
        .collect();
    Ok((order, String::new()))
}

/// `modules [list|info|enable|disable|profile|pin]`.
pub async fn run_modules(action: &ModulesAction) -> u8 {
    match action {
        ModulesAction::List { json } => match within(READ_TIMEOUT, read()).await {
            Ok((list, profile, over)) => {
                if *json {
                    out::line(&modules_line(&list, &profile, &over));
                    for (i, m) in list.modules.iter().enumerate() {
                        out::line(&module_line(
                            list.serial,
                            i,
                            list.modules.len(),
                            m,
                            over.enabled_of(&m.id),
                        ));
                    }
                } else {
                    out::print(&list_text(&list, &profile, &over));
                }
                EXIT_OK
            }
            Err(f) => report(&f, *json),
        },
        ModulesAction::Info { id, json } => {
            let res = within(READ_TIMEOUT, async {
                let (list, _, over) = read().await?;
                let m = list.find(id).cloned().ok_or_else(|| {
                    Failure::Rejected(IpcError::InvalidArgument(format!(
                        "No module {id} is installed"
                    )))
                })?;
                Ok((list.serial, m, over))
            })
            .await;
            match res {
                Ok((serial, m, over)) => {
                    if *json {
                        out::line(&module_line(serial, 0, 1, &m, over.enabled_of(&m.id)));
                    } else {
                        out::print(&info_text(&m, &over));
                    }
                    EXIT_OK
                }
                Err(f) => report(&f, *json),
            }
        }
        ModulesAction::Profile { profile: None } => match within(READ_TIMEOUT, read()).await {
            Ok((_, profile, over)) => {
                out::print(&format!(
                    "Profile: {}{}\n",
                    printable(&profile),
                    set_by(&over.profile)
                ));
                EXIT_OK
            }
            Err(f) => report(&f, false),
        },
        change => change_modules(change).await,
    }
}

/// The changes of `modules`, then the list (or the module) read back.
async fn change_modules(action: &ModulesAction) -> u8 {
    let res = within(CALL_TIMEOUT, async {
        let conn = bus::connect().await?;
        let proxy = bus::proxy(&conn).await?;
        let revision = proxy
            .interface_revision()
            .await
            .map_err(|e| bus::classify(&e, true))?;
        bus::require_modules(revision)?;
        let reply = match action {
            ModulesAction::Enable { id } => proxy.set_module_enabled(id, true).await,
            ModulesAction::Disable { id } => proxy.set_module_enabled(id, false).await,
            ModulesAction::Profile { profile: Some(p) } => proxy.set_backend_profile(p).await,
            ModulesAction::Pin {
                stage,
                module,
                clear,
            } => {
                let module = if *clear {
                    ""
                } else {
                    module.as_deref().unwrap_or_default()
                };
                proxy.set_stage_pin(stage, module).await
            }
            _ => unreachable!("not a change"),
        };
        reply.map_err(bus::classify_ipc)?;
        read().await
    })
    .await;
    match res {
        Ok((list, profile, over)) => {
            let text = match action {
                ModulesAction::Enable { id } | ModulesAction::Disable { id } => {
                    list.find(id).map_or_else(
                        || list_text(&list, &profile, &over),
                        |m| info_text(m, &over),
                    )
                }
                ModulesAction::Profile { .. } => format!("Profile: {}\n", printable(&profile)),
                // The pin just set is what the reply says: an audio stage's
                // provider in the list is the worker's catalogue, refreshed
                // after the call, so it still names the previous one.
                ModulesAction::Pin {
                    stage,
                    module: Some(module),
                    clear: false,
                } => {
                    let name = list
                        .find(module)
                        .map_or(module.as_str(), |m| m.name.as_str());
                    format!(
                        "{}: pinned to {} ({})\n",
                        printable(stage),
                        printable(name),
                        printable(module)
                    )
                }
                ModulesAction::Pin { stage, .. } => match list.provider_of(stage) {
                    Some(m) if m.kind == "audio-filter" => {
                        format!("{}: pin cleared\n", printable(stage))
                    }
                    Some(m) => format!(
                        "{}: provided by {} ({})\n",
                        printable(stage),
                        printable(&m.name),
                        printable(&m.id)
                    ),
                    None => format!("{}: no module can provide it now\n", printable(stage)),
                },
                _ => list_text(&list, &profile, &over),
            };
            out::print(&text);
            EXIT_OK
        }
        Err(f) => report(&f, false),
    }
}

/// `order [show|set|reset]`. `reset` sets the empty order, which the
/// daemon takes as the default one.
pub async fn run_order(action: &OrderAction) -> u8 {
    let json = matches!(action, OrderAction::Show { json: true });
    let res = match action {
        OrderAction::Show { .. } => within(READ_TIMEOUT, read_order()).await,
        OrderAction::Set { .. } | OrderAction::Reset => {
            within(CALL_TIMEOUT, async {
                let conn = bus::connect().await?;
                let proxy = bus::proxy(&conn).await?;
                let revision = proxy
                    .interface_revision()
                    .await
                    .map_err(|e| bus::classify(&e, true))?;
                bus::require_modules(revision)?;
                let order: Vec<&str> = match action {
                    OrderAction::Set { order } => order.iter().map(|s| s.trim()).collect(),
                    _ => Vec::new(),
                };
                proxy
                    .set_stage_order(&order)
                    .await
                    .map_err(bus::classify_ipc)?;
                read_order().await
            })
            .await
        }
    };
    match res {
        Ok((order, overridden_by)) => {
            if json {
                out::line(&order_line(&order, &overridden_by));
            } else {
                out::print(&order_text(&order, &overridden_by));
            }
            EXIT_OK
        }
        Err(f) => report(&f, json),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use obc_ipc::StageInfo;

    fn maxine() -> ModuleInfo {
        ModuleInfo {
            id: "br.com.dias.maxine-vfx".into(),
            name: "NVIDIA Maxine video effects".into(),
            version: "0.24.0".into(),
            licence: "MIT".into(),
            kind: "video-stage".into(),
            library: "/usr/lib/omarchy-broadcast/modules/libobc_module_maxine.so".into(),
            manifest: "/usr/lib/omarchy-broadcast/modules.d/maxine-vfx.toml".into(),
            enabled: true,
            status: "available".into(),
            reason: String::new(),
            stages: vec![
                StageInfo {
                    id: "background".into(),
                    slot: "matte".into(),
                    rank: 300,
                    device: "nvidia-gpu".into(),
                    available: true,
                    reason: String::new(),
                    selected: true,
                },
                StageInfo {
                    id: "relight".into(),
                    slot: "post-matte".into(),
                    rank: 300,
                    device: "nvidia-gpu".into(),
                    available: false,
                    reason: "Not installed".into(),
                    selected: false,
                },
            ],
            ..ModuleInfo::default()
        }
    }

    #[test]
    fn no_module_says_so() {
        let empty = ModuleList::default();
        let none = Overrides::default();
        assert_eq!(
            list_text(&empty, "quality", &none),
            "No effect modules installed\n"
        );
        assert_eq!(order_text(&[], ""), "No effect modules installed\n");
        assert_eq!(
            modules_line(&empty, "quality", &none),
            r#"{"v":1,"type":"modules","serial":0,"profile":"quality","count":0,"profile_overridden_by":""}"#
        );
    }

    #[test]
    fn the_list_shows_status_and_stages() {
        let list = ModuleList {
            serial: 2,
            modules: vec![maxine()],
        };
        let mut over = Overrides::default();
        let text = list_text(&list, "quality", &over);
        assert!(text.starts_with("Profile: quality\n"), "{text}");
        assert!(
            text.contains("br.com.dias.maxine-vfx  NVIDIA Maxine video effects 0.24.0  available")
        );
        assert!(text.contains("    background (matte, nvidia-gpu, rank 300) selected\n"));
        assert!(text.contains(
            "    relight (post-matte, nvidia-gpu, rank 300) unavailable: Not installed\n"
        ));
        let info = info_text(&list.modules[0], &over);
        assert!(info.contains("Enabled:     yes\n"), "{info}");
        assert!(info.contains("Status:      available\n"), "{info}");
        assert!(!info.contains("Set by:"), "{info}");
        // A later file's choices show with the file.
        over.profile = "config.d/90-x.toml".into();
        over.enabled
            .insert("br.com.dias.maxine-vfx".into(), "config.d/90-x.toml".into());
        let text = list_text(&list, "quality", &over);
        assert!(
            text.starts_with("Profile: quality (set by config.d/90-x.toml)\n"),
            "{text}"
        );
        assert!(
            text.contains("available (set by config.d/90-x.toml)\n"),
            "{text}"
        );
        let info = info_text(&list.modules[0], &over);
        assert!(info.contains("Set by:      config.d/90-x.toml\n"), "{info}");
    }

    #[test]
    fn the_json_lines_are_bounded_and_carry_the_fields() {
        let mut m = maxine();
        let line = module_line(2, 0, 1, &m, "config.d/90-x.toml");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["type"], "module");
        assert_eq!(v["module"]["stages"][0]["selected"], true);
        assert_eq!(v["module"]["stages_truncated"], false);
        assert_eq!(v["module"]["overridden_by"], "config.d/90-x.toml");
        // Many long stages: the line stays under the cap.
        let long = "x".repeat(300);
        m.stages = (0..40)
            .map(|i| StageInfo {
                id: format!("s{i}"),
                reason: long.clone(),
                ..StageInfo::default()
            })
            .collect();
        m.library = format!("/{}", "d/".repeat(400));
        let line = module_line(2, 0, 1, &m, "");
        assert!(line.len() <= json::MAX_LINE_BYTES, "{}", line.len());
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["module"]["stages_truncated"], true);
        assert!(v["module"]["library"].as_str().unwrap().starts_with('…'));
        let stage = |id: &str, slot: &str, up: bool, down: bool| OrderStage {
            id: id.into(),
            slot: slot.into(),
            up,
            down,
        };
        let order = [
            stage("enhance", "pre-matte", false, true),
            stage("denoise", "pre-matte", true, false),
            stage("background", "matte", false, false),
        ];
        assert_eq!(
            order_line(&order, ""),
            r#"{"v":1,"type":"order","order":["enhance","denoise","background"],"order_truncated":false,"stages":[{"id":"enhance","slot":"pre-matte","up":false,"down":true},{"id":"denoise","slot":"pre-matte","up":true,"down":false},{"id":"background","slot":"matte","up":false,"down":false}],"overridden_by":""}"#
        );
        assert_eq!(
            order_text(&order, "config.d/90-x.toml"),
            "1. enhance\n2. denoise\n3. background\nThe order is set by config.d/90-x.toml: \
             change it there.\n"
        );
    }
}
