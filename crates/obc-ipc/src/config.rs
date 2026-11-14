//! The daemon's configuration over the bus (revision 10): the config schema
//! `GetConfigSchema` returns, the value `SetConfig` takes and the file list
//! of `GetConfigFiles`, read into a typed model and written back.
//!
//! - `GetConfigSchema() -> (u serial, aa{sv} groups)` ([`ConfigSchema`]):
//!   `serial` is the `ConfigSerial` property's value when the schema was
//!   made; a group is `id` s, `label` s, `settings` aa{sv} (its rows, the
//!   key named as the settings channel names it);
//! - a row: `id` s (the dotted configuration key), `label` s, `description`
//!   s, `type` s, `value`, an optional `default`, `editable` b, `source` s
//!   (the file that sets the value in force, relative to the configuration
//!   directory; `""` for the default), `overridden_by` s (`""` or the file
//!   after the managed one that sets it), `apply` s (`live`, `reload` or
//!   `restart`), `enabled` b, `disabled_reason` s and (revision 22)
//!   `notice` s (a note on the value now, such as a change that waits for
//!   the camera to restart; `""` when none); then by type (bool:
//!   `value`/`default` b; int: `min`, `max`, `step` and `value`/`default`
//!   x, an optional `unit` s; float: the same in d; enum: `options` a(ss)
//!   and `value`/`default` s; string and path: `value`/`default` s; list:
//!   `value`/`default` as);
//! - `SetConfig(s key, v value)`: the value as [`ConfigValue::to_variant`]
//!   writes it; `ResetConfig(s key)`;
//! - `GetConfigFiles() -> (u serial, s dir, a(ssbs) files)`
//!   ([`ConfigFiles`]): each file in load order as `(path, status, managed,
//!   reason)`, status `loaded`, `absent` or `error`;
//! - `ConfigChanged(u serial)` and the `ConfigSerial` property.
//!
//! Decoding is tolerant, as for the settings channel: keys this crate does
//! not know are ignored; a row of a type it does not know, or without its
//! `id`, `type`, `value` or the keys of its type, is dropped alone (a
//! client skips it); a missing common key reads as its default (a missing
//! `editable` as false, a missing `enabled` as true). Integers are read from
//! any D-Bus integer type that fits an `i64`, a float also from an integer.

use zbus::Variant;

use crate::settings::{
    Entries, VariantDict, entries_of, entries_of_dict, fields_of, get_bool, get_int, get_str,
    int_of, list_of, owned, peel, put, str_of, to_owned,
};

/// The first [`crate::INTERFACE_REVISION`] with the configuration methods:
/// `GetConfigSchema`, `SetConfig`, `ResetConfig`, `GetConfigFiles`,
/// `ConfigChanged` and `ConfigSerial`.
pub const CONFIG_REVISION: u32 = 10;

/// The reply of `GetConfigSchema`: `(u serial, aa{sv} groups)`.
pub type ConfigSchemaReply = (u32, Vec<VariantDict>);

/// One file of `GetConfigFiles`: `(s path, s status, b managed, s reason)`.
pub type WireConfigFile = (String, String, bool, String);

/// The reply of `GetConfigFiles`: `(u serial, s dir, a(ssbs) files)`.
pub type ConfigFilesReply = (u32, String, Vec<WireConfigFile>);

/// `apply`: the new value takes effect at once.
pub const APPLY_LIVE: &str = "live";
/// `apply`: at the next pipeline start or device reopen.
pub const APPLY_RELOAD: &str = "reload";
/// `apply`: at the next daemon start.
pub const APPLY_RESTART: &str = "restart";

/// A file's `status`: read and valid.
pub const FILE_LOADED: &str = "loaded";
/// A file's `status`: not there.
pub const FILE_ABSENT: &str = "absent";
/// A file's `status`: it cannot be used (`reason` says why).
pub const FILE_ERROR: &str = "error";

