//! The camera settings of the settings channel (D-Bus revision 7), as a pure
//! model: no device, no clock, no I/O.
//!
//! [`build`] turns what a camera's node reported (its controls, from
//! `obc_pipeline::ControlNode::enumerate`, and its Extension Unit rows, from
//! `obc_pipeline::xu`), what the user stored for it and whether the session
//! has it open into a [`CameraSettings`]: one [`Entry`] per published
//! setting, grouped and ordered by the [`catalogue`], each with its
//! published [`Setting`] (`enabled` computed here) and what the daemon needs
//! to write it (the control or row, how a setting value maps to the raw
//! device value).
//!
//! - **Which controls.** Every user- and camera-class control of an exposed
//!   type (integer, boolean, menu, button, integer menu), except those never
//!   exposed (`obc_pipeline::control_node::is_never_exposed`: privacy and
//!   the relative, continuous and speed motion controls). Known ones get the
//!   catalogue's group, label, unit, option ids and rule; any other goes to
//!   `other` as `camera.other.<name>`, labelled with the device's name
//!   ([`sanitise`]d). A control whose value cannot be read, one whose name
//!   is not a storable control name (`obc_core::config::valid_control_name`)
//!   or is a catalogue name under another control id, and a menu without an
//!   item are left out, each with a note for the caller to log once. At most
//!   [`MAX_SETTINGS`] settings, [`MAX_OPTIONS`] options each and
//!   [`MAX_CONDITIONS`] conditions each.
//! - **Values.** The raw device value maps to the setting's: an int as it
//!   is, a bool as `raw != 0` (the opposite for the inverted
//!   `camera.exposure.keep_framerate`), a menu item to its option id, an
//!   Extension Unit byte to its row option. Stored values are raw, under the
//!   control's name.
//! - **Effective value** (what is published): while the session has the
//!   camera open, the device's; otherwise the stored value, else raw 0 for
//!   `exposure_dynamic_framerate` when the next open pins it
//!   ([`Inputs::fixed_framerate`]; the pin holds over a pending reset),
//!   else the default after a pending reset (whole or of that control),
//!   else the device's.
//! - **Enabled.** Not when the device flags the control disabled or
//!   read-only ([`REASON_FIXED`]) or grabbed ([`REASON_GRABBED`]), nor an
//!   Extension Unit row whose current byte it does not know
//!   ([`REASON_XU_UNKNOWN`]). A control with a catalogue rule is enabled
//!   while the rule holds over its master's effective value and, while the
//!   camera is open, the device does not flag it inactive (its reason is the
//!   rule's); a rule whose master the camera does not expose (or exposes as
//!   another type) is dropped. A control without a rule is not enabled while
//!   the device flags it inactive ([`REASON_INACTIVE`]).
//! - **Writes.** [`CameraSettings::check`] validates a `SetSetting` value
//!   (unknown, then disabled, then invalid) and gives the raw value to
//!   write. [`CameraSettings::apply_order`] puts masters (settings without
//!   a condition) before the settings that depend on them,
//!   [`CameraSettings::reset_order`] the other way round.
//! - **Deltas.** [`diff`] compares two published trees: a structural change
//!   (another camera, a setting added or gone, or its label, type, range,
//!   step, default, unit, options, conditions or hint changed) or the
//!   settings whose value, `stored`, `enabled` or reason changed.

use obc_core::config::{CameraControlState, valid_control_name};
use obc_ipc::settings::SCOPE_CAMERA;
use obc_ipc::{
    Condition, ConditionOp, Setting, SettingChange, SettingKind, SettingValue, SettingsGroup,
    SettingsTree, valid_setting_id,
};
use obc_pipeline::control_node::{CtrlValue, MenuItemValue, NodeControl, is_never_exposed};
use obc_pipeline::controls::{
    CTRL_FLAG_DISABLED, CTRL_FLAG_GRABBED, CTRL_FLAG_INACTIVE, CTRL_FLAG_READ_ONLY,
    CTRL_FLAG_UPDATE, CTRL_TYPE_BOOLEAN, CTRL_TYPE_BUTTON, CTRL_TYPE_INTEGER,
    CTRL_TYPE_INTEGER_MENU, CTRL_TYPE_MENU,
};
use obc_pipeline::xu::{XuRow, XuState};

