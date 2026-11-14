//! The settings channel (revision 7, global settings from revision 8): the typed model of the tree
//! `GetSettings` returns and the deltas `SettingsChanged` carries, and its
//! codec to and from the D-Bus shapes.
//!
//! On the bus the tree is a set of dictionaries (`a{sv}`), so that a later
//! daemon can add keys and setting types without breaking a client:
//!
//! - `GetSettings(a{sv} options) -> (u revision, u serial, s camera, aa{sv} groups)`
//!   ([`SettingsReply`], decoded by [`SettingsTree::from_wire`]);
//! - a group: `id` s, `label` s, `settings` aa{sv}, and an optional
//!   `surface` s (revision 13: `mic` for the microphone's groups; absent,
//!   the group belongs where its scope puts it);
//! - a setting: `id` s, `label` s, `type` s, `scope` s, `stored` b,
//!   `enabled` b, `disabled_reason` s, `enabled_when` a(ssv), then by type
//!   (int: `min`, `max`, `step`, `default`, `value` x and an optional
//!   `unit` s, and from revision 27 an optional `display_offset` x and
//!   `display_scale` x ([`IntDisplay`]); bool: `default`, `value` b; enum: `options` a(ss),
//!   `default`, `value` s; action: none), and an optional `hint` s;
//! - `SettingsChanged(u revision, u serial, a(sa{sv}) changes)`, a change
//!   being `(id, {value v, stored b, enabled b, disabled_reason s})`, with
//!   no `value` for an action ([`SettingChange`]).
//!
//! Encoding writes exactly these shapes. Decoding is tolerant, so that an
//! older client keeps working against a newer daemon: a key it does not
//! know is ignored; a setting of a type it does not know decodes as
//! [`SettingKind::Unknown`] (a client skips it); a setting, group, change,
//! condition or option that is malformed (a key missing or of the wrong
//! type, an id outside [`valid_setting_id`]) is dropped on its own, never
//! the whole tree. Integer keys are read from any D-Bus integer type that
//! fits an `i64`, nested variants are looked through.
//!
//! The model carries what is on the wire and nothing more: whether a value
//! is in range, an enum value one of its options, or a condition true is
//! the daemon's business, which computes `enabled` itself.

use std::collections::HashMap;

use zbus::{OwnedVariant, Variant};

use crate::IpcError;

/// A dictionary of the settings channel (`a{sv}`), as zbus decodes it.
pub type VariantDict = HashMap<String, OwnedVariant>;

/// The reply of `GetSettings`: `(u revision, u serial, s camera, aa{sv} groups)`.
pub type SettingsReply = (u32, u32, String, Vec<VariantDict>);

/// One change of `SettingsChanged`: `(s id, a{sv} dict)`.
pub type WireChange = (String, VariantDict);

/// The longest setting id, in bytes.
pub const SETTING_ID_MAX: usize = 48;

/// The `scope` of a setting kept per camera id (revision 7).
pub const SCOPE_CAMERA: &str = "camera";

/// The `scope` of a setting kept for the daemon, the same whatever the
/// camera: the video effects (revision 8, ids `effect.<group>.<name>`).
/// Its groups follow the camera's in the tree and are there with no camera
/// too.
pub const SCOPE_GLOBAL: &str = "global";

/// How every global setting id of the video effects starts (revision 8):
/// `effect.`.
pub const GLOBAL_ID_PREFIX: &str = "effect.";

/// How every setting id of the microphone starts (revision 13): `mic.`.
/// They are global settings too ([`SCOPE_GLOBAL`]).
pub const MIC_ID_PREFIX: &str = "mic.";

/// The `surface` of the microphone's groups (revision 13): a client shows
/// them on its microphone page; one that does not know the key shows them
/// with the other global groups.
pub const SURFACE_MIC: &str = "mic";

/// Whether `id` names a global setting ([`GLOBAL_ID_PREFIX`], or
/// [`MIC_ID_PREFIX`] from revision 13): one the daemon answers whatever the
/// camera, so its camera option is ignored.
pub fn is_global_id(id: &str) -> bool {
    id.starts_with(GLOBAL_ID_PREFIX) || id.starts_with(MIC_ID_PREFIX)
}

/// Whether `id` is a valid setting id: `^[a-z0-9]+(\.[a-z0-9_]+){1,3}$`, at
/// most [`SETTING_ID_MAX`] bytes. That is two to four segments joined by
/// `.`, each non-empty, the first of `a-z` and `0-9`, the others of `a-z`,
/// `0-9` and `_`. Revision-7 ids are `camera.<group>.<name>`; revision 8
/// adds `effect.<group>.<name>`.
pub fn valid_setting_id(id: &str) -> bool {
    if id.is_empty() || id.len() > SETTING_ID_MAX {
        return false;
    }
    let segments: Vec<&str> = id.split('.').collect();
    if !(2..=4).contains(&segments.len()) {
        return false;
    }
    segments.iter().enumerate().all(|(i, s)| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || (i > 0 && b == b'_'))
    })
}

/// A value of the settings channel: a setting's value or default, a
/// condition's operand, the argument of `SetSetting`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingValue {
    /// `b`: a bool setting, or `true` to fire an action.
    Bool(bool),
    /// `x`: an int setting (decoded from any D-Bus integer type).
    Int(i64),
    /// `s`: an enum setting's option value id.
    Str(String),
    /// `av`: the operand of an `in` condition.
    List(Vec<SettingValue>),
}

impl SettingValue {
    /// The value as a D-Bus variant: `b`, `x`, `s`, or `av` for a list.
    pub fn to_variant(&self) -> OwnedVariant {
        match self {
            SettingValue::Bool(b) => owned(Variant::from(*b)),
            SettingValue::Int(i) => owned(Variant::from(*i)),
            SettingValue::Str(s) => owned(Variant::from(s.as_str())),
            SettingValue::List(items) => {
                let items: Vec<OwnedVariant> = items.iter().map(SettingValue::to_variant).collect();
                to_owned(&items)
            }
        }
    }

    /// The value a variant holds, looking through nested variants: `b`; any
    /// integer type (`y n q i u x t`) whose value fits an `i64`; `s`; an
    /// array whose every element decodes. `None` for any other type (a `t`
    /// above `i64::MAX` included), which the daemon refuses as
    /// `InvalidArgument`.
    pub fn from_variant(v: &Variant<'_>) -> Option<SettingValue> {
        let v = peel(v);
        if let Some(b) = bool_of(v) {
            return Some(SettingValue::Bool(b));
        }
        if let Some(i) = int_of(v) {
            return Some(SettingValue::Int(i));
        }
        if let Some(s) = str_of(v) {
            return Some(SettingValue::Str(s.to_string()));
        }
        match v {
            Variant::Array(a) => a
                .inner()
                .iter()
                .map(SettingValue::from_variant)
                .collect::<Option<Vec<_>>>()
                .map(SettingValue::List),
            _ => None,
        }
    }
}