/// A value of a configuration row, and the argument of `SetConfig`.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigValue {
    /// `b`.
    Bool(bool),
    /// `x` (decoded from any D-Bus integer type).
    Int(i64),
    /// `d`.
    Float(f64),
    /// `s`: an enum option, a string or a path.
    Str(String),
    /// `as`.
    List(Vec<String>),
}

impl ConfigValue {
    /// The value as a D-Bus variant: `b`, `x`, `d`, `s` or `as`.
    pub fn to_variant(&self) -> zbus::zvariant::OwnedVariant {
        match self {
            ConfigValue::Bool(b) => owned(Variant::from(*b)),
            ConfigValue::Int(i) => owned(Variant::from(*i)),
            ConfigValue::Float(f) => owned(Variant::from(*f)),
            ConfigValue::Str(s) => owned(Variant::from(s.as_str())),
            ConfigValue::List(items) => {
                let items: Vec<&str> = items.iter().map(String::as_str).collect();
                to_owned(&items)
            }
        }
    }

    /// The value a variant holds, looking through nested variants: `b`; any
    /// integer type whose value fits an `i64`; `d`; `s`; an array of
    /// strings. `None` for anything else (the daemon refuses it as
    /// `InvalidArgument`).
    pub fn from_variant(v: &Variant<'_>) -> Option<ConfigValue> {
        let v = peel(v);
        if let Variant::Bool(b) = v {
            return Some(ConfigValue::Bool(*b));
        }
        if let Some(i) = int_of(v) {
            return Some(ConfigValue::Int(i));
        }
        if let Variant::F64(f) = v {
            return Some(ConfigValue::Float(*f));
        }
        if let Some(s) = str_of(v) {
            return Some(ConfigValue::Str(s.to_string()));
        }
        strings_of(v).map(ConfigValue::List)
    }
}

fn strings_of(v: &Variant<'_>) -> Option<Vec<String>> {
    list_of(v)?
        .iter()
        .map(|s| str_of(s).map(str::to_string))
        .collect()
}

fn float_of(v: &Variant<'_>) -> Option<f64> {
    match peel(v) {
        Variant::F64(f) => Some(*f),
        other => int_of(other).map(|i| i as f64),
    }
}

/// The type of a row, with its range or options.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigKind {
    /// `bool`.
    Bool,
    /// `int`: valid values are `min..=max`, offered in steps of `step`.
    Int {
        min: i64,
        max: i64,
        step: i64,
        unit: Option<String>,
    },
    /// `float`.
    Float {
        min: f64,
        max: f64,
        step: f64,
        unit: Option<String>,
    },
    /// `enum`: one of `options`, each `(value, label)`.
    Enum { options: Vec<(String, String)> },
    /// `string`.
    String,
    /// `path`: an absolute path, empty when not set.
    Path,
    /// `list`: a list of strings.
    List,
}

impl ConfigKind {
    /// The `type` key.
    pub fn type_name(&self) -> &'static str {
        match self {
            ConfigKind::Bool => "bool",
            ConfigKind::Int { .. } => "int",
            ConfigKind::Float { .. } => "float",
            ConfigKind::Enum { .. } => "enum",
            ConfigKind::String => "string",
            ConfigKind::Path => "path",
            ConfigKind::List => "list",
        }
    }

    /// The value of `v` for this type, `None` when it is of another one.
    fn value_of(&self, v: &Variant<'_>) -> Option<ConfigValue> {
        match self {
            ConfigKind::Bool => match peel(v) {
                Variant::Bool(b) => Some(ConfigValue::Bool(*b)),
                _ => None,
            },
            ConfigKind::Int { .. } => int_of(v).map(ConfigValue::Int),
            ConfigKind::Float { .. } => float_of(v).map(ConfigValue::Float),
            ConfigKind::Enum { .. } | ConfigKind::String | ConfigKind::Path => {
                str_of(v).map(|s| ConfigValue::Str(s.to_string()))
            }
            ConfigKind::List => strings_of(v).map(ConfigValue::List),
        }
    }

    /// The kind a row dictionary describes, `None` for an unknown type or
    /// a key of its type missing.
    fn from_entries(e: &Entries<'_, '_>) -> Option<ConfigKind> {
        let unit = get_str(e, "unit").map(str::to_string);
        Some(match get_str(e, "type")? {
            "bool" => ConfigKind::Bool,
            "int" => ConfigKind::Int {
                min: get_int(e, "min")?,
                max: get_int(e, "max")?,
                step: get_int(e, "step")?,
                unit,
            },
            "float" => ConfigKind::Float {
                min: float_of(e.get("min")?)?,
                max: float_of(e.get("max")?)?,
                step: float_of(e.get("step")?)?,
                unit,
            },
            "enum" => ConfigKind::Enum {
                options: list_of(e.get("options")?)?
                    .iter()
                    .filter_map(|o| match fields_of(o)? {
                        [v, l] => Some((str_of(v)?.to_string(), str_of(l)?.to_string())),
                        _ => None,
                    })
                    .collect(),
            },
            "string" => ConfigKind::String,
            "path" => ConfigKind::Path,
            "list" => ConfigKind::List,
            _ => return None,
        })
    }
}

