//! The JSON documents a module hands to the daemon: the probe report and
//! the settings description.
//!
//! Both are plain data with builders, and serialise to the documents the
//! ABI's `probe` and `describe_settings` return. They also deserialise, so
//! a module's tests can read a document back.

use serde::{Deserialize, Serialize};

/// Which of a module's stages can run here ([`crate::Module::probe`]):
///
/// ```json
/// {"stages":[{"id":"denoise","available":true},
///            {"id":"relight","available":false,"reason":"Not installed"}]}
/// ```
///
/// Built with [`ProbeReport::new`] and its methods: the structure may gain
/// fields in a later release.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProbeReport {
    /// One entry per stage the module provides.
    pub stages: Vec<StageProbe>,
}

/// The availability of one stage, added with [`ProbeReport::available`]
/// or [`ProbeReport::unavailable`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StageProbe {
    /// The stage id.
    pub id: String,
    /// Whether the stage can run here.
    pub available: bool,
    /// Why not, for the user (for example `Not installed`); absent when
    /// available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl ProbeReport {
    /// An empty report.
    pub fn new() -> Self {
        ProbeReport::default()
    }

    /// Adds stage `id` as available.
    pub fn available(mut self, id: impl Into<String>) -> Self {
        self.stages.push(StageProbe {
            id: id.into(),
            available: true,
            reason: None,
        });
        self
    }

    /// Adds stage `id` as unavailable, for `reason`.
    pub fn unavailable(mut self, id: impl Into<String>, reason: impl Into<String>) -> Self {
        self.stages.push(StageProbe {
            id: id.into(),
            available: false,
            reason: Some(reason.into()),
        });
        self
    }

    /// The JSON document.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("a probe report always serialises")
    }

    /// A report read back from its JSON document.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}

/// A module's settings ([`crate::Module::settings`]): groups of settings,
/// in the daemon's self-describing settings model.
///
/// Setting ids are `effect.<group>.<name>` for a video module and
/// `mic.<group>.<name>` for an audio module (lower-case letters, digits and
/// `_`; at most 48 bytes). A setting with a `stage` and the
/// [`Role::Enable`] role switches that stage on and off; one with a `stage`
/// and a `key` is a parameter of the stage, applied as its [`Apply`] says.
///
/// Built with [`SettingsDescription::new`], [`Group::new`] and the
/// constructors of [`Setting`], and their methods: these structures may gain
/// fields in a later release.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SettingsDescription {
    /// The groups, in display order.
    pub groups: Vec<Group>,
}

impl SettingsDescription {
    /// An empty description.
    pub fn new() -> Self {
        SettingsDescription::default()
    }

    /// Adds a group.
    pub fn group(mut self, group: Group) -> Self {
        self.groups.push(group);
        self
    }

    /// The JSON document.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("a settings description always serialises")
    }

    /// A description read back from its JSON document.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}

/// A group of settings, shown together under its label.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Group {
    /// The group id, the `<group>` of its settings' ids.
    pub id: String,
    /// The label shown.
    pub label: String,
    /// Where the clients show the group, when not with the effects: `mic`
    /// for the microphone's settings (ABI 1.2). Absent: with the effects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
    /// The settings, in display order.
    pub settings: Vec<Setting>,
}

impl Group {
    /// An empty group.
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Group {
            id: id.into(),
            label: label.into(),
            surface: None,
            settings: Vec::new(),
        }
    }

    /// Sets where the clients show the group (`mic`: with the
    /// microphone's settings).
    pub fn surface(mut self, surface: impl Into<String>) -> Self {
        self.surface = Some(surface.into());
        self
    }

    /// Adds a setting.
    pub fn setting(mut self, setting: Setting) -> Self {
        self.settings.push(setting);
        self
    }
}

/// The type of a setting, with the keys of that type (the `type` key of the
/// JSON). Made by the constructors of [`Setting`].
///
/// A later release may add types, for settings types of a later ABI minor
/// version, and keys to a type: a `match` on it has a catch-all arm, and a
/// pattern of a type ends in `..`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
#[non_exhaustive]
pub enum SettingKind {
    /// `bool`: a switch.
    #[non_exhaustive]
    Bool {
        /// The default value.
        default: bool,
    },
    /// `int`: `min..=max`, counted from `min` in steps of `step`.
    #[non_exhaustive]
    Int {
        /// The smallest value.
        min: i64,
        /// The largest value.
        max: i64,
        /// The step between values.
        step: i64,
        /// The default value.
        default: i64,
        /// A unit shown after the value (`°`, `%`), if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unit: Option<String>,
    },
    /// `enum`: one of `options`, each `(value id, label)`.
    #[non_exhaustive]
    Enum {
        /// The options, in display order.
        options: Vec<(String, String)>,
        /// The default option's value id.
        default: String,
    },
}

