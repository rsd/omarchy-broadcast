//! The daemon's configuration file and its runtime state file (see
//! `docs/adr/0011-daemon-lifecycle-and-dbus-api.md`, "Configuration and
//! state").
//!
//! - [`Config`]: `$XDG_CONFIG_HOME/omarchy-broadcast/config.toml`, what the user
//!   decides. Versioned (`version = 1`), every key optional, unknown keys
//!   refused. The daemon reads it and never writes it. The commented
//!   reference is `docs/config.example.toml`.
//! - [`RuntimeState`]: `$XDG_STATE_HOME/omarchy-broadcast/state.toml`, what
//!   changes at run time (`Enabled`, the selected camera, the effect and its
//!   settings, each camera's image controls). Written atomically by the
//!   daemon; a damaged file falls back to the defaults (the daemon then
//!   starts `off`).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    AspectPolicy, CameraSelector, ColourRange, DEFAULT_SINK_LABEL, EffectMode, EffectOnFailure,
    ModeRequest, OutputContract, OutputFormat, SinkSelector, fmt_size_rate,
};

/// Config schema version this build reads.
pub const CONFIG_VERSION: u32 = 1;

/// Directory name under the XDG base directories.
pub const APP_DIR: &str = "omarchy-broadcast";

/// The camera image controls a user may set (their v4l2-ctl names), the
/// only names [`RuntimeState::camera_controls`] keeps: the allowlist of
/// `obc_pipeline::controls::IMAGE_CONTROLS`, in its order.
pub const CAMERA_CONTROL_NAMES: [&str; 5] = [
    "brightness",
    "contrast",
    "saturation",
    "sharpness",
    "backlight_compensation",
];

/// Bounds of the duration keys, inclusive unless stated otherwise.
pub mod limits {
    /// `output.hold_timeout_s`: greater than 0, at most this.
    pub const MAX_HOLD_TIMEOUT_S: f64 = 60.0;
    /// `output.input_gap_ms`: 1 to this.
    pub const MAX_INPUT_GAP_MS: u64 = 60_000;
    /// `lifecycle.idle_grace_s`: 0 to this.
    pub const MAX_IDLE_GRACE_S: f64 = 3600.0;
    /// `lifecycle.start_debounce_ms`: 0 to this.
    pub const MAX_START_DEBOUNCE_MS: u64 = 10_000;
    /// `effect.blur_strength`: this to [`MAX_BLUR_STRENGTH`].
    pub const MIN_BLUR_STRENGTH: f64 = 0.0;
    /// `effect.blur_strength`: [`MIN_BLUR_STRENGTH`] to this.
    pub const MAX_BLUR_STRENGTH: f64 = 1.0;
}

/// A config file that cannot be used. The previous config stays in force.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file exists but cannot be read.
    #[error("{}: {source}", .path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The file is not valid TOML, or a key or value is not accepted.
    /// `line` and `column` are 1-based; `key` is the dotted key the error
    /// is on, when it can be told.
    #[error("{}{}: {}{message}", .path.display(), fmt_position(*.line, *.column), fmt_key(.key.as_deref()))]
    Parse {
        path: PathBuf,
        line: Option<usize>,
        column: Option<usize>,
        key: Option<String>,
        message: String,
    },
    /// A value fails validation in a config built in code
    /// ([`Config::validate`]).
    #[error("{key}: {reason}")]
    Invalid { key: &'static str, reason: String },
}

fn fmt_position(line: Option<usize>, column: Option<usize>) -> String {
    match (line, column) {
        (Some(l), Some(c)) => format!(":{l}:{c}"),
        (Some(l), None) => format!(":{l}"),
        _ => String::new(),
    }
}

fn fmt_key(key: Option<&str>) -> String {
    key.map(|k| format!("{k}: ")).unwrap_or_default()
}

/// The configuration file.
///
/// Every key is optional; a missing file is [`Config::default`]. Use the
/// accessors for the values the rest of the code consumes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Schema version; only [`CONFIG_VERSION`] is accepted.
    #[serde(deserialize_with = "de_version")]
    pub version: u32,
    pub camera: CameraConfig,
    pub output: OutputConfig,
    pub loopback: LoopbackConfig,
    pub lifecycle: LifecycleConfig,
    pub effect: EffectConfig,
    pub pipewire: PipeWireConfig,
}

/// `[camera]`: which camera, and what to ask it for.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CameraConfig {
    /// Preferred cameras, most preferred first: card-name substrings,
    /// `vid:pid` or `/dev/videoN`. Cameras not listed follow in discovery
    /// order.
    #[serde(serialize_with = "ser_selectors", deserialize_with = "de_selectors")]
    pub preference: Vec<CameraSelector>,
    /// Mode requested from the camera (`[FORMAT:]WxH@FPS`).
    #[serde(serialize_with = "ser_display", deserialize_with = "de_from_str")]
    pub mode: ModeRequest,
    /// Pin the camera's exposure so it cannot lower the frame rate in the
    /// dark.
    pub fixed_framerate: bool,
}

/// `[output]`: what the virtual camera delivers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputConfig {
    /// Size and frame rate (`WxH@FPS`). Only those fields are read from it;
    /// the format is [`Self::format`] and the range is always limited. Use
    /// [`Config::output_contract`].
    #[serde(serialize_with = "ser_size_rate", deserialize_with = "de_from_str")]
    pub mode: OutputContract,
    #[serde(serialize_with = "ser_display", deserialize_with = "de_from_str")]
    pub format: OutputFormat,
    #[serde(serialize_with = "ser_display", deserialize_with = "de_from_str")]
    pub aspect: AspectPolicy,
    /// Seconds a camera switch holds the last frame before placeholders.
    #[serde(deserialize_with = "de_hold_timeout")]
    pub hold_timeout_s: f64,
    /// Milliseconds without input before placeholders replace repeats.
    #[serde(deserialize_with = "de_input_gap")]
    pub input_gap_ms: u64,
}

impl Default for OutputConfig {
    fn default() -> Self {
        OutputConfig {
            mode: OutputContract::default(),
            format: OutputFormat::default(),
            aspect: AspectPolicy::default(),
            hold_timeout_s: 3.0,
            input_gap_ms: 500,
        }
    }
}

/// `[loopback]`: which v4l2loopback device receives the output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoopbackConfig {
    /// Exact card label of the loopback device.
    #[serde(deserialize_with = "de_label")]
    pub label: String,
    /// Explicit device node; overrides [`Self::label`].
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_device"
    )]
    pub device: Option<PathBuf>,
}

impl Default for LoopbackConfig {
    fn default() -> Self {
        LoopbackConfig {
            label: DEFAULT_SINK_LABEL.to_string(),
            device: None,
        }
    }
}

/// `[lifecycle]`: when the camera and the GPU pipeline start and stop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LifecycleConfig {
    /// Seconds the stream keeps running after the last consumer leaves.
    #[serde(deserialize_with = "de_idle_grace")]
    pub idle_grace_s: f64,
    /// Milliseconds a consumer must be present before the camera starts.
    #[serde(deserialize_with = "de_start_debounce")]
    pub start_debounce_ms: u64,
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        LifecycleConfig {
            idle_grace_s: 5.0,
            start_debounce_ms: 300,
        }
    }
}

/// `[effect]`: the video effect (see
/// `docs/adr/0012-video-effects-and-maxine-backend.md`). Also the type of
/// the effective settings the daemon runs with, once the state file's
/// overrides are applied ([`RuntimeState::effect_over`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EffectConfig {
    /// The effect: `none`, `blur` or `replace`.
    #[serde(serialize_with = "ser_display", deserialize_with = "de_from_str")]
    pub mode: EffectMode,
    /// Strength of `blur`, 0.0 to 1.0.
    #[serde(deserialize_with = "de_blur_strength")]
    pub blur_strength: f64,
    /// The image `replace` puts behind the person: an absolute path. Empty
    /// (`None`) by default; required with `mode = "replace"`. Whether the
    /// file exists is found out when the effect starts, not here.
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_background_image"
    )]
    pub background_image: Option<PathBuf>,
    /// Video noise removal, a stage of its own that runs before the effect
    /// (or alone, with `mode = "none"`). Off by default.
    pub denoise: bool,
    /// What the virtual camera shows when the effect cannot run.
    #[serde(serialize_with = "ser_display", deserialize_with = "de_from_str")]
    pub on_failure: EffectOnFailure,
}

impl EffectConfig {
    /// The default blur strength.
    pub const DEFAULT_BLUR_STRENGTH: f64 = 0.5;

