//! The settings a module describes (`describe_settings`), read into a typed
//! model and validated.
//!
//! The document is the daemon's self-describing settings model:
//!
//! ```json
//! {"groups":[{"id":"lighting","label":"Lighting","settings":[
//!   {"id":"effect.lighting.relight","label":"Relighting","type":"bool","default":false,
//!    "stage":"relight","role":"enable"},
//!   {"id":"effect.lighting.direction","label":"Light direction","type":"int",
//!    "min":-90,"max":90,"step":5,"default":0,"unit":"°",
//!    "stage":"relight","key":"direction","apply":"live",
//!    "enabled_when":[["effect.lighting.relight","eq",true]],
//!    "disabled_reason":"Turn on relighting to change this","hint":"slider"}]}]}
//! ```
//!
//! Reading is tolerant per setting: a setting (or a group) that is
//! malformed or breaks a rule is dropped with a log line, never the module
//! or the rest of the description. The rules:
//!
//! * a group id is one setting-id segment (`a-z`, `0-9`, `_`), not the
//!   daemon's own group of the module's kind ([`CORE_GROUP`], the
//!   background group, for a video module; [`AUDIO_CORE_GROUP`], the
//!   microphone input group, for an audio module), and unique; its label
//!   is non-empty and at most [`LABEL_MAX`] bytes; its optional `surface`
//!   (where the clients show it: `mic` for the microphone's settings) is
//!   one segment too, or it is left out with a log line;
//! * a setting id is `effect.<group>.<name>` for a video module and
//!   `mic.<group>.<name>` for an audio module ([`id_prefix`]), `<group>`
//!   its group's id, a valid setting id ([`valid_setting_id`]), unique in
//!   the description;
//! * a label is non-empty and at most [`LABEL_MAX`] bytes;
//! * `int`: `min <= default <= max`, `step > 0`; `enum`: 1 to
//!   [`MAX_OPTIONS`] options of unique values and valid labels, the default
//!   one of them; `bool`, `action`: nothing more; another type is dropped;
//! * `stage` is a stage of the module's manifest; `role: "enable"` needs a
//!   `bool` with a stage; `key` needs a stage, and its `apply` defaults to
//!   `reload`; `apply` needs a key;
//! * `enabled_when` conditions name valid setting ids, with `eq`, `ne` or
//!   `in`;
//! * at most [`MAX_SETTINGS`] settings in all: the rest are dropped.

use std::collections::BTreeSet;

use obc_backend::StageId;
use serde::Deserialize;

use crate::manifest::ModuleKind;

/// The most settings a module describes.
pub const MAX_SETTINGS: usize = 64;
/// The most options of an `enum` setting.
pub const MAX_OPTIONS: usize = 16;
/// The longest label (of a group, a setting or an option), in bytes.
pub const LABEL_MAX: usize = 48;
/// The longest setting id, in bytes.
pub const SETTING_ID_MAX: usize = 48;
/// The prefix of every video module setting id.
pub const ID_PREFIX: &str = "effect.";
/// The prefix of every audio module setting id (ABI 1.2).
pub const AUDIO_ID_PREFIX: &str = "mic.";
/// The group the daemon describes itself (the `background` stage's mode
/// and blur strength): a video module's group of this id is dropped.
pub const CORE_GROUP: &str = "background";
/// The microphone group the daemon describes itself (`mic.input`: the
/// processing switch and the input device): an audio module's group of
/// this id is dropped.
pub const AUDIO_CORE_GROUP: &str = "input";
/// The `surface` of the microphone's settings groups.
pub const SURFACE_MIC: &str = "mic";

/// The prefix of the setting ids of a module of `kind`: [`AUDIO_ID_PREFIX`]
/// for an audio filter, [`ID_PREFIX`] for the others.
pub fn id_prefix(kind: ModuleKind) -> &'static str {
    if kind.is_audio() {
        AUDIO_ID_PREFIX
    } else {
        ID_PREFIX
    }
}

/// The daemon's own group of the settings of a module of `kind`.
pub fn core_group(kind: ModuleKind) -> &'static str {
    if kind.is_audio() {
        AUDIO_CORE_GROUP
    } else {
        CORE_GROUP
    }
}