/// What a setting with a `stage` is to that stage. A later release may add
/// roles: a `match` on it has a catch-all arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Role {
    /// The setting switches the stage on and off (a `bool`).
    Enable,
}

/// When a parameter's new value takes effect. A later release may add
/// moments: a `match` on it has a catch-all arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Apply {
    /// From the next frame, as a per-frame parameter, without a reload.
    Live,
    /// After reloading the chain (a load-time parameter).
    Reload,
    /// At the next pipeline start.
    Restart,
}

/// The operator of a [`Condition`]. A later release may add operators: a
/// `match` on it has a catch-all arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Op {
    /// The setting's value equals the operand.
    Eq,
    /// The setting's value differs from the operand.
    Ne,
    /// The setting's value is one of the operand's (a list).
    In,
}

/// An `enabled_when` condition: `[id, op, value]` in JSON. A setting is
/// enabled while all its conditions hold. Added with
/// [`Setting::enabled_when`], or made from an `(id, op, value)` tuple.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(
    from = "(String, Op, serde_json::Value)",
    into = "(String, Op, serde_json::Value)"
)]
pub struct Condition {
    /// The id of the setting compared.
    pub id: String,
    /// The comparison.
    pub op: Op,
    /// The operand: a boolean, an integer, a string, or for [`Op::In`] a
    /// list of those.
    pub value: serde_json::Value,
}

impl From<(String, Op, serde_json::Value)> for Condition {
    fn from((id, op, value): (String, Op, serde_json::Value)) -> Self {
        Condition { id, op, value }
    }
}

impl From<Condition> for (String, Op, serde_json::Value) {
    fn from(c: Condition) -> Self {
        (c.id, c.op, c.value)
    }
}