pub mod catalogue;

use catalogue::{
    AUTO_EXPOSURE_ALONE, CATALOGUE, DYNAMIC_FRAMERATE, Known, OTHER, Operand, Rule, XU,
};

/// The most settings published for one camera.
pub const MAX_SETTINGS: usize = 64;
/// The most options of one enum setting.
pub const MAX_OPTIONS: usize = 16;
/// The most `enabled_when` conditions of one setting.
pub const MAX_CONDITIONS: usize = 4;
/// The longest label taken from a device string, in bytes.
pub const LABEL_MAX: usize = 48;
/// The longest option label taken from a device string (a menu item's
/// name), in bytes: the bound the CLI's JSON lines keep.
pub const OPTION_LABEL_MAX: usize = 32;

/// Why a disabled or read-only control is greyed.
pub const REASON_FIXED: &str = "The camera does not allow changing this";
/// Why a grabbed control is greyed.
pub const REASON_GRABBED: &str = "The camera does not allow changing this while it streams";
/// Why an inactive control without a catalogue rule is greyed.
pub const REASON_INACTIVE: &str = "The camera does not allow changing this now";
/// Why an Extension Unit setting with an unknown current byte is greyed.
pub const REASON_XU_UNKNOWN: &str = "The camera reports a field of view this version does not know";

/// A device string made safe to show: C0 and C1 control characters, the
/// bidirectional embeddings, overrides and isolates (U+202A to U+202E,
/// U+2066 to U+2069) and the zero-width and direction marks removed,
/// whitespace runs collapsed to one space and trimmed, at most `max` bytes
/// (cut on a character boundary).
pub fn sanitise(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max));
    let mut gap = false;
    for c in s.chars() {
        if c.is_whitespace() {
            gap = true;
            continue;
        }
        if c.is_control() || is_invisible(c) {
            continue;
        }
        if gap && !out.is_empty() {
            out.push(' ');
        }
        gap = false;
        out.push(c);
    }
    if out.len() > max {
        let mut end = max;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        let trimmed = out.trim_end().len();
        out.truncate(trimmed);
    }
    out
}

/// The bidirectional formatting characters and the zero-width ones.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
            | '\u{200b}'..='\u{200f}'
            | '\u{2060}'..='\u{2064}'
            | '\u{061c}'
            | '\u{feff}'
            | '\u{180e}'
    )
}

/// One Extension Unit row as read on a camera
/// (`obc_pipeline::xu::XuControl::probe`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XuReading {
    pub row: &'static XuRow,
    pub state: XuState,
}

/// What [`build`] reads.
#[derive(Debug, Clone, Copy)]
pub struct Inputs<'a> {
    /// The camera's id.
    pub camera: &'a str,
    /// Every control its node reported, in enumeration order.
    pub controls: &'a [NodeControl],
    /// Its Extension Unit rows the device agreed to.
    pub xu: &'a [XuReading],
    /// What the user stored for it.
    pub stored: Option<&'a CameraControlState>,
    /// The session has the camera open: values are the device's.
    pub open: bool,
    /// The camera's next open pins `exposure_dynamic_framerate` to 0
    /// (`[camera] fixed_framerate`, and nothing stored for it).
    pub fixed_framerate: bool,
}

/// What a setting is written to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// A V4L2 control, by id.
    Ctrl(u32),
    /// An Extension Unit data row.
    Xu(&'static XuRow),
}

/// How a setting value maps to the raw device value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Values {
    /// An int setting: the raw value.
    Int,
    /// A bool setting: `raw != 0`, or its opposite when `inverted`.
    Bool { inverted: bool },
    /// An enum setting: each option id with the raw value it stands for.
    Options(Vec<(String, i32)>),
    /// An action: no value, written as 1.
    Action,
}

impl Values {
    /// The setting value of `raw`.
    pub fn of(&self, raw: i32) -> Option<SettingValue> {
        match self {
            Values::Int => Some(SettingValue::Int(raw.into())),
            Values::Bool { inverted } => Some(SettingValue::Bool((raw != 0) != *inverted)),
            Values::Options(o) => Some(SettingValue::Str(option_id(o, raw))),
            Values::Action => None,
        }
    }
}

