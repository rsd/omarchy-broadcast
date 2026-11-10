//! The keys of the module system in the config and state files: which
//! modules are enabled, the stage order, the backend profile and the
//! per-stage pins, and the values of the settings modules describe.
//!
//! The config's `[modules]` table ([`ModulesConfig`]) is what the user
//! decides; the state file's `[modules]` table ([`ModulesState`]) holds the
//! choices made at run time (over D-Bus), which override it. The values of
//! module settings are kept in the state file's `[settings]` table by
//! setting id ([`StoredValue`]). Each module's own configuration is a free
//! table of the config, `[module."<id>"]`, handed to the module as it is.
//!
//! The stage and module ids are checked here for their form only; whether a
//! module or a stage exists is the daemon's to say.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The longest stage id, in bytes.
pub const STAGE_ID_MAX: usize = 32;
/// The longest module id, in bytes.
pub const MODULE_ID_MAX: usize = 64;

/// Whether `id` is a stage id: `^[a-z][a-z0-9-]{0,31}$`.
pub fn valid_stage_id(id: &str) -> bool {
    let mut bytes = id.bytes();
    id.len() <= STAGE_ID_MAX
        && bytes.next().is_some_and(|b| b.is_ascii_lowercase())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Whether `id` is a module id: reverse-DNS, `^[a-z0-9]+(\.[a-z0-9-]+)+$`,
/// at most [`MODULE_ID_MAX`] bytes.
pub fn valid_module_id(id: &str) -> bool {
    if id.is_empty() || id.len() > MODULE_ID_MAX {
        return false;
    }
    let mut parts = id.split('.');
    let first = parts.next().unwrap_or_default();
    let first_ok = !first.is_empty()
        && first
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    let mut rest = 0;
    let rest_ok = parts.all(|p| {
        rest += 1;
        !p.is_empty()
            && p.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    });
    first_ok && rest_ok && rest > 0
}

/// Which backend a stage prefers when several modules provide it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendProfile {
    /// The highest rank.
    #[default]
    Quality,
    /// The least power: an NPU, then the CPU, then a GPU, then rank.
    Battery,
}

impl BackendProfile {
    /// The file and wire name: `quality` or `battery`.
    pub fn as_str(self) -> &'static str {
        match self {
            BackendProfile::Quality => "quality",
            BackendProfile::Battery => "battery",
        }
    }
}

impl fmt::Display for BackendProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for BackendProfile {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "quality" => Ok(BackendProfile::Quality),
            "battery" => Ok(BackendProfile::Battery),
            _ => Err(format!(
                "unknown backend profile {s:?} (expected quality or battery)"
            )),
        }
    }
}

impl Serialize for BackendProfile {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for BackendProfile {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// The config's `[modules]` table.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModulesConfig {
    /// More directories of module manifests, absolute paths, read after the
    /// system's and the user's (a later directory overrides an earlier one
    /// by module id) and before `OBC_MODULE_PATH`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dirs: Vec<PathBuf>,
    /// Module ids that are not loaded.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub disabled: Vec<String>,
    /// The stage order, by stage id; empty: the default order (by slot,
    /// then each stage's preferred position). An order that is not valid
    /// for the modules found is replaced by the default, with a warning.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub order: Vec<String>,
    /// The backend profile (`quality` by default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<BackendProfile>,
    /// The module that provides a stage, by stage id, whatever the profile
    /// says, while it can.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub pin: BTreeMap<String, String>,
}

impl ModulesConfig {
    /// Checks the values: absolute directories, module and stage ids of the
    /// right form, no stage twice in the order. The error names the key.
    pub fn validate(&self) -> Result<(), (&'static str, String)> {
        if let Some(d) = self.dirs.iter().find(|d| !d.is_absolute()) {
            return Err((
                "modules.dirs",
                format!(
                    "must hold absolute paths, got {:?}",
                    d.display().to_string()
                ),
            ));
        }
        if let Some(id) = self.disabled.iter().find(|id| !valid_module_id(id)) {
            return Err(("modules.disabled", not_a_module_id(id)));
        }
        for (i, s) in self.order.iter().enumerate() {
            if !valid_stage_id(s) {
                return Err(("modules.order", not_a_stage_id(s)));
            }
            if self.order[..i].contains(s) {
                return Err(("modules.order", format!("names the stage {s:?} twice")));
            }
        }
        for (stage, module) in &self.pin {
            if !valid_stage_id(stage) {
                return Err(("modules.pin", not_a_stage_id(stage)));
            }
            if !valid_module_id(module) {
                return Err(("modules.pin", not_a_module_id(module)));
            }
        }
        Ok(())
    }
}

/// The error text of a string that is not a module id.
pub fn not_a_module_id(id: &str) -> String {
    format!("{id:?} is not a module id (reverse-DNS, such as \"org.example.module\")")
}

/// The error text of a string that is not a stage id.
pub fn not_a_stage_id(id: &str) -> String {
    format!(
        "{id:?} is not a stage id (a lower-case letter, then up to {} of a-z, 0-9 and -)",
        STAGE_ID_MAX - 1
    )
}

/// The state file's `[modules]` table: the choices made at run time, each
/// over the config's.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModulesState {
    /// Modules enabled (`true`) or disabled (`false`) with
    /// `SetModuleEnabled`, by module id, over `[modules] disabled`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub enabled: BTreeMap<String, bool>,
    /// The stage order set with `SetStageOrder` (`None`: the config's).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<Vec<String>>,
    /// The profile set with `SetBackendProfile` (`None`: the config's).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<BackendProfile>,
    /// The pins set with `SetStagePin`, by stage id: a module id, or `""`
    /// for a pin of the config cleared.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub pin: BTreeMap<String, String>,
}