/// One setting, made with [`Setting::bool`], [`Setting::int`] or
/// [`Setting::enumeration`] and set up with the methods.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Setting {
    /// The id, `effect.<group>.<name>`.
    pub id: String,
    /// The label shown.
    pub label: String,
    /// The type and its keys.
    #[serde(flatten)]
    pub kind: SettingKind,
    /// The stage this setting belongs to, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    /// What the setting is to its stage, if not a parameter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    /// The parameter key the value is passed under, for a parameter of
    /// `stage`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// When a new value takes effect, for a parameter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply: Option<Apply>,
    /// The conditions under which the setting is enabled (all must hold).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enabled_when: Vec<Condition>,
    /// Why the setting is disabled while its conditions do not hold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    /// A display hint (`switch`, `slider`, `segmented`), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl Setting {
    fn with_kind(id: impl Into<String>, label: impl Into<String>, kind: SettingKind) -> Self {
        Setting {
            id: id.into(),
            label: label.into(),
            kind,
            stage: None,
            role: None,
            key: None,
            apply: None,
            enabled_when: Vec::new(),
            disabled_reason: None,
            hint: None,
        }
    }

    /// A `bool` setting.
    pub fn bool(id: impl Into<String>, label: impl Into<String>, default: bool) -> Self {
        Setting::with_kind(id, label, SettingKind::Bool { default })
    }

    /// An `int` setting: `min..=max` in steps of `step`.
    pub fn int(
        id: impl Into<String>,
        label: impl Into<String>,
        min: i64,
        max: i64,
        step: i64,
        default: i64,
    ) -> Self {
        Setting::with_kind(
            id,
            label,
            SettingKind::Int {
                min,
                max,
                step,
                default,
                unit: None,
            },
        )
    }

    /// An `enum` setting: `options` are `(value id, label)` pairs, and
    /// `default` a value id.
    pub fn enumeration<V, L>(
        id: impl Into<String>,
        label: impl Into<String>,
        options: impl IntoIterator<Item = (V, L)>,
        default: impl Into<String>,
    ) -> Self
    where
        V: Into<String>,
        L: Into<String>,
    {
        let options = options
            .into_iter()
            .map(|(v, l)| (v.into(), l.into()))
            .collect();
        Setting::with_kind(
            id,
            label,
            SettingKind::Enum {
                options,
                default: default.into(),
            },
        )
    }

    /// Sets the stage the setting belongs to.
    pub fn stage(mut self, stage: impl Into<String>) -> Self {
        self.stage = Some(stage.into());
        self
    }

    /// Sets the setting's role for its stage.
    pub fn role(mut self, role: Role) -> Self {
        self.role = Some(role);
        self
    }

    /// Sets the parameter key the value is passed under.
    pub fn key(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    /// Sets when a new value takes effect.
    pub fn apply(mut self, apply: Apply) -> Self {
        self.apply = Some(apply);
        self
    }

    /// Adds an `enabled_when` condition on setting `id`.
    pub fn enabled_when(
        mut self,
        id: impl Into<String>,
        op: Op,
        value: impl Into<serde_json::Value>,
    ) -> Self {
        self.enabled_when.push(Condition {
            id: id.into(),
            op,
            value: value.into(),
        });
        self
    }

    /// Sets why the setting is disabled while its conditions do not hold.
    pub fn disabled_reason(mut self, reason: impl Into<String>) -> Self {
        self.disabled_reason = Some(reason.into());
        self
    }

    /// Sets the unit of an `int` setting; ignored for other types.
    pub fn unit(mut self, unit: impl Into<String>) -> Self {
        if let SettingKind::Int { unit: u, .. } = &mut self.kind {
            *u = Some(unit.into());
        }
        self
    }

    /// Sets the display hint.
    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn probe_report_is_the_documented_json() {
        let report = ProbeReport::new()
            .available("denoise")
            .unavailable("relight", "Not installed");
        let json = report.to_json();
        assert_eq!(
            json,
            r#"{"stages":[{"id":"denoise","available":true},{"id":"relight","available":false,"reason":"Not installed"}]}"#
        );
        assert_eq!(ProbeReport::from_json(&json).unwrap(), report);
    }

    #[test]
    fn settings_description_is_the_documented_json() {
        let desc = SettingsDescription::new().group(
            Group::new("lighting", "Lighting")
                .setting(
                    Setting::bool("effect.lighting.relight", "Relighting", false)
                        .stage("relight")
                        .role(Role::Enable),
                )
                .setting(
                    Setting::enumeration(
                        "effect.lighting.light",
                        "Light",
                        [("warm", "Warm"), ("neutral", "Neutral"), ("cool", "Cool")],
                        "neutral",
                    )
                    .stage("relight")
                    .key("light")
                    .apply(Apply::Reload)
                    .enabled_when("effect.lighting.relight", Op::Eq, true)
                    .disabled_reason("Turn on relighting to change this"),
                )
                .setting(
                    Setting::int("effect.lighting.direction", "Direction", -90, 90, 5, 0)
                        .unit("°")
                        .stage("relight")
                        .key("direction")
                        .apply(Apply::Live)
                        .hint("slider"),
                ),
        );
        let value: serde_json::Value = serde_json::from_str(&desc.to_json()).unwrap();
        assert_eq!(
            value,
            json!({"groups":[{"id":"lighting","label":"Lighting","settings":[
                {"id":"effect.lighting.relight","label":"Relighting","type":"bool","default":false,
                 "stage":"relight","role":"enable"},
                {"id":"effect.lighting.light","label":"Light","type":"enum",
                 "options":[["warm","Warm"],["neutral","Neutral"],["cool","Cool"]],
                 "default":"neutral","stage":"relight","key":"light","apply":"reload",
                 "enabled_when":[["effect.lighting.relight","eq",true]],
                 "disabled_reason":"Turn on relighting to change this"},
                {"id":"effect.lighting.direction","label":"Direction","type":"int","min":-90,"max":90,
                 "step":5,"default":0,"unit":"°","stage":"relight","key":"direction","apply":"live",
                 "hint":"slider"}]}]})
        );
        assert_eq!(
            SettingsDescription::from_json(&desc.to_json()).unwrap(),
            desc
        );
    }

    #[test]
    fn a_mic_group_carries_its_surface() {
        let desc = SettingsDescription::new().group(
            Group::new("noise", "Noise removal")
                .surface("mic")
                .setting(Setting::bool("mic.noise.enabled", "Noise removal", true)),
        );
        let value: serde_json::Value = serde_json::from_str(&desc.to_json()).unwrap();
        assert_eq!(
            value,
            json!({"groups":[{"id":"noise","label":"Noise removal","surface":"mic","settings":[
                {"id":"mic.noise.enabled","label":"Noise removal","type":"bool","default":true}]}]})
        );
        assert_eq!(
            SettingsDescription::from_json(&desc.to_json()).unwrap(),
            desc
        );
    }

    #[test]
    fn there_is_no_action_type() {
        // A module setting of the type `action` reaches no module: the type
        // is not in the module settings vocabulary.
        let json = r#"{"groups":[{"id":"a","label":"A","settings":[
            {"id":"effect.a.reset","label":"Reset","type":"action"}]}]}"#;
        assert!(SettingsDescription::from_json(json).is_err());
    }

    #[test]
    fn unit_only_applies_to_int() {
        let s = Setting::bool("effect.a.b", "B", true).unit("%");
        assert_eq!(s.kind, SettingKind::Bool { default: true });
    }
}