/// The option id of `raw`: its option, or `m<raw>` when it is none.
fn option_id(options: &[(String, i32)], raw: i32) -> String {
    options
        .iter()
        .find(|(_, v)| *v == raw)
        .map_or_else(|| format!("m{raw}"), |(id, _)| id.clone())
}

/// One published setting and what the daemon writes it with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Its group id ([`catalogue::GROUPS`]).
    pub group: &'static str,
    /// The control's name: the storage key.
    pub name: String,
    pub target: Target,
    pub values: Values,
    /// The raw value on the device (`None`: an action).
    pub device: Option<i32>,
    /// The raw default (`None`: an action).
    pub default: Option<i32>,
    /// The device's flags (`V4L2_CTRL_FLAG_*`; 0 for an Extension Unit row).
    pub flags: u32,
    /// Another published setting depends on this one, or the device says a
    /// change of it may change others (`V4L2_CTRL_FLAG_UPDATE`): after a
    /// write, what depends on it (or, when the daemon cannot name that,
    /// every control) is read again.
    pub updates_others: bool,
    /// The published setting.
    pub setting: Setting,
}

impl Entry {
    /// It has published conditions: it depends on another setting.
    pub fn is_dependent(&self) -> bool {
        !self.setting.enabled_when.is_empty()
    }

    /// The device flags it inactive now.
    pub fn inactive(&self) -> bool {
        self.flags & CTRL_FLAG_INACTIVE != 0
    }

    /// An action (a button): nothing to store.
    pub fn is_action(&self) -> bool {
        self.values == Values::Action
    }

    /// Whether the device takes the raw value `raw` now (a stored value
    /// checked before it is written at an open): in the range and on the
    /// step of an int, 0 or 1 for a bool, one of the options of an enum.
    pub fn check_raw(&self, raw: i32) -> Result<(), String> {
        let name = &self.name;
        match (&self.setting.kind, &self.values) {
            (SettingKind::Int { min, max, step, .. }, _) => {
                let v = i64::from(raw);
                if v < *min || v > *max {
                    Err(format!("{name} {raw} is outside its range {min}..={max}"))
                } else if (v - min) % step != 0 {
                    Err(format!(
                        "{name} {raw} is not a whole number of steps of {step} from {min}"
                    ))
                } else {
                    Ok(())
                }
            }
            (_, Values::Bool { .. }) if raw == 0 || raw == 1 => Ok(()),
            (_, Values::Bool { .. }) => Err(format!("{name} {raw} is not 0 or 1")),
            (_, Values::Options(o)) if o.iter().any(|(_, v)| *v == raw) => Ok(()),
            (_, Values::Options(_)) => Err(format!("{name} {raw} is not one of its options")),
            _ => Err(format!("{name} is an action")),
        }
    }
}

/// The settings of one camera ([`build`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CameraSettings {
    /// The camera's id; empty for no camera.
    pub camera: String,
    /// The published settings, in publishing order.
    pub entries: Vec<Entry>,
    /// What was left out or cut, for the caller to log (once per camera
    /// and text).
    pub notes: Vec<String>,
}

/// Why [`CameraSettings::check`] refused a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// No such setting id (`UnknownSetting`).
    Unknown(String),
    /// Not enabled now; the message is its reason (`Disabled`).
    Disabled(String),
    /// The wrong type, out of range, off the step, not an option, an action
    /// fired with something but `true` (`InvalidArgument`).
    Invalid(String),
}

/// A setting found while building, before its effective value.
struct Candidate {
    group: &'static str,
    /// The catalogue position (known) or after it (enumeration order).
    order: (usize, usize),
    name: String,
    id: String,
    label: String,
    unit: Option<String>,
    target: Target,
    values: Values,
    device: Option<i32>,
    default: Option<i32>,
    flags: u32,
    /// The int range and step.
    range: (i64, i64, i64),
    rule: Option<Rule>,
    /// The device's current byte is not one the row knows.
    unknown_xu: bool,
    /// The option labels, in the order of `values`.
    labels: Vec<String>,
}

