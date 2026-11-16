//! The microphone, seen from the daemon (ADR 0029): the daemon is the
//! façade of the Omarchy Broadcast Mic and the single writer of its
//! settings, but runs none of it. Three processes share the work: the mic
//! host (`omarchy-broadcast-mic.service`, a PipeWire filter-chain with the
//! forwarding plugin), the audio worker (`omarchy-broadcast-audiod`, which
//! hosts the audio modules) and this daemon. The daemon never opens an
//! audio module's library and has no PipeWire code for the microphone: it
//! asks the worker, over its internal bus interface, and the service
//! manager.
//!
//! This module is the pure part, tested without a bus:
//!
//! - [`groups`]: the `mic.*` settings of the tree, after the camera's and
//!   the effects' groups, each with `surface` `mic`: the daemon's own
//!   `input` group (`mic.input.enabled`, `mic.input.device` and, while the
//!   worker reports one, `mic.input.hardware_gain_db`, which are the config
//!   keys `mic.enabled`, `mic.device` and `mic.hardware_gain_db`; the actions
//!   `mic.input.make_default` and `mic.input.restore_default`), then the
//!   audio modules' groups as the worker describes them ([`parse_groups`]:
//!   one group per id, the active provider's, decision G4), their values
//!   those of the state file.
//! - [`check`]: what a `SetSetting` of a `mic.*` id does ([`Write`]).
//! - [`snapshot`]: the audio settings snapshot the worker runs with, which
//!   the daemon writes to `audio-settings.json` beside the state file and
//!   pushes with `ApplySnapshot`.
//! - [`status`]: the `Mic*` properties from the mic host's unit state and
//!   the worker's status.
//! - [`merge_modules`]: `GetModules` with the worker's view of the audio
//!   modules.
//!
//! The asynchronous part (the worker's bus interface, the service manager,
//! the snapshot file) is [`agent`].

pub mod agent;

use std::collections::{BTreeMap, BTreeSet};

use obc_core::config::{Config, MIC_DEVICE_DEFAULT, RuntimeState};
use obc_core::config_d::{ConfigSet, MANAGED_REL};
use obc_core::modules::StoredValue;
use obc_ipc::settings::{MIC_ID_PREFIX, SCOPE_GLOBAL, SURFACE_MIC};
use obc_ipc::{
    Condition, ConditionOp, MIC_OFF, MicStatus, ModuleInfo, Setting, SettingKind, SettingValue,
    SettingsGroup, StageInfo,
};
use serde_json::{Value as Json, json};

use crate::settings::Refusal;

/// The mic host's unit (decision G1): enabled and started by `mic on`.
pub const HOST_UNIT: &str = "omarchy-broadcast-mic.service";
/// The filter-chain units of ADR 0014, which share the mic's node name: the
/// mic host is refused while one runs (decision G2).
pub const LEGACY_UNITS: [&str; 2] = [
    "omarchy-broadcast-mic@rnnoise.service",
    "omarchy-broadcast-mic@deepfilter.service",
];

/// The snapshot's file name, beside the state file.
pub const SNAPSHOT_FILE: &str = "audio-settings.json";
/// The snapshot's format version.
pub const SNAPSHOT_VERSION: u32 = 1;

/// The daemon's own group.
pub const INPUT: &str = "input";
/// Its label.
pub const INPUT_LABEL: &str = "Microphone";
/// Processing on (the config key [`KEY_ENABLED`]).
pub const ENABLED: &str = "mic.input.enabled";
/// The microphone captured (the config key [`KEY_DEVICE`]).
pub const DEVICE: &str = "mic.input.device";
/// The microphone's hardware capture gain, in whole dB (the config key
/// [`KEY_HARDWARE_GAIN`]): offered while the worker reports the control
/// ([`HardwareGain`]).
pub const HARDWARE_GAIN: &str = "mic.input.hardware_gain_db";
/// The action that makes the Omarchy Broadcast Mic the default microphone.
pub const MAKE_DEFAULT: &str = "mic.input.make_default";
/// The action that makes the previous default microphone the default
/// again.
pub const RESTORE_DEFAULT: &str = "mic.input.restore_default";
/// The config key of [`ENABLED`].
pub const KEY_ENABLED: &str = "mic.enabled";
/// The config key of [`DEVICE`].
pub const KEY_DEVICE: &str = "mic.device";
/// The config key of [`HARDWARE_GAIN`] (a float: the setting's whole dB
/// are one way of writing it).
pub const KEY_HARDWARE_GAIN: &str = "mic.hardware_gain_db";

/// The most options `mic.input.device` offers, `default` included.
pub const MAX_DEVICES: usize = 16;

// The bounds of what the worker describes, applied before the daemon
// publishes it (the worker is another process; its text is never trusted
// to be short or printable).

