//! The configuration schema: every configuration key the daemon publishes,
//! self-described as the settings are (label, help text, type, range or
//! options, default), with where its value comes from and whether the
//! daemon changes it ([`config_schema`]).
//!
//! The core keys are a table fixed at build time ([`CORE_KEYS`]); the rows
//! of the module system are made from what the module registry found
//! ([`RegistryFacts`]): `modules.enabled.<id>` for each installed module and
//! `modules.pin.<stage>` for each stage a module offers. The daemon turns
//! the rows into the D-Bus dictionaries of `GetConfigSchema`.
//!
//! A row is `editable` when the daemon accepts `SetConfig` for it; the
//! others are shown read only. `apply` says when a new value takes effect:
//! at once (`live`), at the next pipeline start or device reopen
//! (`reload`), or at the next daemon start (`restart`). A row that a file
//! after the managed one sets is overridden: not enabled, with the reason
//! "Overridden by <file>".

use crate::EffectOnFailure;
use crate::config::{Config, EffectConfig};
use crate::config_d::{ConfigSet, table_get};
use crate::modules::{FRAME_GENERATION_STAGE, valid_module_id, valid_stage_id};

/// The longest description of a row, in bytes.
pub const DESCRIPTION_MAX: usize = 160;

/// A value of a configuration row: its value or default, the argument of
/// `SetConfig`.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    /// An enum option, a string or a path.
    Str(String),
    /// A list of strings.
    List(Vec<String>),
}

/// When a new value of a key takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Apply {
    /// At once.
    Live,
    /// At the next pipeline start or device reopen (once no application
    /// uses the virtual camera, for the output).
    Reload,
    /// At the next daemon start.
    Restart,
}

impl Apply {
    /// The wire name: `live`, `reload` or `restart`.
    pub fn as_str(self) -> &'static str {
        match self {
            Apply::Live => "live",
            Apply::Reload => "reload",
            Apply::Restart => "restart",
        }
    }
}

/// The type of a row, with its range or options.
#[derive(Debug, Clone, PartialEq)]
pub enum RowKind {
    Bool,
    /// Valid values are `min..=max`; `step` is the step a control offers.
    Int {
        min: i64,
        max: i64,
        step: i64,
        unit: Option<String>,
    },
    Float {
        min: f64,
        max: f64,
        step: f64,
        unit: Option<String>,
    },
    /// One of `options`, each `(value, label)`.
    Enum {
        options: Vec<(String, String)>,
    },
    String,
    /// An absolute path; empty when not set.
    Path,
    /// A list of strings.
    List,
}

impl RowKind {
    /// The wire name: `bool`, `int`, `float`, `enum`, `string`, `path`,
    /// `list`.
    pub fn type_name(&self) -> &'static str {
        match self {
            RowKind::Bool => "bool",
            RowKind::Int { .. } => "int",
            RowKind::Float { .. } => "float",
            RowKind::Enum { .. } => "enum",
            RowKind::String => "string",
            RowKind::Path => "path",
            RowKind::List => "list",
        }
    }
}

/// The type of a core key, as the table spells it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SpecKind {
    Bool,
    Int {
        min: i64,
        max: i64,
        step: i64,
        unit: &'static str,
    },
    Float {
        min: f64,
        max: f64,
        step: f64,
        unit: &'static str,
    },
    Enum(&'static [(&'static str, &'static str)]),
    /// One of the options the daemon computes at run time
    /// ([`RegistryFacts::output_modes`] for `output.mode`,
    /// [`RegistryFacts::on_failure_options`] for `effect.on_failure`).
    Runtime,
    String,
    Path,
    List,
}

impl SpecKind {
    /// The row type, the options of a [`SpecKind::Runtime`] key `key` from
    /// `facts` (or `config`, for those it can tell when `facts` has none).
    fn row_kind_of(self, key: &str, facts: &RegistryFacts, config: Option<&Config>) -> RowKind {
        match self {
            SpecKind::Runtime => RowKind::Enum {
                options: runtime_options(key, facts, config),
            },
            other => other.row_kind(),
        }
    }