/// One row of the config schema.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigRow {
    /// The dotted configuration key (`output.format`,
    /// `modules.enabled.<id>`).
    pub id: String,
    pub label: String,
    /// Help text.
    pub description: String,
    pub kind: ConfigKind,
    /// The value in force.
    pub value: ConfigValue,
    /// The built-in default, `None` when there is none.
    pub default: Option<ConfigValue>,
    /// The daemon accepts `SetConfig` and `ResetConfig` for it.
    pub editable: bool,
    /// The file that sets the value in force, `""` for the default.
    pub source: String,
    /// The file after the managed one that sets it, `""` when none.
    pub overridden_by: String,
    /// [`APPLY_LIVE`], [`APPLY_RELOAD`] or [`APPLY_RESTART`].
    pub apply: String,
    /// It can be changed now (false when overridden).
    pub enabled: bool,
    /// Why not, when not enabled; empty when enabled.
    pub disabled_reason: String,
    /// A note on the value now (revision 22), such as a change that waits
    /// for the camera to restart; empty when none, and from an older
    /// daemon.
    pub notice: String,
}

fn put_value(d: &mut VariantDict, key: &str, v: &ConfigValue) {
    d.insert(key.to_string(), v.to_variant());
}

impl ConfigRow {
    /// The row dictionary.
    pub fn to_dict(&self) -> VariantDict {
        let mut d = VariantDict::new();
        put(&mut d, "id", Variant::from(self.id.as_str()));
        put(&mut d, "label", Variant::from(self.label.as_str()));
        put(
            &mut d,
            "description",
            Variant::from(self.description.as_str()),
        );
        put(&mut d, "type", Variant::from(self.kind.type_name()));
        put_value(&mut d, "value", &self.value);
        if let Some(default) = &self.default {
            put_value(&mut d, "default", default);
        }
        put(&mut d, "editable", Variant::from(self.editable));
        put(&mut d, "source", Variant::from(self.source.as_str()));
        put(
            &mut d,
            "overridden_by",
            Variant::from(self.overridden_by.as_str()),
        );
        put(&mut d, "apply", Variant::from(self.apply.as_str()));
        put(&mut d, "enabled", Variant::from(self.enabled));
        put(
            &mut d,
            "disabled_reason",
            Variant::from(self.disabled_reason.as_str()),
        );
        put(&mut d, "notice", Variant::from(self.notice.as_str()));
        match &self.kind {
            ConfigKind::Int {
                min,
                max,
                step,
                unit,
            } => {
                put(&mut d, "min", Variant::from(*min));
                put(&mut d, "max", Variant::from(*max));
                put(&mut d, "step", Variant::from(*step));
                if let Some(unit) = unit {
                    put(&mut d, "unit", Variant::from(unit.as_str()));
                }
            }
            ConfigKind::Float {
                min,
                max,
                step,
                unit,
            } => {
                put(&mut d, "min", Variant::from(*min));
                put(&mut d, "max", Variant::from(*max));
                put(&mut d, "step", Variant::from(*step));
                if let Some(unit) = unit {
                    put(&mut d, "unit", Variant::from(unit.as_str()));
                }
            }
            ConfigKind::Enum { options } => {
                let options: Vec<(&str, &str)> = options
                    .iter()
                    .map(|(v, l)| (v.as_str(), l.as_str()))
                    .collect();
                d.insert("options".into(), to_owned(&options));
            }
            ConfigKind::Bool | ConfigKind::String | ConfigKind::Path | ConfigKind::List => {}
        }
        d
    }