/// The most module groups taken from the worker.
pub const MAX_GROUPS: usize = 16;
/// The longest group id taken from the worker, in bytes.
pub const GROUP_ID_MAX: usize = 32;
/// The longest label (a group's, a setting's, a module's name), in bytes.
pub const LABEL_MAX: usize = crate::settings::LABEL_MAX;
/// The longest option label, in bytes.
pub const OPTION_LABEL_MAX: usize = crate::settings::OPTION_LABEL_MAX;
/// The longest option value, in bytes (the bound the CLI's JSON lines
/// keep, so a value is never cut short there): an option of a module's
/// with a longer one is left out, since a value cut short could not be
/// written back. The device options stay within it by being tokens
/// ([`device_token`]), never node names.
pub const OPTION_VALUE_MAX: usize = 32;
/// The longest device `node.name` taken from the worker, in bytes (what
/// the configuration accepts).
pub const DEVICE_NAME_MAX: usize = obc_core::config::limits::MAX_MIC_DEVICE_BYTES;
/// What the token of a device option starts with.
pub const DEVICE_TOKEN_PREFIX: &str = "pw-";
/// The hex characters of a device token, before any extension.
const DEVICE_TOKEN_HEX: usize = 12;
/// The longest hint or unit, in bytes.
pub const TAG_MAX: usize = 16;
/// The longest reason, status or short field (a version, a slot), in bytes.
pub const REASON_MAX: usize = crate::modules::ABSENT_REASON_MAX;
/// The longest description or path, in bytes.
pub const TEXT_MAX: usize = 256;
/// The longest `MicState`, in bytes.
pub const STATE_MAX: usize = 32;

/// Why the default-microphone actions are greyed while the mic host is
/// off.
pub const REASON_HOST_OFF: &str = "Turn the microphone on first";
/// Why the restore action is greyed until "Use as default microphone" was
/// used.
pub const REASON_NOT_DEFAULT: &str = "The Omarchy Broadcast Mic was not made the default here";

/// Whether `id` is a microphone setting id.
pub fn is_mic(id: &str) -> bool {
    id.starts_with(MIC_ID_PREFIX)
}

/// Whether `id` is one of the daemon's own `mic.input.*` settings.
fn is_input(id: &str) -> bool {
    id.strip_prefix(MIC_ID_PREFIX)
        .is_some_and(|r| r.starts_with(&format!("{INPUT}.")))
}

/// What the worker told about itself: the audio modules, their settings
/// and the devices; `known` once it answered.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalogue {
    /// The worker answered at least once (it is installed): the `input`
    /// group is shown from then on.
    pub known: bool,
    /// The modules' `mic.*` groups, one per id ([`parse_groups`]).
    pub groups: Vec<SettingsGroup>,
    /// The audio modules ([`parse_modules`]).
    pub modules: Vec<ModuleInfo>,
    /// The hardware sources, `(node.name, description)`.
    pub devices: Vec<(String, String)>,
    /// The captured microphone's hardware gain, from the worker's status
    /// ([`parse_hardware_gain`]); `None` when it reports none.
    pub hardware_gain: Option<HardwareGain>,
}

/// The hardware capture gain the worker reports (its status's
/// `hardware_gain`), in dB.
#[derive(Debug, Clone, PartialEq)]
pub struct HardwareGain {
    /// The ALSA control it is.
    pub control: String,
    /// The gain now.
    pub db: f64,
    /// The range's ends.
    pub min_db: f64,
    pub max_db: f64,
    /// The step (0 for a control of one value).
    pub step_db: f64,
    /// The device holds the configured value.
    pub applied: bool,
}

impl HardwareGain {
    /// The range in whole dB, `(min, max, step)`: the ends rounded inwards,
    /// the step always 1 (the worker rounds a value to the device's own
    /// step: a coarser setting step would refuse values off its grid, the
    /// one shown among them); `None` when no whole dB lies within it.
    pub fn whole_db(&self) -> Option<(i64, i64, i64)> {
        let (min, max) = (self.min_db.ceil(), self.max_db.floor());
        if min > max {
            return None;
        }
        // Within the bounds the parse keeps, so the casts are exact.
        Some((min as i64, max as i64, 1))
    }
}

/// What [`groups`] needs.
#[derive(Debug, Clone, Copy)]
pub struct Inputs<'a> {
    pub config: &'a Config,
    /// Where each key comes from (`stored`, and which are overridden).
    pub config_set: &'a ConfigSet,
    pub runtime: &'a RuntimeState,
    pub catalogue: &'a Catalogue,
    /// The mic host runs (`MicState` is not `off`).
    pub host_on: bool,
}

fn setting(id: &str, label: &str, kind: SettingKind) -> Setting {
    Setting {
        id: id.into(),
        label: label.into(),
        kind,
        scope: SCOPE_GLOBAL.into(),
        stored: false,
        enabled: true,
        disabled_reason: String::new(),
        enabled_when: Vec::new(),
        hint: None,
        description: None,
    }
}

/// A setting bound to the config key `key`: stored when the managed file
/// sets it, greyed when a later file does.
fn bound(mut s: Setting, key: &str, set: &ConfigSet) -> Setting {
    s.stored = set.source(key) == MANAGED_REL;
    let over = set.overridden_by(key);
    if !over.is_empty() {
        s.enabled = false;
        s.disabled_reason = format!("Overridden by {over}");
    }
    s
}