/// The operator of an `enabled_when` condition. The set is closed for
/// settings schema 1: a client meeting [`ConditionOp::Unknown`] trusts the
/// daemon's `enabled`, as it always should.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConditionOp {
    /// `eq`: the master's value equals the operand.
    Eq,
    /// `ne`: the master's value differs from the operand.
    Ne,
    /// `in`: the master's value is one of the operand's (a list).
    In,
    /// Any other operator, as received.
    Unknown(String),
}

impl ConditionOp {
    /// The wire form: `eq`, `ne`, `in`, or the unknown operator as received.
    pub fn as_str(&self) -> &str {
        match self {
            ConditionOp::Eq => "eq",
            ConditionOp::Ne => "ne",
            ConditionOp::In => "in",
            ConditionOp::Unknown(s) => s,
        }
    }

    /// The operator of a wire form; anything but `eq`, `ne` and `in` is
    /// [`ConditionOp::Unknown`].
    pub fn parse(s: &str) -> ConditionOp {
        match s {
            "eq" => ConditionOp::Eq,
            "ne" => ConditionOp::Ne,
            "in" => ConditionOp::In,
            other => ConditionOp::Unknown(other.to_string()),
        }
    }
}

/// One `enabled_when` condition, a D-Bus struct `(ssv)`: the setting is
/// enabled only while the setting `id` compares to `value` by `op`
/// (conditions AND-ed). A hint for display: the daemon computes `enabled`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    /// The id of the master setting.
    pub id: String,
    pub op: ConditionOp,
    /// `b`, `x` or `s`, or for `in` a list (`av`) of those.
    pub value: SettingValue,
}

/// The type of a setting, with the keys that type carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingKind {
    /// `type` `bool`: a switch.
    Bool { default: bool, value: bool },
    /// `type` `int`: valid values are `min..=max`, counted from `min` in
    /// steps of `step`.
    Int {
        min: i64,
        max: i64,
        step: i64,
        default: i64,
        value: i64,
        /// A unit to show after the value (`K`, `×100 µs`), if any.
        unit: Option<String>,
        /// How a client shows the value (revision 27), if not as it is:
        /// `min`, `max`, `step`, `default` and `value` stay raw.
        display: Option<IntDisplay>,
    },
    /// `type` `enum`: one of `options`, each `(value id, label)`; `default`
    /// and `value` are option value ids.
    Enum {
        options: Vec<(String, String)>,
        default: String,
        value: String,
    },
    /// `type` `action`: no value; `SetSetting(id, true)` fires it.
    Action,
    /// A type this crate does not know, by its wire name. Clients skip such
    /// a setting.
    Unknown(String),
}

/// How a client shows the raw value of an int setting:
/// `(value - offset) * scale` with an explicit sign, then the unit (`+0`,
/// `-3`, `+280 K`). Only for display: what a client writes, and every
/// number of the setting, stays raw. On the bus `display_offset` x and
/// `display_scale` x (revision 27); a setting without them is shown as it
/// is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntDisplay {
    /// The raw value shown as 0.
    pub offset: i64,
    /// What one raw step is in the shown unit (> 0).
    pub scale: i64,
}

impl IntDisplay {
    /// The shown number of the raw value `raw`: `(raw - offset) * scale`
    /// (saturating).
    pub fn shown(&self, raw: i64) -> i64 {
        raw.saturating_sub(self.offset).saturating_mul(self.scale)
    }

    /// The shown text of `raw`: the signed number (`+0`, `-3`, `+280`),
    /// then a space and `unit` when there is one (`+280 K`).
    pub fn format(&self, raw: i64, unit: Option<&str>) -> String {
        let n = self.shown(raw);
        match unit {
            Some(u) if !u.is_empty() => format!("{n:+} {u}"),
            _ => format!("{n:+}"),
        }
    }

    /// The transform of a setting dictionary: `None` without a
    /// `display_offset`, with one of the wrong type, or with a
    /// `display_scale` of the wrong type or not above 0; an absent
    /// `display_scale` is 1.
    fn from_entries(e: &Entries<'_, '_>) -> Option<IntDisplay> {
        let offset = get_int(e, "display_offset")?;
        let scale = match e.get("display_scale") {
            None => 1,
            Some(_) => get_int(e, "display_scale")?,
        };
        (scale > 0).then_some(IntDisplay { offset, scale })
    }
}

impl SettingKind {
    /// The `type` key: `bool`, `int`, `enum`, `action`, or the unknown
    /// type's name.
    pub fn type_name(&self) -> &str {
        match self {
            SettingKind::Bool { .. } => "bool",
            SettingKind::Int { .. } => "int",
            SettingKind::Enum { .. } => "enum",
            SettingKind::Action => "action",
            SettingKind::Unknown(t) => t,
        }
    }

    /// The current value, `None` for an action or an unknown type.
    pub fn value(&self) -> Option<SettingValue> {
        match self {
            SettingKind::Bool { value, .. } => Some(SettingValue::Bool(*value)),
            SettingKind::Int { value, .. } => Some(SettingValue::Int(*value)),
            SettingKind::Enum { value, .. } => Some(SettingValue::Str(value.clone())),
            SettingKind::Action | SettingKind::Unknown(_) => None,
        }
    }

    /// The default, `None` for an action or an unknown type.
    pub fn default_value(&self) -> Option<SettingValue> {
        match self {
            SettingKind::Bool { default, .. } => Some(SettingValue::Bool(*default)),
            SettingKind::Int { default, .. } => Some(SettingValue::Int(*default)),
            SettingKind::Enum { default, .. } => Some(SettingValue::Str(default.clone())),
            SettingKind::Action | SettingKind::Unknown(_) => None,
        }
    }
}

/// One setting of the tree, a setting dictionary on the bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    /// A [`valid_setting_id`]; `camera.<group>.<name>` in revision 7.
    pub id: String,
    pub label: String,
    pub kind: SettingKind,
    /// Where it is kept: [`SCOPE_CAMERA`] (per camera, revision 7) or
    /// [`SCOPE_GLOBAL`] (for the daemon, revision 8). Every setting of a
    /// group has the same scope.
    pub scope: String,
    /// The user has a stored value for it.
    pub stored: bool,
    /// It can be changed now (computed by the daemon).
    pub enabled: bool,
    /// Why not, when not enabled; empty when enabled.
    pub disabled_reason: String,
    pub enabled_when: Vec<Condition>,
    /// A widget suggestion: `slider` (int), `switch` (bool), `segmented`
    /// (enum).
    pub hint: Option<String>,
    /// A sentence for people (revision 26): what the setting does or what
    /// it is called elsewhere. `None` (no `description` key) when there is
    /// nothing to add to the label.
    pub description: Option<String>,
}