impl ModulesState {
    /// Nothing is set.
    pub fn is_empty(&self) -> bool {
        *self == ModulesState::default()
    }

    /// Whether module `id` is enabled: this state's choice, else not in
    /// `config`'s `disabled` list.
    pub fn enabled(&self, config: &ModulesConfig, id: &str) -> bool {
        self.enabled
            .get(id)
            .copied()
            .unwrap_or_else(|| !config.disabled.iter().any(|d| d == id))
    }

    /// The stage order in force: this state's, else the config's (empty:
    /// the default order).
    pub fn order<'a>(&'a self, config: &'a ModulesConfig) -> &'a [String] {
        self.order.as_deref().unwrap_or(&config.order)
    }

    /// The profile in force.
    pub fn profile(&self, config: &ModulesConfig) -> BackendProfile {
        self.profile.or(config.profile).unwrap_or_default()
    }

    /// The pins in force: the config's, then this state's over them (an
    /// empty module clears the stage's pin).
    pub fn pins(&self, config: &ModulesConfig) -> BTreeMap<String, String> {
        let mut pins = config.pin.clone();
        for (stage, module) in &self.pin {
            if module.is_empty() {
                pins.remove(stage);
            } else {
                pins.insert(stage.clone(), module.clone());
            }
        }
        pins
    }
}

/// A value of a module setting, as the state file keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredValue {
    /// A switch.
    Bool(bool),
    /// An integer.
    Int(i64),
    /// An option id.
    Str(String),
}

impl fmt::Display for StoredValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoredValue::Bool(b) => write!(f, "{b}"),
            StoredValue::Int(i) => write!(f, "{i}"),
            StoredValue::Str(s) => f.write_str(s),
        }
    }
}

impl Serialize for StoredValue {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            StoredValue::Bool(b) => s.serialize_bool(*b),
            StoredValue::Int(i) => s.serialize_i64(*i),
            StoredValue::Str(v) => s.serialize_str(v),
        }
    }
}

impl StoredValue {
    /// The value of a TOML value, if it is of a type a setting takes.
    pub fn from_toml(v: &toml::Value) -> Option<StoredValue> {
        match v {
            toml::Value::Boolean(b) => Some(StoredValue::Bool(*b)),
            toml::Value::Integer(i) => Some(StoredValue::Int(*i)),
            toml::Value::String(s) => Some(StoredValue::Str(s.clone())),
            _ => None,
        }
    }
}

/// The `[settings]` table, leniently: each value of a type a setting takes
/// is kept, by setting id; the others dropped with a warning
/// ([`settings_from`]); never an error.
pub(crate) fn de_settings<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<String, StoredValue>, D::Error> {
    let value = toml::Value::deserialize(d)?;
    Ok(settings_from(&value).0)
}

/// The values of the `[settings]` value, and a warning for each part
/// dropped: a value that is not a boolean, an integer or a string, or a
/// `[settings]` that is not a table.
pub(crate) fn settings_from(value: &toml::Value) -> (BTreeMap<String, StoredValue>, Vec<String>) {
    let mut out = BTreeMap::new();
    let mut warnings = Vec::new();
    let Some(table) = value.as_table() else {
        warnings.push(format!(
            "settings must be a table (found {}): ignored",
            value.type_str()
        ));
        return (out, warnings);
    };
    for (id, v) in table {
        match StoredValue::from_toml(v) {
            Some(v) => {
                out.insert(id.clone(), v);
            }
            None => warnings.push(format!(
                "settings.{id:?} must be a boolean, an integer or a string (found {}): ignored",
                v.type_str()
            )),
        }
    }
    (out, warnings)
}