    fn row_kind(self) -> RowKind {
        let unit = |u: &str| (!u.is_empty()).then(|| u.to_string());
        match self {
            SpecKind::Bool => RowKind::Bool,
            SpecKind::Int {
                min,
                max,
                step,
                unit: u,
            } => RowKind::Int {
                min,
                max,
                step,
                unit: unit(u),
            },
            SpecKind::Float {
                min,
                max,
                step,
                unit: u,
            } => RowKind::Float {
                min,
                max,
                step,
                unit: unit(u),
            },
            SpecKind::Enum(options) => RowKind::Enum {
                options: options
                    .iter()
                    .map(|(v, l)| (v.to_string(), l.to_string()))
                    .collect(),
            },
            SpecKind::Runtime => RowKind::Enum {
                options: Vec::new(),
            },
            SpecKind::String => RowKind::String,
            SpecKind::Path => RowKind::Path,
            SpecKind::List => RowKind::List,
        }
    }
}

/// One core key of the configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeySpec {
    /// The dotted key.
    pub key: &'static str,
    /// The id of its group ([`GROUPS`]).
    pub group: &'static str,
    pub label: &'static str,
    /// Help text, at most [`DESCRIPTION_MAX`] bytes.
    pub description: &'static str,
    pub kind: SpecKind,
    pub editable: bool,
    pub apply: Apply,
}

/// The groups of the schema, `(id, label)`, in order.
pub const GROUPS: [(&str, &str); 7] = [
    ("camera", "Camera"),
    ("output", "Output"),
    ("loopback", "Virtual camera device"),
    ("lifecycle", "Start and stop"),
    ("effect", "Effect defaults"),
    ("pipewire", "PipeWire output"),
    ("modules", "Modules"),
];