/// The 64-bit FNV-1a hash of `s`: stable across runs and processes (unlike
/// `std`'s hashers), so a device keeps its token.
fn fnv1a64(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// The token of the device `node_name` (`pw-` and 12 hex characters of its
/// hash), before any collision is settled ([`device_entries`]): short
/// enough for a CLI JSON line however long the node name is.
pub fn device_token(node_name: &str) -> String {
    format!(
        "{DEVICE_TOKEN_PREFIX}{}",
        &format!("{:016x}", fnv1a64(node_name))[..DEVICE_TOKEN_HEX]
    )
}

/// One option of `mic.input.device`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceEntry {
    /// The option value: `default` or a token.
    pub token: String,
    /// The `node.name` the config key stores (`default` for the first).
    pub name: String,
    /// What the user reads.
    pub label: String,
}

/// The options of `mic.input.device` with `hash` as the hash: `default`,
/// then each device (at most [`MAX_DEVICES`] in all), with `configured`
/// among them (marked unavailable when the worker does not list it). A
/// token already taken by an earlier option is extended with further hex
/// characters of the hash (then `-2`, `-3`...): the same list always gives
/// the same tokens.
fn device_entries_with(
    devices: &[(String, String)],
    configured: &str,
    hash: impl Fn(&str) -> u64,
) -> Vec<DeviceEntry> {
    let mut names: Vec<(&str, String)> = Vec::new();
    for (name, description) in devices {
        if name == MIC_DEVICE_DEFAULT || names.iter().any(|(n, _)| n == name) {
            continue;
        }
        let label = if description.is_empty() {
            name
        } else {
            description
        };
        names.push((name, clean(label, LABEL_MAX)));
    }
    let mut unavailable = None;
    if configured != MIC_DEVICE_DEFAULT && !names.iter().any(|(n, _)| *n == configured) {
        unavailable = Some((
            configured,
            clean(&format!("Unavailable: {configured}"), LABEL_MAX),
        ));
    }
    let mut out = vec![DeviceEntry {
        token: MIC_DEVICE_DEFAULT.into(),
        name: MIC_DEVICE_DEFAULT.into(),
        label: "System default".into(),
    }];
    for (name, label) in names.iter().chain(unavailable.as_ref()) {
        let full = format!("{:016x}", hash(name));
        let mut token = format!("{DEVICE_TOKEN_PREFIX}{}", &full[..DEVICE_TOKEN_HEX]);
        let mut longer = DEVICE_TOKEN_HEX;
        let mut n = 1;
        while out.iter().any(|e| e.token == token) {
            if longer < full.len() {
                longer += 1;
                token = format!("{DEVICE_TOKEN_PREFIX}{}", &full[..longer]);
            } else {
                n += 1;
                token = format!("{DEVICE_TOKEN_PREFIX}{full}-{n}");
            }
        }
        out.push(DeviceEntry {
            token,
            name: (*name).to_string(),
            label: label.clone(),
        });
    }
    // Over the cap: the tail goes, but not the configured device.
    if out.len() > MAX_DEVICES {
        match out.iter().position(|e| e.name == configured) {
            Some(i) if i >= MAX_DEVICES => {
                let keep = out.remove(i);
                out.truncate(MAX_DEVICES - 1);
                out.push(keep);
            }
            _ => out.truncate(MAX_DEVICES),
        }
    }
    out
}

/// [`device_entries_with`] with the FNV-1a hash.
pub fn device_entries(devices: &[(String, String)], configured: &str) -> Vec<DeviceEntry> {
    device_entries_with(devices, configured, fnv1a64)
}

/// The option of `mic.input.device` that `given` names: its token, or a
/// raw `node.name` (as `omarchy-broadcast mic device NAME` sends) of a
/// device now listed or configured.
pub fn find_device<'a>(entries: &'a [DeviceEntry], given: &str) -> Option<&'a DeviceEntry> {
    entries
        .iter()
        .find(|e| e.token == given)
        .or_else(|| entries.iter().find(|e| e.name == given))
}