/// The setting ids of the module settings that revision 8 kept in typed
/// keys of `[effect]` (config) and of the state file, in that order:
/// noise removal, enhancement, relighting, its light, direction and
/// brightness.
pub mod legacy {
    /// `[effect] denoise`, the state's `denoise`.
    pub const DENOISE: &str = "effect.video.denoise";
    /// `[effect] enhance`, the state's `enhance`.
    pub const ENHANCE: &str = "effect.video.enhance";
    /// `[effect] relight`, the state's `relight`.
    pub const RELIGHT: &str = "effect.lighting.relight";
    /// `[effect] relight_light`, the state's `relight_light`.
    pub const LIGHT: &str = "effect.lighting.light";
    /// `[effect] relight_direction`, the state's `relight_direction`.
    pub const DIRECTION: &str = "effect.lighting.direction";
    /// `[effect] relight_brightness`, the state's `relight_brightness`.
    pub const BRIGHTNESS: &str = "effect.lighting.brightness";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_have_their_forms() {
        for ok in [
            "a",
            "denoise",
            "hello-2",
            "x2345678901234567890123456789012",
        ] {
            assert!(valid_stage_id(ok), "{ok}");
        }
        for bad in [
            "",
            "2a",
            "A",
            "a_b",
            "a.b",
            "x23456789012345678901234567890123",
        ] {
            assert!(!valid_stage_id(bad), "{bad}");
        }
        for ok in ["br.com.dias.maxine-vfx", "org.example.hello", "a.b"] {
            assert!(valid_module_id(ok), "{ok}");
        }
        for bad in [
            "", "hello", "org..x", "Org.x", "org.x_y", ".org.x", "org-x.y",
        ] {
            assert!(!valid_module_id(bad), "{bad}");
        }
    }

    #[test]
    fn the_state_overrides_the_config() {
        let config = ModulesConfig {
            disabled: vec!["org.x.off".into()],
            order: vec!["denoise".into()],
            profile: Some(BackendProfile::Battery),
            pin: [
                ("background".to_string(), "org.x.a".to_string()),
                ("relight".to_string(), "org.x.b".to_string()),
            ]
            .into(),
            ..ModulesConfig::default()
        };
        let mut state = ModulesState::default();
        assert!(!state.enabled(&config, "org.x.off"));
        assert!(state.enabled(&config, "org.x.on"));
        assert_eq!(state.order(&config), ["denoise"]);
        assert_eq!(state.profile(&config), BackendProfile::Battery);
        assert_eq!(state.pins(&config).len(), 2);
        state.enabled.insert("org.x.off".into(), true);
        state.enabled.insert("org.x.on".into(), false);
        state.order = Some(Vec::new());
        state.profile = Some(BackendProfile::Quality);
        state.pin.insert("background".into(), "org.x.c".into());
        state.pin.insert("relight".into(), String::new());
        assert!(state.enabled(&config, "org.x.off"));
        assert!(!state.enabled(&config, "org.x.on"));
        assert!(state.order(&config).is_empty());
        assert_eq!(state.profile(&config), BackendProfile::Quality);
        assert_eq!(
            state.pins(&config),
            [("background".to_string(), "org.x.c".to_string())].into()
        );
    }

    #[test]
    fn the_config_table_is_checked() {
        let ok = ModulesConfig {
            dirs: vec!["/srv/modules".into()],
            ..ModulesConfig::default()
        };
        assert!(ok.validate().is_ok());
        let bad = ModulesConfig {
            dirs: vec!["modules".into()],
            ..ModulesConfig::default()
        };
        assert_eq!(bad.validate().unwrap_err().0, "modules.dirs");
        let bad = ModulesConfig {
            order: vec!["denoise".into(), "denoise".into()],
            ..ModulesConfig::default()
        };
        assert!(bad.validate().unwrap_err().1.contains("twice"));
        let bad = ModulesConfig {
            pin: [("background".to_string(), "maxine".to_string())].into(),
            ..ModulesConfig::default()
        };
        assert_eq!(bad.validate().unwrap_err().0, "modules.pin");
    }

    #[test]
    fn settings_values_are_kept_leniently() {
        let v: toml::Value = toml::from_str(
            "\"effect.a.b\" = true\n\"effect.a.c\" = 5\n\"effect.a.d\" = \"warm\"\n\
             \"effect.a.e\" = 1.5\n",
        )
        .unwrap();
        let (values, warnings) = settings_from(&v);
        assert_eq!(values.len(), 3);
        assert_eq!(values["effect.a.c"], StoredValue::Int(5));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("effect.a.e"), "{warnings:?}");
    }
}