/// The core keys, in display order within each group.
pub const CORE_KEYS: &[KeySpec] = &[
    KeySpec {
        key: "camera.preference",
        group: "camera",
        label: "Preferred cameras",
        description: "Cameras to use first: name substrings, vid:pid or device paths. Others follow in discovery order.",
        kind: SpecKind::List,
        editable: false,
        apply: Apply::Live,
    },
    KeySpec {
        key: "camera.mode",
        group: "camera",
        label: "Camera mode",
        description: "The mode asked of the camera, [FORMAT:]WxH@FPS, such as 1920x1080@30.",
        kind: SpecKind::String,
        editable: false,
        apply: Apply::Live,
    },
    KeySpec {
        key: "camera.fixed_framerate",
        group: "camera",
        label: "Fixed frame rate",
        description: "Keep the camera's exposure short enough that it cannot lower the frame rate in a dark room.",
        kind: SpecKind::Bool,
        editable: true,
        apply: Apply::Reload,
    },
    KeySpec {
        key: "output.mode",
        group: "output",
        label: "Output size and rate",
        description: "Size and frame rate of the virtual camera. A generated rate doubles the camera's with frame generation.",
        kind: SpecKind::Runtime,
        editable: true,
        apply: Apply::Reload,
    },
    KeySpec {
        key: "output.format",
        group: "output",
        label: "Pixel format",
        description: "The pixel format the virtual camera delivers.",
        kind: SpecKind::Enum(&[("nv12", "NV12"), ("i420", "I420")]),
        editable: true,
        apply: Apply::Reload,
    },
    KeySpec {
        key: "output.aspect",
        group: "output",
        label: "Aspect ratio",
        description: "How a camera image of another aspect ratio is fitted: cropped to fill the frame, or scaled whole with black borders.",
        kind: SpecKind::Enum(&[("crop", "Crop to fill"), ("letterbox", "Letterbox")]),
        editable: true,
        apply: Apply::Reload,
    },
    KeySpec {
        key: "output.hold_timeout_s",
        group: "output",
        label: "Hold on camera switch",
        description: "How long a camera switch keeps showing the last frame before the placeholder.",
        kind: SpecKind::Float {
            min: 0.1,
            max: crate::config::limits::MAX_HOLD_TIMEOUT_S,
            step: 0.1,
            unit: "s",
        },
        editable: true,
        apply: Apply::Reload,
    },
    KeySpec {
        key: "output.input_gap_ms",
        group: "output",
        label: "Input gap",
        description: "How long the camera may send nothing before the placeholder replaces the repeated last frame.",
        kind: SpecKind::Int {
            min: 1,
            max: crate::config::limits::MAX_INPUT_GAP_MS as i64,
            step: 1,
            unit: "ms",
        },
        editable: true,
        apply: Apply::Reload,
    },
    KeySpec {
        key: "loopback.label",
        group: "loopback",
        label: "Device label",
        description: "The card label of the v4l2loopback device that receives the output.",
        kind: SpecKind::String,
        editable: false,
        apply: Apply::Reload,
    },
    KeySpec {
        key: "loopback.device",
        group: "loopback",
        label: "Device path",
        description: "An explicit device node for the output, used instead of the label.",
        kind: SpecKind::Path,
        editable: false,
        apply: Apply::Reload,
    },
    KeySpec {
        key: "lifecycle.idle_grace_s",
        group: "lifecycle",
        label: "Idle grace",
        description: "How long the camera keeps running after the last application stops using the virtual camera.",
        kind: SpecKind::Float {
            min: 0.0,
            max: crate::config::limits::MAX_IDLE_GRACE_S,
            step: 0.5,
            unit: "s",
        },
        editable: true,
        apply: Apply::Live,
    },
    KeySpec {
        key: "lifecycle.start_debounce_ms",
        group: "lifecycle",
        label: "Start delay",
        description: "How long an application must use the virtual camera before the camera starts.",
        kind: SpecKind::Int {
            min: 0,
            max: crate::config::limits::MAX_START_DEBOUNCE_MS as i64,
            step: 50,
            unit: "ms",
        },
        editable: true,
        apply: Apply::Live,
    },
    KeySpec {
        key: "effect.mode",
        group: "effect",
        label: "Background effect",
        description: RUNTIME_NOTE_MODE,
        kind: SpecKind::Enum(&[("none", "None"), ("blur", "Blur"), ("replace", "Replace")]),
        editable: false,
        apply: Apply::Live,
    },
    KeySpec {
        key: "effect.blur_strength",
        group: "effect",
        label: "Blur strength",
        description: "The default strength of the background blur, 0 to 1. The panel's choices override it.",
        kind: SpecKind::Float {
            min: crate::config::limits::MIN_BLUR_STRENGTH,
            max: crate::config::limits::MAX_BLUR_STRENGTH,
            step: 0.05,
            unit: "",
        },
        editable: false,
        apply: Apply::Live,
    },
    KeySpec {
        key: "effect.background_image",
        group: "effect",
        label: "Background image",
        description: "The default image behind the person with Replace. The panel's choices override it.",
        kind: SpecKind::Path,
        editable: false,
        apply: Apply::Live,
    },
    KeySpec {
        key: "effect.denoise",
        group: "effect",
        label: "Noise removal",
        description: "Whether video noise removal is on by default. The panel's choices override it.",
        kind: SpecKind::Bool,
        editable: false,
        apply: Apply::Live,
    },
    KeySpec {
        key: "effect.enhance",
        group: "effect",
        label: "Enhance low resolution",
        description: "Whether a low-resolution camera is upscaled by default. The panel's choices override it.",
        kind: SpecKind::Bool,
        editable: false,
        apply: Apply::Live,
    },
    KeySpec {
        key: "effect.relight",
        group: "effect",
        label: "Relighting",
        description: "Whether the person is relit by default. The panel's choices override it.",
        kind: SpecKind::Bool,
        editable: false,
        apply: Apply::Live,
    },
    KeySpec {
        key: "effect.relight_light",
        group: "effect",
        label: "Relighting colour",
        description: "The default colour of the relighting light. The panel's choices override it.",
        kind: SpecKind::Enum(&[("warm", "Warm"), ("neutral", "Neutral"), ("cool", "Cool")]),
        editable: false,
        apply: Apply::Live,
    },
    KeySpec {
        key: "effect.relight_direction",
        group: "effect",
        label: "Relighting direction",
        description: "The default direction of the relighting light, -90 to 90 degrees. The panel's choices override it.",
        kind: SpecKind::Int {
            min: crate::config::limits::MIN_RELIGHT_DIRECTION as i64,
            max: crate::config::limits::MAX_RELIGHT_DIRECTION as i64,
            step: crate::config::limits::RELIGHT_STEP as i64,
            unit: "°",
        },
        editable: false,
        apply: Apply::Live,
    },
    KeySpec {
        key: "effect.relight_brightness",
        group: "effect",
        label: "Relighting brightness",
        description: "The default brightness of the relit person, 50 to 200 percent. The panel's choices override it.",
        kind: SpecKind::Int {
            min: crate::config::limits::MIN_RELIGHT_BRIGHTNESS as i64,
            max: crate::config::limits::MAX_RELIGHT_BRIGHTNESS as i64,
            step: crate::config::limits::RELIGHT_STEP as i64,
            unit: "%",
        },
        editable: false,
        apply: Apply::Live,
    },
    KeySpec {
        key: "effect.on_failure",
        group: "effect",
        label: "When an effect can't run",
        description: "Hide the camera with black frames or your image, or show the camera without the effect.",
        kind: SpecKind::Runtime,
        editable: true,
        apply: Apply::Live,
    },
    KeySpec {
        key: "effect.fallback_image",
        group: "effect",
        label: "Fallback image",
        description: "The image that hides the camera when an effect can't run, with Hide the camera: image. Set it in the configuration file.",
        kind: SpecKind::Path,
        editable: false,
        apply: Apply::Live,
    },
    KeySpec {
        key: "effect.maxine_root",
        group: "effect",
        label: "Maxine SDK directory",
        description: "The NVIDIA Maxine Video Effects SDK root. MAXINE_VFX_ROOT, when set, takes precedence.",
        kind: SpecKind::Path,
        editable: false,
        apply: Apply::Restart,
    },
    KeySpec {
        key: "pipewire.enabled",
        group: "pipewire",
        label: "PipeWire video source",
        description: "Also offer the output as a PipeWire video source, next to the virtual camera device.",
        kind: SpecKind::Bool,
        editable: true,
        apply: Apply::Reload,
    },
    KeySpec {
        key: "pipewire.offer_nv12",
        group: "pipewire",
        label: "Offer NV12",
        description: "Offer NV12 on the PipeWire source. Turn off for OBS Studio, which mis-sizes NV12 rows.",
        kind: SpecKind::Bool,
        editable: true,
        apply: Apply::Reload,
    },
    KeySpec {
        key: "modules.profile",
        group: "modules",
        label: "Backend profile",
        description: "Which module provides a stage when several can: the best quality, or the least power.",
        kind: SpecKind::Enum(&[("quality", "Quality"), ("battery", "Battery")]),
        editable: true,
        apply: Apply::Reload,
    },
    KeySpec {
        key: "modules.order",
        group: "modules",
        label: "Stage order",
        description: "The order of the effect stages, by stage id; empty for the default order. Change it in the Order section.",
        kind: SpecKind::List,
        editable: false,
        apply: Apply::Reload,
    },
    KeySpec {
        key: "modules.dirs",
        group: "modules",
        label: "Module directories",
        description: "More directories of module manifests, read after the system's and the user's.",
        kind: SpecKind::List,
        editable: false,
        apply: Apply::Reload,
    },
    KeySpec {
        key: "modules.disabled",
        group: "modules",
        label: "Disabled modules",
        description: "Module ids that are not loaded. A module's own switch overrides this list.",
        kind: SpecKind::List,
        editable: false,
        apply: Apply::Reload,
    },
];