impl Setting {
    /// The setting dictionary: every common key, the keys of its type, and
    /// `unit`, `display_offset` and `display_scale`, `hint` and
    /// `description` when set. A [`SettingKind::Unknown`] writes its
    /// type name and no type key.
    pub fn to_dict(&self) -> VariantDict {
        let mut d = VariantDict::new();
        put(&mut d, "id", Variant::from(self.id.as_str()));
        put(&mut d, "label", Variant::from(self.label.as_str()));
        put(&mut d, "type", Variant::from(self.kind.type_name()));
        put(&mut d, "scope", Variant::from(self.scope.as_str()));
        put(&mut d, "stored", Variant::from(self.stored));
        put(&mut d, "enabled", Variant::from(self.enabled));
        put(
            &mut d,
            "disabled_reason",
            Variant::from(self.disabled_reason.as_str()),
        );
        let conditions: Vec<(&str, &str, OwnedVariant)> = self
            .enabled_when
            .iter()
            .map(|c| (c.id.as_str(), c.op.as_str(), c.value.to_variant()))
            .collect();
        d.insert("enabled_when".into(), to_owned(&conditions));
        match &self.kind {
            SettingKind::Bool { default, value } => {
                put(&mut d, "default", Variant::from(*default));
                put(&mut d, "value", Variant::from(*value));
            }
            SettingKind::Int {
                min,
                max,
                step,
                default,
                value,
                unit,
                display,
            } => {
                put(&mut d, "min", Variant::from(*min));
                put(&mut d, "max", Variant::from(*max));
                put(&mut d, "step", Variant::from(*step));
                put(&mut d, "default", Variant::from(*default));
                put(&mut d, "value", Variant::from(*value));
                if let Some(unit) = unit {
                    put(&mut d, "unit", Variant::from(unit.as_str()));
                }
                if let Some(display) = display {
                    put(&mut d, "display_offset", Variant::from(display.offset));
                    put(&mut d, "display_scale", Variant::from(display.scale));
                }
            }
            SettingKind::Enum {
                options,
                default,
                value,
            } => {
                let options: Vec<(&str, &str)> = options
                    .iter()
                    .map(|(v, l)| (v.as_str(), l.as_str()))
                    .collect();
                d.insert("options".into(), to_owned(&options));
                put(&mut d, "default", Variant::from(default.as_str()));
                put(&mut d, "value", Variant::from(value.as_str()));
            }
            SettingKind::Action | SettingKind::Unknown(_) => {}
        }
        if let Some(hint) = &self.hint {
            put(&mut d, "hint", Variant::from(hint.as_str()));
        }
        if let Some(description) = &self.description {
            put(&mut d, "description", Variant::from(description.as_str()));
        }
        d
    }

    /// The setting a dictionary describes, or `None` when it is malformed:
    /// a common key or a key of its (known) type missing or of the wrong
    /// type, or an id that is not a [`valid_setting_id`]. Unknown keys are
    /// ignored; a `unit`, `hint` or `description` of the wrong type is read as absent, and so
    /// is a display transform of the wrong type or with a scale not above 0; a
    /// condition or an option that does not decode is dropped alone.
    pub fn from_dict(dict: &VariantDict) -> Option<Setting> {
        Setting::from_entries(&entries_of_dict(dict))
    }

    fn from_entries(e: &Entries<'_, '_>) -> Option<Setting> {
        let id = get_str(e, "id")?;
        if !valid_setting_id(id) {
            return None;
        }
        let type_name = get_str(e, "type")?;
        let kind = match type_name {
            "bool" => SettingKind::Bool {
                default: get_bool(e, "default")?,
                value: get_bool(e, "value")?,
            },
            "int" => SettingKind::Int {
                min: get_int(e, "min")?,
                max: get_int(e, "max")?,
                step: get_int(e, "step")?,
                default: get_int(e, "default")?,
                value: get_int(e, "value")?,
                unit: get_str(e, "unit").map(str::to_string),
                display: IntDisplay::from_entries(e),
            },
            "enum" => SettingKind::Enum {
                options: list_of(e.get("options")?)?
                    .iter()
                    .filter_map(|o| match fields_of(o)? {
                        [v, l] => Some((str_of(v)?.to_string(), str_of(l)?.to_string())),
                        _ => None,
                    })
                    .collect(),
                default: get_str(e, "default")?.to_string(),
                value: get_str(e, "value")?.to_string(),
            },
            "action" => SettingKind::Action,
            other => SettingKind::Unknown(other.to_string()),
        };
        let enabled_when = list_of(e.get("enabled_when")?)?
            .iter()
            .filter_map(|c| match fields_of(c)? {
                [id, op, value] => Some(Condition {
                    id: str_of(id)?.to_string(),
                    op: ConditionOp::parse(str_of(op)?),
                    value: SettingValue::from_variant(value)?,
                }),
                _ => None,
            })
            .collect();
        Some(Setting {
            id: id.to_string(),
            label: get_str(e, "label")?.to_string(),
            kind,
            scope: get_str(e, "scope")?.to_string(),
            stored: get_bool(e, "stored")?,
            enabled: get_bool(e, "enabled")?,
            disabled_reason: get_str(e, "disabled_reason")?.to_string(),
            enabled_when,
            hint: get_str(e, "hint").map(str::to_string),
            description: get_str(e, "description").map(str::to_string),
        })
    }
}

/// A group of settings, a group dictionary on the bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsGroup {
    /// `image`, `white_balance`, `exposure`, `focus` or `other` in revision
    /// 7; revision 8 adds the global `background` and `video`.
    pub id: String,
    pub label: String,
    pub settings: Vec<Setting>,
    /// Where a client shows the group (revision 13): [`SURFACE_MIC`] for
    /// the microphone's; `None` (no `surface` key) where its scope puts it.
    pub surface: Option<String>,
}

impl SettingsGroup {
    /// Whether the group holds global settings ([`SCOPE_GLOBAL`]): its
    /// first setting's scope, which every setting of a group shares.
    pub fn is_global(&self) -> bool {
        self.settings
            .first()
            .is_some_and(|s| s.scope == SCOPE_GLOBAL)
    }

    /// The group dictionary: `id`, `label`, `settings` (aa{sv}), and
    /// `surface` when set.
    pub fn to_dict(&self) -> VariantDict {
        let mut d = VariantDict::new();
        put(&mut d, "id", Variant::from(self.id.as_str()));
        put(&mut d, "label", Variant::from(self.label.as_str()));
        let settings: Vec<VariantDict> = self.settings.iter().map(Setting::to_dict).collect();
        d.insert("settings".into(), to_owned(&settings));
        if let Some(surface) = &self.surface {
            put(&mut d, "surface", Variant::from(surface.as_str()));
        }
        d
    }

    /// The group a dictionary describes, or `None` when its `id`, `label`
    /// or `settings` (an array) is missing or of the wrong type. A setting
    /// that does not decode is dropped alone ([`Setting::from_dict`]).
    pub fn from_dict(dict: &VariantDict) -> Option<SettingsGroup> {
        let e = entries_of_dict(dict);
        let settings = list_of(e.get("settings")?)?
            .iter()
            .filter_map(|s| Setting::from_entries(&entries_of(s)?))
            .collect();
        Some(SettingsGroup {
            id: get_str(&e, "id")?.to_string(),
            label: get_str(&e, "label")?.to_string(),
            settings,
            surface: get_str(&e, "surface").map(str::to_string),
        })
    }
}

/// The settings of one camera at one moment: the reply of `GetSettings`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsTree {
    /// `SettingsRevision` when the tree was read.
    pub revision: u32,
    /// `SettingsSchemaSerial` when the tree was read.
    pub serial: u32,
    /// The camera's id; empty when there is no camera, and then only the
    /// global groups are there (none before revision 8).
    pub camera: String,
    /// The camera's groups, then the global ones ([`SCOPE_GLOBAL`]).
    pub groups: Vec<SettingsGroup>,
}