    /// The row a dictionary describes, or `None` when it is of an unknown
    /// type, or its `id` (empty or missing), `type`, `value` or a key of
    /// its type is missing or of the wrong type. A `default` of the wrong
    /// type reads as none; other missing keys as their defaults.
    pub fn from_dict(dict: &VariantDict) -> Option<ConfigRow> {
        ConfigRow::from_entries(&entries_of_dict(dict))
    }

    fn from_entries(e: &Entries<'_, '_>) -> Option<ConfigRow> {
        let id = get_str(e, "id").filter(|id| !id.is_empty())?;
        let kind = ConfigKind::from_entries(e)?;
        let value = kind.value_of(e.get("value")?)?;
        let default = e.get("default").and_then(|v| kind.value_of(v));
        let text = |key: &str| get_str(e, key).unwrap_or_default().to_string();
        Some(ConfigRow {
            id: id.to_string(),
            label: text("label"),
            description: text("description"),
            kind,
            value,
            default,
            editable: get_bool(e, "editable").unwrap_or(false),
            source: text("source"),
            overridden_by: text("overridden_by"),
            apply: text("apply"),
            enabled: get_bool(e, "enabled").unwrap_or(true),
            disabled_reason: text("disabled_reason"),
            notice: text("notice"),
        })
    }
}

/// A group of rows.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigGroup {
    /// `camera`, `output`, `loopback`, `lifecycle`, `effect`, `pipewire` or
    /// `modules`.
    pub id: String,
    pub label: String,
    pub rows: Vec<ConfigRow>,
}

impl ConfigGroup {
    /// The group dictionary: `id`, `label`, `settings` (aa{sv}).
    pub fn to_dict(&self) -> VariantDict {
        let mut d = VariantDict::new();
        put(&mut d, "id", Variant::from(self.id.as_str()));
        put(&mut d, "label", Variant::from(self.label.as_str()));
        let rows: Vec<VariantDict> = self.rows.iter().map(ConfigRow::to_dict).collect();
        d.insert("settings".into(), to_owned(&rows));
        d
    }

    /// The group a dictionary describes, or `None` when its `id`, `label`
    /// or `settings` (an array) is missing or of the wrong type. A row that
    /// does not decode is dropped alone.
    pub fn from_dict(dict: &VariantDict) -> Option<ConfigGroup> {
        let e = entries_of_dict(dict);
        let rows = list_of(e.get("settings")?)?
            .iter()
            .filter_map(|r| ConfigRow::from_entries(&entries_of(r)?))
            .collect();
        Some(ConfigGroup {
            id: get_str(&e, "id")?.to_string(),
            label: get_str(&e, "label")?.to_string(),
            rows,
        })
    }
}

/// The reply of `GetConfigSchema`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfigSchema {
    /// `ConfigSerial` when the schema was made.
    pub serial: u32,
    pub groups: Vec<ConfigGroup>,
}

impl ConfigSchema {
    /// The reply on the wire.
    pub fn to_wire(&self) -> ConfigSchemaReply {
        (
            self.serial,
            self.groups.iter().map(ConfigGroup::to_dict).collect(),
        )
    }

    /// The schema of a reply. Never fails: a group that does not decode is
    /// dropped alone.
    pub fn from_wire(reply: &ConfigSchemaReply) -> ConfigSchema {
        ConfigSchema {
            serial: reply.0,
            groups: reply.1.iter().filter_map(ConfigGroup::from_dict).collect(),
        }
    }