/// The settings of a camera (see the module docs).
pub fn build(inp: &Inputs<'_>) -> CameraSettings {
    let mut notes = Vec::new();
    let mut found: Vec<Candidate> = Vec::new();
    for (seq, c) in inp.controls.iter().enumerate() {
        if let Some(cand) = control_candidate(c, seq, &mut notes) {
            if found.iter().any(|f| f.name == cand.name) {
                notes.push(format!(
                    "{} ({:#010x}) has the name of another control: left out",
                    cand.name, c.id
                ));
                continue;
            }
            found.push(cand);
        }
    }
    for (seq, x) in inp.xu.iter().enumerate() {
        let cand = xu_candidate(x, inp.controls.len() + seq, &mut notes);
        if found.iter().any(|f| f.name == cand.name) {
            continue;
        }
        found.push(cand);
    }
    found.sort_by_key(|c| (catalogue::group_index(c.group), c.order));
    if found.len() > MAX_SETTINGS {
        notes.push(format!(
            "{} settings found, more than the {MAX_SETTINGS} published: the last {} are left out",
            found.len(),
            found.len() - MAX_SETTINGS
        ));
        found.truncate(MAX_SETTINGS);
    }
    let effective: Vec<Option<i32>> = found.iter().map(|c| effective(c, inp)).collect();
    let mut entries: Vec<Entry> = Vec::with_capacity(found.len());
    for (i, c) in found.iter().enumerate() {
        let conditions = conditions(c, &found);
        let held = conditions
            .iter()
            .all(|(m, cond)| holds(cond, found[*m].values.of(effective[*m].unwrap_or(0))));
        let (enabled, reason) = enabled(c, !conditions.is_empty(), held, inp.open);
        let value = effective[i];
        let kind = kind(c, value);
        let setting = Setting {
            id: c.id.clone(),
            label: c.label.clone(),
            kind,
            scope: SCOPE_CAMERA.into(),
            stored: inp
                .stored
                .is_some_and(|s| s.values.contains_key(&c.name) && c.values != Values::Action),
            enabled,
            disabled_reason: reason.to_string(),
            enabled_when: conditions.into_iter().map(|(_, cond)| cond).collect(),
            hint: hint(&c.values),
        };
        entries.push(Entry {
            group: c.group,
            name: c.name.clone(),
            target: c.target,
            values: c.values.clone(),
            device: c.device,
            default: c.default,
            flags: c.flags,
            updates_others: c.flags & CTRL_FLAG_UPDATE != 0,
            setting,
        });
    }
    // Masters of a published condition read the camera again after a write.
    let masters: Vec<String> = entries
        .iter()
        .flat_map(|e| e.setting.enabled_when.iter().map(|c| c.id.clone()))
        .collect();
    for e in &mut entries {
        if masters.contains(&e.setting.id) {
            e.updates_others = true;
        }
    }
    CameraSettings {
        camera: inp.camera.to_string(),
        entries,
        notes,
    }
}