    /// Checks the values and that `replace` has an image. The key named in
    /// the error is the dotted config key.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let invalid =
            |key: &'static str| move |reason: String| ConfigError::Invalid { key, reason };
        check_blur_strength(self.blur_strength).map_err(invalid("effect.blur_strength"))?;
        if let Some(p) = &self.background_image {
            check_background_image(p).map_err(invalid("effect.background_image"))?;
        }
        if self.mode == EffectMode::Replace && self.background_image.is_none() {
            return Err(invalid("effect.mode")(
                "\"replace\" needs a background image: set effect.background_image".into(),
            ));
        }
        Ok(())
    }
}

impl Default for EffectConfig {
    fn default() -> Self {
        EffectConfig {
            mode: EffectMode::None,
            blur_strength: Self::DEFAULT_BLUR_STRENGTH,
            background_image: None,
            denoise: false,
            on_failure: EffectOnFailure::Black,
        }
    }
}

/// `[pipewire]`: an optional second output, a native PipeWire video
/// source next to the v4l2loopback device. It carries the same frames as
/// the loopback, and an application linked to it counts as a consumer like
/// one holding the loopback.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PipeWireConfig {
    /// Offer the PipeWire video source. Off by default: the loopback alone.
    pub enabled: bool,
    /// Offer NV12 on the node (next to YUY2 and I420). On by default. OBS
    /// Studio's PipeWire camera source mis-sizes NV12 rows and crashes when
    /// NV12 is chosen by hand; off, the node offers YUY2 and I420 only.
    pub offer_nv12: bool,
}

impl Default for PipeWireConfig {
    fn default() -> Self {
        PipeWireConfig {
            enabled: false,
            offer_nv12: true,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            version: CONFIG_VERSION,
            camera: CameraConfig::default(),
            output: OutputConfig::default(),
            loopback: LoopbackConfig::default(),
            lifecycle: LifecycleConfig::default(),
            effect: EffectConfig::default(),
            pipewire: PipeWireConfig::default(),
        }
    }
}

impl Config {
    /// Reads `path`. A missing file is the defaults; anything else that
    /// prevents using the file is an error naming it.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        match fs::read_to_string(path) {
            Ok(text) => Config::parse(&text, path),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
            Err(source) => Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Parses and validates `text`; `path` only names the origin in errors.
    pub fn parse(text: &str, path: &Path) -> Result<Config, ConfigError> {
        let config: Config = toml::from_str(text).map_err(|e| parse_error(&e, text, path))?;
        // Every key is checked while deserializing; this catches what spans
        // keys (`replace` without an image) and keeps the two paths in
        // agreement. Its error names the file and the line of the key.
        config.validate().map_err(|e| match e {
            ConfigError::Invalid { key, reason } => ConfigError::Parse {
                path: path.to_path_buf(),
                line: line_of_key(text, key),
                column: None,
                key: Some(key.to_string()),
                message: reason,
            },
            other => other,
        })?;
        Ok(config)
    }

    /// The config as TOML, every key spelled out.
    pub fn to_toml(&self) -> String {
        toml::to_string(self).expect("a Config always serializes")
    }

    /// Checks every value, for configs built or changed in code (a parsed
    /// config has already passed these checks).
    pub fn validate(&self) -> Result<(), ConfigError> {
        let invalid =
            |key: &'static str| move |reason: String| ConfigError::Invalid { key, reason };
        check_version(self.version).map_err(invalid("version"))?;
        if self
            .camera
            .preference
            .iter()
            .any(|s| matches!(s, CameraSelector::Name(n) if n.trim().is_empty()))
        {
            return Err(invalid("camera.preference")(
                crate::CoreError::EmptyCameraSelector.to_string(),
            ));
        }
        self.output_contract()
            .validate()
            .map_err(|e| invalid("output.mode")(e.to_string()))?;
        check_hold_timeout(self.output.hold_timeout_s).map_err(invalid("output.hold_timeout_s"))?;
        check_input_gap(self.output.input_gap_ms).map_err(invalid("output.input_gap_ms"))?;
        check_label(&self.loopback.label).map_err(invalid("loopback.label"))?;
        if let Some(device) = &self.loopback.device {
            check_device(device).map_err(invalid("loopback.device"))?;
        }
        check_idle_grace(self.lifecycle.idle_grace_s).map_err(invalid("lifecycle.idle_grace_s"))?;
        check_start_debounce(self.lifecycle.start_debounce_ms)
            .map_err(invalid("lifecycle.start_debounce_ms"))?;
        self.effect.validate()
    }

    /// The output contract: `output.mode`'s size and rate, `output.format`,
    /// limited range.
    pub fn output_contract(&self) -> OutputContract {
        OutputContract {
            format: self.output.format,
            range: ColourRange::Limited,
            ..self.output.mode
        }
    }

    /// The camera preference list, most preferred first.
    pub fn camera_preference(&self) -> Vec<CameraSelector> {
        self.camera.preference.clone()
    }

    /// The mode requested from the camera.
    pub fn camera_mode(&self) -> ModeRequest {
        self.camera.mode
    }

    /// The loopback to write to: `loopback.device` if set, else the label.
    pub fn sink_selector(&self) -> SinkSelector {
        match &self.loopback.device {
            Some(path) => SinkSelector::Path(path.clone()),
            None => SinkSelector::Label(self.loopback.label.clone()),
        }
    }

    /// `output.hold_timeout_s`.
    pub fn hold_timeout(&self) -> Duration {
        Duration::from_secs_f64(self.output.hold_timeout_s)
    }

    /// `output.input_gap_ms`.
    pub fn input_gap(&self) -> Duration {
        Duration::from_millis(self.output.input_gap_ms)
    }

    /// `lifecycle.idle_grace_s`.
    pub fn idle_grace(&self) -> Duration {
        Duration::from_secs_f64(self.lifecycle.idle_grace_s)
    }

    /// `lifecycle.start_debounce_ms`.
    pub fn start_debounce(&self) -> Duration {
        Duration::from_millis(self.lifecycle.start_debounce_ms)
    }
}

/// A TOML error as a [`ConfigError::Parse`], with a 1-based position and
/// the dotted key it is on.
fn parse_error(e: &toml::de::Error, text: &str, path: &Path) -> ConfigError {
    let (line, column, key) = match e.span() {
        Some(span) => {
            let start = span.start.min(text.len());
            let before = &text[..start];
            let line = before.matches('\n').count() + 1;
            let line_start = before.rfind('\n').map_or(0, |i| i + 1);
            let column = text[line_start..start].chars().count() + 1;
            (Some(line), Some(column), key_at(text, line))
        }
        None => (None, None, None),
    };
    ConfigError::Parse {
        path: path.to_path_buf(),
        line,
        column,
        key,
        message: e.message().trim().to_string(),
    }
}

/// The dotted key on 1-based `line`: the last `[table]` header above it
/// and the bare key of `key = ...` on the line itself. Best effort; `None`
/// when the line holds no key (an array element on its own line, a header).
fn key_at(text: &str, line: usize) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let here = lines.get(line.checked_sub(1)?)?.trim();
    let key = here
        .split_once('=')
        .map(|(k, _)| k.trim())
        .filter(|k| !k.is_empty() && !k.starts_with('#') && !k.starts_with('['))?;
    let table = lines[..line - 1]
        .iter()
        .rev()
        .map(|l| l.trim())
        .find(|l| l.starts_with('[') && !l.starts_with("[["))
        .and_then(|l| l.strip_prefix('[')?.split(']').next())
        .map(str::trim);
    Some(match table {
        Some(t) if !t.is_empty() => format!("{t}.{key}"),
        _ => key.to_string(),
    })
}

/// The 1-based line of the dotted `key` (`table.key`, or a top-level key)
/// in `text`, best effort: the line of `key = ...` inside `[table]`.
fn line_of_key(text: &str, key: &str) -> Option<usize> {
    let (table, bare) = key.rsplit_once('.').unwrap_or(("", key));
    let mut current = "";
    for (i, line) in text.lines().enumerate() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix('[') {
            current = rest.split(']').next().unwrap_or("").trim();
            continue;
        }
        if current == table
            && let Some((k, _)) = l.split_once('=')
            && k.trim() == bare
        {
            return Some(i + 1);
        }
    }
    None
}

fn check_version(v: u32) -> Result<(), String> {
    if v == CONFIG_VERSION {
        Ok(())
    } else {
        Err(format!(
            "unsupported config version {v} (this build reads version {CONFIG_VERSION})"
        ))
    }
}

fn check_secs(v: f64, allow_zero: bool, max: f64) -> Result<(), String> {
    let low_ok = if allow_zero { v >= 0.0 } else { v > 0.0 };
    if v.is_finite() && low_ok && v <= max {
        Ok(())
    } else if allow_zero {
        Err(format!("must be between 0 and {max} seconds, got {v}"))
    } else {
        Err(format!(
            "must be greater than 0 and at most {max} seconds, got {v}"
        ))
    }
}