const RUNTIME_NOTE_MODE: &str =
    "The default background effect: none, blur or replace. The panel's choices override it.";

/// The key whose options are the output modes of [`RegistryFacts`].
pub const OUTPUT_MODE: &str = "output.mode";
/// The key whose options are [`RegistryFacts::on_failure_options`].
pub const ON_FAILURE: &str = "effect.on_failure";

/// The options of the [`SpecKind::Runtime`] key `key`. For
/// `effect.on_failure`, those of `facts`, else those of `config` (else of
/// the defaults: black and none).
fn runtime_options(
    key: &str,
    facts: &RegistryFacts,
    config: Option<&Config>,
) -> Vec<(String, String)> {
    match key {
        OUTPUT_MODE => facts.output_modes.clone(),
        ON_FAILURE if !facts.on_failure_options.is_empty() => facts.on_failure_options.clone(),
        ON_FAILURE => on_failure_options(&config.map(|c| c.effect.clone()).unwrap_or_default()),
        _ => Vec::new(),
    }
}

/// The options of `effect.on_failure` for `effect`, `(value, label)`:
/// black, then the image (offered only when `effect.fallback_image` is
/// set, or when it is the value in force), then none.
pub fn on_failure_options(effect: &EffectConfig) -> Vec<(String, String)> {
    let image = effect.fallback_image.is_some() || effect.on_failure == EffectOnFailure::Image;
    [
        (EffectOnFailure::Black, "Hide the camera: black", true),
        (EffectOnFailure::Image, "Hide the camera: image", image),
        (EffectOnFailure::None, "Show the camera", true),
    ]
    .into_iter()
    .filter(|(_, _, offered)| *offered)
    .map(|(v, label, _)| (v.as_str().to_string(), label.to_string()))
    .collect()
}