/// The daemon's `input` group.
fn input_group(inp: &Inputs<'_>) -> SettingsGroup {
    let mic = &inp.config.mic;
    let defaults = obc_core::config::MicConfig::default();
    let mut enabled = bound(
        setting(
            ENABLED,
            "Noise and echo processing",
            SettingKind::Bool {
                default: defaults.enabled,
                value: mic.enabled,
            },
        ),
        KEY_ENABLED,
        inp.config_set,
    );
    enabled.hint = Some("switch".into());
    let entries = device_entries(&inp.catalogue.devices, &mic.device);
    let gain = inp.catalogue.hardware_gain.as_ref().and_then(|g| {
        let (min, max, step) = g.whole_db()?;
        let value = (g.db.round() as i64).clamp(min, max);
        let mut s = bound(
            setting(
                HARDWARE_GAIN,
                "Hardware gain",
                SettingKind::Int {
                    min,
                    max,
                    step,
                    // Unset leaves the device as it is: its gain now.
                    default: value,
                    value,
                    unit: Some("dB".into()),
                    display: None,
                },
            ),
            KEY_HARDWARE_GAIN,
            inp.config_set,
        );
        s.hint = Some("slider".into());
        Some(s)
    });
    let device = bound(
        setting(
            DEVICE,
            "Input device",
            SettingKind::Enum {
                options: entries
                    .iter()
                    .map(|e| (e.token.clone(), e.label.clone()))
                    .collect(),
                default: defaults.device,
                value: entries
                    .iter()
                    .find(|e| e.name == mic.device)
                    .map_or_else(|| MIC_DEVICE_DEFAULT.into(), |e| e.token.clone()),
            },
        ),
        KEY_DEVICE,
        inp.config_set,
    );
    // "Use as default microphone" stays offered while the host runs, even
    // with a previous default on record: the user may have chosen another
    // microphone since, and making it the default again is harmless (the
    // worker does not say whether the Omarchy Broadcast Mic is the default
    // now).
    let previous = inp.runtime.mic_previous_default.is_some();
    let mut make = setting(
        MAKE_DEFAULT,
        "Use as default microphone",
        SettingKind::Action,
    );
    let mut restore = setting(
        RESTORE_DEFAULT,
        "Use the previous default microphone",
        SettingKind::Action,
    );
    if !inp.host_on {
        make.enabled = false;
        make.disabled_reason = REASON_HOST_OFF.into();
    }
    if !previous {
        restore.enabled = false;
        restore.disabled_reason = REASON_NOT_DEFAULT.into();
    }
    SettingsGroup {
        id: INPUT.into(),
        label: INPUT_LABEL.into(),
        settings: [Some(enabled), Some(device), gain, Some(make), Some(restore)]
            .into_iter()
            .flatten()
            .collect(),
        surface: Some(SURFACE_MIC.into()),
    }
}

/// `v` as a value of `kind`, if it is one (a stored value of another type,
/// a number out of the range or an option no longer offered is not shown).
fn fits(kind: &SettingKind, v: &StoredValue) -> Option<SettingKind> {
    let mut k = kind.clone();
    match (&mut k, v) {
        (SettingKind::Bool { value, .. }, StoredValue::Bool(b)) => *value = *b,
        (
            SettingKind::Int {
                value, min, max, ..
            },
            StoredValue::Int(i),
        ) if (*min..=*max).contains(i) => *value = *i,
        (SettingKind::Enum { value, options, .. }, StoredValue::Str(s))
            if options.iter().any(|(o, _)| o == s) =>
        {
            *value = s.clone()
        }
        _ => return None,
    }
    Some(k)
}

/// The `mic.*` groups now: the daemon's `input` group once the worker is
/// known to be installed (or the mic host runs), then the modules' groups,
/// their values and `stored` those of the state file.
pub fn groups(inp: &Inputs<'_>) -> Vec<SettingsGroup> {
    if !inp.catalogue.known && !inp.host_on {
        return Vec::new();
    }
    let mut out = vec![input_group(inp)];
    for g in &inp.catalogue.groups {
        let mut g = g.clone();
        g.surface = Some(SURFACE_MIC.into());
        for s in &mut g.settings {
            s.scope = SCOPE_GLOBAL.into();
            match inp
                .runtime
                .settings
                .get(&s.id)
                .and_then(|v| fits(&s.kind, v))
            {
                Some(kind) => {
                    s.kind = kind;
                    s.stored = true;
                }
                None => s.stored = false,
            }
        }
        out.push(g);
    }
    out
}

/// What a `SetSetting` of a `mic.*` id does.
#[derive(Debug, Clone, PartialEq)]
pub enum Write {
    /// Writes the config key (to the managed file) with this value.
    Config(&'static str, obc_core::config_schema::ConfigValue),
    /// Makes the Omarchy Broadcast Mic the default microphone.
    MakeDefault,
    /// Makes the previous default microphone the default again.
    RestoreDefault,
    /// Keeps a module setting's value in the state file.
    Value(String, StoredValue),
}

/// The setting `id` of `groups`.
pub fn find<'a>(groups: &'a [SettingsGroup], id: &str) -> Option<&'a Setting> {
    groups
        .iter()
        .flat_map(|g| g.settings.iter())
        .find(|s| s.id == id)
}