impl SettingsTree {
    /// The reply of `GetSettings` for this tree.
    pub fn to_wire(&self) -> SettingsReply {
        (
            self.revision,
            self.serial,
            self.camera.clone(),
            self.groups.iter().map(SettingsGroup::to_dict).collect(),
        )
    }

    /// The tree of a `GetSettings` reply. Never fails: a group that does
    /// not decode is dropped alone ([`SettingsGroup::from_dict`]).
    pub fn from_wire(reply: &SettingsReply) -> SettingsTree {
        let (revision, serial, camera, groups) = reply;
        SettingsTree {
            revision: *revision,
            serial: *serial,
            camera: camera.clone(),
            groups: groups.iter().filter_map(SettingsGroup::from_dict).collect(),
        }
    }

    /// The setting `id`, in any group.
    pub fn find(&self, id: &str) -> Option<&Setting> {
        self.groups
            .iter()
            .flat_map(|g| g.settings.iter())
            .find(|s| s.id == id)
    }
}

/// One change of `SettingsChanged`: the new state of a setting whose
/// structure did not change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingChange {
    pub id: String,
    /// The new value; `None` for an action.
    pub value: Option<SettingValue>,
    pub stored: bool,
    pub enabled: bool,
    pub disabled_reason: String,
}

impl SettingChange {
    /// The change as `SettingsChanged` carries it: `(id, {value, stored,
    /// enabled, disabled_reason})`, without `value` when it is `None`.
    pub fn to_wire(&self) -> WireChange {
        let mut d = VariantDict::new();
        if let Some(value) = &self.value {
            d.insert("value".into(), value.to_variant());
        }
        put(&mut d, "stored", Variant::from(self.stored));
        put(&mut d, "enabled", Variant::from(self.enabled));
        put(
            &mut d,
            "disabled_reason",
            Variant::from(self.disabled_reason.as_str()),
        );
        (self.id.clone(), d)
    }

    /// The change of a `SettingsChanged` entry, or `None` when it is
    /// malformed: an id that is not a [`valid_setting_id`], `stored`,
    /// `enabled` or `disabled_reason` missing or of the wrong type, or a
    /// `value` that does not decode ([`SettingValue::from_variant`]).
    /// Unknown keys are ignored.
    pub fn from_wire(id: &str, dict: &VariantDict) -> Option<SettingChange> {
        if !valid_setting_id(id) {
            return None;
        }
        let e = entries_of_dict(dict);
        let value = match e.get("value") {
            None => None,
            Some(v) => Some(SettingValue::from_variant(v)?),
        };
        Some(SettingChange {
            id: id.to_string(),
            value,
            stored: get_bool(&e, "stored")?,
            enabled: get_bool(&e, "enabled")?,
            disabled_reason: get_str(&e, "disabled_reason")?.to_string(),
        })
    }

    /// The changes of a `SettingsChanged` list, each malformed one dropped
    /// alone.
    pub fn list_from_wire(changes: &[WireChange]) -> Vec<SettingChange> {
        changes
            .iter()
            .filter_map(|(id, d)| SettingChange::from_wire(id, d))
            .collect()
    }
}

/// The scope argument of `ResetSettings`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResetScope {
    /// `camera`: every setting of the camera.
    Camera,
    /// `group:<group id>`: the settings of one group.
    Group(String),
    /// `setting:<id>`: one setting.
    Setting(String),
}

impl ResetScope {
    /// The wire form: `camera`, `group:<id>`, `setting:<id>`.
    pub fn to_wire(&self) -> String {
        match self {
            ResetScope::Camera => "camera".into(),
            ResetScope::Group(g) => format!("group:{g}"),
            ResetScope::Setting(s) => format!("setting:{s}"),
        }
    }

    /// The scope of a wire form; `None` for anything but `camera`,
    /// `group:<id>` and `setting:<id>` with a non-empty id (the daemon
    /// replies `InvalidArgument`). Whether the group or setting exists is
    /// not checked here.
    pub fn parse(s: &str) -> Option<ResetScope> {
        if s == "camera" {
            return Some(ResetScope::Camera);
        }
        if let Some(g) = s.strip_prefix("group:").filter(|g| !g.is_empty()) {
            return Some(ResetScope::Group(g.to_string()));
        }
        s.strip_prefix("setting:")
            .filter(|id| !id.is_empty())
            .map(|id| ResetScope::Setting(id.to_string()))
    }
}

/// The `options` of `GetSettings`, `SetSetting` and `ResetSettings` naming
/// `camera` (an id of `Cameras`); empty when `camera` is `""` (the
/// `ActiveCamera`).
pub fn camera_options(camera: &str) -> HashMap<&'static str, Variant<'_>> {
    let mut options = HashMap::new();
    if !camera.is_empty() {
        options.insert("camera", Variant::from(camera));
    }
    options
}

/// The camera `options` name, on the daemon's side: `""` (the
/// `ActiveCamera`) when absent, else its string. Other keys are ignored. A
/// `camera` that is not a string is `InvalidArgument`.
pub fn camera_option(options: &VariantDict) -> Result<String, IpcError> {
    match options.get("camera") {
        None => Ok(String::new()),
        Some(v) => str_of(v).map(str::to_string).ok_or_else(|| {
            IpcError::InvalidArgument(format!(
                "the camera option must be a string (found {})",
                peel(v).value_signature()
            ))
        }),
    }
}

/// A dictionary seen as its keys and values, nested variants looked through.
pub(crate) type Entries<'v, 'a> = HashMap<&'v str, &'v Variant<'a>>;

pub(crate) fn entries_of_dict(dict: &VariantDict) -> Entries<'_, 'static> {
    dict.iter().map(|(k, v)| (k.as_str(), peel(v))).collect()
}

/// The entries of a dictionary variant with string keys (`a{s*}`).
pub(crate) fn entries_of<'v, 'a>(v: &'v Variant<'a>) -> Option<Entries<'v, 'a>> {
    match peel(v) {
        Variant::Dict(d) => d.iter().map(|(k, v)| Some((str_of(k)?, peel(v)))).collect(),
        _ => None,
    }
}

pub(crate) fn peel<'v, 'a>(mut v: &'v Variant<'a>) -> &'v Variant<'a> {
    while let Variant::Variant(inner) = v {
        v = inner;
    }
    v
}

pub(crate) fn bool_of(v: &Variant<'_>) -> Option<bool> {
    match peel(v) {
        Variant::Bool(b) => Some(*b),
        _ => None,
    }
}

pub(crate) fn int_of(v: &Variant<'_>) -> Option<i64> {
    match peel(v) {
        Variant::U8(x) => Some(i64::from(*x)),
        Variant::I16(x) => Some(i64::from(*x)),
        Variant::U16(x) => Some(i64::from(*x)),
        Variant::I32(x) => Some(i64::from(*x)),
        Variant::U32(x) => Some(i64::from(*x)),
        Variant::I64(x) => Some(*x),
        Variant::U64(x) => i64::try_from(*x).ok(),
        _ => None,
    }
}