/// The prefix of the row of each installed module.
pub const ENABLED_PREFIX: &str = "modules.enabled.";
/// The prefix of the row of each stage offered.
pub const PIN_PREFIX: &str = "modules.pin.";

/// An installed module, as the registry knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleFact {
    pub id: String,
    /// Its display name.
    pub name: String,
}

/// A stage some module offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageFact {
    pub id: String,
    /// The ids of the modules offering it, in display order.
    pub modules: Vec<String>,
}

/// What the module registry found: the facts the module rows are made
/// from; and the options the daemon computes for the
/// [`SpecKind::Runtime`] keys.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegistryFacts {
    /// The installed modules, in display order.
    pub modules: Vec<ModuleFact>,
    /// The stages offered, in display order.
    pub stages: Vec<StageFact>,
    /// The options of `output.mode`, `(value, label)`, in display order:
    /// the modes the selected camera can feed (and those frame generation
    /// can make), with the configured value among them. Empty: none is
    /// accepted.
    pub output_modes: Vec<(String, String)>,
    /// The options of `effect.on_failure` ([`on_failure_options`] of the
    /// configuration in force). Empty: those of the configuration the
    /// schema is made of, or for a write those of the defaults.
    pub on_failure_options: Vec<(String, String)>,
}

impl RegistryFacts {
    /// The display name of module `id`, else the id.
    pub fn name_of<'a>(&'a self, id: &'a str) -> &'a str {
        self.modules
            .iter()
            .find(|m| m.id == id)
            .map_or(id, |m| m.name.as_str())
    }
}

/// What a key is, for a write: its type, whether it is editable and when
/// it applies ([`lookup`]).
#[derive(Debug, Clone, PartialEq)]
pub struct KeyInfo {
    pub kind: RowKind,
    pub editable: bool,
    pub apply: Apply,
}

/// The core key `key`, if it is one.
pub fn core_key(key: &str) -> Option<&'static KeySpec> {
    CORE_KEYS.iter().find(|k| k.key == key)
}

/// The options of the pin row of `stage`: automatic, then each module
/// offering it.
fn pin_options(stage: &StageFact, facts: &RegistryFacts) -> Vec<(String, String)> {
    std::iter::once((String::new(), "Automatic".to_string()))
        .chain(
            stage
                .modules
                .iter()
                .map(|m| (m.clone(), facts.name_of(m).to_string())),
        )
        .collect()
}

/// What `key` is: a core key, `modules.enabled.<id>` of an installed
/// module or `modules.pin.<stage>` of a stage offered; `None` for any other
/// key.
pub fn lookup(key: &str, facts: &RegistryFacts) -> Option<KeyInfo> {
    if let Some(spec) = core_key(key) {
        return Some(KeyInfo {
            kind: spec.kind.row_kind_of(spec.key, facts, None),
            editable: spec.editable,
            apply: spec.apply,
        });
    }
    if let Some(id) = key.strip_prefix(ENABLED_PREFIX) {
        return (valid_module_id(id) && facts.modules.iter().any(|m| m.id == id)).then_some(
            KeyInfo {
                kind: RowKind::Bool,
                editable: true,
                apply: Apply::Reload,
            },
        );
    }
    if let Some(stage) = key.strip_prefix(PIN_PREFIX) {
        let fact = facts
            .stages
            .iter()
            .find(|s| s.id == stage && valid_stage_id(stage))?;
        return Some(KeyInfo {
            kind: RowKind::Enum {
                options: pin_options(fact, facts),
            },
            editable: true,
            apply: Apply::Reload,
        });
    }
    None
}