fn check_ms(v: u64, min: u64, max: u64) -> Result<(), String> {
    if (min..=max).contains(&v) {
        Ok(())
    } else {
        Err(format!(
            "must be between {min} and {max} milliseconds, got {v}"
        ))
    }
}

fn check_hold_timeout(v: f64) -> Result<(), String> {
    check_secs(v, false, limits::MAX_HOLD_TIMEOUT_S)
}

fn check_input_gap(v: u64) -> Result<(), String> {
    check_ms(v, 1, limits::MAX_INPUT_GAP_MS)
}

fn check_idle_grace(v: f64) -> Result<(), String> {
    check_secs(v, true, limits::MAX_IDLE_GRACE_S)
}

fn check_start_debounce(v: u64) -> Result<(), String> {
    check_ms(v, 0, limits::MAX_START_DEBOUNCE_MS)
}

fn check_label(v: &str) -> Result<(), String> {
    if v.trim().is_empty() {
        Err("must not be empty".to_string())
    } else {
        Ok(())
    }
}

fn check_device(v: &Path) -> Result<(), String> {
    if v.starts_with("/dev/") && v != Path::new("/dev/") {
        Ok(())
    } else {
        Err(format!(
            "must be a device node under /dev/, got {:?}",
            v.display().to_string()
        ))
    }
}

fn check_blur_strength(v: f64) -> Result<(), String> {
    if v.is_finite() && (limits::MIN_BLUR_STRENGTH..=limits::MAX_BLUR_STRENGTH).contains(&v) {
        Ok(())
    } else {
        Err(format!(
            "must be between {} and {}, got {v}",
            limits::MIN_BLUR_STRENGTH,
            limits::MAX_BLUR_STRENGTH
        ))
    }
}

fn check_background_image(v: &Path) -> Result<(), String> {
    if v.is_absolute() {
        Ok(())
    } else {
        Err(format!(
            "must be an absolute path (or empty), got {:?}",
            v.display().to_string()
        ))
    }
}

/// A value checked by `check`; the error carries the span of the value.
fn de_checked<'de, D, T>(d: D, check: fn(T) -> Result<(), String>) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Copy,
{
    let v = T::deserialize(d)?;
    check(v).map_err(serde::de::Error::custom)?;
    Ok(v)
}

fn de_version<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    de_checked(d, check_version)
}

/// A number of seconds; an integer is accepted too (`hold_timeout_s = 3`).
fn de_secs<'de, D: Deserializer<'de>>(
    d: D,
    check: fn(f64) -> Result<(), String>,
) -> Result<f64, D::Error> {
    de_number(d, "a number of seconds", check)
}

/// A number, float or integer, checked by `check`; `what` is what the
/// error says was expected.
fn de_number<'de, D: Deserializer<'de>>(
    d: D,
    what: &'static str,
    check: fn(f64) -> Result<(), String>,
) -> Result<f64, D::Error> {
    struct Number(&'static str);
    impl serde::de::Visitor<'_> for Number {
        type Value = f64;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
        }
        fn visit_f64<E>(self, v: f64) -> Result<f64, E> {
            Ok(v)
        }
        fn visit_i64<E>(self, v: i64) -> Result<f64, E> {
            Ok(v as f64)
        }
        fn visit_u64<E>(self, v: u64) -> Result<f64, E> {
            Ok(v as f64)
        }
    }
    let v = d.deserialize_any(Number(what))?;
    check(v).map_err(serde::de::Error::custom)?;
    Ok(v)
}

fn de_blur_strength<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    de_number(d, "a number from 0 to 1", check_blur_strength)
}

/// `effect.background_image`: empty is `None`, anything else an absolute
/// path.
fn de_background_image<'de, D: Deserializer<'de>>(d: D) -> Result<Option<PathBuf>, D::Error> {
    let v = String::deserialize(d)?;
    if v.is_empty() {
        return Ok(None);
    }
    let p = PathBuf::from(v);
    check_background_image(&p).map_err(serde::de::Error::custom)?;
    Ok(Some(p))
}

/// The state file's effect: `Some` of a known name.
fn de_state_effect<'de, D: Deserializer<'de>>(d: D) -> Result<Option<EffectMode>, D::Error> {
    de_from_str(d).map(Some)
}

fn de_state_blur_strength<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    de_blur_strength(d).map(Some)
}

fn de_state_denoise<'de, D: Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    bool::deserialize(d).map(Some)
}

fn de_hold_timeout<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    de_secs(d, check_hold_timeout)
}

fn de_idle_grace<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    de_secs(d, check_idle_grace)
}

fn de_input_gap<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    de_checked(d, check_input_gap)
}

fn de_start_debounce<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    de_checked(d, check_start_debounce)
}

fn de_label<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let v = String::deserialize(d)?;
    check_label(&v).map_err(serde::de::Error::custom)?;
    Ok(v)
}

fn de_device<'de, D: Deserializer<'de>>(d: D) -> Result<Option<PathBuf>, D::Error> {
    let v = PathBuf::from(String::deserialize(d)?);
    check_device(&v).map_err(serde::de::Error::custom)?;
    Ok(Some(v))
}

/// A string parsed with [`FromStr`]; its error is the message.
fn de_from_str<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
    T::Err: fmt::Display,
{
    String::deserialize(d)?
        .parse()
        .map_err(serde::de::Error::custom)
}

fn de_selectors<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<CameraSelector>, D::Error> {
    Vec::<String>::deserialize(d)?
        .iter()
        .map(|s| s.parse().map_err(serde::de::Error::custom))
        .collect()
}

fn ser_display<S: Serializer, T: fmt::Display>(v: &T, s: S) -> Result<S::Ok, S::Error> {
    s.collect_str(v)
}

/// `WxH@FPS`, the form `output.mode` is written in.
fn ser_size_rate<S: Serializer>(v: &OutputContract, s: S) -> Result<S::Ok, S::Error> {
    struct SizeRate<'a>(&'a OutputContract);
    impl fmt::Display for SizeRate<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let c = self.0;
            fmt_size_rate(f, c.width, c.height, c.fps_num, c.fps_den)
        }
    }
    s.collect_str(&SizeRate(v))
}

/// Each selector in the form it is written in the config.
fn ser_selectors<S: Serializer>(v: &[CameraSelector], s: S) -> Result<S::Ok, S::Error> {
    s.collect_seq(v.iter().map(selector_form))
}

/// The config form of a selector, which parses back to the same selector.
pub fn selector_form(s: &CameraSelector) -> String {
    match s {
        CameraSelector::Path(p) => p.display().to_string(),
        CameraSelector::Usb(u) => u.to_string(),
        CameraSelector::Name(n) => n.clone(),
    }
}

/// The image controls the user set on one camera
/// ([`RuntimeState::camera_controls`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CameraControlState {
    /// The values the user set, by control name ([`CAMERA_CONTROL_NAMES`]),
    /// written to the camera whenever it opens. A value equal to the
    /// control's default is kept all the same: the user set it.
    pub values: BTreeMap<String, i32>,
    /// A reset was asked while the camera did not stream: every control is
    /// written its default the next time the camera opens (before
    /// `values`, which a reset leaves empty and a later set may fill).
    pub reset_pending: bool,
}

impl CameraControlState {
    /// Nothing to keep: no value and no pending reset.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty() && !self.reset_pending
    }
}

/// What the daemon changes at run time and keeps across restarts.
///
/// The effect keys override the config's `[effect]` keys of the same name
/// while set (`None`: the config's value applies). A value that is not
/// valid (an unknown effect, a strength outside 0 to 1, a relative image
/// path, a `denoise` that is not a boolean) makes the whole file
/// unparseable, so the daemon's fail-closed rule for a damaged state file
/// applies: it starts `off`, with the camera closed, and never streams the
/// raw image in place of an effect it could not read. Unknown keys (a newer
/// daemon's) are ignored.
///
/// The camera image controls are the exception: they hide nothing, so a bad
/// entry of `[camera_controls]` is dropped on its own, with a warning
/// ([`RuntimeState::load_noting`]), and never damages the file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeState {
    /// `Enabled` (false = the privacy state `off`).
    pub enabled: bool,
    /// The camera chosen with `SelectCamera` / `NextCamera` (its id), if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub camera: Option<String>,
    /// The effect chosen with `SetEffect`.
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_state_effect"
    )]
    pub effect: Option<EffectMode>,
    /// The blur strength set with `SetBlurStrength`, 0.0 to 1.0.
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_state_blur_strength"
    )]
    pub blur_strength: Option<f64>,
    /// The background image set with `SetBackgroundImage`: an absolute
    /// path. Clearing it (`SetBackgroundImage("")`) removes the override, so
    /// the config's image applies again; an empty value in the file is read
    /// the same way, as no override.
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_background_image"
    )]
    pub background_image: Option<PathBuf>,
    /// Noise removal switched with `SetDenoise`.
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_state_denoise"
    )]
    pub denoise: Option<bool>,
    /// The image controls set with `SetCameraControl` (or reset with
    /// `ResetCameraControls`), by camera id. Never holds an empty entry
    /// ([`CameraControlState::is_empty`]). In the file, one table per
    /// camera, after the other keys (written by [`RuntimeState::save`]):
    ///
    /// ```toml
    /// [camera_controls."usb-046d_0944-video-index0"]
    /// values = { saturation = 128, sharpness = 128 }
    /// reset_pending = true
    /// ```
    ///
    /// `values` is left out when empty, `reset_pending` when false, and the
    /// whole table when there is no entry.
    #[serde(skip_serializing, deserialize_with = "de_camera_controls")]
    pub camera_controls: BTreeMap<String, CameraControlState>,
}