/// The candidate of one enumerated control, or `None` (with a note when it
/// is worth one) when it is not published.
fn control_candidate(c: &NodeControl, seq: usize, notes: &mut Vec<String>) -> Option<Candidate> {
    if is_never_exposed(c.id, &c.name) {
        return None;
    }
    let known = catalogue::by_cid(c.id);
    if known.is_none() {
        if catalogue::by_name(&c.name).is_some() {
            notes.push(format!(
                "{} ({:#010x}) has the name of a known control but another id: left out",
                c.name, c.id
            ));
            return None;
        }
        if !valid_control_name(&c.name) {
            notes.push(format!(
                "the control {:?} ({:#010x}) has no storable name: left out",
                sanitise(&c.info.name, LABEL_MAX),
                c.id
            ));
            return None;
        }
    }
    let (order, name, group, id_name) = match known {
        Some((i, k)) => (
            (i, 0),
            k.name.to_string(),
            k.group,
            k.id_name.unwrap_or(k.name),
        ),
        None => (
            (CATALOGUE.len(), seq),
            c.name.clone(),
            OTHER,
            c.name.as_str(),
        ),
    };
    let id = format!("camera.{group}.{id_name}");
    if !valid_setting_id(&id) {
        notes.push(format!(
            "{name}: {id:?} is not a valid setting id: left out"
        ));
        return None;
    }
    let device = match (&c.value, c.info.type_) {
        (_, CTRL_TYPE_BUTTON) => None,
        (CtrlValue::Value(v), _) => Some(*v),
        // Unreadable: the caller logs it with the error.
        (CtrlValue::Unreadable { .. }, _) => return None,
        (CtrlValue::None, _) => {
            notes.push(format!("{name} has no readable value: left out"));
            return None;
        }
    };
    let k = known.map(|(_, k)| k);
    let label = k.map_or_else(
        || {
            let l = sanitise(&c.info.name, LABEL_MAX);
            if l.is_empty() { name.clone() } else { l }
        },
        |k| k.label.to_string(),
    );
    let info = &c.info;
    let (values, labels) = match info.type_ {
        CTRL_TYPE_INTEGER => (Values::Int, Vec::new()),
        CTRL_TYPE_BOOLEAN => (
            Values::Bool {
                inverted: k.is_some_and(|k| k.inverted),
            },
            Vec::new(),
        ),
        CTRL_TYPE_BUTTON => (Values::Action, Vec::new()),
        CTRL_TYPE_MENU | CTRL_TYPE_INTEGER_MENU => {
            let (options, labels) = menu_options(c, k, &name, notes);
            if options.is_empty() {
                notes.push(format!("{name} is a menu without an item: left out"));
                return None;
            }
            (Values::Options(options), labels)
        }
        _ => return None,
    };
    Some(Candidate {
        group,
        order,
        name,
        id,
        label,
        unit: k.and_then(|k| k.unit).map(str::to_string),
        target: Target::Ctrl(c.id),
        values,
        device,
        default: (info.type_ != CTRL_TYPE_BUTTON).then_some(info.default_value),
        flags: info.flags,
        range: (
            info.minimum.into(),
            info.maximum.into(),
            i64::from(info.step.max(1)),
        ),
        rule: k.and_then(|k| k.rule),
        unknown_xu: false,
        labels,
    })
}

/// The options of a menu or integer menu: exactly the items the device
/// reported, known ones with the catalogue's id and label, others as
/// `m<index>` with the device's label (sanitised, at most
/// [`OPTION_LABEL_MAX`] bytes) or integer. At most [`MAX_OPTIONS`].
fn menu_options(
    c: &NodeControl,
    known: Option<&Known>,
    name: &str,
    notes: &mut Vec<String>,
) -> (Vec<(String, i32)>, Vec<String>) {
    let mut options = Vec::new();
    let mut labels = Vec::new();
    let has_zero = c.menu.iter().any(|m| m.index == 0);
    for item in &c.menu {
        let Ok(raw) = i32::try_from(item.index) else {
            continue;
        };
        let known_item = known.and_then(|k| k.options.iter().find(|(i, _, _)| *i == item.index));
        let (id, label) = match known_item {
            Some((3, id, _)) if name == "auto_exposure" && !has_zero => {
                (id.to_string(), AUTO_EXPOSURE_ALONE.to_string())
            }
            Some((_, id, label)) => (id.to_string(), label.to_string()),
            None => {
                let label = match &item.value {
                    MenuItemValue::Name(n) => sanitise(n, OPTION_LABEL_MAX),
                    MenuItemValue::Integer(v) => v.to_string(),
                };
                let id = format!("m{}", item.index);
                let label = if label.is_empty() { id.clone() } else { label };
                (id, label)
            }
        };
        options.push((id, raw));
        labels.push(label);
    }
    if options.len() > MAX_OPTIONS {
        notes.push(format!(
            "{name} has {} options, more than the {MAX_OPTIONS} published: the last {} are left \
             out",
            options.len(),
            options.len() - MAX_OPTIONS
        ));
        options.truncate(MAX_OPTIONS);
        labels.truncate(MAX_OPTIONS);
    }
    (options, labels)
}