/// `value` as the TOML value of a key of type `kind`, or the reason it is
/// refused (naming `key`): a value of another type, an option that is not
/// one, a number that is not finite. A whole number is taken for a
/// `float`. Ranges are left to the configuration's own checks.
pub fn to_toml(key: &str, kind: &RowKind, value: &ConfigValue) -> Result<toml::Value, String> {
    let wrong = |what: &str| Err(format!("{key}: expects {what}"));
    match (kind, value) {
        (RowKind::Bool, ConfigValue::Bool(b)) => Ok(toml::Value::Boolean(*b)),
        (RowKind::Bool, _) => wrong("true or false"),
        (RowKind::Int { .. }, ConfigValue::Int(i)) => Ok(toml::Value::Integer(*i)),
        (RowKind::Int { .. }, _) => wrong("a whole number"),
        (RowKind::Float { .. }, ConfigValue::Float(f)) if f.is_finite() => {
            Ok(toml::Value::Float(*f))
        }
        (RowKind::Float { .. }, ConfigValue::Int(i)) => Ok(toml::Value::Float(*i as f64)),
        (RowKind::Float { .. }, _) => wrong("a number"),
        (RowKind::Enum { options }, ConfigValue::Str(s)) => {
            if options.iter().any(|(v, _)| v == s) {
                Ok(toml::Value::String(s.clone()))
            } else {
                let names: Vec<String> = options
                    .iter()
                    .map(|(v, _)| {
                        if v.is_empty() {
                            "\"\" (automatic)".to_string()
                        } else {
                            v.clone()
                        }
                    })
                    .collect();
                Err(format!("{key}: {s:?} is not one of: {}", names.join(", ")))
            }
        }
        (RowKind::Enum { .. }, _) => wrong("one of its options"),
        (RowKind::String | RowKind::Path, ConfigValue::Str(s)) => {
            Ok(toml::Value::String(s.clone()))
        }
        (RowKind::String, _) => wrong("a string"),
        (RowKind::Path, _) => wrong("a path"),
        (RowKind::List, ConfigValue::List(items)) => Ok(toml::Value::Array(
            items.iter().cloned().map(toml::Value::String).collect(),
        )),
        (RowKind::List, _) => wrong("a list of strings"),
    }
}

/// One row of the schema.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigRow {
    /// The dotted key.
    pub id: String,
    pub label: String,
    pub description: String,
    pub kind: RowKind,
    /// The value in force.
    pub value: ConfigValue,
    /// The built-in default; `None` when there is none (a path not set).
    pub default: Option<ConfigValue>,
    pub editable: bool,
    /// The file that sets the value in force (relative to the
    /// configuration directory); `""` for the default.
    pub source: String,
    /// The file after the managed one that sets it; `""` when none.
    pub overridden_by: String,
    pub apply: Apply,
    /// It can be changed now: false when overridden.
    pub enabled: bool,
    /// Why not, when not enabled; empty when enabled.
    pub disabled_reason: String,
}

/// A group of rows.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigGroup {
    pub id: String,
    pub label: String,
    pub rows: Vec<ConfigRow>,
}