pub(crate) fn str_of<'v>(v: &'v Variant<'_>) -> Option<&'v str> {
    match peel(v) {
        Variant::Str(s) => Some(s.as_str()),
        _ => None,
    }
}

pub(crate) fn list_of<'v, 'a>(v: &'v Variant<'a>) -> Option<&'v [Variant<'a>]> {
    match peel(v) {
        Variant::Array(a) => Some(a.inner()),
        _ => None,
    }
}

pub(crate) fn fields_of<'v, 'a>(v: &'v Variant<'a>) -> Option<&'v [Variant<'a>]> {
    match peel(v) {
        Variant::Structure(s) => Some(s.fields()),
        _ => None,
    }
}

pub(crate) fn get_str<'v>(e: &Entries<'v, '_>, key: &str) -> Option<&'v str> {
    str_of(e.get(key)?)
}

pub(crate) fn get_bool(e: &Entries<'_, '_>, key: &str) -> Option<bool> {
    bool_of(e.get(key)?)
}

pub(crate) fn get_int(e: &Entries<'_, '_>, key: &str) -> Option<i64> {
    int_of(e.get(key)?)
}

/// `v` owned. Only a file descriptor can fail to convert, and the settings
/// channel carries none.
pub(crate) fn owned(v: Variant<'_>) -> OwnedVariant {
    OwnedVariant::try_from(v).expect("a variant without file descriptors converts")
}

/// `value` as a variant of its own D-Bus type (arrays keep their element
/// signature when empty).
pub(crate) fn to_owned<T: serde::Serialize + zbus::Type>(value: &T) -> OwnedVariant {
    zbus::as_variant::to_owned_variant(value)
        .expect("a value without file descriptors converts to a variant")
}