impl Default for RuntimeState {
    fn default() -> Self {
        RuntimeState {
            enabled: true,
            camera: None,
            effect: None,
            blur_strength: None,
            background_image: None,
            denoise: None,
            camera_controls: BTreeMap::new(),
        }
    }
}

/// `[camera_controls]`, leniently: what is valid is kept, the rest dropped
/// ([`camera_controls_from`]); never an error.
fn de_camera_controls<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<String, CameraControlState>, D::Error> {
    let value = toml::Value::deserialize(d)?;
    Ok(camera_controls_from(&value).0)
}

/// The camera control entries of the `[camera_controls]` value, and a
/// warning for each part dropped: an entry that is not a table, a control
/// name outside [`CAMERA_CONTROL_NAMES`], a value that is not an integer or
/// does not fit 32 bits, a `values` that is not a table, a `reset_pending`
/// that is not a boolean. An entry left with nothing to keep is dropped
/// (silently when nothing in it was bad). Unknown keys of an entry (a newer
/// daemon's) are ignored.
fn camera_controls_from(
    value: &toml::Value,
) -> (BTreeMap<String, CameraControlState>, Vec<String>) {
    let mut out = BTreeMap::new();
    let mut warnings = Vec::new();
    let Some(table) = value.as_table() else {
        warnings.push(format!(
            "camera_controls must be a table (found {}): ignored",
            value.type_str()
        ));
        return (out, warnings);
    };
    for (camera, entry) in table {
        let Some(entry) = entry.as_table() else {
            warnings.push(format!(
                "camera_controls.{camera:?} must be a table (found {}): ignored",
                entry.type_str()
            ));
            continue;
        };
        let mut state = CameraControlState::default();
        match entry.get("values") {
            None => {}
            Some(toml::Value::Table(values)) => {
                for (name, v) in values {
                    if !CAMERA_CONTROL_NAMES.contains(&name.as_str()) {
                        warnings.push(format!(
                            "camera_controls.{camera:?}: {name:?} is not a camera control \
                             (expected one of {}): ignored",
                            CAMERA_CONTROL_NAMES.join(", ")
                        ));
                        continue;
                    }
                    match v.as_integer().map(i32::try_from) {
                        Some(Ok(v)) => {
                            state.values.insert(name.clone(), v);
                        }
                        Some(Err(_)) => warnings.push(format!(
                            "camera_controls.{camera:?}: {name} = {v} does not fit 32 bits: \
                             ignored"
                        )),
                        None => warnings.push(format!(
                            "camera_controls.{camera:?}: {name} must be an integer (found {}): ignored",
                            v.type_str()
                        )),
                    }
                }
            }
            Some(other) => warnings.push(format!(
                "camera_controls.{camera:?}: values must be a table (found {}): ignored",
                other.type_str()
            )),
        }
        match entry.get("reset_pending") {
            None => {}
            Some(toml::Value::Boolean(b)) => state.reset_pending = *b,
            Some(other) => warnings.push(format!(
                "camera_controls.{camera:?}: reset_pending must be a boolean (found {}): ignored",
                other.type_str()
            )),
        }
        if !state.is_empty() {
            out.insert(camera.clone(), state);
        }
    }
    (out, warnings)
}

/// `s` as a TOML basic string (`"..."`), valid as a quoted key: `"` and
/// `\` escaped, and every control character (U+0000 to U+001F, U+007F to
/// U+009F) written as its short escape or `\uXXXX`. (The `toml` crate may
/// write a literal or a multi-line string instead, and a multi-line string
/// is no valid key.)
fn toml_basic_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The `[camera_controls."<id>"]` tables of [`RuntimeState::save`], each
/// with `values` as an inline table; empty entries are left out.
fn camera_controls_toml(controls: &BTreeMap<String, CameraControlState>) -> String {
    let mut text = String::new();
    for (camera, state) in controls.iter().filter(|(_, s)| !s.is_empty()) {
        text.push_str(&format!(
            "\n[camera_controls.{}]\n",
            toml_basic_string(camera)
        ));
        if !state.values.is_empty() {
            let values: toml::Table = state
                .values
                .iter()
                .map(|(k, v)| (k.clone(), toml::Value::Integer(i64::from(*v))))
                .collect();
            text.push_str(&format!("values = {}\n", toml::Value::Table(values)));
        }
        if state.reset_pending {
            text.push_str("reset_pending = true\n");
        }
    }
    text
}

impl RuntimeState {
    /// The effect settings in force: each key of `config` overridden by
    /// this state's value when it has one (`on_failure` is config only).
    pub fn effect_over(&self, config: &EffectConfig) -> EffectConfig {
        EffectConfig {
            mode: self.effect.unwrap_or(config.mode),
            blur_strength: self.blur_strength.unwrap_or(config.blur_strength),
            // An empty path is no override, as in the file.
            background_image: match &self.background_image {
                Some(p) if !p.as_os_str().is_empty() => Some(p.clone()),
                _ => config.background_image.clone(),
            },
            denoise: self.denoise.unwrap_or(config.denoise),
            on_failure: config.on_failure,
        }
    }

    /// Reads `path`. Never fails: a missing file is the defaults, and a file
    /// that cannot be read or parsed is the defaults plus a warning saying
    /// why. What [`Self::load_noting`] reads, without its notes.
    pub fn load(path: &Path) -> (RuntimeState, Option<String>) {
        let (state, damaged, _) = Self::load_noting(path);
        (state, damaged)
    }

    /// [`Self::load`], and the parts of `[camera_controls]` it dropped, one
    /// line each naming the file (they never make the file damaged).
    pub fn load_noting(path: &Path) -> (RuntimeState, Option<String>, Vec<String>) {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return (RuntimeState::default(), None, Vec::new());
            }
            Err(e) => {
                return (
                    RuntimeState::default(),
                    Some(format!("{}: {e}; using the default state", path.display())),
                    Vec::new(),
                );
            }
        };
        match toml::from_str::<RuntimeState>(&text) {
            Ok(state) => {
                // The second read of a small file, for what the first one
                // dropped.
                let notes = toml::from_str::<toml::Table>(&text)
                    .ok()
                    .and_then(|t| t.get("camera_controls").map(camera_controls_from))
                    .map(|(_, w)| w)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|w| format!("{}: {w}", path.display()))
                    .collect();
                (state, None, notes)
            }
            Err(e) => {
                let err = parse_error(&e, &text, path);
                (
                    RuntimeState::default(),
                    Some(format!("{err}; using the default state")),
                    Vec::new(),
                )
            }
        }
    }

    /// Writes `path` atomically: a temporary file with a name of its own in
    /// the same directory, flushed to disk, renamed over `path`, then the
    /// directory flushed so the rename is durable. Creates the parent
    /// directories.
    ///
    /// Saves may run concurrently, from threads or processes: each writes its
    /// own temporary file, and the last rename wins whole. A temporary file
    /// is removed when its save fails; those left by a process that died
    /// during a save are removed by the next successful save.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let dir = match path.parent() {
            Some(d) if !d.as_os_str().is_empty() => d,
            _ => Path::new("."),
        };
        fs::create_dir_all(dir)?;
        let name = path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "state path has no file name")
        })?;
        let mut text = toml::to_string(self).map_err(io::Error::other)?;
        text.push_str(&camera_controls_toml(&self.camera_controls));
        let (tmp, f) = create_temp(dir, name)?;
        let write = |mut f: fs::File| -> io::Result<()> {
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            drop(f);
            fs::rename(&tmp, path)
        };
        if let Err(e) = write(f) {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        remove_stale_temps(dir, name);
        // Make the rename itself durable.
        fs::File::open(dir)?.sync_all()
    }
}