/// The candidate of an Extension Unit row.
fn xu_candidate(x: &XuReading, seq: usize, notes: &mut Vec<String>) -> Candidate {
    let row = x.row;
    let known = catalogue::by_name(row.name).filter(|(_, k)| k.cid == XU);
    let (order, group, label) = match known {
        Some((i, k)) => ((i, 0), k.group, k.label.to_string()),
        None => ((CATALOGUE.len(), seq), OTHER, row.label.to_string()),
    };
    let mut options: Vec<(String, i32)> = row
        .options
        .iter()
        .map(|o| (o.id.to_string(), i32::from(o.value)))
        .collect();
    let mut labels: Vec<String> = row.options.iter().map(|o| o.label.to_string()).collect();
    if options.len() > MAX_OPTIONS {
        notes.push(format!(
            "{} has more than {MAX_OPTIONS} options: the last are left out",
            row.name
        ));
        options.truncate(MAX_OPTIONS);
        labels.truncate(MAX_OPTIONS);
    }
    Candidate {
        group,
        order,
        name: row.name.to_string(),
        id: format!("camera.{group}.{}", row.name),
        label,
        unit: None,
        target: Target::Xu(row),
        values: Values::Options(options),
        device: Some(i32::from(x.state.current)),
        default: Some(i32::from(x.state.default.value)),
        flags: 0,
        range: (0, 0, 1),
        rule: None,
        unknown_xu: x.state.current_option.is_none(),
        labels,
    }
}

/// The candidate's effective raw value (see the module docs).
fn effective(c: &Candidate, inp: &Inputs<'_>) -> Option<i32> {
    c.device?;
    if inp.open {
        return c.device;
    }
    let stored = inp.stored;
    if let Some(v) = stored.and_then(|s| s.values.get(&c.name)) {
        return Some(*v);
    }
    // The pin holds over a pending reset: the open does not write the
    // default over it.
    if c.name == DYNAMIC_FRAMERATE && inp.fixed_framerate {
        return Some(0);
    }
    if stored.is_some_and(|s| s.reset_all || s.reset_names.contains(&c.name)) {
        return c.default;
    }
    c.device
}

/// The published conditions of `c` (with the index of each master): its
/// rule over a master the camera exposes with the right type, else none.
fn conditions(c: &Candidate, all: &[Candidate]) -> Vec<(usize, Condition)> {
    let Some(rule) = c.rule else {
        return Vec::new();
    };
    let Some((m, master)) = all.iter().enumerate().find(|(_, m)| m.name == rule.master) else {
        return Vec::new();
    };
    let cond = match (rule.operand, &master.values) {
        (Operand::Is(b), Values::Bool { .. }) => Condition {
            id: master.id.clone(),
            op: ConditionOp::Eq,
            value: SettingValue::Bool(b),
        },
        (Operand::OneOf(ids), Values::Options(_)) => Condition {
            id: master.id.clone(),
            op: ConditionOp::In,
            value: SettingValue::List(
                ids.iter()
                    .map(|s| SettingValue::Str((*s).to_string()))
                    .collect(),
            ),
        },
        _ => return Vec::new(),
    };
    let mut out = vec![(m, cond)];
    out.truncate(MAX_CONDITIONS);
    out
}

/// Whether `cond` holds for the master's value.
fn holds(cond: &Condition, master: Option<SettingValue>) -> bool {
    let Some(v) = master else {
        return true;
    };
    match &cond.op {
        ConditionOp::Eq => v == cond.value,
        ConditionOp::Ne => v != cond.value,
        ConditionOp::In => match &cond.value {
            SettingValue::List(items) => items.contains(&v),
            other => *other == v,
        },
        ConditionOp::Unknown(_) => true,
    }
}

/// `(enabled, disabled_reason)` (see the module docs).
fn enabled(c: &Candidate, ruled: bool, held: bool, open: bool) -> (bool, &'static str) {
    if c.flags & (CTRL_FLAG_DISABLED | CTRL_FLAG_READ_ONLY) != 0 {
        return (false, REASON_FIXED);
    }
    if c.flags & CTRL_FLAG_GRABBED != 0 {
        return (false, REASON_GRABBED);
    }
    if c.unknown_xu {
        return (false, REASON_XU_UNKNOWN);
    }
    let inactive = c.flags & CTRL_FLAG_INACTIVE != 0;
    match c.rule {
        Some(rule) if ruled => {
            if !held || (open && inactive) {
                return (false, rule.reason);
            }
        }
        _ => {
            if inactive {
                return (false, REASON_INACTIVE);
            }
        }
    }
    (true, "")
}