/// Checks `value` for the setting `id` of `groups` (as [`groups`] built
/// them): unknown, greyed (its reason), or a value its type does not take.
pub fn check(
    groups: &[SettingsGroup],
    id: &str,
    value: Option<&SettingValue>,
) -> Result<Write, Refusal> {
    let Some(s) = find(groups, id) else {
        return Err(Refusal::Unknown(format!("there is no setting {id:?}")));
    };
    if !s.enabled {
        return Err(Refusal::Disabled(s.disabled_reason.clone()));
    }
    let wrong = |what: &str| Refusal::Invalid(format!("{id}: expects {what}"));
    let stored = match (&s.kind, value) {
        (SettingKind::Bool { .. }, Some(SettingValue::Bool(b))) => StoredValue::Bool(*b),
        (SettingKind::Bool { .. }, _) => return Err(wrong("true or false")),
        (SettingKind::Int { min, max, step, .. }, Some(SettingValue::Int(v))) => {
            if v < min || v > max {
                return Err(Refusal::Invalid(format!(
                    "{id}: {v} is out of range ({min} to {max})"
                )));
            }
            if *step > 1 && (v - min) % step != 0 {
                return Err(Refusal::Invalid(format!(
                    "{id}: {v} is not on the step of {step} from {min}"
                )));
            }
            StoredValue::Int(*v)
        }
        (SettingKind::Int { .. }, _) => return Err(wrong("a whole number")),
        (SettingKind::Enum { options, .. }, Some(SettingValue::Str(v))) => {
            if !options.iter().any(|(o, _)| o == v) {
                let names: Vec<&str> = options.iter().map(|(o, _)| o.as_str()).collect();
                return Err(Refusal::Invalid(format!(
                    "{id}: {v:?} is not one of: {}",
                    names.join(", ")
                )));
            }
            StoredValue::Str(v.clone())
        }
        (SettingKind::Enum { .. }, _) => return Err(wrong("one of its options")),
        (SettingKind::Action, Some(SettingValue::Bool(true))) => {
            return match id {
                MAKE_DEFAULT => Ok(Write::MakeDefault),
                RESTORE_DEFAULT => Ok(Write::RestoreDefault),
                _ => Err(Refusal::Invalid(format!(
                    "{id}: an audio module's action cannot be run"
                ))),
            };
        }
        (SettingKind::Action, _) => return Err(wrong("true (an action)")),
        (SettingKind::Unknown(t), _) => {
            return Err(Refusal::Invalid(format!(
                "{id}: a setting of type {t:?} cannot be set"
            )));
        }
    };
    use obc_core::config_schema::ConfigValue as C;
    Ok(match (id, stored) {
        (ENABLED, StoredValue::Bool(b)) => Write::Config(KEY_ENABLED, C::Bool(b)),
        (DEVICE, StoredValue::Str(s)) => Write::Config(KEY_DEVICE, C::Str(s)),
        // Within the reported range (checked above), so within the key's.
        (HARDWARE_GAIN, StoredValue::Int(i)) => {
            Write::Config(KEY_HARDWARE_GAIN, C::Float(i as f64))
        }
        (id, v) => Write::Value(id.to_string(), v),
    })
}

/// [`check`] for a write of `mic.input.device`, whose value is a token
/// ([`device_token`]) or a raw `node.name` of a device now listed or
/// configured (`omarchy-broadcast mic device NAME`): the config key stores
/// the `node.name`. Any other write is [`check`]'s.
pub fn check_input(
    groups: &[SettingsGroup],
    devices: &[(String, String)],
    configured: &str,
    id: &str,
    value: Option<&SettingValue>,
) -> Result<Write, Refusal> {
    use obc_core::config_schema::ConfigValue as C;
    let (DEVICE, Some(SettingValue::Str(given))) = (id, value) else {
        return check(groups, id, value);
    };
    let entries = device_entries(devices, configured);
    let Some(entry) = find_device(&entries, given) else {
        // Not an option: the generic refusal (the setting is off, or the
        // value is not one of the tokens).
        return check(groups, id, value);
    };
    check(groups, id, Some(&SettingValue::Str(entry.token.clone())))
        .map(|_| Write::Config(KEY_DEVICE, C::Str(entry.name.clone())))
}

/// The ids of the settings of the module group `group` (none for the
/// daemon's `input` group or a group not there).
pub fn module_group_ids(groups: &[SettingsGroup], group: &str) -> Option<Vec<String>> {
    if group == INPUT {
        return None;
    }
    groups
        .iter()
        .find(|g| g.id == group)
        .map(|g| g.settings.iter().map(|s| s.id.clone()).collect())
}

/// The config keys the `input` group's reset removes from the managed
/// file.
pub const INPUT_KEYS: [&str; 3] = [KEY_ENABLED, KEY_DEVICE, KEY_HARDWARE_GAIN];

/// The config key of the daemon's own setting `id`, if it has one.
pub fn config_key_of(id: &str) -> Option<&'static str> {
    match id {
        ENABLED => Some(KEY_ENABLED),
        DEVICE => Some(KEY_DEVICE),
        HARDWARE_GAIN => Some(KEY_HARDWARE_GAIN),
        _ => None,
    }
}

/// A value of the state file as JSON.
fn json_of(v: &StoredValue) -> Json {
    match v {
        StoredValue::Bool(b) => json!(b),
        StoredValue::Int(i) => json!(i),
        StoredValue::Str(s) => json!(s),
    }
}