/// Whether `id` is a valid setting id, as the daemon's settings channel
/// says: `^[a-z0-9]+(\.[a-z0-9_]+){1,3}$`, at most [`SETTING_ID_MAX`]
/// bytes.
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

fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn check_label(what: &str, label: &str) -> Result<(), String> {
    if label.trim().is_empty() {
        Err(format!("{what} has an empty label"))
    } else if label.len() > LABEL_MAX {
        Err(format!(
            "{what} has a label of {} bytes, more than {LABEL_MAX}",
            label.len()
        ))
    } else {
        Ok(())
    }
}

/// The module's settings, valid ones only, in display order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SettingsDescription {
    /// The groups.
    pub groups: Vec<SettingsGroup>,
}

impl SettingsDescription {
    /// Every setting, group after group.
    pub fn settings(&self) -> impl Iterator<Item = &SettingDesc> {
        self.groups.iter().flat_map(|g| g.settings.iter())
    }

    /// The setting `id`.
    pub fn setting(&self, id: &str) -> Option<&SettingDesc> {
        self.settings().find(|s| s.id == id)
    }

    /// The setting that is parameter `key` of `stage`.
    pub fn param(&self, stage: &str, key: &str) -> Option<&SettingDesc> {
        self.settings()
            .find(|s| s.stage.as_ref().is_some_and(|t| t == stage) && s.key.as_deref() == Some(key))
    }

    /// The setting that switches `stage` on and off.
    pub fn enable_of(&self, stage: &str) -> Option<&SettingDesc> {
        self.settings()
            .find(|s| s.role == Some(Role::Enable) && s.stage.as_ref().is_some_and(|t| t == stage))
    }
}

/// A group of settings.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsGroup {
    /// The group id, the `<group>` of its settings' ids.
    pub id: String,
    /// The label shown.
    pub label: String,
    /// Where the clients show the group, when the module said
    /// ([`SURFACE_MIC`]: with the microphone's settings); `None`: with the
    /// effects.
    pub surface: Option<String>,
    /// Its settings; a group whose settings were all dropped is kept,
    /// empty.
    pub settings: Vec<SettingDesc>,
}

/// The type of a setting, with its keys.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingType {
    /// A switch.
    Bool {
        /// The default value.
        default: bool,
    },
    /// An integer in `min..=max`, in steps of `step` from `min`.
    Int {
        /// The smallest value.
        min: i64,
        /// The largest value.
        max: i64,
        /// The step.
        step: i64,
        /// The default value.
        default: i64,
        /// A unit shown after the value, if any.
        unit: Option<String>,
    },
    /// One of `options`, `(value, label)`.
    Enum {
        /// The options, in display order.
        options: Vec<(String, String)>,
        /// The default value.
        default: String,
    },
    /// An action: no value.
    Action,
}

impl SettingType {
    /// The wire name (`bool`, `int`, `enum`, `action`).
    pub fn as_str(&self) -> &'static str {
        match self {
            SettingType::Bool { .. } => "bool",
            SettingType::Int { .. } => "int",
            SettingType::Enum { .. } => "enum",
            SettingType::Action => "action",
        }
    }
}

/// What a setting with a stage is to that stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// It switches the stage on and off.
    Enable,
}

/// When a parameter's new value takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Apply {
    /// From the next frame, as a live parameter.
    Live,
    /// After a reload of the chain, as a load-time parameter.
    Reload,
    /// At the next pipeline start.
    Restart,
}

/// The operator of a condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    /// Equal.
    Eq,
    /// Not equal.
    Ne,
    /// One of a list.
    In,
}

/// An `enabled_when` condition: the setting `id`'s value compared with
/// `value` by `op`.
#[derive(Debug, Clone, PartialEq)]
pub struct Condition {
    /// The setting compared.
    pub id: String,
    /// The comparison.
    pub op: Op,
    /// The operand (for [`Op::In`], a list).
    pub value: serde_json::Value,
}

