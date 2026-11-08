//! The daemon's configuration file and its runtime state file (see
//! `docs/adr/0011-daemon-lifecycle-and-dbus-api.md`, "Configuration and
//! state").
//!
//! - [`Config`]: `$XDG_CONFIG_HOME/oma-broadcast/config.toml`, what the user
//!   decides. Versioned (`version = 1`), every key optional, unknown keys
//!   refused. The daemon reads it and never writes it. The commented
//!   reference is `docs/config.example.toml`.
//! - [`RuntimeState`]: `$XDG_STATE_HOME/oma-broadcast/state.toml`, what
//!   changes at run time (`Enabled`, the selected camera). Written
//!   atomically by the daemon; a damaged file falls back to the defaults.

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    AspectPolicy, CameraSelector, ColourRange, DEFAULT_SINK_LABEL, ModeRequest, OutputContract,
    OutputFormat, SinkSelector, fmt_size_rate,
};

/// Config schema version this build reads.
pub const CONFIG_VERSION: u32 = 1;

/// Directory name under the XDG base directories.
pub const APP_DIR: &str = "oma-broadcast";

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

impl Default for Config {
    fn default() -> Self {
        Config {
            version: CONFIG_VERSION,
            camera: CameraConfig::default(),
            output: OutputConfig::default(),
            loopback: LoopbackConfig::default(),
            lifecycle: LifecycleConfig::default(),
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
        // Every key is checked while deserializing; this only catches what
        // spans keys (nothing yet) and keeps the two paths in agreement.
        config.validate()?;
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
        Ok(())
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
    struct Secs;
    impl serde::de::Visitor<'_> for Secs {
        type Value = f64;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a number of seconds")
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
    let v = d.deserialize_any(Secs)?;
    check(v).map_err(serde::de::Error::custom)?;
    Ok(v)
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

/// What the daemon changes at run time and keeps across restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeState {
    /// `Enabled` (false = the privacy state `off`).
    pub enabled: bool,
    /// The camera chosen with `SelectCamera` / `NextCamera` (its id), if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub camera: Option<String>,
}

impl Default for RuntimeState {
    fn default() -> Self {
        RuntimeState {
            enabled: true,
            camera: None,
        }
    }
}

impl RuntimeState {
    /// Reads `path`. Never fails: a missing file is the defaults, and a file
    /// that cannot be read or parsed is the defaults plus a warning saying
    /// why.
    pub fn load(path: &Path) -> (RuntimeState, Option<String>) {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return (RuntimeState::default(), None);
            }
            Err(e) => {
                return (
                    RuntimeState::default(),
                    Some(format!("{}: {e}; using the default state", path.display())),
                );
            }
        };
        match toml::from_str::<RuntimeState>(&text) {
            Ok(state) => (state, None),
            Err(e) => {
                let err = parse_error(&e, &text, path);
                (
                    RuntimeState::default(),
                    Some(format!("{err}; using the default state")),
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
        let text = toml::to_string(self).map_err(io::Error::other)?;
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

/// `$XDG_CONFIG_HOME/oma-broadcast/config.toml`, else
/// `$HOME/.config/oma-broadcast/config.toml`. `None` with neither set.
pub fn config_path() -> Option<PathBuf> {
    config_path_in(|k| std::env::var_os(k))
}

/// `$XDG_STATE_HOME/oma-broadcast/state.toml`, else
/// `$HOME/.local/state/oma-broadcast/state.toml`. `None` with neither set.
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
        Config::parse(text, Path::new("/home/u/.config/oma-broadcast/config.toml"))
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
            s.starts_with("/home/u/.config/oma-broadcast/config.toml:4:1: camera.mdoe: "),
            "{s}"
        );
        assert!(!s.contains('\n'), "one line: {s}");
        assert!(parse_err("[cmaera]\n").contains("unknown field `cmaera`"));
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
                s.starts_with("/home/u/.config/oma-broadcast/config.toml:"),
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

    #[test]
    fn concurrent_saves_leave_one_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let states: Vec<RuntimeState> = (0..8)
            .map(|i| RuntimeState {
                enabled: i % 2 == 0,
                camera: Some(format!("camera-{i}-{}", "x".repeat(i * 512))),
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
            Some(PathBuf::from("/x/cfg/oma-broadcast/config.toml"))
        );
        assert_eq!(
            state_path_in(xdg),
            Some(PathBuf::from("/x/state/oma-broadcast/state.toml"))
        );
        let home = env(&[("HOME", "/home/u"), ("XDG_CONFIG_HOME", "relative/ignored")]);
        assert_eq!(
            config_path_in(home),
            Some(PathBuf::from("/home/u/.config/oma-broadcast/config.toml"))
        );
        assert_eq!(
            state_path_in(home),
            Some(PathBuf::from(
                "/home/u/.local/state/oma-broadcast/state.toml"
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