/// The audio settings snapshot (the worker's `ApplySnapshot` document and
/// `audio-settings.json`), as compact JSON text: `[mic]`, the backend
/// profile, the pins of the `mic-*` stages, the modules' `mic.*` values of
/// the state file, the modules disabled, whether the camera session runs,
/// `mic.force_conflicts` (as `forced_conflicts`), `mic.hardware_gain_db`
/// (`null` when unset: the worker leaves the device's gain as it is) and
/// `gain_serial` (as `hardware_gain_serial`: the controller's count of
/// explicit sets of the gain, so that setting the configured value again
/// applies it again) and the state file's `mic_previous_in_force` (as
/// `previous_in_force`, `null` when not recorded: the real source in force
/// when the Omarchy Broadcast Mic was made the default, which the worker's
/// `Pin` of the default and `RestoreDefault` read). A key is only ever
/// added in this version: a worker
/// that does not know it ignores it, and one that misses it takes its
/// default.
pub fn snapshot(
    config: &Config,
    runtime: &RuntimeState,
    camera_active: bool,
    gain_serial: u64,
) -> String {
    let mic = &config.mic;
    let depth = if mic.pipeline_depth == 0 {
        json!({ "cpu": 1, "gpu": 2 })
    } else {
        json!(mic.pipeline_depth)
    };
    let pins: BTreeMap<&str, &str> = config
        .modules
        .pin
        .iter()
        .filter(|(stage, module)| stage.starts_with("mic-") && !module.is_empty())
        .map(|(s, m)| (s.as_str(), m.as_str()))
        .collect();
    let values: BTreeMap<&str, Json> = runtime
        .settings
        .iter()
        .filter(|(id, _)| is_mic(id) && !is_input(id))
        .map(|(id, v)| (id.as_str(), json_of(v)))
        .collect();
    let m = &config.modules;
    let disabled: BTreeSet<&str> = m
        .disabled
        .iter()
        .chain(m.enabled.keys())
        .filter(|id| !m.is_enabled(id))
        .map(String::as_str)
        .collect();
    json!({
        "version": SNAPSHOT_VERSION,
        "enabled": mic.enabled,
        "device": mic.device,
        "pipeline_depth": depth,
        "idle_exit_s": mic.idle_exit_s,
        "gpu_warm_s": mic.gpu_warm_s,
        "gpu_policy": mic.gpu_policy.as_str(),
        "camera_active": camera_active,
        "profile": m.profile_or_default().as_str(),
        "pins": pins,
        "values": values,
        "disabled_modules": disabled,
        "forced_conflicts": mic.force_conflicts,
        "hardware_gain_db": mic.hardware_gain_db,
        "hardware_gain_serial": gain_serial,
        "previous_in_force": runtime.mic_previous_in_force,
    })
    .to_string()
}

// ---- the worker's documents ----

/// The string `key` of `v`, made one printable line of at most `max`
/// bytes ([`crate::settings::sanitise`]); empty when there is none.
fn text(v: &Json, key: &str, max: usize) -> String {
    clean(v.get(key).and_then(Json::as_str).unwrap_or_default(), max)
}

/// `s` made one printable line of at most `max` bytes.
fn clean(s: &str, max: usize) -> String {
    crate::settings::sanitise(s, max)
}

/// An identifier-like string of the worker's (an option value, a group id):
/// taken as it is when it is short and plain, else not at all.
fn plain(s: &str, max: usize) -> Option<&str> {
    (!s.is_empty() && s.len() <= max && !s.chars().any(|c| c.is_control() || c.is_whitespace()))
        .then_some(s)
}

fn flag(v: &Json, key: &str) -> Option<bool> {
    v.get(key).and_then(Json::as_bool)
}

/// A JSON value as a setting value (a condition's operand: an option value
/// or a list of them, a number or a boolean).
fn value_of(v: &Json) -> Option<SettingValue> {
    match v {
        Json::Bool(b) => Some(SettingValue::Bool(*b)),
        Json::Number(n) => n.as_i64().map(SettingValue::Int),
        Json::String(s) => plain(s, OPTION_VALUE_MAX).map(|s| SettingValue::Str(s.into())),
        Json::Array(a) => a
            .iter()
            .take(crate::settings::MAX_OPTIONS)
            .map(value_of)
            .collect::<Option<Vec<_>>>()
            .map(SettingValue::List),
        _ => None,
    }
}