    /// The row `id`, in any group.
    pub fn find(&self, id: &str) -> Option<&ConfigRow> {
        self.groups
            .iter()
            .flat_map(|g| g.rows.iter())
            .find(|r| r.id == id)
    }
}

/// One file of the configuration set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigFileEntry {
    /// Relative to the configuration directory.
    pub path: String,
    /// [`FILE_LOADED`], [`FILE_ABSENT`] or [`FILE_ERROR`].
    pub status: String,
    /// The daemon writes it (`config.d/50-panel.toml`).
    pub managed: bool,
    /// Why it cannot be used, empty when it can.
    pub reason: String,
}

/// The reply of `GetConfigFiles`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigFiles {
    /// `ConfigSerial` when the list was made.
    pub serial: u32,
    /// The configuration directory (absolute).
    pub dir: String,
    /// In load order.
    pub files: Vec<ConfigFileEntry>,
}

impl ConfigFiles {
    /// The reply on the wire.
    pub fn to_wire(&self) -> ConfigFilesReply {
        (
            self.serial,
            self.dir.clone(),
            self.files
                .iter()
                .map(|f| {
                    (
                        f.path.clone(),
                        f.status.clone(),
                        f.managed,
                        f.reason.clone(),
                    )
                })
                .collect(),
        )
    }