pub(crate) fn put(d: &mut VariantDict, key: &str, v: Variant<'_>) {
    d.insert(key.to_string(), owned(v));
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::wire::serialized::Context;
    use zbus::wire::{LE, to_bytes};

    /// `value` encoded to D-Bus bytes and decoded back, as a peer sees it.
    fn over_the_wire<T>(value: &T) -> T
    where
        T: serde::Serialize + zbus::Type + for<'de> serde::Deserialize<'de>,
    {
        let bytes = to_bytes(Context::new(LE, 0), value).unwrap();
        bytes.deserialize::<T>().unwrap().0
    }

    fn base(id: &str, kind: SettingKind) -> Setting {
        Setting {
            id: id.into(),
            label: "A label".into(),
            kind,
            scope: SCOPE_CAMERA.into(),
            stored: false,
            enabled: true,
            disabled_reason: String::new(),
            enabled_when: Vec::new(),
            hint: None,
            description: None,
        }
    }

    fn every_type() -> Vec<Setting> {
        let mut temp = base(
            "camera.white_balance.white_balance_temperature",
            SettingKind::Int {
                min: 2800,
                max: 7500,
                step: 1,
                default: 4000,
                value: 5000,
                unit: Some("K".into()),
                display: None,
            },
        );
        temp.enabled = false;
        temp.disabled_reason = "Turn off automatic white balance to change this".into();
        temp.stored = true;
        temp.hint = Some("slider".into());
        temp.enabled_when = vec![Condition {
            id: "camera.white_balance.white_balance_automatic".into(),
            op: ConditionOp::Eq,
            value: SettingValue::Bool(false),
        }];
        let mut exposure = base(
            "camera.exposure.exposure_time_absolute",
            SettingKind::Int {
                min: -3,
                max: i64::MAX,
                step: 2,
                default: i64::MIN,
                value: -1,
                unit: None,
                display: None,
            },
        );
        exposure.enabled_when = vec![
            Condition {
                id: "camera.exposure.auto_exposure".into(),
                op: ConditionOp::In,
                value: SettingValue::List(vec![
                    SettingValue::Str("manual".into()),
                    SettingValue::Str("shutter_priority".into()),
                ]),
            },
            Condition {
                id: "camera.exposure.gain".into(),
                op: ConditionOp::Ne,
                value: SettingValue::Int(-7),
            },
            Condition {
                id: "camera.exposure.x".into(),
                op: ConditionOp::Unknown("gt".into()),
                value: SettingValue::List(Vec::new()),
            },
        ];
        let mut keep = base(
            "camera.exposure.keep_framerate",
            SettingKind::Bool {
                default: false,
                value: true,
            },
        );
        keep.hint = Some("switch".into());
        let mut mode = base(
            "camera.exposure.auto_exposure",
            SettingKind::Enum {
                options: vec![
                    ("manual".into(), "Manual".into()),
                    ("aperture_priority".into(), "Auto".into()),
                ],
                default: "aperture_priority".into(),
                value: "manual".into(),
            },
        );
        mode.hint = Some("segmented".into());
        let empty_enum = base(
            "camera.other.m",
            SettingKind::Enum {
                options: Vec::new(),
                default: String::new(),
                value: String::new(),
            },
        );
        vec![
            temp,
            exposure,
            keep,
            mode,
            empty_enum,
            base("camera.focus.pan_reset", SettingKind::Action),
            base("camera.other.future", SettingKind::Unknown("float".into())),
        ]
    }

    fn tree() -> SettingsTree {
        let settings = every_type();
        SettingsTree {
            revision: 12,
            serial: 3,
            camera: "usb-046d_0944-video-index0".into(),
            groups: vec![
                SettingsGroup {
                    id: "white_balance".into(),
                    label: "White balance".into(),
                    settings: settings[..1].to_vec(),
                    surface: None,
                },
                SettingsGroup {
                    id: "exposure".into(),
                    label: "Exposure".into(),
                    settings: settings[1..4].to_vec(),
                    surface: None,
                },
                SettingsGroup {
                    id: "other".into(),
                    label: "Other".into(),
                    settings: settings[4..].to_vec(),
                    surface: None,
                },
                SettingsGroup {
                    id: "focus".into(),
                    label: "Focus & zoom".into(),
                    settings: Vec::new(),
                    surface: None,
                },
            ],
        }
    }

    #[test]
    fn setting_ids_follow_the_grammar() {
        for good in [
            "camera.image.brightness",
            "camera.exposure.keep_framerate",
            "a.b",
            "a1.b_c.d.e9",
            "0.0",
            "camera.focus._",
            "camera.other.white_balance_temperature_",
        ] {
            assert!(valid_setting_id(good), "{good:?}");
        }
        // 48 bytes exactly, and one more.
        let at_max = format!("camera.other.{}", "a".repeat(48 - 13));
        assert_eq!(at_max.len(), 48);
        assert!(valid_setting_id(&at_max));
        assert!(!valid_setting_id(&format!("{at_max}a")));
        for bad in [
            "",
            "camera",
            "camera.",
            ".camera.image",
            "camera..image",
            "camera.image.",
            "a.b.c.d.e",
            "Camera.image.x",
            "camera.Image.x",
            "camera_x.image",
            "_.image",
            "camera.image.bright-ness",
            "camera.image.bright ness",
            "camera.image.é",
            "camera.image.x\n",
            "camera/image/x",
        ] {
            assert!(!valid_setting_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn every_type_round_trips_in_memory_and_over_the_wire() {
        let tree = tree();
        let wire = tree.to_wire();
        assert_eq!(SettingsTree::from_wire(&wire), tree);
        let back = over_the_wire(&wire);
        assert_eq!(SettingsTree::from_wire(&back), tree);
        for s in every_type() {
            assert_eq!(Setting::from_dict(&s.to_dict()), Some(s.clone()), "{s:?}");
            assert_eq!(
                Setting::from_dict(&over_the_wire(&s.to_dict())),
                Some(s.clone()),
                "{s:?}"
            );
        }
        // No camera: an empty tree.
        let none = SettingsTree {
            revision: 1,
            serial: 1,
            ..SettingsTree::default()
        };
        assert_eq!(
            SettingsTree::from_wire(&over_the_wire(&none.to_wire())),
            none
        );
        assert_eq!(
            tree.find("camera.exposure.keep_framerate").map(|s| &s.kind),
            Some(&SettingKind::Bool {
                default: false,
                value: true
            })
        );
        assert!(tree.find("camera.image.nope").is_none());
    }

    #[test]
    fn a_description_round_trips_and_is_absent_when_none() {
        let mut s = base("camera.image.backlight_compensation", SettingKind::Action);
        assert!(!s.to_dict().contains_key("description"));
        assert_eq!(Setting::from_dict(&s.to_dict()).unwrap().description, None);
        s.description = Some("HDR limits the camera to 30 fps".into());
        let d = s.to_dict();
        assert!(d.contains_key("description"));
        assert_eq!(Setting::from_dict(&d), Some(s.clone()));
        assert_eq!(over_the_wire(&d), d);
        // A description of the wrong type is read as absent.
        let mut bad = d.clone();
        bad.insert("description".into(), to_owned(&7u32));
        assert_eq!(Setting::from_dict(&bad).unwrap().description, None);
    }

    #[test]
    fn a_global_tree_without_a_camera_round_trips() {
        let mut mode = base(
            "effect.background.mode",
            SettingKind::Enum {
                options: vec![
                    ("none".into(), "Off".into()),
                    ("blur".into(), "Blur".into()),
                ],
                default: "none".into(),
                value: "blur".into(),
            },
        );
        mode.scope = SCOPE_GLOBAL.into();
        assert!(valid_setting_id(&mode.id));
        assert!(valid_setting_id("effect.background.blur_strength"));
        let tree = SettingsTree {
            revision: 3,
            serial: 2,
            camera: String::new(),
            groups: vec![SettingsGroup {
                id: "background".into(),
                label: "Background effect".into(),
                settings: vec![mode],
                surface: None,
            }],
        };
        let back = SettingsTree::from_wire(&over_the_wire(&tree.to_wire()));
        assert_eq!(back, tree);
        assert!(back.groups[0].is_global());
        assert!(is_global_id("effect.background.mode"));
        assert!(!is_global_id("camera.image.sharpness"));
        assert!(!is_global_id("effects.x.y"));
        let empty = SettingsGroup {
            settings: Vec::new(),
            ..tree.groups[0].clone()
        };
        assert!(!empty.is_global());
        assert_eq!(
            back.find("effect.background.mode")
                .map(|s| s.scope.as_str()),
            Some(SCOPE_GLOBAL)
        );
    }

    #[test]
    fn the_encoded_shapes_are_the_contracts() {
        use zbus::Type;
        assert_eq!(SettingsReply::SIGNATURE.to_string(), "(uusaa{sv})");
        assert_eq!(<Vec<WireChange>>::SIGNATURE.to_string(), "a(sa{sv})");
        let sig = |d: &VariantDict, k: &str| d[k].value_signature().to_string();
        let s = every_type();
        let temp = s[0].to_dict();
        for (k, want) in [
            ("id", "s"),
            ("label", "s"),
            ("type", "s"),
            ("scope", "s"),
            ("stored", "b"),
            ("enabled", "b"),
            ("disabled_reason", "s"),
            ("enabled_when", "a(ssv)"),
            ("min", "x"),
            ("max", "x"),
            ("step", "x"),
            ("default", "x"),
            ("value", "x"),
            ("unit", "s"),
            ("hint", "s"),
        ] {
            assert_eq!(sig(&temp, k), want, "{k}");
        }
        assert_eq!(temp.len(), 15);
        assert_eq!(<&str>::try_from(&temp["type"]).unwrap(), "int");
        // No unit, no hint: the keys are left out.
        let exposure = s[1].to_dict();
        assert!(!exposure.contains_key("unit") && !exposure.contains_key("hint"));
        let keep = s[2].to_dict();
        assert_eq!(
            (sig(&keep, "default"), sig(&keep, "value")),
            ("b".into(), "b".into())
        );
        let mode = s[3].to_dict();
        assert_eq!(sig(&mode, "options"), "a(ss)");
        assert_eq!(
            (sig(&mode, "default"), sig(&mode, "value")),
            ("s".into(), "s".into())
        );
        // An empty array keeps its element signature.
        assert_eq!(sig(&s[4].to_dict(), "options"), "a(ss)");
        assert_eq!(sig(&s[4].to_dict(), "enabled_when"), "a(ssv)");
        let action = s[5].to_dict();
        assert!(!action.contains_key("value") && !action.contains_key("default"));
        assert_eq!(action.len(), 8);
        let group = SettingsGroup {
            id: "g".into(),
            label: "G".into(),
            settings: Vec::new(),
            surface: None,
        }
        .to_dict();
        assert_eq!(sig(&group, "settings"), "aa{sv}");
        // Condition operands: b, x, s, av.
        assert_eq!(
            SettingValue::Bool(true)
                .to_variant()
                .value_signature()
                .to_string(),
            "b"
        );
        assert_eq!(
            SettingValue::Int(1)
                .to_variant()
                .value_signature()
                .to_string(),
            "x"
        );
        assert_eq!(
            SettingValue::Str("a".into())
                .to_variant()
                .value_signature()
                .to_string(),
            "s"
        );
        assert_eq!(
            SettingValue::List(vec![])
                .to_variant()
                .value_signature()
                .to_string(),
            "av"
        );
    }

    #[test]
    fn decoding_ignores_unknown_keys_and_reads_integers_of_any_type() {
        let s = &every_type()[0];
        let mut d = s.to_dict();
        d.insert("future_key".into(), owned(Variant::from(3.5f64)));
        d.insert("min".into(), owned(Variant::from(2800i32)));
        d.insert("max".into(), owned(Variant::from(7500u16)));
        d.insert("step".into(), owned(Variant::from(1u8)));
        d.insert("default".into(), owned(Variant::from(4000u64)));
        d.insert("value".into(), owned(Variant::from(5000i16)));
        assert_eq!(Setting::from_dict(&d).as_ref(), Some(s));
        assert_eq!(Setting::from_dict(&over_the_wire(&d)).as_ref(), Some(s));
        // A u64 beyond i64 is no integer of the model.
        d.insert("value".into(), owned(Variant::from(u64::MAX)));
        assert_eq!(Setting::from_dict(&d), None);
        // A value wrapped in a variant is looked through.
        d.insert(
            "value".into(),
            owned(Variant::Variant(Box::new(Variant::from(5000i64)))),
        );
        assert_eq!(Setting::from_dict(&d).as_ref(), Some(s));
        // A wrong-typed unit or hint is read as absent.
        let mut d = s.to_dict();
        d.insert("unit".into(), owned(Variant::from(1u32)));
        d.insert("hint".into(), owned(Variant::from(true)));
        let got = Setting::from_dict(&d).unwrap();
        assert_eq!(got.hint, None);
        assert!(matches!(got.kind, SettingKind::Int { unit: None, .. }));
    }

    /// The MX Brio's temperature compensation as the daemon publishes it:
    /// raw 0..=100, 50 is 0 K, 40 K a step.
    fn compensation() -> Setting {
        base(
            "camera.white_balance.temperature_compensation",
            SettingKind::Int {
                min: 0,
                max: 100,
                step: 1,
                default: 50,
                value: 57,
                unit: Some("K".into()),
                display: Some(IntDisplay {
                    offset: 50,
                    scale: 40,
                }),
            },
        )
    }

    #[test]
    fn a_display_transform_is_two_int_keys_and_round_trips() {
        let s = compensation();
        let d = s.to_dict();
        let sig = |k: &str| d[k].value_signature().to_string();
        assert_eq!(
            (sig("display_offset"), sig("display_scale")),
            ("x".into(), "x".into())
        );
        assert_eq!(i64::try_from(&d["display_offset"]).unwrap(), 50);
        assert_eq!(i64::try_from(&d["display_scale"]).unwrap(), 40);
        // The numbers stay raw.
        assert_eq!(i64::try_from(&d["value"]).unwrap(), 57);
        assert_eq!(i64::try_from(&d["min"]).unwrap(), 0);
        assert_eq!(Setting::from_dict(&d).as_ref(), Some(&s));
        assert_eq!(Setting::from_dict(&over_the_wire(&d)).as_ref(), Some(&s));
        // None: neither key.
        let plain = &every_type()[0];
        let d = plain.to_dict();
        assert!(!d.contains_key("display_offset") && !d.contains_key("display_scale"));
    }

    #[test]
    fn a_display_transform_is_decoded_tolerantly() {
        let display = |d: &VariantDict| match Setting::from_dict(d).unwrap().kind {
            SettingKind::Int { display, .. } => display,
            k => panic!("{k:?}"),
        };
        let s = compensation();
        // No scale: 1. Any integer type.
        let mut d = s.to_dict();
        d.remove("display_scale");
        d.insert("display_offset".into(), owned(Variant::from(128u16)));
        assert_eq!(
            display(&d),
            Some(IntDisplay {
                offset: 128,
                scale: 1
            })
        );
        // A wrong-typed offset or scale, or a scale not above 0: none, and
        // the setting itself still decodes.
        for (key, bad) in [
            ("display_offset", Variant::from("50")),
            ("display_offset", Variant::from(true)),
            ("display_scale", Variant::from("40")),
            ("display_scale", Variant::from(0i64)),
            ("display_scale", Variant::from(-40i64)),
            ("display_scale", Variant::from(u64::MAX)),
        ] {
            let mut d = s.to_dict();
            d.insert(key.into(), owned(bad.clone()));
            assert_eq!(display(&d), None, "{key} {bad:?}");
        }
        // A scale without an offset: none.
        let mut d = s.to_dict();
        d.remove("display_offset");
        assert_eq!(display(&d), None);
    }

    #[test]
    fn a_display_transform_shows_a_signed_number_then_the_unit() {
        let t = IntDisplay {
            offset: 50,
            scale: 40,
        };
        assert_eq!(t.shown(57), 280);
        assert_eq!(t.shown(50), 0);
        assert_eq!(t.shown(0), -2000);
        assert_eq!(t.format(57, Some("K")), "+280 K");
        assert_eq!(t.format(49, Some("K")), "-40 K");
        assert_eq!(t.format(50, Some("K")), "+0 K");
        let e = IntDisplay {
            offset: 128,
            scale: 1,
        };
        assert_eq!(e.format(128, None), "+0");
        assert_eq!(e.format(125, None), "-3");
        assert_eq!(e.format(255, Some("")), "+127");
        // Saturating, never a panic.
        let big = IntDisplay {
            offset: i64::MIN,
            scale: i64::MAX,
        };
        assert_eq!(big.shown(i64::MAX), i64::MAX);
    }

    #[test]
    fn an_unknown_type_decodes_as_unknown_with_its_common_keys() {
        let mut s = base("camera.other.path", SettingKind::Unknown("path".into()));
        s.hint = Some("chooser".into());
        let mut d = s.to_dict();
        d.insert("kind".into(), owned(Variant::from("image")));
        d.insert("default".into(), owned(Variant::from("/x.png")));
        let got = Setting::from_dict(&d).unwrap();
        assert_eq!(got, s);
        assert_eq!(got.kind.type_name(), "path");
        assert_eq!(got.kind.value(), None);
    }

    #[test]
    fn a_malformed_setting_is_dropped_alone() {
        let s = every_type();
        let good = s[0].to_dict();
        let mut cases: Vec<(&str, VariantDict)> = Vec::new();
        for key in [
            "id",
            "label",
            "type",
            "scope",
            "stored",
            "enabled",
            "disabled_reason",
            "enabled_when",
            "min",
            "max",
            "step",
            "default",
            "value",
        ] {
            let mut d = good.clone();
            d.remove(key);
            cases.push((key, d));
        }
        let wrong = |key: &str, v: Variant<'_>| {
            let mut d = good.clone();
            d.insert(key.into(), owned(v));
            d
        };
        cases.push(("stored as a string", wrong("stored", Variant::from("yes"))));
        cases.push(("enabled as an int", wrong("enabled", Variant::from(1u32))));
        cases.push(("min as a string", wrong("min", Variant::from("0"))));
        cases.push(("value as a bool", wrong("value", Variant::from(true))));
        cases.push(("label as an int", wrong("label", Variant::from(1i64))));
        cases.push(("bad id", wrong("id", Variant::from("Camera.Image.X"))));
        cases.push((
            "enabled_when not an array",
            wrong("enabled_when", Variant::from("x")),
        ));
        let mut bool_dict = s[2].to_dict();
        bool_dict.insert("value".into(), owned(Variant::from(1i64)));
        cases.push(("bool value as an int", bool_dict));
        let mut enum_dict = s[3].to_dict();
        enum_dict.insert("options".into(), owned(Variant::from("manual")));
        cases.push(("options not an array", enum_dict));
        let mut enum_dict = s[3].to_dict();
        enum_dict.remove("value");
        cases.push(("enum without value", enum_dict));

        for (what, bad) in cases {
            assert_eq!(Setting::from_dict(&bad), None, "{what}");
            // In a group, only that setting goes.
            let group = SettingsGroup {
                id: "exposure".into(),
                label: "Exposure".into(),
                settings: s[1..3].to_vec(),
                surface: None,
            };
            let mut gd = group.to_dict();
            let mut settings: Vec<VariantDict> = s[1..3].iter().map(Setting::to_dict).collect();
            settings.insert(1, bad);
            gd.insert("settings".into(), to_owned(&settings));
            let reply: SettingsReply = (5, 2, "cam".into(), vec![gd]);
            let tree = SettingsTree::from_wire(&over_the_wire(&reply));
            assert_eq!(tree.groups, vec![group], "{what}");
        }
    }

    #[test]
    fn bad_conditions_options_and_groups_are_dropped_alone() {
        let s = every_type();
        // A condition with an operand that is no value (a double), and one of
        // the wrong shape, among good ones.
        let mut d = s[1].to_dict();
        let conditions: Vec<OwnedVariant> = vec![
            to_owned(&("camera.exposure.gain", "eq", owned(Variant::from(1.5f64)))),
            to_owned(&("camera.exposure.gain", "eq")),
            to_owned(&("camera.exposure.gain", "ne", owned(Variant::from(-7i64)))),
        ];
        // As `av`: the only array that holds structures of several shapes.
        d.insert("enabled_when".into(), to_owned(&conditions));
        let got = Setting::from_dict(&d).unwrap();
        assert_eq!(
            got.enabled_when,
            vec![Condition {
                id: "camera.exposure.gain".into(),
                op: ConditionOp::Ne,
                value: SettingValue::Int(-7)
            }]
        );
        // An option of the wrong shape.
        let mut d = s[3].to_dict();
        let options: Vec<OwnedVariant> = vec![
            to_owned(&("manual", "Manual")),
            to_owned(&("x", 1u32)),
            to_owned(&("aperture_priority", "Auto")),
        ];
        d.insert("options".into(), to_owned(&options));
        assert_eq!(Setting::from_dict(&d), Some(s[3].clone()));
        // A group without a label, or with settings that are not an array,
        // goes; the others stay.
        let good = tree();
        let mut wire = good.to_wire();
        let mut no_label = wire.3[0].clone();
        no_label.remove("label");
        let mut not_array = wire.3[0].clone();
        not_array.insert("settings".into(), owned(Variant::from(3u32)));
        wire.3.insert(0, no_label);
        wire.3.push(not_array);
        assert_eq!(SettingsTree::from_wire(&over_the_wire(&wire)), good);
    }

    #[test]
    fn changes_round_trip_and_bad_ones_are_dropped_alone() {
        let changes = vec![
            SettingChange {
                id: "camera.image.brightness".into(),
                value: Some(SettingValue::Int(-64)),
                stored: true,
                enabled: true,
                disabled_reason: String::new(),
            },
            SettingChange {
                id: "camera.exposure.auto_exposure".into(),
                value: Some(SettingValue::Str("manual".into())),
                stored: false,
                enabled: true,
                disabled_reason: String::new(),
            },
            SettingChange {
                id: "camera.white_balance.red_balance".into(),
                value: Some(SettingValue::Bool(false)),
                stored: false,
                enabled: false,
                disabled_reason: "Turn off automatic white balance to change this".into(),
            },
            SettingChange {
                id: "camera.focus.pan_reset".into(),
                value: None,
                stored: false,
                enabled: true,
                disabled_reason: String::new(),
            },
        ];
        let wire: Vec<WireChange> = changes.iter().map(SettingChange::to_wire).collect();
        assert_eq!(wire[3].1.len(), 3, "no value for an action");
        assert_eq!(wire[0].1.len(), 4);
        assert_eq!(SettingChange::list_from_wire(&wire), changes);
        assert_eq!(
            SettingChange::list_from_wire(&over_the_wire(&wire)),
            changes
        );

        let mut bad = wire.clone();
        bad[0].1.remove("stored");
        bad[1]
            .1
            .insert("value".into(), owned(Variant::from(0.5f64)));
        bad[2].0 = "not an id".into();
        bad[3].1.insert("extra".into(), owned(Variant::from(1u32)));
        assert_eq!(
            SettingChange::list_from_wire(&over_the_wire(&bad)),
            changes[3..].to_vec()
        );
    }

    #[test]
    fn values_decode_from_every_integer_type() {
        for v in [
            Variant::from(7u8),
            Variant::from(7i16),
            Variant::from(7u16),
            Variant::from(7i32),
            Variant::from(7u32),
            Variant::from(7i64),
            Variant::from(7u64),
        ] {
            assert_eq!(SettingValue::from_variant(&v), Some(SettingValue::Int(7)));
        }
        assert_eq!(SettingValue::from_variant(&Variant::from(u64::MAX)), None);
        assert_eq!(SettingValue::from_variant(&Variant::from(1.0f64)), None);
        assert_eq!(
            SettingValue::from_variant(&Variant::from(true)),
            Some(SettingValue::Bool(true))
        );
        assert_eq!(
            SettingValue::from_variant(&Variant::from("m1")),
            Some(SettingValue::Str("m1".into()))
        );
        // A list of plain strings (as) decodes too.
        assert_eq!(
            SettingValue::from_variant(&Variant::from(vec!["a", "b"])),
            Some(SettingValue::List(vec![
                SettingValue::Str("a".into()),
                SettingValue::Str("b".into())
            ]))
        );
        let nested = SettingValue::List(vec![SettingValue::Int(1), SettingValue::Bool(true)]);
        assert_eq!(
            SettingValue::from_variant(&nested.to_variant()),
            Some(nested)
        );
    }

    #[test]
    fn reset_scopes_and_operators_parse_back() {
        for scope in [
            ResetScope::Camera,
            ResetScope::Group("white_balance".into()),
            ResetScope::Setting("camera.image.sharpness".into()),
        ] {
            assert_eq!(ResetScope::parse(&scope.to_wire()), Some(scope));
        }
        for bad in [
            "", "global", "group:", "setting:", "Camera", "group", " camera",
        ] {
            assert_eq!(ResetScope::parse(bad), None, "{bad:?}");
        }
        for op in [ConditionOp::Eq, ConditionOp::Ne, ConditionOp::In] {
            assert_eq!(ConditionOp::parse(op.as_str()), op);
        }
        assert_eq!(ConditionOp::parse("gt"), ConditionOp::Unknown("gt".into()));
        assert_eq!(ConditionOp::Unknown("gt".into()).as_str(), "gt");
    }

    #[test]
    fn the_camera_option() {
        assert!(camera_options("").is_empty());
        let o = camera_options("usb-a");
        assert_eq!(<&str>::try_from(&o["camera"]).unwrap(), "usb-a");
        let mut d = VariantDict::new();
        assert_eq!(camera_option(&d).unwrap(), "");
        d.insert("other".into(), owned(Variant::from(1u32)));
        assert_eq!(camera_option(&d).unwrap(), "");
        d.insert("camera".into(), owned(Variant::from("usb-a")));
        assert_eq!(camera_option(&d).unwrap(), "usb-a");
        d.insert("camera".into(), owned(Variant::from(1u32)));
        match camera_option(&d) {
            Err(IpcError::InvalidArgument(m)) => assert!(m.contains("string"), "{m}"),
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }
}