/// One setting of the worker's `GetSettings` document, or `None` when it
/// does not read (an id that is not a `mic.*` id, a key of its type
/// missing).
fn parse_setting(v: &Json) -> Option<Setting> {
    let id = v.get("id")?.as_str()?;
    if !is_mic(id) || is_input(id) || !obc_ipc::valid_setting_id(id) {
        return None;
    }
    let int = |k: &str| v.get(k).and_then(Json::as_i64);
    let kind = match v.get("type")?.as_str()? {
        "bool" => {
            let default = flag(v, "default").unwrap_or(false);
            SettingKind::Bool {
                default,
                value: flag(v, "value").unwrap_or(default),
            }
        }
        "int" => {
            let (min, max) = (int("min")?, int("max")?);
            if min > max {
                return None;
            }
            let within = |n: i64| (min..=max).contains(&n);
            let default = int("default").filter(|d| within(*d)).unwrap_or(min);
            SettingKind::Int {
                min,
                max,
                step: int("step").unwrap_or(1).max(1),
                default,
                value: int("value").filter(|n| within(*n)).unwrap_or(default),
                unit: v
                    .get("unit")
                    .and_then(Json::as_str)
                    .map(|u| clean(u, TAG_MAX)),
                display: None,
            }
        }
        "enum" => {
            let options: Vec<(String, String)> = v
                .get("options")?
                .as_array()?
                .iter()
                .filter_map(|o| {
                    let pair = o.as_array()?;
                    let value = plain(pair.first()?.as_str()?, OPTION_VALUE_MAX)?;
                    Some((
                        value.to_string(),
                        clean(pair.get(1)?.as_str()?, OPTION_LABEL_MAX),
                    ))
                })
                .take(crate::settings::MAX_OPTIONS)
                .collect();
            let offered = |o: &&str| options.iter().any(|(v, _)| v == o);
            let default = v
                .get("default")
                .and_then(Json::as_str)
                .filter(offered)
                .map(str::to_string)
                .or_else(|| options.first().map(|(o, _)| o.clone()))
                .unwrap_or_default();
            let value = v
                .get("value")
                .and_then(Json::as_str)
                .filter(offered)
                .map_or_else(|| default.clone(), str::to_string);
            SettingKind::Enum {
                options,
                default,
                value,
            }
        }
        "action" => SettingKind::Action,
        other => SettingKind::Unknown(clean(other, TAG_MAX)),
    };
    let enabled_when = v
        .get("enabled_when")
        .and_then(Json::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|c| {
                    let c = c.as_array()?;
                    Some(Condition {
                        id: c.first()?.as_str()?.to_string(),
                        op: ConditionOp::parse(c.get(1)?.as_str()?),
                        value: value_of(c.get(2)?)?,
                    })
                })
                .filter(|c| obc_ipc::valid_setting_id(&c.id))
                .take(crate::settings::MAX_CONDITIONS)
                .collect()
        })
        .unwrap_or_default();
    let enabled = flag(v, "enabled").unwrap_or(true);
    Some(Setting {
        id: id.to_string(),
        label: text(v, "label", LABEL_MAX),
        kind,
        scope: SCOPE_GLOBAL.into(),
        stored: flag(v, "stored").unwrap_or(false),
        enabled,
        disabled_reason: if enabled {
            String::new()
        } else {
            text(v, "disabled_reason", REASON_MAX)
        },
        enabled_when,
        hint: v
            .get("hint")
            .and_then(Json::as_str)
            .map(|h| clean(h, TAG_MAX)),
        description: None,
    })
}

/// Whether a group is offered by a module that does not run its stage (a
/// setting says "Provided by" another).
fn of_another_provider(g: &SettingsGroup) -> bool {
    g.settings
        .iter()
        .any(|s| !s.enabled && s.disabled_reason.starts_with("Provided by"))
}

/// The groups of the worker's `GetSettings` document (`{"groups": [...]}`),
/// one per id: several modules may describe the same group (decision G4,
/// `mic.noise` whichever provider runs the stage), and the one kept is the
/// active provider's, the first that runs its stage, else the first one.
pub fn parse_groups(doc: &Json) -> Vec<SettingsGroup> {
    let mut out: Vec<SettingsGroup> = Vec::new();
    let Some(list) = doc.get("groups").and_then(Json::as_array) else {
        return out;
    };
    for g in list {
        let Some(id) = g
            .get("id")
            .and_then(Json::as_str)
            .and_then(|id| plain(id, GROUP_ID_MAX))
        else {
            continue;
        };
        let settings: Vec<Setting> = g
            .get("settings")
            .and_then(Json::as_array)
            .map(|l| {
                l.iter()
                    .filter_map(parse_setting)
                    .take(crate::settings::MAX_SETTINGS)
                    .collect()
            })
            .unwrap_or_default();
        if settings.is_empty() || id == INPUT {
            continue;
        }
        let group = SettingsGroup {
            id: id.to_string(),
            label: text(g, "label", LABEL_MAX),
            settings,
            surface: Some(SURFACE_MIC.into()),
        };
        let room = out.len() < MAX_GROUPS;
        match out.iter_mut().find(|o| o.id == group.id) {
            Some(kept) if of_another_provider(kept) && !of_another_provider(&group) => {
                *kept = group;
            }
            Some(_) => {}
            None if room => out.push(group),
            None => {}
        }
    }
    out
}