    /// The list of a reply.
    pub fn from_wire(reply: &ConfigFilesReply) -> ConfigFiles {
        let (serial, dir, files) = reply;
        ConfigFiles {
            serial: *serial,
            dir: dir.clone(),
            files: files
                .iter()
                .map(|(path, status, managed, reason)| ConfigFileEntry {
                    path: path.clone(),
                    status: status.clone(),
                    managed: *managed,
                    reason: reason.clone(),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{SettingValue, VariantDict};
    use zbus::wire::serialized::Context;
    use zbus::wire::{LE, to_bytes};

    fn over_the_wire<T>(value: &T) -> T
    where
        T: serde::Serialize + zbus::Type + for<'de> serde::Deserialize<'de>,
    {
        let bytes = to_bytes(Context::new(LE, 0), value).unwrap();
        bytes.deserialize::<T>().unwrap().0
    }

    fn row(
        id: &str,
        kind: ConfigKind,
        value: ConfigValue,
        default: Option<ConfigValue>,
    ) -> ConfigRow {
        ConfigRow {
            id: id.into(),
            label: "A label".into(),
            description: "Some help.".into(),
            kind,
            value,
            default,
            editable: true,
            source: String::new(),
            overridden_by: String::new(),
            apply: APPLY_RELOAD.into(),
            enabled: true,
            disabled_reason: String::new(),
            notice: String::new(),
        }
    }

    fn schema() -> ConfigSchema {
        let mut aspect = row(
            "output.aspect",
            ConfigKind::Enum {
                options: vec![
                    ("crop".into(), "Crop to fill".into()),
                    ("letterbox".into(), "Letterbox".into()),
                ],
            },
            ConfigValue::Str("letterbox".into()),
            Some(ConfigValue::Str("crop".into())),
        );
        aspect.source = "config.d/90-local.toml".into();
        aspect.overridden_by = "config.d/90-local.toml".into();
        aspect.enabled = false;
        aspect.disabled_reason = "Overridden by config.d/90-local.toml".into();
        aspect.notice = "Applies when the camera restarts".into();
        let mut preference = row(
            "camera.preference",
            ConfigKind::List,
            ConfigValue::List(vec!["Brio".into(), "046d:0944".into()]),
            Some(ConfigValue::List(Vec::new())),
        );
        preference.editable = false;
        preference.apply = APPLY_LIVE.into();
        ConfigSchema {
            serial: 7,
            groups: vec![
                ConfigGroup {
                    id: "camera".into(),
                    label: "Camera".into(),
                    rows: vec![
                        preference,
                        row(
                            "camera.fixed_framerate",
                            ConfigKind::Bool,
                            ConfigValue::Bool(true),
                            Some(ConfigValue::Bool(false)),
                        ),
                    ],
                },
                ConfigGroup {
                    id: "output".into(),
                    label: "Output".into(),
                    rows: vec![
                        aspect,
                        row(
                            "output.hold_timeout_s",
                            ConfigKind::Float {
                                min: 0.1,
                                max: 60.0,
                                step: 0.1,
                                unit: Some("s".into()),
                            },
                            ConfigValue::Float(2.5),
                            Some(ConfigValue::Float(3.0)),
                        ),
                        row(
                            "output.input_gap_ms",
                            ConfigKind::Int {
                                min: 1,
                                max: 60_000,
                                step: 1,
                                unit: None,
                            },
                            ConfigValue::Int(500),
                            Some(ConfigValue::Int(500)),
                        ),
                        row(
                            "output.mode",
                            ConfigKind::String,
                            ConfigValue::Str("1920x1080@30".into()),
                            Some(ConfigValue::Str("1920x1080@30".into())),
                        ),
                    ],
                },
                ConfigGroup {
                    id: "loopback".into(),
                    label: "Virtual camera device".into(),
                    rows: vec![row(
                        "loopback.device",
                        ConfigKind::Path,
                        ConfigValue::Str(String::new()),
                        None,
                    )],
                },
            ],
        }
    }

    #[test]
    fn the_schema_round_trips_in_memory_and_over_the_wire() {
        let s = schema();
        assert_eq!(ConfigSchema::from_wire(&s.to_wire()), s);
        assert_eq!(ConfigSchema::from_wire(&over_the_wire(&s.to_wire())), s);
        assert_eq!(
            s.find("output.aspect").unwrap().disabled_reason,
            "Overridden by config.d/90-local.toml"
        );
    }

    #[test]
    fn the_encoded_shapes_are_the_contract() {
        let s = schema();
        let (serial, groups) = s.to_wire();
        assert_eq!(serial, 7);
        let e = entries_of_dict(&groups[1]);
        let rows = list_of(e.get("settings").unwrap()).unwrap();
        let float = entries_of(&rows[1]).unwrap();
        assert_eq!(
            float.get("type").unwrap().value_signature().to_string(),
            "s"
        );
        for (key, sig) in [
            ("value", "d"),
            ("default", "d"),
            ("min", "d"),
            ("max", "d"),
            ("step", "d"),
            ("unit", "s"),
            ("editable", "b"),
            ("source", "s"),
            ("overridden_by", "s"),
            ("apply", "s"),
            ("enabled", "b"),
            ("disabled_reason", "s"),
            ("notice", "s"),
            ("description", "s"),
        ] {
            assert_eq!(
                float.get(key).unwrap().value_signature().to_string(),
                sig,
                "{key}"
            );
        }
        let int = entries_of(&rows[2]).unwrap();
        assert_eq!(int.get("min").unwrap().value_signature().to_string(), "x");
        assert!(!int.contains_key("unit"));
        let en = entries_of(&rows[0]).unwrap();
        assert_eq!(
            en.get("options").unwrap().value_signature().to_string(),
            "a(ss)"
        );
        let camera = entries_of_dict(&groups[0]);
        let list = entries_of(&list_of(camera.get("settings").unwrap()).unwrap()[0]).unwrap();
        assert_eq!(
            list.get("value").unwrap().value_signature().to_string(),
            "as"
        );
        let path = entries_of_dict(&groups[2]);
        let path = entries_of(&list_of(path.get("settings").unwrap()).unwrap()[0]).unwrap();
        assert!(!path.contains_key("default"));
    }

    #[test]
    fn unknown_types_and_malformed_rows_are_dropped_alone() {
        let good = schema().groups[1].rows[2].to_dict();
        let mut unknown = good.clone();
        put(&mut unknown, "type", Variant::from("colour"));
        let mut no_value = good.clone();
        no_value.remove("value");
        let mut no_min = good.clone();
        no_min.remove("min");
        let mut wrong_value = good.clone();
        put(&mut wrong_value, "value", Variant::from("five"));
        let mut extra = good.clone();
        put(&mut extra, "future_key", Variant::from(1u8));
        put(&mut extra, "min", Variant::from(1u32));
        let mut bad_default = good.clone();
        put(&mut bad_default, "default", Variant::from(true));
        let mut sparse = VariantDict::new();
        put(&mut sparse, "id", Variant::from("pipewire.enabled"));
        put(&mut sparse, "type", Variant::from("bool"));
        put(&mut sparse, "value", Variant::from(true));
        let rows = vec![
            good,
            unknown,
            no_value,
            no_min,
            wrong_value,
            extra,
            bad_default,
            sparse,
        ];
        let mut group = VariantDict::new();
        put(&mut group, "id", Variant::from("output"));
        put(&mut group, "label", Variant::from("Output"));
        group.insert("settings".into(), to_owned(&rows));
        let mut broken_group = VariantDict::new();
        put(&mut broken_group, "id", Variant::from("x"));
        let s = ConfigSchema::from_wire(&over_the_wire(&(3u32, vec![group, broken_group])));
        assert_eq!(s.groups.len(), 1);
        let rows = &s.groups[0].rows;
        assert_eq!(rows.len(), 4, "{rows:?}");
        assert_eq!(rows[1].kind, rows[0].kind);
        assert_eq!(rows[2].default, None);
        assert_eq!(rows[3].value, ConfigValue::Bool(true));
        assert!(rows[3].enabled && !rows[3].editable);
        assert_eq!(rows[3].source, "");
        // An older daemon sends no notice: none.
        assert_eq!(rows[3].notice, "");
    }

    #[test]
    fn values_decode_from_their_variants() {
        for v in [
            ConfigValue::Bool(true),
            ConfigValue::Int(-3),
            ConfigValue::Float(0.25),
            ConfigValue::Str("letterbox".into()),
            ConfigValue::List(vec!["a".into(), "b".into()]),
            ConfigValue::List(Vec::new()),
        ] {
            let wire = over_the_wire(&v.to_variant());
            assert_eq!(ConfigValue::from_variant(&wire), Some(v));
        }
        assert_eq!(
            ConfigValue::from_variant(&Variant::from(7u16)),
            Some(ConfigValue::Int(7))
        );
        let nested = Variant::Variant(Box::new(Variant::from(2.0f64)));
        assert_eq!(
            ConfigValue::from_variant(&nested),
            Some(ConfigValue::Float(2.0))
        );
        assert_eq!(ConfigValue::from_variant(&Variant::from(u64::MAX)), None);
        // An array of something else is no list.
        let ints = SettingValue::List(vec![SettingValue::Int(1)]).to_variant();
        assert_eq!(ConfigValue::from_variant(&ints), None);
        // A float row reads an integer too.
        let mut d = schema().groups[1].rows[1].to_dict();
        put(&mut d, "value", Variant::from(2i32));
        assert_eq!(
            ConfigRow::from_dict(&d).unwrap().value,
            ConfigValue::Float(2.0)
        );
    }

    #[test]
    fn the_file_list_round_trips() {
        let files = ConfigFiles {
            serial: 4,
            dir: "/home/u/.config/omarchy-broadcast".into(),
            files: vec![
                ConfigFileEntry {
                    path: "config.toml".into(),
                    status: FILE_LOADED.into(),
                    managed: false,
                    reason: String::new(),
                },
                ConfigFileEntry {
                    path: "config.d/50-panel.toml".into(),
                    status: FILE_ABSENT.into(),
                    managed: true,
                    reason: String::new(),
                },
                ConfigFileEntry {
                    path: "config.d/90-x.toml".into(),
                    status: FILE_ERROR.into(),
                    managed: false,
                    reason: "line 3, column 1: unknown field".into(),
                },
            ],
        };
        let wire = over_the_wire(&files.to_wire());
        assert_eq!(ConfigFiles::from_wire(&wire), files);
        assert_eq!(
            <ConfigFilesReply as zbus::zvariant::Type>::SIGNATURE.to_string(),
            "(usa(ssbs))"
        );
    }
}