/// One setting.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingDesc {
    /// The id, `effect.<group>.<name>` (`mic.<group>.<name>` for an audio
    /// module).
    pub id: String,
    /// The label.
    pub label: String,
    /// The type and its keys.
    pub kind: SettingType,
    /// The stage it belongs to, if any.
    pub stage: Option<StageId>,
    /// What it is to its stage, if not a parameter.
    pub role: Option<Role>,
    /// The parameter key of its stage, for a parameter.
    pub key: Option<String>,
    /// When a new value of a parameter takes effect ([`Apply::Reload`] when
    /// the module gave a key but no `apply`).
    pub apply: Option<Apply>,
    /// The conditions under which it is enabled (all must hold).
    pub enabled_when: Vec<Condition>,
    /// Why it is disabled while its conditions do not hold.
    pub disabled_reason: Option<String>,
    /// A display hint, if any.
    pub hint: Option<String>,
}

#[derive(Deserialize)]
struct RawDoc {
    groups: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct RawGroup {
    id: String,
    label: String,
    #[serde(default)]
    surface: Option<serde_json::Value>,
    #[serde(default)]
    settings: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct RawSetting {
    id: String,
    label: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    default: Option<serde_json::Value>,
    #[serde(default)]
    min: Option<i64>,
    #[serde(default)]
    max: Option<i64>,
    #[serde(default)]
    step: Option<i64>,
    #[serde(default)]
    unit: Option<String>,
    #[serde(default)]
    options: Option<Vec<(String, String)>>,
    #[serde(default)]
    stage: Option<String>,
    #[serde(default)]
    role: Option<Role>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    apply: Option<Apply>,
    #[serde(default)]
    enabled_when: Vec<(String, Op, serde_json::Value)>,
    #[serde(default)]
    disabled_reason: Option<String>,
    #[serde(default)]
    hint: Option<String>,
}

fn need<T>(v: Option<T>, setting: &str, key: &str) -> Result<T, String> {
    v.ok_or_else(|| format!("setting {setting} lacks {key}"))
}

impl RawSetting {
    fn validate(
        self,
        group: &str,
        stages: &[StageId],
        kind: ModuleKind,
    ) -> Result<SettingDesc, String> {
        let id = self.id;
        let prefix = format!("{}{group}.", id_prefix(kind));
        let name_ok = id.strip_prefix(&prefix).is_some_and(valid_segment);
        if !name_ok || !valid_setting_id(&id) {
            return Err(format!(
                "setting {id:?} is not a valid id of the form {prefix}<name>"
            ));
        }
        check_label(&format!("setting {id}"), &self.label)?;
        let default = self.default;
        let kind = match self.kind.as_str() {
            "bool" => SettingType::Bool {
                default: need(
                    default.as_ref().and_then(|v| v.as_bool()),
                    &id,
                    "a bool default",
                )?,
            },
            "int" => {
                let min = need(self.min, &id, "min")?;
                let max = need(self.max, &id, "max")?;
                let step = need(self.step, &id, "step")?;
                let default = need(
                    default.as_ref().and_then(|v| v.as_i64()),
                    &id,
                    "an int default",
                )?;
                if min > max || step <= 0 || !(min..=max).contains(&default) {
                    return Err(format!(
                        "setting {id}: min {min}, max {max}, step {step} and default {default} \
                         are inconsistent"
                    ));
                }
                SettingType::Int {
                    min,
                    max,
                    step,
                    default,
                    unit: self.unit.filter(|u| !u.is_empty()),
                }
            }
            "enum" => {
                let options = need(self.options, &id, "options")?;
                if options.is_empty() || options.len() > MAX_OPTIONS {
                    return Err(format!(
                        "setting {id} has {} options; 1 to {MAX_OPTIONS} are allowed",
                        options.len()
                    ));
                }
                let mut values = BTreeSet::new();
                for (value, label) in &options {
                    if value.is_empty() || !values.insert(value.as_str()) {
                        return Err(format!(
                            "setting {id} has an empty or repeated option {value:?}"
                        ));
                    }
                    check_label(&format!("option {value} of {id}"), label)?;
                }
                let default = need(
                    default
                        .as_ref()
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    &id,
                    "a string default",
                )?;
                if !values.contains(default.as_str()) {
                    return Err(format!(
                        "setting {id}: the default {default:?} is not an option"
                    ));
                }
                SettingType::Enum { options, default }
            }
            "action" => SettingType::Action,
            other => {
                return Err(format!(
                    "setting {id} has a type {other:?} this daemon does not know"
                ));
            }
        };
        let stage = match self.stage {
            None => None,
            Some(s) => {
                let st = StageId::new(s.clone())
                    .map_err(|_| format!("setting {id}: {s:?} is not a stage id"))?;
                if !stages.contains(&st) {
                    return Err(format!(
                        "setting {id} belongs to the stage {st}, which the manifest does not declare"
                    ));
                }
                Some(st)
            }
        };
        if self.role == Some(Role::Enable)
            && (stage.is_none() || !matches!(kind, SettingType::Bool { .. }))
        {
            return Err(format!(
                "setting {id}: an enable role needs a bool setting with a stage"
            ));
        }
        let key = self.key.filter(|k| !k.is_empty());
        if key.is_some() && stage.is_none() {
            return Err(format!("setting {id} has a key but no stage"));
        }
        if key.is_some() && self.role.is_some() {
            return Err(format!("setting {id} has both a role and a key"));
        }
        if self.apply.is_some() && key.is_none() {
            return Err(format!("setting {id} has an apply but no key"));
        }
        let apply = key.as_ref().map(|_| self.apply.unwrap_or(Apply::Reload));
        let mut enabled_when = Vec::with_capacity(self.enabled_when.len());
        for (cid, op, value) in self.enabled_when {
            if !valid_setting_id(&cid) {
                return Err(format!(
                    "setting {id}: a condition names {cid:?}, not a setting id"
                ));
            }
            if op == Op::In && !value.is_array() {
                return Err(format!("setting {id}: an `in` condition needs a list"));
            }
            enabled_when.push(Condition { id: cid, op, value });
        }
        Ok(SettingDesc {
            id,
            label: self.label,
            kind,
            stage,
            role: self.role,
            key,
            apply,
            enabled_when,
            disabled_reason: self.disabled_reason.filter(|r| !r.is_empty()),
            hint: self.hint.filter(|h| !h.is_empty()),
        })
    }
}

/// Reads and validates the settings description `json` of a video module
/// whose manifest declares `stages` ([`parse_settings_for`] of
/// [`ModuleKind::VideoStage`]).
pub fn parse_settings(
    json: &str,
    stages: &[StageId],
    drop_line: impl FnMut(String),
) -> Result<SettingsDescription, String> {
    parse_settings_for(json, stages, ModuleKind::VideoStage, drop_line)
}

/// Reads and validates the settings description `json` of a module of
/// `kind` whose manifest declares `stages`. Each setting or group dropped
/// (and each `surface` left out) is reported to `drop_line` with the
/// reason. Malformed JSON, or a document without a `groups` list, is an
/// error.
pub fn parse_settings_for(
    json: &str,
    stages: &[StageId],
    kind: ModuleKind,
    mut drop_line: impl FnMut(String),
) -> Result<SettingsDescription, String> {
    let core = core_group(kind);
    let doc: RawDoc = serde_json::from_str(json)
        .map_err(|e| format!("the settings description is malformed: {e}"))?;
    let mut groups: Vec<SettingsGroup> = Vec::new();
    let mut ids = BTreeSet::new();
    let mut count = 0usize;
    for raw in doc.groups {
        let group: RawGroup = match serde_json::from_value(raw) {
            Ok(g) => g,
            Err(e) => {
                drop_line(format!("a settings group was dropped: {e}"));
                continue;
            }
        };
        if !valid_segment(&group.id) {
            drop_line(format!(
                "the settings group {:?} was dropped: not a valid group id",
                group.id
            ));
            continue;
        }
        if group.id == core {
            drop_line(format!(
                "the settings group {core} was dropped: the daemon describes it"
            ));
            continue;
        }
        if groups.iter().any(|g| g.id == group.id) {
            drop_line(format!(
                "the settings group {} was dropped: described twice",
                group.id
            ));
            continue;
        }
        if let Err(e) = check_label(&format!("group {}", group.id), &group.label) {
            drop_line(format!("the settings group {} was dropped: {e}", group.id));
            continue;
        }
        let surface = match group.surface {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(s)) if valid_segment(&s) => Some(s),
            Some(other) => {
                drop_line(format!(
                    "the surface {other} of the settings group {} was left out: not a valid \
                     surface",
                    group.id
                ));
                None
            }
        };
        let mut settings = Vec::new();
        for raw in group.settings {
            let parsed = serde_json::from_value::<RawSetting>(raw)
                .map_err(|e| format!("a setting of the group {} is malformed: {e}", group.id))
                .and_then(|s| s.validate(&group.id, stages, kind));
            match parsed {
                Err(e) => drop_line(format!("a setting was dropped: {e}")),
                Ok(s) if ids.contains(&s.id) => {
                    drop_line(format!("the setting {} was dropped: described twice", s.id));
                }
                Ok(s) if count >= MAX_SETTINGS => {
                    drop_line(format!(
                        "the setting {} was dropped: more than {MAX_SETTINGS} settings",
                        s.id
                    ));
                }
                Ok(s) => {
                    ids.insert(s.id.clone());
                    count += 1;
                    settings.push(s);
                }
            }
        }
        groups.push(SettingsGroup {
            id: group.id,
            label: group.label,
            surface,
            settings,
        });
    }
    Ok(SettingsDescription { groups })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stages() -> Vec<StageId> {
        vec![StageId::RELIGHT, StageId::DENOISE]
    }

    fn parse(doc: serde_json::Value) -> (SettingsDescription, Vec<String>) {
        let mut dropped = Vec::new();
        let d = parse_settings(&doc.to_string(), &stages(), |l| dropped.push(l)).unwrap();
        (d, dropped)
    }

    fn lighting() -> serde_json::Value {
        json!({"groups":[{"id":"lighting","label":"Lighting","settings":[
            {"id":"effect.lighting.relight","label":"Relighting","type":"bool","default":false,
             "stage":"relight","role":"enable","hint":"switch"},
            {"id":"effect.lighting.light","label":"Light","type":"enum",
             "options":[["warm","Warm"],["neutral","Neutral"],["cool","Cool"]],
             "default":"neutral","stage":"relight","key":"light","apply":"reload",
             "enabled_when":[["effect.lighting.relight","eq",true]],
             "disabled_reason":"Turn on relighting to change this"},
            {"id":"effect.lighting.direction","label":"Light direction","type":"int","min":-90,
             "max":90,"step":5,"default":0,"unit":"°","stage":"relight","key":"direction",
             "apply":"live"},
            {"id":"effect.lighting.go","label":"Go","type":"action"}
        ]}]})
    }

    #[test]
    fn a_valid_description_reads_into_the_model() {
        let (d, dropped) = parse(lighting());
        assert!(dropped.is_empty(), "{dropped:?}");
        assert_eq!(d.groups.len(), 1);
        let g = &d.groups[0];
        assert_eq!(
            (g.id.as_str(), g.label.as_str(), g.settings.len()),
            ("lighting", "Lighting", 4)
        );
        let relight = d.enable_of("relight").unwrap();
        assert_eq!(relight.id, "effect.lighting.relight");
        assert_eq!(relight.kind, SettingType::Bool { default: false });
        assert_eq!(relight.hint.as_deref(), Some("switch"));
        let light = d.param("relight", "light").unwrap();
        assert_eq!(light.apply, Some(Apply::Reload));
        assert_eq!(
            light.enabled_when,
            [Condition {
                id: "effect.lighting.relight".into(),
                op: Op::Eq,
                value: json!(true)
            }]
        );
        assert_eq!(
            light.disabled_reason.as_deref(),
            Some("Turn on relighting to change this")
        );
        let dir = d.setting("effect.lighting.direction").unwrap();
        assert_eq!(
            dir.kind,
            SettingType::Int {
                min: -90,
                max: 90,
                step: 5,
                default: 0,
                unit: Some("°".into())
            }
        );
        assert_eq!(
            (dir.apply, dir.stage.as_ref()),
            (Some(Apply::Live), Some(&StageId::RELIGHT))
        );
        assert_eq!(
            d.setting("effect.lighting.go").unwrap().kind,
            SettingType::Action
        );
    }

    #[test]
    fn invalid_settings_are_dropped_alone() {
        let bad = [
            json!({"id":"effect.other.x","label":"X","type":"bool","default":true}),
            json!({"id":"effect.lighting.Bad","label":"X","type":"bool","default":true}),
            json!({"id":"effect.lighting.empty","label":"","type":"bool","default":true}),
            json!({"id":"effect.lighting.long","label":"x".repeat(49),"type":"bool","default":true}),
            json!({"id":"effect.lighting.range","label":"R","type":"int","min":5,"max":1,"step":1,"default":2}),
            json!({"id":"effect.lighting.step","label":"R","type":"int","min":0,"max":1,"step":0,"default":0}),
            json!({"id":"effect.lighting.outside","label":"R","type":"int","min":0,"max":1,"step":1,"default":3}),
            json!({"id":"effect.lighting.noopts","label":"E","type":"enum","options":[],"default":"a"}),
            json!({"id":"effect.lighting.nodefault","label":"E","type":"enum","options":[["a","A"]],"default":"b"}),
            json!({"id":"effect.lighting.dup","label":"E","type":"enum","options":[["a","A"],["a","B"]],"default":"a"}),
            json!({"id":"effect.lighting.many","label":"E","type":"enum",
                   "options":(0..17).map(|i| (format!("o{i}"), format!("O{i}"))).collect::<Vec<_>>(),
                   "default":"o0"}),
            json!({"id":"effect.lighting.colour","label":"C","type":"colour","default":"red"}),
            json!({"id":"effect.lighting.stage","label":"S","type":"bool","default":true,"stage":"hello"}),
            json!({"id":"effect.lighting.role","label":"S","type":"int","min":0,"max":1,"step":1,"default":0,"stage":"relight","role":"enable"}),
            json!({"id":"effect.lighting.key","label":"K","type":"bool","default":true,"key":"k"}),
            json!({"id":"effect.lighting.apply","label":"A","type":"bool","default":true,"apply":"live"}),
            json!({"id":"effect.lighting.cond","label":"C","type":"bool","default":true,"enabled_when":[["Nope","eq",true]]}),
            json!({"id":"effect.lighting.in","label":"C","type":"bool","default":true,"enabled_when":[["effect.lighting.x","in",true]]}),
            json!({"label":"no id","type":"bool","default":true}),
            json!({"id":"effect.lighting.wrongdefault","label":"B","type":"bool","default":"yes"}),
        ];
        let mut doc = lighting();
        let settings = doc["groups"][0]["settings"].as_array_mut().unwrap();
        settings.extend(bad.iter().cloned());
        // A repeat of a valid id.
        settings.push(
            json!({"id":"effect.lighting.relight","label":"Again","type":"bool","default":true}),
        );
        let (d, dropped) = parse(doc);
        assert_eq!(d.groups[0].settings.len(), 4, "the valid ones stay");
        assert_eq!(dropped.len(), bad.len() + 1, "{dropped:#?}");
        assert!(dropped.last().unwrap().contains("described twice"));
    }

    #[test]
    fn a_key_without_apply_reloads() {
        let doc = json!({"groups":[{"id":"video","label":"Video","settings":[
            {"id":"effect.video.level","label":"Level","type":"int","min":0,"max":2,"step":1,
             "default":1,"stage":"denoise","key":"level"}]}]});
        let (d, dropped) = parse(doc);
        assert!(dropped.is_empty());
        assert_eq!(
            d.param("denoise", "level").unwrap().apply,
            Some(Apply::Reload)
        );
    }

    #[test]
    fn groups_are_validated_and_the_core_group_is_the_daemons() {
        let doc = json!({"groups":[
            {"id":"background","label":"Background","settings":[]},
            {"id":"Bad","label":"B","settings":[]},
            {"id":"video","label":"","settings":[]},
            {"id":"video","label":"Video","settings":[]},
            {"id":"video","label":"Again","settings":[]},
            {"label":"no id"},
        ]});
        let (d, dropped) = parse(doc);
        let ids: Vec<&str> = d.groups.iter().map(|g| g.id.as_str()).collect();
        assert_eq!(ids, ["video"]);
        assert_eq!(d.groups[0].label, "Video");
        assert_eq!(dropped.len(), 5, "{dropped:#?}");
        assert!(dropped[0].contains("the daemon describes it"));
    }

    #[test]
    fn an_audio_module_describes_mic_settings_with_a_surface() {
        let stages = [StageId::new("mic-denoise").unwrap()];
        let doc = json!({"groups":[
            {"id":"noise","label":"Noise removal","surface":"mic","settings":[
                {"id":"mic.noise.enabled","label":"Noise removal","type":"bool","default":true,
                 "stage":"mic-denoise","role":"enable"},
                {"id":"mic.noise.strength","label":"Strength","type":"int","min":0,"max":100,
                 "step":5,"default":100,"stage":"mic-denoise","key":"strength","apply":"live"},
                {"id":"effect.noise.x","label":"Video id","type":"bool","default":true}]},
            {"id":"input","label":"Input","settings":[]},
            {"id":"echo","label":"Echo","surface":"Not ok","settings":[]},
            {"id":"plain","label":"Plain","settings":[]},
        ]});
        let mut dropped = Vec::new();
        let d = parse_settings_for(&doc.to_string(), &stages, ModuleKind::AudioFilter, |l| {
            dropped.push(l)
        })
        .unwrap();
        let ids: Vec<&str> = d.groups.iter().map(|g| g.id.as_str()).collect();
        assert_eq!(
            ids,
            ["noise", "echo", "plain"],
            "the daemon's input group is dropped"
        );
        assert_eq!(d.groups[0].surface.as_deref(), Some(SURFACE_MIC));
        assert_eq!(d.groups[1].surface, None, "an invalid surface is left out");
        assert_eq!(d.groups[2].surface, None);
        assert_eq!(d.enable_of("mic-denoise").unwrap().id, "mic.noise.enabled");
        assert_eq!(
            d.param("mic-denoise", "strength").unwrap().apply,
            Some(Apply::Live)
        );
        assert_eq!(dropped.len(), 3, "{dropped:#?}");
        assert!(dropped[0].contains("mic.noise.<name>"), "{}", dropped[0]);
        assert!(dropped[1].contains("the daemon describes it"));
        assert!(dropped[2].contains("left out"));

        // A video module keeps its prefix, and may carry a surface too.
        let (d, dropped) = parse(json!({"groups":[{"id":"video","label":"Video",
            "surface":"mic","settings":[
            {"id":"mic.video.x","label":"X","type":"bool","default":true},
            {"id":"effect.video.y","label":"Y","type":"bool","default":true}]}]}));
        assert_eq!(d.groups[0].settings.len(), 1);
        assert_eq!(d.groups[0].surface.as_deref(), Some("mic"));
        assert_eq!(dropped.len(), 1);
        assert_eq!(id_prefix(ModuleKind::VideoStage), ID_PREFIX);
        assert_eq!(id_prefix(ModuleKind::AudioFilter), AUDIO_ID_PREFIX);
    }

    #[test]
    fn at_most_64_settings_are_kept() {
        let settings: Vec<_> = (0..70)
            .map(|i| json!({"id":format!("effect.many.s{i}"),"label":"S","type":"bool","default":false}))
            .collect();
        let (d, dropped) =
            parse(json!({"groups":[{"id":"many","label":"Many","settings":settings}]}));
        assert_eq!(d.groups[0].settings.len(), MAX_SETTINGS);
        assert_eq!(dropped.len(), 6);
        assert!(dropped[0].contains("more than 64"));
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert!(parse_settings("{", &stages(), |_| {}).is_err());
        assert!(parse_settings("{}", &stages(), |_| {}).is_err());
        assert!(
            parse_settings(r#"{"groups":[]}"#, &stages(), |_| {})
                .unwrap()
                .groups
                .is_empty()
        );
    }

    #[test]
    fn setting_ids_follow_the_settings_channel() {
        for ok in [
            "effect.video.denoise",
            "camera.image.brightness",
            "a.b",
            "a.b_c.d.e",
        ] {
            assert!(valid_setting_id(ok), "{ok}");
        }
        for bad in [
            "",
            "effect",
            "a_b.c",
            "a.b.c.d.e",
            "a..b",
            "A.b",
            &format!("a.{}", "b".repeat(47)),
        ] {
            assert!(!valid_setting_id(bad), "{bad}");
        }
    }
}