/// The modules of the worker's `GetModules` document
/// (`{"modules": [...]}`), its audio modules only.
pub fn parse_modules(doc: &Json) -> Vec<ModuleInfo> {
    let Some(list) = doc.get("modules").and_then(Json::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter(|m| m.get("kind").and_then(Json::as_str) == Some("audio-filter"))
        .filter_map(|m| {
            let id = plain(
                m.get("id")?.as_str()?,
                obc_module_host::manifest::MODULE_ID_MAX,
            )?
            .to_string();
            let stages = m
                .get("stages")
                .and_then(Json::as_array)
                .map(|l| {
                    l.iter()
                        .filter_map(|s| {
                            Some(StageInfo {
                                id: plain(s.get("id")?.as_str()?, GROUP_ID_MAX)?.to_string(),
                                slot: text(s, "slot", TAG_MAX),
                                rank: s
                                    .get("rank")
                                    .and_then(Json::as_u64)
                                    .and_then(|r| u32::try_from(r).ok())
                                    .unwrap_or_default(),
                                device: text(s, "device", TAG_MAX),
                                available: flag(s, "available").unwrap_or(false),
                                reason: text(s, "reason", REASON_MAX),
                                selected: flag(s, "selected").unwrap_or(false),
                            })
                        })
                        .take(MAX_GROUPS)
                        .collect()
                })
                .unwrap_or_default();
            Some(ModuleInfo {
                id,
                name: text(m, "name", crate::modules::NAME_MAX),
                version: text(m, "version", STATE_MAX),
                licence: text(m, "licence", crate::modules::NAME_MAX),
                kind: text(m, "kind", STATE_MAX),
                library: text(m, "library", TEXT_MAX),
                manifest: text(m, "manifest", TEXT_MAX),
                description: text(m, "description", TEXT_MAX),
                enabled: flag(m, "enabled").unwrap_or(false),
                status: text(m, "status", STATE_MAX),
                reason: text(m, "reason", REASON_MAX),
                forced: false,
                stages,
            })
        })
        .collect()
}

/// The devices of the worker's `GetDevices` document
/// (`{"devices": [{name, description}]}`), at most [`MAX_DEVICES`].
pub fn parse_devices(doc: &Json) -> Vec<(String, String)> {
    let Some(list) = doc.get("devices").and_then(Json::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|d| {
            // A name the configuration would refuse is not offered.
            let name = plain(d.get("name")?.as_str()?, DEVICE_NAME_MAX)?;
            Some((name.to_string(), text(d, "description", LABEL_MAX)))
        })
        .take(MAX_DEVICES)
        .collect()
}

/// The hardware gain of the worker's status (its `hardware_gain` object),
/// or `None` when it is `null`, missing or does not read: a number not
/// finite or outside the configuration's bounds, the ends the wrong way
/// round, a negative step. A status without the key is pending (the
/// worker has not read the device yet): see [`gain_pending`].
pub fn parse_hardware_gain(status: &Json) -> Option<HardwareGain> {
    use obc_core::config::limits::{MAX_MIC_HARDWARE_GAIN_DB, MIN_MIC_HARDWARE_GAIN_DB};
    let g = status.get("hardware_gain")?.as_object()?;
    let num = |k: &str| {
        g.get(k)
            .and_then(Json::as_f64)
            .filter(|v| (MIN_MIC_HARDWARE_GAIN_DB..=MAX_MIC_HARDWARE_GAIN_DB).contains(v))
    };
    let (db, min_db, max_db) = (num("db")?, num("min_db")?, num("max_db")?);
    let step_db = g
        .get("step_db")
        .and_then(Json::as_f64)
        .filter(|v| (0.0..=MAX_MIC_HARDWARE_GAIN_DB - MIN_MIC_HARDWARE_GAIN_DB).contains(v))?;
    if min_db > max_db {
        return None;
    }
    Some(HardwareGain {
        control: clean(
            g.get("control").and_then(Json::as_str).unwrap_or_default(),
            LABEL_MAX,
        ),
        db,
        min_db,
        max_db,
        step_db,
        applied: g.get("applied").and_then(Json::as_bool).unwrap_or(false),
    })
}

/// Whether the worker's status leaves its hardware gain out: not read yet
/// (the worker starting), so the gain known before stands.
pub fn gain_pending(status: &Json) -> bool {
    status.get("hardware_gain").is_none()
}

/// `GetModules`: the daemon's modules, an audio module (which the daemon
/// lists without opening it) replaced by the worker's entry for it, and
/// the worker's audio modules the daemon did not find after them.
pub fn merge_modules(daemon: Vec<ModuleInfo>, worker: &[ModuleInfo]) -> Vec<ModuleInfo> {
    let mut out: Vec<ModuleInfo> = daemon
        .into_iter()
        .map(|m| match worker.iter().find(|w| w.id == m.id) {
            Some(w) if m.kind == "audio-filter" => w.clone(),
            _ => m,
        })
        .collect();
    for w in worker {
        if !out.iter().any(|m| m.id == w.id) {
            out.push(w.clone());
        }
    }
    out
}

/// The `Mic*` properties: `off` while the mic host does not run; with it
/// running, the worker's status (its `off`, processing switched off, is
/// raw passthrough), or `passthrough` while no worker runs (nothing to
/// process: it starts with the sound).
pub fn status(host_on: bool, worker: Option<&Json>) -> MicStatus {
    if !host_on {
        return MicStatus::default();
    }
    let Some(w) = worker else {
        return MicStatus {
            state: "passthrough".into(),
            ..MicStatus::default()
        };
    };
    let state = match w.get("state").and_then(Json::as_str) {
        None | Some(MIC_OFF) => "passthrough".to_string(),
        Some(s) => plain(s, STATE_MAX).unwrap_or("error").to_string(),
    };
    let provider = w
        .get("provider")
        .and_then(Json::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(stage, module)| Some(format!("{stage}: {}", module.as_str()?)))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .map(|p| clean(&p, TEXT_MAX))
        .unwrap_or_default();
    let latency_ms = w
        .get("latency_ms")
        .and_then(Json::as_f64)
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(0.0);
    MicStatus {
        state,
        consumers: w
            .get("consumers")
            .and_then(Json::as_u64)
            .map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX)),
        provider,
        latency_ms,
        error: text(w, "error", TEXT_MAX),
    }
}

#[cfg(test)]
mod tests;