/// The prefix of the temporary files of [`RuntimeState::save`] for the file
/// `name`: `.<name>.tmp-`, followed by `<pid>-<sequence>-<nanoseconds>`.
fn temp_prefix(name: &std::ffi::OsStr) -> OsString {
    let mut prefix = OsString::from(".");
    prefix.push(name);
    prefix.push(".tmp-");
    prefix
}

/// Creates a temporary file next to the file `name` in `dir`, with a name no
/// other save uses: the process id, a per-process sequence number and the
/// time, opened with `create_new` (and a fresh name on a collision).
fn create_temp(dir: &Path, name: &std::ffi::OsStr) -> io::Result<(PathBuf, fs::File)> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let mut attempts = 0;
    loop {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let mut tmp_name = temp_prefix(name);
        tmp_name.push(format!(
            "{}-{}-{nanos}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let tmp = dir.join(tmp_name);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(f) => return Ok((tmp, f)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempts < 16 => attempts += 1,
            Err(e) => return Err(e),
        }
    }
}

/// Removes, best effort, the temporary files of saves of `name` made by
/// processes that no longer run (they died during a save). The temporary
/// files of running processes, this one included, are left alone: their
/// saves may be in progress.
fn remove_stale_temps(dir: &Path, name: &std::ffi::OsStr) {
    let prefix = temp_prefix(name);
    let prefix = prefix.to_string_lossy();
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(rest) = file_name.to_str().and_then(|n| n.strip_prefix(&*prefix)) else {
            continue;
        };
        let Some(pid) = rest.split('-').next().and_then(|p| p.parse::<u32>().ok()) else {
            continue;
        };
        if pid != std::process::id() && !Path::new(&format!("/proc/{pid}")).exists() {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// `$XDG_CONFIG_HOME/omarchy-broadcast/config.toml`, else
/// `$HOME/.config/omarchy-broadcast/config.toml`. `None` with neither set.
pub fn config_path() -> Option<PathBuf> {
    config_path_in(|k| std::env::var_os(k))
}

/// `$XDG_STATE_HOME/omarchy-broadcast/state.toml`, else
/// `$HOME/.local/state/omarchy-broadcast/state.toml`. `None` with neither set.
pub fn state_path() -> Option<PathBuf> {
    state_path_in(|k| std::env::var_os(k))
}

/// [`config_path`] with the environment looked up through `env`.
pub fn config_path_in(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    xdg_dir(&env, "XDG_CONFIG_HOME", ".config").map(|d| d.join(APP_DIR).join("config.toml"))
}

/// [`state_path`] with the environment looked up through `env`.
pub fn state_path_in(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    xdg_dir(&env, "XDG_STATE_HOME", ".local/state").map(|d| d.join(APP_DIR).join("state.toml"))
}

/// An XDG base directory: the variable if it holds an absolute path (the
/// specification says relative ones are ignored), else `$HOME/<fallback>`.
fn xdg_dir(env: &impl Fn(&str) -> Option<OsString>, var: &str, fallback: &str) -> Option<PathBuf> {
    if let Some(dir) = env(var).map(PathBuf::from).filter(|p| p.is_absolute()) {
        return Some(dir);
    }
    env("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map(|h| h.join(fallback))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UsbId;

    const EXAMPLE: &str = include_str!("../../../docs/config.example.toml");

    fn parse(text: &str) -> Result<Config, ConfigError> {
        Config::parse(
            text,
            Path::new("/home/u/.config/omarchy-broadcast/config.toml"),
        )
    }

    fn parse_err(text: &str) -> String {
        parse(text).unwrap_err().to_string()
    }

    #[test]
    fn defaults() {
        let c = parse("").unwrap();
        assert_eq!(c, Config::default());
        assert_eq!(c.version, 1);
        assert!(c.camera_preference().is_empty());
        assert_eq!(c.camera_mode().to_string(), "1920x1080@60");
        assert!(!c.camera.fixed_framerate);
        assert_eq!(c.output_contract(), OutputContract::default());
        assert_eq!(c.output.aspect, AspectPolicy::Crop);
        assert_eq!(c.hold_timeout(), Duration::from_secs(3));
        assert_eq!(c.input_gap(), Duration::from_millis(500));
        assert_eq!(c.sink_selector(), SinkSelector::default());
        assert_eq!(c.idle_grace(), Duration::from_secs(5));
        assert_eq!(c.start_debounce(), Duration::from_millis(300));
        assert!(!c.pipewire.enabled);
        assert!(c.pipewire.offer_nv12);
        c.validate().unwrap();
    }

    #[test]
    fn full_file_and_round_trip() {
        let text = r#"
version = 1

[camera]
preference = ["Brio", "046d:0944", "/dev/video4"]
mode = "MJPG:1280x720@30000/1001"
fixed_framerate = true

[output]
mode = "1280x720@60"
format = "i420"
aspect = "letterbox"
hold_timeout_s = 1.5
input_gap_ms = 250

[loopback]
label = "My Cam"
device = "/dev/video11"

[lifecycle]
idle_grace_s = 0
start_debounce_ms = 0

[effect]
mode = "replace"
blur_strength = 1
background_image = "/home/u/Pictures/office.png"
denoise = true
on_failure = "none"

[pipewire]
enabled = true
offer_nv12 = false
"#;
        let c = parse(text).unwrap();
        assert_eq!(
            c.camera_preference(),
            vec![
                CameraSelector::Name("Brio".into()),
                CameraSelector::Usb(UsbId {
                    vendor: 0x046d,
                    product: 0x0944
                }),
                CameraSelector::Path("/dev/video4".into()),
            ]
        );
        assert_eq!(c.camera_mode().to_string(), "MJPEG:1280x720@30000/1001");
        assert!(c.camera.fixed_framerate);
        let out = c.output_contract();
        assert_eq!(out.format, OutputFormat::I420);
        assert_eq!(
            (out.width, out.height, out.fps_num, out.fps_den),
            (1280, 720, 60, 1)
        );
        assert_eq!(out.range, ColourRange::Limited);
        assert_eq!(c.output.aspect, AspectPolicy::Letterbox);
        assert_eq!(c.hold_timeout(), Duration::from_millis(1500));
        assert_eq!(c.input_gap(), Duration::from_millis(250));
        assert_eq!(c.sink_selector(), SinkSelector::Path("/dev/video11".into()));
        assert_eq!(c.loopback.label, "My Cam");
        assert_eq!(c.idle_grace(), Duration::ZERO);
        assert_eq!(c.start_debounce(), Duration::ZERO);
        assert_eq!(
            c.effect,
            EffectConfig {
                mode: EffectMode::Replace,
                blur_strength: 1.0,
                background_image: Some("/home/u/Pictures/office.png".into()),
                denoise: true,
                on_failure: EffectOnFailure::None,
            }
        );
        assert_eq!(
            c.pipewire,
            PipeWireConfig {
                enabled: true,
                offer_nv12: false
            }
        );

        let again = parse(&c.to_toml()).unwrap();
        assert_eq!(again, c);
        assert_eq!(
            parse(&Config::default().to_toml()).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn example_file_is_the_defaults() {
        // As shipped, docs/config.example.toml parses to the defaults.
        assert_eq!(parse(EXAMPLE).unwrap(), Config::default());
        // Its commented-out keys are valid too: uncommented, the camera
        // preference lists one entry of each form.
        let uncommented: String = EXAMPLE
            .lines()
            .map(|l| match l.strip_prefix("# ") {
                Some(rest)
                    if rest.split_once(" = ").is_some_and(|(k, _)| {
                        k.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                    }) =>
                {
                    format!("{rest}\n")
                }
                _ => format!("{l}\n"),
            })
            .collect();
        let c = parse(&uncommented).unwrap();
        let mut expected = Config::default();
        expected.camera.preference = vec![
            CameraSelector::Name("Brio".into()),
            CameraSelector::Usb(UsbId {
                vendor: 0x046d,
                product: 0x0944,
            }),
            CameraSelector::Path("/dev/v4l/by-id/usb-046d_MX_Brio-video-index0".into()),
        ];
        expected.loopback.device = Some("/dev/video11".into());
        expected.effect.background_image = Some("/home/user/Pictures/background.png".into());
        assert_eq!(c, expected);
        // Every key of the schema appears in the example.
        for key in [
            "version",
            "preference",
            "mode",
            "fixed_framerate",
            "format",
            "aspect",
            "hold_timeout_s",
            "input_gap_ms",
            "label",
            "device",
            "idle_grace_s",
            "start_debounce_ms",
            "blur_strength",
            "background_image",
            "denoise",
            "on_failure",
            "enabled",
            "offer_nv12",
        ] {
            assert!(
                EXAMPLE.lines().any(|l| l
                    .trim_start_matches(['#', ' '])
                    .starts_with(&format!("{key} ="))),
                "{key} missing from docs/config.example.toml"
            );
        }
    }

    #[test]
    fn unknown_key_is_refused_with_position() {
        let e = parse("version = 1\n\n[camera]\nmdoe = \"1280x720@30\"\n").unwrap_err();
        match &e {
            ConfigError::Parse {
                line,
                column,
                key,
                message,
                ..
            } => {
                assert_eq!(*line, Some(4));
                assert_eq!(*column, Some(1));
                assert_eq!(key.as_deref(), Some("camera.mdoe"));
                assert!(message.contains("unknown field `mdoe`"), "{message}");
            }
            other => panic!("expected a parse error, got {other:?}"),
        }
        let s = e.to_string();
        assert!(
            s.starts_with("/home/u/.config/omarchy-broadcast/config.toml:4:1: camera.mdoe: "),
            "{s}"
        );
        assert!(!s.contains('\n'), "one line: {s}");
        assert!(parse_err("[cmaera]\n").contains("unknown field `cmaera`"));
        let e = parse_err("[pipewire]\nenable = true\n");
        assert!(
            e.contains(":2:1: pipewire.enable: ") && e.contains("unknown field `enable`"),
            "{e}"
        );
        assert!(parse_err("[pipe_wire]\n").contains("unknown field `pipe_wire`"));
        assert!(parse_err("colour = 1\n").contains("unknown field `colour`"));
    }

    #[test]
    fn invalid_values_name_the_key_and_line() {
        let cases: &[(&str, &str, &str)] = &[
            ("version = 2", "version", "unsupported config version 2"),
            (
                "[camera]\nmode = \"fast\"",
                "camera.mode",
                "invalid video mode",
            ),
            (
                "[camera]\npreference = [\"\"]",
                "camera.preference",
                "empty camera selector",
            ),
            (
                "[camera]\nfixed_framerate = \"yes\"",
                "camera.fixed_framerate",
                "invalid type",
            ),
            ("[output]\nmode = \"1921x1080@30\"", "output.mode", "even"),
            (
                "[output]\nmode = \"1920x1080@500\"",
                "output.mode",
                "at most 240",
            ),
            (
                "[output]\nmode = \"NV12:1920x1080@30\"",
                "output.mode",
                "invalid output contract",
            ),
            (
                "[output]\nformat = \"yuyv\"",
                "output.format",
                "unsupported output format",
            ),
            (
                "[output]\naspect = \"stretch\"",
                "output.aspect",
                "unsupported aspect policy",
            ),
            (
                "[output]\nhold_timeout_s = 0",
                "output.hold_timeout_s",
                "greater than 0",
            ),
            (
                "[output]\nhold_timeout_s = -1.0",
                "output.hold_timeout_s",
                "greater than 0",
            ),
            (
                "[output]\nhold_timeout_s = 61",
                "output.hold_timeout_s",
                "at most 60",
            ),
            (
                "[output]\nhold_timeout_s = nan",
                "output.hold_timeout_s",
                "greater than 0",
            ),
            (
                "[output]\ninput_gap_ms = 0",
                "output.input_gap_ms",
                "between 1 and 60000",
            ),
            (
                "[output]\ninput_gap_ms = -5",
                "output.input_gap_ms",
                "invalid value",
            ),
            (
                "[loopback]\nlabel = \" \"",
                "loopback.label",
                "must not be empty",
            ),
            (
                "[loopback]\ndevice = \"video11\"",
                "loopback.device",
                "under /dev/",
            ),
            (
                "[lifecycle]\nidle_grace_s = -0.5",
                "lifecycle.idle_grace_s",
                "between 0 and 3600",
            ),
            (
                "[lifecycle]\nidle_grace_s = 4000",
                "lifecycle.idle_grace_s",
                "between 0 and 3600",
            ),
            (
                "[lifecycle]\nstart_debounce_ms = 20000",
                "lifecycle.start_debounce_ms",
                "between 0 and 10000",
            ),
            (
                "[lifecycle]\nidle_grace_s = \"5\"",
                "lifecycle.idle_grace_s",
                "expected a number of seconds",
            ),
            (
                "[effect]\nmode = \"sparkle\"",
                "effect.mode",
                "unknown effect \"sparkle\"",
            ),
            (
                "[effect]\nblur_strength = 1.5",
                "effect.blur_strength",
                "between 0 and 1",
            ),
            (
                "[effect]\nblur_strength = -0.1",
                "effect.blur_strength",
                "between 0 and 1",
            ),
            (
                "[effect]\nblur_strength = nan",
                "effect.blur_strength",
                "between 0 and 1",
            ),
            (
                "[effect]\nblur_strength = \"0.5\"",
                "effect.blur_strength",
                "expected a number from 0 to 1",
            ),
            (
                "[effect]\nbackground_image = \"office.png\"",
                "effect.background_image",
                "absolute path",
            ),
            (
                "[effect]\ndenoise = \"yes\"",
                "effect.denoise",
                "invalid type",
            ),
            (
                "[effect]\non_failure = \"raw\"",
                "effect.on_failure",
                "unsupported effect failure policy",
            ),
            (
                "[effect]\nmode = \"replace\"",
                "effect.mode",
                "needs a background image",
            ),
            (
                "[effect]\nbackground_image = \"\"\nmode = \"replace\"",
                "effect.mode",
                "needs a background image",
            ),
            (
                "[pipewire]\nenabled = \"yes\"",
                "pipewire.enabled",
                "invalid type",
            ),
            (
                "[pipewire]\noffer_nv12 = 1",
                "pipewire.offer_nv12",
                "invalid type",
            ),
        ];
        for (text, key, reason) in cases {
            let e = parse(text).unwrap_err();
            let s = e.to_string();
            match &e {
                ConfigError::Parse { line, key: k, .. } => {
                    assert_eq!(k.as_deref(), Some(*key), "{text}: {s}");
                    assert_eq!(*line, Some(text.lines().count()), "{text}: {s}");
                }
                other => panic!("{text}: expected a parse error, got {other:?}"),
            }
            assert!(s.contains(reason), "{text}: {s}");
            assert!(
                s.starts_with("/home/u/.config/omarchy-broadcast/config.toml:"),
                "{s}"
            );
        }
        assert!(parse_err("[output\n").contains("config.toml:1:"));
    }

    #[test]
    fn validate_checks_configs_built_in_code() {
        let mut c = Config::default();
        c.output.hold_timeout_s = 0.0;
        assert!(matches!(
            c.validate(),
            Err(ConfigError::Invalid {
                key: "output.hold_timeout_s",
                ..
            })
        ));
        let mut c = Config::default();
        c.output.mode.width = 1921;
        let e = c.validate().unwrap_err();
        assert!(e.to_string().starts_with("output.mode: "), "{e}");
        let c = Config {
            version: 7,
            ..Config::default()
        };
        assert!(c.validate().is_err());
        let mut c = Config::default();
        c.lifecycle.start_debounce_ms = 10_001;
        assert!(c.validate().is_err());
        let mut c = Config::default();
        c.effect.blur_strength = f64::INFINITY;
        assert!(matches!(
            c.validate(),
            Err(ConfigError::Invalid {
                key: "effect.blur_strength",
                ..
            })
        ));
        let mut c = Config::default();
        c.effect.mode = EffectMode::Replace;
        let e = c.validate().unwrap_err();
        assert!(e.to_string().starts_with("effect.mode: "), "{e}");
        c.effect.background_image = Some("relative.png".into());
        assert!(matches!(
            c.validate(),
            Err(ConfigError::Invalid {
                key: "effect.background_image",
                ..
            })
        ));
        c.effect.background_image = Some("/a/b.png".into());
        c.validate().unwrap();
    }

    #[test]
    fn effect_defaults_and_a_cross_key_error_names_the_file_and_line() {
        let c = parse("[effect]\n").unwrap();
        assert_eq!(c.effect, EffectConfig::default());
        assert_eq!(c.effect.mode, EffectMode::None);
        assert_eq!(c.effect.blur_strength, 0.5);
        assert_eq!(c.effect.background_image, None);
        assert!(!c.effect.denoise);
        assert_eq!(c.effect.on_failure, EffectOnFailure::Black);
        // An empty image is the default, and is not written back.
        let c = parse("[effect]\nbackground_image = \"\"\n").unwrap();
        assert_eq!(c.effect.background_image, None);
        assert!(!c.to_toml().contains("background_image"));
        // A file that does not exist is accepted here: it is a run-time
        // failure of the effect.
        let c =
            parse("[effect]\nmode = \"replace\"\nbackground_image = \"/nowhere/x.png\"\n").unwrap();
        assert_eq!(parse(&c.to_toml()).unwrap(), c);
        let e = parse_err("version = 1\n\n[effect]\nblur_strength = 0.2\nmode = \"replace\"\n");
        assert!(
            e.starts_with("/home/u/.config/omarchy-broadcast/config.toml:5: effect.mode: "),
            "{e}"
        );
        assert!(!parse_err("[effect]\nstrength = 1\n").is_empty());
        assert!(parse_err("[effect]\nstrength = 1\n").contains("unknown field `strength`"));
    }

    #[test]
    fn missing_file_is_the_defaults_and_unreadable_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope/config.toml");
        assert_eq!(Config::load(&missing).unwrap(), Config::default());

        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[output]\nformat = \"nv12\"\nmode = \"1280x720@30\"\n",
        )
        .unwrap();
        assert_eq!(Config::load(&path).unwrap().output.mode.width, 1280);

        fs::write(&path, "[output]\nmode = 3\n").unwrap();
        let e = Config::load(&path).unwrap_err().to_string();
        assert!(e.starts_with(&format!("{}:2:", path.display())), "{e}");

        // A directory where the file should be: present but unreadable.
        let e = Config::load(dir.path()).unwrap_err();
        assert!(matches!(e, ConfigError::Read { .. }), "{e:?}");
    }

    #[test]
    fn state_saves_atomically_and_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep/er/state.toml");

        assert_eq!(RuntimeState::load(&path), (RuntimeState::default(), None));

        let st = RuntimeState {
            enabled: false,
            camera: Some("usb-046d_0944".into()),
            effect: Some(EffectMode::Blur),
            blur_strength: Some(0.25),
            background_image: Some("/home/u/bg.png".into()),
            denoise: Some(true),
            camera_controls: BTreeMap::new(),
        };
        st.save(&path).unwrap();
        assert_eq!(RuntimeState::load(&path), (st.clone(), None));
        // Only the state file is left: the temporary file was renamed.
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![OsString::from("state.toml")]);

        // Overwrite with fewer keys.
        RuntimeState::default().save(&path).unwrap();
        assert_eq!(RuntimeState::load(&path), (RuntimeState::default(), None));
        assert_eq!(fs::read_to_string(&path).unwrap().trim(), "enabled = true");
    }

    fn controls(values: &[(&str, i32)], reset_pending: bool) -> CameraControlState {
        CameraControlState {
            values: values.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            reset_pending,
        }
    }

    #[test]
    fn camera_controls_round_trip_in_their_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let mut st = RuntimeState {
            camera: Some("usb-046d_0944-video-index0".into()),
            ..RuntimeState::default()
        };
        st.camera_controls.insert(
            "usb-046d_0944-video-index0".into(),
            controls(&[("sharpness", 128), ("saturation", 128)], false),
        );
        st.camera_controls.insert(
            "pci-0000:00:14.0-usb-0:2:1.0-video-index0".into(),
            controls(&[], true),
        );
        st.camera_controls.insert(
            "platform-x".into(),
            controls(&[("brightness", -64), ("backlight_compensation", 0)], true),
        );
        st.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "enabled = true\n\
             camera = \"usb-046d_0944-video-index0\"\n\
             \n\
             [camera_controls.\"pci-0000:00:14.0-usb-0:2:1.0-video-index0\"]\n\
             reset_pending = true\n\
             \n\
             [camera_controls.\"platform-x\"]\n\
             values = { backlight_compensation = 0, brightness = -64 }\n\
             reset_pending = true\n\
             \n\
             [camera_controls.\"usb-046d_0944-video-index0\"]\n\
             values = { saturation = 128, sharpness = 128 }\n"
        );
        assert_eq!(RuntimeState::load_noting(&path), (st.clone(), None, vec![]));

        // An id that needs escaping still round-trips.
        let mut odd = RuntimeState::default();
        odd.camera_controls.insert(
            "a \"quoted\" id\\ \u{e9}".into(),
            controls(&[("contrast", 3)], false),
        );
        odd.save(&path).unwrap();
        assert_eq!(RuntimeState::load_noting(&path), (odd, None, vec![]));
        // Every id the toml crate would quote otherwise (a literal string,
        // or a multi-line one, which is no valid key), all in one file.
        let mut odd = RuntimeState::default();
        let ids = [
            "usb-plain",
            "it's",
            "say \"hi\"",
            "both ' and \"",
            "both ' and \" and\na newline",
            "line\nbreak",
            "tab\there",
            "back\\slash \\\" mixed",
            "c\u{1}ontrol \u{7f} \u{85} \r \u{8} \u{c}",
            "caf\u{e9} \u{1f4f7} \u{202e}",
            "'''triple''' \"\"\"quotes\"\"\"",
            "",
        ];
        for (i, id) in ids.iter().enumerate() {
            odd.camera_controls.insert(
                id.to_string(),
                controls(&[("contrast", i as i32)], i % 2 == 0),
            );
        }
        odd.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(toml::from_str::<toml::Table>(&text).is_ok(), "{text}");
        assert_eq!(
            text.lines()
                .filter(|l| l.starts_with("[camera_controls."))
                .count(),
            ids.len(),
            "one header line each: {text}"
        );
        assert!(text.contains("[camera_controls.\"usb-plain\"]\n"), "{text}");
        assert_eq!(RuntimeState::load_noting(&path), (odd, None, vec![]));

        // No entry: no table at all; an empty entry is not written.
        let mut empty = RuntimeState::default();
        empty
            .camera_controls
            .insert("usb-x".into(), CameraControlState::default());
        empty.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.trim(), "enabled = true");
        assert_eq!(RuntimeState::load(&path), (RuntimeState::default(), None));
        RuntimeState::default().save(&path).unwrap();
        assert!(
            !fs::read_to_string(&path)
                .unwrap()
                .contains("camera_controls")
        );

        // The header form serde would write reads the same.
        fs::write(
            &path,
            "enabled = true\n[camera_controls.usb-x]\nreset_pending = false\n\
             [camera_controls.usb-x.values]\nsharpness = 0\n",
        )
        .unwrap();
        let (got, damaged, notes) = RuntimeState::load_noting(&path);
        assert_eq!((damaged, notes), (None, vec![]));
        assert_eq!(
            got.camera_controls["usb-x"],
            controls(&[("sharpness", 0)], false)
        );
    }

    #[test]
    fn bad_camera_control_entries_are_dropped_and_never_damage_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        fs::write(
            &path,
            r#"enabled = false
effect = "blur"

[camera_controls."usb-good"]
values = { sharpness = 128, zoom_absolute = 3, contrast = "high", saturation = 4294967296, brightness = -2147483648 }
reset_pending = "yes"
future_key = 1

[camera_controls."usb-only-bad"]
values = { exposure_dynamic_framerate = 0 }

[camera_controls."usb-pending"]
values = 5
reset_pending = true

[camera_controls."usb-nothing"]
reset_pending = false
"#,
        )
        .unwrap();
        let (st, damaged, notes) = RuntimeState::load_noting(&path);
        assert_eq!(damaged, None, "never damaged");
        // The rest of the file is read as usual.
        assert!(!st.enabled);
        assert_eq!(st.effect, Some(EffectMode::Blur));
        let mut want = BTreeMap::new();
        want.insert(
            "usb-good".to_string(),
            controls(&[("sharpness", 128), ("brightness", i32::MIN)], false),
        );
        want.insert("usb-pending".to_string(), controls(&[], true));
        assert_eq!(st.camera_controls, want);
        let all = notes.join("\n");
        for part in [
            "\"zoom_absolute\" is not a camera control",
            "contrast must be an integer (found string)",
            "saturation = 4294967296 does not fit 32 bits",
            "reset_pending must be a boolean (found string)",
            "\"exposure_dynamic_framerate\" is not a camera control",
            "values must be a table (found integer)",
        ] {
            assert!(all.contains(part), "{part:?} not in {all}");
        }
        assert_eq!(notes.len(), 6, "{all}");
        assert!(!all.contains("  "), "no run of spaces: {all}");
        assert!(
            notes
                .iter()
                .all(|n| n.starts_with(&path.display().to_string()))
        );
        // `load` reads the same, without the notes.
        assert_eq!(RuntimeState::load(&path), (st.clone(), None));

        // A camera_controls that is not a table, or an entry that is not one.
        fs::write(&path, "camera_controls = 3\n").unwrap();
        let (st, damaged, notes) = RuntimeState::load_noting(&path);
        assert_eq!((st, damaged), (RuntimeState::default(), None));
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].contains("camera_controls must be a table (found integer)"),
            "{notes:?}"
        );
        fs::write(&path, "[camera_controls]\nusb-a = \"x\"\n").unwrap();
        let (st, damaged, notes) = RuntimeState::load_noting(&path);
        assert_eq!((st, damaged), (RuntimeState::default(), None));
        assert!(
            notes[0].contains("must be a table (found string)"),
            "{notes:?}"
        );

        // A damaged file elsewhere is still damaged.
        fs::write(
            &path,
            "enabled = maybe\n[camera_controls.a]\nreset_pending = true\n",
        )
        .unwrap();
        let (st, damaged, notes) = RuntimeState::load_noting(&path);
        assert_eq!(st, RuntimeState::default());
        assert!(damaged.is_some());
        assert!(notes.is_empty());
    }

    #[test]
    fn concurrent_saves_leave_one_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let states: Vec<RuntimeState> = (0..8)
            .map(|i| RuntimeState {
                enabled: i % 2 == 0,
                camera: Some(format!("camera-{i}-{}", "x".repeat(i * 512))),
                ..RuntimeState::default()
            })
            .collect();
        std::thread::scope(|scope| {
            for st in &states {
                let path = &path;
                scope.spawn(move || {
                    for _ in 0..50 {
                        st.save(path).unwrap();
                    }
                });
            }
        });
        let (got, warning) = RuntimeState::load(&path);
        assert_eq!(warning, None);
        assert!(states.contains(&got), "{got:?}");
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![OsString::from("state.toml")]);
    }

    #[test]
    fn a_save_removes_the_temporary_files_of_dead_processes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        // No process has the pid `pid_max`: pids stay below it.
        let pid_max: u32 = fs::read_to_string("/proc/sys/kernel/pid_max")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(4_194_304);
        let dead = dir.path().join(format!(".state.toml.tmp-{pid_max}-0-0"));
        let ours = dir
            .path()
            .join(format!(".state.toml.tmp-{}-999999-0", std::process::id()));
        let foreign = dir.path().join(format!(".other.toml.tmp-{pid_max}-0-0"));
        for p in [&dead, &ours, &foreign] {
            fs::write(p, "partial").unwrap();
        }
        RuntimeState::default().save(&path).unwrap();
        assert!(!dead.exists(), "a dead process's temporary file stays");
        assert!(ours.exists(), "a running process's save may be in progress");
        assert!(
            foreign.exists(),
            "only temporary files of this state file go"
        );
        assert_eq!(RuntimeState::load(&path), (RuntimeState::default(), None));
    }

    #[test]
    fn a_failed_save_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        // A directory in the place of the state file: the rename fails.
        let path = dir.path().join("state.toml");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("keep"), "").unwrap();
        assert!(RuntimeState::default().save(&path).is_err());
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![OsString::from("state.toml")]);
    }

    #[test]
    fn corrupt_state_is_the_defaults_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        fs::write(&path, "enabled = maybe\n").unwrap();
        let (st, warning) = RuntimeState::load(&path);
        assert_eq!(st, RuntimeState::default());
        let w = warning.expect("a warning");
        assert!(w.contains("state.toml:1:"), "{w}");
        assert!(w.ends_with("using the default state"), "{w}");

        // Unknown keys (a newer daemon's) are not an error.
        fs::write(&path, "enabled = false\nfuture = 1\n").unwrap();
        let (st, warning) = RuntimeState::load(&path);
        assert!(!st.enabled);
        assert_eq!(warning, None);

        // Unreadable: a directory in its place.
        let (st, warning) = RuntimeState::load(dir.path());
        assert_eq!(st, RuntimeState::default());
        assert!(warning.is_some());
    }

    #[test]
    fn state_effect_keys_round_trip_and_override_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let st = RuntimeState {
            effect: Some(EffectMode::Replace),
            background_image: Some("/state/bg.png".into()),
            ..RuntimeState::default()
        };
        st.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("effect = \"replace\""), "{text}");
        assert!(
            text.contains("background_image = \"/state/bg.png\""),
            "{text}"
        );
        assert_eq!(RuntimeState::load(&path), (st.clone(), None));
        // No image override: no key written.
        let cleared = RuntimeState {
            effect: Some(EffectMode::Replace),
            ..RuntimeState::default()
        };
        cleared.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("background_image"), "{text}");
        // An empty image in the file (as an older daemon wrote for a
        // cleared one) is no override either.
        fs::write(&path, "enabled = true\nbackground_image = \"\"\n").unwrap();
        let (read, warning) = RuntimeState::load(&path);
        assert_eq!(warning, None);
        assert_eq!(read.background_image, None);

        let config = EffectConfig {
            mode: EffectMode::Blur,
            blur_strength: 0.8,
            background_image: Some("/cfg/bg.png".into()),
            denoise: true,
            on_failure: EffectOnFailure::None,
        };
        // Nothing set: the config.
        assert_eq!(RuntimeState::default().effect_over(&config), config);
        // Each key set: the state's value.
        let over = RuntimeState {
            effect: Some(EffectMode::None),
            blur_strength: Some(0.1),
            background_image: Some("/state/bg.png".into()),
            denoise: Some(false),
            ..RuntimeState::default()
        }
        .effect_over(&config);
        assert_eq!(
            over,
            EffectConfig {
                mode: EffectMode::None,
                blur_strength: 0.1,
                background_image: Some("/state/bg.png".into()),
                denoise: false,
                on_failure: EffectOnFailure::None,
            }
        );
        // No image in the state: the config's image applies.
        assert_eq!(
            cleared.effect_over(&config).background_image,
            Some("/cfg/bg.png".into())
        );
        assert_eq!(cleared.effect_over(&config).mode, EffectMode::Replace);
        assert_eq!(
            read.effect_over(&config).background_image,
            Some("/cfg/bg.png".into())
        );
        let empty = RuntimeState {
            background_image: Some(PathBuf::new()),
            ..RuntimeState::default()
        };
        assert_eq!(
            empty.effect_over(&config).background_image,
            Some("/cfg/bg.png".into())
        );
        // An integer strength is a number too.
        fs::write(&path, "enabled = true\nblur_strength = 1\n").unwrap();
        assert_eq!(RuntimeState::load(&path).0.blur_strength, Some(1.0));
        // Denoise: written only while set, and an override in either
        // direction.
        assert!(!text.contains("denoise"), "{text}");
        let on = RuntimeState {
            denoise: Some(true),
            ..RuntimeState::default()
        };
        on.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("denoise = true"), "{text}");
        assert_eq!(RuntimeState::load(&path), (on.clone(), None));
        let config_off = EffectConfig::default();
        assert!(on.effect_over(&config_off).denoise);
        assert!(RuntimeState::default().effect_over(&config).denoise);
    }

    #[test]
    fn an_invalid_effect_value_damages_the_whole_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        for text in [
            "enabled = true\neffect = \"sparkle\"\n",
            "enabled = true\neffect = 1\n",
            "enabled = true\nblur_strength = 2.0\n",
            "enabled = true\nblur_strength = nan\n",
            "enabled = true\nbackground_image = \"bg.png\"\n",
            "enabled = true\ndenoise = \"yes\"\n",
            "enabled = true\ndenoise = 1\n",
        ] {
            fs::write(&path, text).unwrap();
            let (st, warning) = RuntimeState::load(&path);
            // The defaults plus a warning: the daemon then fails closed
            // (off), it never runs without the effect it could not read.
            assert_eq!(st, RuntimeState::default(), "{text}");
            let w = warning.unwrap_or_else(|| panic!("no warning for {text:?}"));
            assert!(w.contains("state.toml:2:"), "{w}");
        }
    }

    #[test]
    fn xdg_paths() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| OsString::from(v))
            }
        };
        let xdg = env(&[
            ("XDG_CONFIG_HOME", "/x/cfg"),
            ("XDG_STATE_HOME", "/x/state"),
            ("HOME", "/home/u"),
        ]);
        assert_eq!(
            config_path_in(xdg),
            Some(PathBuf::from("/x/cfg/omarchy-broadcast/config.toml"))
        );
        assert_eq!(
            state_path_in(xdg),
            Some(PathBuf::from("/x/state/omarchy-broadcast/state.toml"))
        );
        let home = env(&[("HOME", "/home/u"), ("XDG_CONFIG_HOME", "relative/ignored")]);
        assert_eq!(
            config_path_in(home),
            Some(PathBuf::from(
                "/home/u/.config/omarchy-broadcast/config.toml"
            ))
        );
        assert_eq!(
            state_path_in(home),
            Some(PathBuf::from(
                "/home/u/.local/state/omarchy-broadcast/state.toml"
            ))
        );
        let none = env(&[]);
        assert_eq!(config_path_in(none), None);
        assert_eq!(state_path_in(none), None);
    }

    #[test]
    fn selector_form_parses_back() {
        for s in ["Brio", "046d:0944", "/dev/video4", "/dev/v4l/by-id/usb-x"] {
            let sel: CameraSelector = s.parse().unwrap();
            assert_eq!(selector_form(&sel), s);
        }
    }
}