/// The value of `key` in the serialized configuration, of type `kind`. A
/// key the configuration leaves out (a path not set, an empty list, no
/// profile) is the kind's empty value: `""`, `[]` or the first option.
fn value_in(table: &toml::Table, key: &str, kind: &RowKind) -> ConfigValue {
    match table_get(table, key) {
        Some(toml::Value::Boolean(b)) => ConfigValue::Bool(*b),
        Some(toml::Value::Integer(i)) => ConfigValue::Int(*i),
        Some(toml::Value::Float(f)) => ConfigValue::Float(*f),
        Some(toml::Value::String(s)) => ConfigValue::Str(s.clone()),
        Some(toml::Value::Array(a)) => ConfigValue::List(
            a.iter()
                .map(|v| match v {
                    toml::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect(),
        ),
        _ => match kind {
            RowKind::Bool => ConfigValue::Bool(false),
            RowKind::Int { min, .. } => ConfigValue::Int(*min),
            RowKind::Float { min, .. } => ConfigValue::Float(*min),
            RowKind::Enum { options } => {
                ConfigValue::Str(options.first().map(|(v, _)| v.clone()).unwrap_or_default())
            }
            RowKind::String | RowKind::Path => ConfigValue::Str(String::new()),
            RowKind::List => ConfigValue::List(Vec::new()),
        },
    }
}

fn serialized(config: &Config) -> toml::Table {
    match toml::Value::try_from(config) {
        Ok(toml::Value::Table(t)) => t,
        _ => toml::Table::new(),
    }
}

/// The row of `key` with its value, source and override.
#[allow(clippy::too_many_arguments)]
fn row(
    set: &ConfigSet,
    id: String,
    label: &str,
    description: &str,
    kind: RowKind,
    value: ConfigValue,
    default: Option<ConfigValue>,
    editable: bool,
    apply: Apply,
    source: &str,
) -> ConfigRow {
    let overridden_by = set.overridden_by(&id).to_string();
    let (enabled, disabled_reason) = if overridden_by.is_empty() {
        (true, String::new())
    } else {
        (false, format!("Overridden by {overridden_by}"))
    };
    ConfigRow {
        id,
        label: label.to_string(),
        description: description.to_string(),
        kind,
        value,
        default,
        editable,
        source: source.to_string(),
        overridden_by,
        apply,
        enabled,
        disabled_reason,
    }
}

/// The schema of `set` with the module rows of `facts`: the groups of
/// [`GROUPS`] in order, each with its core keys in [`CORE_KEYS`]' order;
/// the `modules` group holds `modules.profile`, a `modules.pin.<stage>` row
/// per stage offered (options: automatic, `""`, then each module offering
/// it, labelled with its name), a `modules.enabled.<id>` row per installed
/// module (labelled with its name), then `modules.order`, `modules.dirs`
/// and `modules.disabled`.
pub fn config_schema(set: &ConfigSet, facts: &RegistryFacts) -> Vec<ConfigGroup> {
    let current = serialized(&set.config);
    let defaults = serialized(&Config::default());
    let core_row = |spec: &KeySpec| {
        let mut kind = spec.kind.row_kind_of(spec.key, facts, Some(&set.config));
        let value = value_in(&current, spec.key, &kind);
        // The value in force is always an option.
        if let (RowKind::Enum { options }, ConfigValue::Str(v)) = (&mut kind, &value)
            && spec.kind == SpecKind::Runtime
            && !options.iter().any(|(o, _)| o == v)
        {
            options.push((v.clone(), v.clone()));
        }
        let default = match (&kind, value_in(&defaults, spec.key, &kind)) {
            (RowKind::Path, ConfigValue::Str(s)) if s.is_empty() => None,
            (_, d) => Some(d),
        };
        row(
            set,
            spec.key.to_string(),
            spec.label,
            spec.description,
            kind,
            value,
            default,
            spec.editable,
            spec.apply,
            set.source(spec.key),
        )
    };
    let modules = &set.config.modules;
    GROUPS
        .iter()
        .map(|(group, label)| {
            let mut rows: Vec<ConfigRow> = CORE_KEYS
                .iter()
                .filter(|k| k.group == *group)
                .map(core_row)
                .collect();
            if *group == "modules" {
                let mut dynamic = Vec::new();
                for stage in &facts.stages {
                    // Frame generation is no user's choice: its pin is
                    // shown only when there is a choice of module.
                    if stage.id == FRAME_GENERATION_STAGE && stage.modules.len() < 2 {
                        continue;
                    }
                    let id = format!("{PIN_PREFIX}{}", stage.id);
                    let mut options = pin_options(stage, facts);
                    let value = modules.pin.get(&stage.id).cloned().unwrap_or_default();
                    if !options.iter().any(|(v, _)| *v == value) {
                        options.push((value.clone(), value.clone()));
                    }
                    let source = set.source(&id).to_string();
                    dynamic.push(row(
                        set,
                        id,
                        &format!("Module for {}", stage.id),
                        "The module that provides this stage. Automatic chooses by the backend profile.",
                        RowKind::Enum { options },
                        ConfigValue::Str(value),
                        Some(ConfigValue::Str(String::new())),
                        true,
                        Apply::Reload,
                        &source,
                    ));
                }
                for m in &facts.modules {
                    let id = format!("{ENABLED_PREFIX}{}", m.id);
                    let mut source = set.source(&id).to_string();
                    if source.is_empty() && modules.disabled.contains(&m.id) {
                        source = set.source("modules.disabled").to_string();
                    }
                    dynamic.push(row(
                        set,
                        id,
                        &m.name,
                        "Load this module. Off, its stages are not offered.",
                        RowKind::Bool,
                        ConfigValue::Bool(modules.is_enabled(&m.id)),
                        Some(ConfigValue::Bool(true)),
                        true,
                        Apply::Reload,
                        &source,
                    ));
                }
                // After modules.profile, the first row.
                let at = rows.len().min(1);
                rows.splice(at..at, dynamic);
            }
            ConfigGroup {
                id: group.to_string(),
                label: label.to_string(),
                rows,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