/// The published type with its value (`raw`, the effective value).
fn kind(c: &Candidate, raw: Option<i32>) -> SettingKind {
    let default = c.default.unwrap_or(0);
    let raw = raw.unwrap_or(default);
    match &c.values {
        Values::Int => SettingKind::Int {
            min: c.range.0,
            max: c.range.1,
            step: c.range.2,
            default: default.into(),
            value: raw.into(),
            unit: c.unit.clone(),
        },
        Values::Bool { inverted } => SettingKind::Bool {
            default: (default != 0) != *inverted,
            value: (raw != 0) != *inverted,
        },
        Values::Options(o) => SettingKind::Enum {
            options: o
                .iter()
                .zip(&c.labels)
                .map(|((id, _), label)| (id.clone(), label.clone()))
                .collect(),
            default: option_id(o, default),
            value: option_id(o, raw),
        },
        Values::Action => SettingKind::Action,
    }
}

/// The widget hint of a type.
fn hint(values: &Values) -> Option<String> {
    match values {
        Values::Int => Some("slider".into()),
        Values::Bool { .. } => Some("switch".into()),
        Values::Options(_) => Some("segmented".into()),
        Values::Action => None,
    }
}

impl CameraSettings {
    /// The settings of no camera: camera `""`, nothing published.
    pub fn none() -> CameraSettings {
        CameraSettings::default()
    }

    /// The index of setting `id`.
    pub fn index(&self, id: &str) -> Option<usize> {
        self.entries.iter().position(|e| e.setting.id == id)
    }

    /// The entry of setting `id`.
    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.setting.id == id)
    }

    /// The entry of the control named `name`.
    pub fn by_name(&self, name: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// The ids of the V4L2 controls published (what a control-event
    /// watcher subscribes to).
    pub fn control_ids(&self) -> Vec<u32> {
        self.entries
            .iter()
            .filter_map(|e| match e.target {
                Target::Ctrl(id) => Some(id),
                Target::Xu(_) => None,
            })
            .collect()
    }

    /// Validates `value` for setting `id`: unknown, then disabled, then
    /// invalid (`None`: a D-Bus type no setting takes). Returns the entry's
    /// index and the raw value to write.
    pub fn check(&self, id: &str, value: Option<&SettingValue>) -> Result<(usize, i32), Refusal> {
        let Some(i) = self.index(id) else {
            return Err(Refusal::Unknown(if self.camera.is_empty() {
                format!("no setting {id:?}: no camera is present")
            } else {
                format!("the camera {} has no setting {id:?}", self.camera)
            }));
        };
        let e = &self.entries[i];
        if !e.setting.enabled {
            return Err(Refusal::Disabled(e.setting.disabled_reason.clone()));
        }
        let invalid = |what: String| Refusal::Invalid(format!("{id}: {what}"));
        let raw = match (&e.setting.kind, &e.values, value) {
            (SettingKind::Bool { .. }, Values::Bool { inverted }, Some(SettingValue::Bool(b))) => {
                i32::from(*b != *inverted)
            }
            (SettingKind::Int { min, max, step, .. }, Values::Int, Some(SettingValue::Int(v))) => {
                if v < min || v > max {
                    return Err(invalid(format!("{v} is outside its range {min}..={max}")));
                }
                if (v - min) % step != 0 {
                    return Err(invalid(format!(
                        "{v} is not a whole number of steps of {step} from {min}"
                    )));
                }
                i32::try_from(*v).map_err(|_| invalid(format!("{v} does not fit 32 bits")))?
            }
            (SettingKind::Enum { .. }, Values::Options(o), Some(SettingValue::Str(s))) => {
                match o.iter().find(|(oid, _)| oid == s) {
                    Some((_, raw)) => *raw,
                    None => {
                        let ids: Vec<&str> = o.iter().map(|(i, _)| i.as_str()).collect();
                        return Err(invalid(format!(
                            "{s:?} is not one of its options ({})",
                            ids.join(", ")
                        )));
                    }
                }
            }
            (SettingKind::Action, Values::Action, Some(SettingValue::Bool(true))) => 1,
            (SettingKind::Action, ..) => {
                return Err(invalid("an action is fired with the boolean true".into()));
            }
            (kind, ..) => {
                let want = match kind {
                    SettingKind::Bool { .. } => "a boolean (b)",
                    SettingKind::Int { .. } => "an integer",
                    _ => "an option id (s)",
                };
                return Err(invalid(format!("expects {want}")));
            }
        };
        Ok((i, raw))
    }

    /// `indices` with the masters (settings without a condition) first,
    /// then the settings that depend on them, each part in tree order: the
    /// order of a multi-setting write.
    pub fn apply_order(&self, indices: &[usize]) -> Vec<usize> {
        let mut indices = indices.to_vec();
        indices.sort_unstable();
        indices.dedup();
        let mut out: Vec<usize> = indices
            .iter()
            .copied()
            .filter(|&i| !self.entries[i].is_dependent())
            .collect();
        out.extend(
            indices
                .iter()
                .copied()
                .filter(|&i| self.entries[i].is_dependent()),
        );
        out
    }

    /// `indices` with the dependent settings first, then the masters: the
    /// order of a reset.
    pub fn reset_order(&self, indices: &[usize]) -> Vec<usize> {
        let mut indices = indices.to_vec();
        indices.sort_unstable();
        indices.dedup();
        let mut out: Vec<usize> = indices
            .iter()
            .copied()
            .filter(|&i| self.entries[i].is_dependent())
            .collect();
        out.extend(
            indices
                .iter()
                .copied()
                .filter(|&i| !self.entries[i].is_dependent()),
        );
        out
    }

    /// The published groups, in [`catalogue::GROUPS`] order, those with no
    /// setting left out.
    pub fn groups(&self) -> Vec<SettingsGroup> {
        catalogue::GROUPS
            .iter()
            .filter_map(|g| {
                let settings: Vec<Setting> = self
                    .entries
                    .iter()
                    .filter(|e| e.group == g.id)
                    .map(|e| e.setting.clone())
                    .collect();
                (!settings.is_empty()).then(|| SettingsGroup {
                    id: g.id.into(),
                    label: g.label.into(),
                    settings,
                })
            })
            .collect()
    }

    /// The `GetSettings` reply of these settings at `revision` and
    /// `serial`.
    pub fn tree(&self, revision: u32, serial: u32) -> SettingsTree {
        SettingsTree {
            revision,
            serial,
            camera: self.camera.clone(),
            groups: self.groups(),
        }
    }
}

/// What changed between two published trees ([`diff`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delta {
    /// The structure changed: clients read the tree again.
    pub structural: bool,
    /// The settings whose value, `stored`, `enabled` or reason changed
    /// (empty when `structural`).
    pub changes: Vec<SettingChange>,
}

/// A setting without its state: what a structural change compares.
fn shape(s: &Setting) -> Setting {
    let mut s = s.clone();
    s.stored = false;
    s.enabled = false;
    s.disabled_reason.clear();
    match &mut s.kind {
        SettingKind::Bool { value, .. } => *value = false,
        SettingKind::Int { value, .. } => *value = 0,
        SettingKind::Enum { value, .. } => value.clear(),
        SettingKind::Action | SettingKind::Unknown(_) => {}
    }
    s
}

/// The change entry of a setting's state.
pub fn change_of(s: &Setting) -> SettingChange {
    SettingChange {
        id: s.id.clone(),
        value: s.kind.value(),
        stored: s.stored,
        enabled: s.enabled,
        disabled_reason: s.disabled_reason.clone(),
    }
}

/// What changed from `old` to `new`, `None` when nothing did.
pub fn diff(old: &CameraSettings, new: &CameraSettings) -> Option<Delta> {
    let structural = old.camera != new.camera
        || old.entries.len() != new.entries.len()
        || old
            .entries
            .iter()
            .zip(&new.entries)
            .any(|(a, b)| a.group != b.group || shape(&a.setting) != shape(&b.setting));
    if structural {
        return Some(Delta {
            structural: true,
            changes: Vec::new(),
        });
    }
    let changes: Vec<SettingChange> = old
        .entries
        .iter()
        .zip(&new.entries)
        .filter(|(a, b)| a.setting != b.setting)
        .map(|(_, b)| change_of(&b.setting))
        .collect();
    (!changes.is_empty()).then_some(Delta {
        structural: false,
        changes,
    })
}

#[cfg(test)]
mod tests;
