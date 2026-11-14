//! The global settings of the tree (scope `global`): the video effects,
//! kept for the daemon whatever the camera, as a pure model built from the
//! modules installed.
//!
//! [`build`] turns the effect settings (the configuration's and the state
//! file's values) and the modules' settings descriptions into one
//! [`GlobalEntry`] per setting, group after group. A value is what is in
//! force, `stored` means the state file holds the value, and the default
//! is the configuration's value (what a reset puts back).
//!
//! - The `background` group is the daemon's own, shown only when an
//!   enabled module offers the `background` stage:
//!   `effect.background.mode` (`none`, `blur` or `replace`, always
//!   enabled; `replace` without a background image is refused by the
//!   write, as `SetEffect` refuses it) and `effect.background.blur_strength`
//!   (the blur strength as a whole percentage, enabled only while the mode
//!   is `blur`). They are the `Effect` and `BlurStrength` properties.
//!   While no enabled module offers the stage but the mode in force is not
//!   `none` (a choice kept from when one did), the group holds the mode
//!   alone, so that it can be turned off: a write of `blur` or `replace`
//!   is then refused with [`REASON_NO_BACKGROUND`].
//!   While the enabled modules that offer the stage cannot take the next
//!   session's frames (they declare it for CUDA frames only, and the
//!   session runs in system memory), the group is shown greyed with the
//!   module's reason (as `GetModules` gives it): the blur strength always,
//!   the mode while it is `none`; a mode in force that is not `none` stays
//!   enabled so that it can be turned off, and a write of `blur` or
//!   `replace` is refused with that reason.
//!   Either way the refusal holds for `SetSetting` writes and for a
//!   `ResetSettings` that would bring the configured `blur` or `replace`
//!   back ([`check_reset`]) only: `SetEffect` and the `Effect` property
//!   still accept them, and the session's `on_failure` then applies.
//! - Then each enabled module's groups, as the module describes them. A
//!   setting that switches a stage on (`role: enable`) is greyed and
//!   published off, the stored choice kept, while the stage cannot run
//!   (its reason: `Not installed`, another module provides it, ...); a
//!   parameter of a stage that cannot run is greyed with the same reason,
//!   and otherwise follows its `enabled_when` conditions with its
//!   `disabled_reason`. When a module cannot say (no SDK to look in), its
//!   stages are offered.
//! - The `enhance` stage's switch is also greyed and published off when
//!   the active camera's capture mode is too large for it to pay
//!   ([`REASON_FULL_RESOLUTION`]), or when its frames come in a format the
//!   stage cannot read ([`REASON_FORMAT`]); enabled with no camera (it
//!   applies when a camera is used).
//!
//! The typed `[effect]` keys of revision 8 (`denoise`, `enhance`,
//! `relight`, `relight_light`, `relight_direction`, `relight_brightness`)
//! are the configured defaults of the settings of those ids, whichever
//! module describes them.

use obc_core::EffectMode;
use obc_core::config::{Config, EffectConfig, RuntimeState};
use obc_core::modules::StoredValue;
use obc_ipc::settings::SCOPE_GLOBAL;
use obc_ipc::{Condition, ConditionOp, Setting, SettingKind, SettingValue};
use obc_module_host::registry::FRAME_GENERATION;
use obc_module_host::settings::{Apply, Op, Role};
use obc_module_host::{SettingDesc, SettingType};
use obc_pipeline::effect::StageId;

use super::Refusal;
use crate::modules::{Modules, configured_default, value_of};

/// The prefix of every global setting id.
pub const PREFIX: &str = obc_ipc::settings::GLOBAL_ID_PREFIX;

/// The daemon's own group.
pub const BACKGROUND: &str = "background";
/// Its label.
pub const BACKGROUND_LABEL: &str = "Background effect";

/// The setting ids of the daemon's own group.
pub const MODE: &str = "effect.background.mode";
/// See [`MODE`].
pub const BLUR_STRENGTH: &str = "effect.background.blur_strength";

/// Why a write of `blur` or `replace` is refused while no enabled module
/// offers the `background` stage.
pub const REASON_NO_BACKGROUND: &str = "No installed module provides the background effect";

/// Why the blur strength is greyed with another mode than `blur`.
pub const REASON_CHOOSE_BLUR: &str = "Choose Blur to change this";
/// Why enhancement is greyed with a camera that captures at the output
/// size or above.
pub const REASON_FULL_RESOLUTION: &str = "The camera already captures at full resolution";
/// Why enhancement is greyed with a camera whose frames it cannot read.
pub const REASON_FORMAT: &str = "The camera's format cannot be enhanced";

/// Whether enhancement applies to the active camera's capture mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applies {
    /// Small enough for it to pay, in a format it reads.
    Yes,
    /// At the output size or above.
    FullResolution,
    /// In a format the stage cannot read.
    Format,
}

impl Applies {
    /// Why it does not apply, if it does not.
    pub fn reason(self) -> Option<&'static str> {
        match self {
            Applies::Yes => None,
            Applies::FullResolution => Some(REASON_FULL_RESOLUTION),
            Applies::Format => Some(REASON_FORMAT),
        }
    }
}

/// What writing a global setting does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Binding {
    /// The background effect (`Effect`).
    Mode,
    /// The background effect while no module offers it, or none that
    /// offers it can take the session's frames: only `none` can be
    /// written; another mode is refused with this reason. Enforced on
    /// `SetSetting` writes ([`check`]) and `ResetSettings`
    /// ([`check_reset`]) only: `SetEffect` and the `Effect` property still
    /// accept `blur` and `replace` (the session's `on_failure` then
    /// applies).
    ModeOffOnly(String),
    /// The blur strength (`BlurStrength`).
    BlurStrength,
    /// Switches the stage on and off.
    Enable(StageId),
    /// A parameter of the stage, taking effect as `Apply` says.
    Param(StageId, Apply),
    /// Kept for the module, bound to no stage.
    Plain,
}

impl Binding {
    fn of(desc: &SettingDesc) -> Binding {
        match (&desc.stage, desc.role, desc.apply) {
            (Some(s), Some(Role::Enable), _) => Binding::Enable(s.clone()),
            (Some(s), None, Some(a)) => Binding::Param(s.clone(), a),
            _ => Binding::Plain,
        }
    }
}

/// Whether `id` is a global setting id (the camera option is ignored).
pub fn is_global(id: &str) -> bool {
    id.starts_with(PREFIX)
}

/// What [`build`] needs.
#[derive(Debug, Clone, Copy)]
pub struct Inputs<'a> {
    pub config: &'a Config,
    pub runtime: &'a RuntimeState,
    pub modules: &'a Modules,
    /// Whether enhancement applies to the active camera (`None`: no
    /// camera).
    pub enhance_applies: Option<Applies>,
}

/// One global setting with its group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalEntry {
    /// The group id.
    pub group: String,
    /// The group's label.
    pub group_label: String,
    /// What a write does.
    pub binding: Binding,
    pub setting: Setting,
}

impl GlobalEntry {
    /// The group id.
    pub fn group(&self) -> &str {
        &self.group
    }
}

/// The blur strength's whole percentage.
pub fn percent(strength: f64) -> i64 {
    (strength.clamp(0.0, 1.0) * 100.0).round() as i64
}

/// `v` on the wire.
pub fn wire_value(v: &StoredValue) -> SettingValue {
    match v {
        StoredValue::Bool(b) => SettingValue::Bool(*b),
        StoredValue::Int(i) => SettingValue::Int(*i),
        StoredValue::Str(s) => SettingValue::Str(s.clone()),
    }
}

/// A condition's operand on the wire: `None` when it is not a value a
/// setting can have (a number with a fraction, out of the integer range,
/// a null, an object). A whole number written as a float is an integer.
fn wire_json(v: &serde_json::Value) -> Option<SettingValue> {
    match v {
        serde_json::Value::Bool(b) => Some(SettingValue::Bool(*b)),
        serde_json::Value::Number(n) => n
            .as_i64()
            .or_else(|| {
                n.as_f64()
                    .filter(|f| f.fract() == 0.0 && (i64::MIN as f64..=i64::MAX as f64).contains(f))
                    .map(|f| f as i64)
            })
            .map(SettingValue::Int),
        serde_json::Value::String(s) => Some(SettingValue::Str(s.clone())),
        serde_json::Value::Array(a) => a
            .iter()
            .map(wire_json)
            .collect::<Option<Vec<_>>>()
            .map(SettingValue::List),
        _ => None,
    }
}

fn holds(op: ConditionOp, value: &SettingValue, operand: &SettingValue) -> bool {
    match (op, operand) {
        (ConditionOp::Eq, o) => value == o,
        (ConditionOp::Ne, o) => value != o,
        (ConditionOp::In, SettingValue::List(l)) => l.contains(value),
        // Not a list: no value is in it.
        (ConditionOp::In, _) => false,
        (ConditionOp::Unknown(_), _) => true,
    }
}

/// Logs once per setting id that `id` is left out, and why.
fn left_out_once(id: &str, why: &str) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static NOTED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let mut noted = NOTED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if noted.insert(id.to_string()) {
        crate::warning!("setting {id} is left out: {why}");
    }
}

/// The global settings now: the daemon's `background` group when an
/// enabled module offers the stage (greyed while none can take the
/// session's frames; or its mode alone while one in force has no module
/// to run it), then each enabled module's groups.
pub fn build(inp: &Inputs<'_>) -> Vec<GlobalEntry> {
    let config = &inp.config.effect;
    let effect = inp.runtime.effect_over(config);
    let runtime = inp.runtime;
    let mut out = Vec::new();
    let stage = StageId::BACKGROUND.as_str();
    let declared = inp.modules.declares(stage).is_some();
    // Declared only for CUDA frames, in a session in system memory: the
    // module's reason, as `GetModules` gives it.
    let cannot_run = (declared && inp.modules.declares_in_memory(stage).is_none())
        .then(|| inp.modules.absent_reason(stage).1);
    let offered = declared && cannot_run.is_none();
    if declared || effect.mode != EffectMode::None {
        let setting =
            |id: &str, label: &str, kind: SettingKind, stored: bool, hint: &str| Setting {
                id: id.into(),
                label: label.into(),
                kind,
                scope: SCOPE_GLOBAL.into(),
                stored,
                enabled: true,
                disabled_reason: String::new(),
                enabled_when: Vec::new(),
                hint: Some(hint.into()),
                description: None,
            };
        let mut mode = setting(
            MODE,
            "Effect",
            SettingKind::Enum {
                options: [
                    (EffectMode::None, "Off"),
                    (EffectMode::Blur, "Blur"),
                    (EffectMode::Replace, "Replace"),
                ]
                .iter()
                .map(|(m, l)| (m.as_str().to_string(), (*l).to_string()))
                .collect(),
                default: config.mode.as_str().into(),
                value: effect.mode.as_str().into(),
            },
            runtime.effect.is_some(),
            "segmented",
        );
        let mut blur = setting(
            BLUR_STRENGTH,
            "Blur strength",
            SettingKind::Int {
                min: 0,
                max: 100,
                step: 1,
                default: percent(config.blur_strength),
                value: percent(effect.blur_strength),
                unit: Some("%".into()),
                display: None,
            },
            runtime.blur_strength.is_some(),
            "slider",
        );
        blur.enabled_when = vec![Condition {
            id: MODE.into(),
            op: ConditionOp::Eq,
            value: SettingValue::Str(EffectMode::Blur.as_str().into()),
        }];
        if effect.mode != EffectMode::Blur {
            blur.enabled = false;
            blur.disabled_reason = REASON_CHOOSE_BLUR.into();
        }
        let shown = match (offered, cannot_run) {
            (true, _) => vec![(mode, Binding::Mode), (blur, Binding::BlurStrength)],
            // Greyed with the module's reason; the mode stays enabled
            // while one in force can still be turned off.
            (false, Some(why)) => {
                if effect.mode == EffectMode::None {
                    mode.enabled = false;
                    mode.disabled_reason.clone_from(&why);
                }
                blur.enabled = false;
                blur.disabled_reason.clone_from(&why);
                vec![
                    (mode, Binding::ModeOffOnly(why)),
                    (blur, Binding::BlurStrength),
                ]
            }
            (false, None) => vec![(mode, Binding::ModeOffOnly(REASON_NO_BACKGROUND.into()))],
        };
        for (s, binding) in shown {
            out.push(GlobalEntry {
                group: BACKGROUND.into(),
                group_label: BACKGROUND_LABEL.into(),
                binding,
                setting: s,
            });
        }
    }
    // The values in force first: what the conditions compare.
    let values = |id: &str| -> Option<SettingValue> {
        if id == MODE {
            return Some(SettingValue::Str(effect.mode.as_str().into()));
        }
        inp.modules
            .setting(id)
            .and_then(|(_, d)| value_of(inp.config, runtime, d))
            .map(|v| wire_value(&v))
    };
    for (module, desc) in inp.modules.described() {
        for group in &desc.groups {
            for d in &group.settings {
                // Frame generation is the output rate's choice, never a
                // switch of its own.
                if d.role == Some(Role::Enable)
                    && d.stage
                        .as_ref()
                        .is_some_and(|s| s.as_str() == FRAME_GENERATION)
                {
                    continue;
                }
                match module_setting(inp, module, d, &values) {
                    Ok(setting) => out.push(GlobalEntry {
                        group: group.id.clone(),
                        group_label: group.label.clone(),
                        binding: Binding::of(d),
                        setting,
                    }),
                    Err(why) => left_out_once(&d.id, &why),
                }
            }
        }
    }
    out
}

/// The published setting of `d`, of `module`; why it cannot be published
/// when one of its `enabled_when` conditions has an operand no setting
/// value can be (dropping the condition alone would enable the setting
/// when it should not be).
fn module_setting(
    inp: &Inputs<'_>,
    module: &str,
    d: &SettingDesc,
    values: &dyn Fn(&str) -> Option<SettingValue>,
) -> Result<Setting, String> {
    let value = value_of(inp.config, inp.runtime, d);
    let default = configured_default(inp.config, d);
    let unavailable = d
        .stage
        .as_ref()
        .and_then(|s| inp.modules.stage_reason(module, s.as_str()));
    let enable = d.role == Some(Role::Enable);
    // The enhance stage applies to some capture modes only.
    let refused = (enable && d.stage.as_ref() == Some(&StageId::ENHANCE))
        .then(|| inp.enhance_applies.and_then(Applies::reason))
        .flatten();
    let enabled_when: Vec<Condition> = d
        .enabled_when
        .iter()
        .map(|c| {
            Ok(Condition {
                id: c.id.clone(),
                op: match c.op {
                    Op::Eq => ConditionOp::Eq,
                    Op::Ne => ConditionOp::Ne,
                    Op::In => ConditionOp::In,
                },
                value: wire_json(&c.value).ok_or_else(|| {
                    format!(
                        "its condition on {} compares with {}, which no setting value can be",
                        c.id, c.value
                    )
                })?,
            })
        })
        .collect::<Result<_, String>>()?;
    let conditions_hold = enabled_when
        .iter()
        .all(|c| values(&c.id).is_none_or(|v| holds(c.op.clone(), &v, &c.value)));
    let (enabled, reason) = match (&unavailable, refused) {
        (Some(why), _) => (false, why.clone()),
        (None, Some(why)) => (false, why.to_string()),
        (None, None) if !conditions_hold => (
            false,
            d.disabled_reason
                .clone()
                .unwrap_or_else(|| "Unavailable".to_string()),
        ),
        _ => (true, String::new()),
    };
    // A switch that cannot take effect is published off, which is what
    // runs; the stored choice is kept.
    let shown = |v: Option<StoredValue>| {
        if enable && (unavailable.is_some() || refused.is_some()) {
            Some(StoredValue::Bool(false))
        } else {
            v
        }
    };
    let kind = match (&d.kind, shown(value), default) {
        (SettingType::Bool { .. }, Some(StoredValue::Bool(v)), Some(StoredValue::Bool(def))) => {
            SettingKind::Bool {
                default: def,
                value: v,
            }
        }
        (
            SettingType::Int {
                min,
                max,
                step,
                unit,
                ..
            },
            Some(StoredValue::Int(v)),
            Some(StoredValue::Int(def)),
        ) => SettingKind::Int {
            min: *min,
            max: *max,
            step: *step,
            default: def,
            value: v,
            unit: unit.clone(),
            display: None,
        },
        (
            SettingType::Enum { options, .. },
            Some(StoredValue::Str(v)),
            Some(StoredValue::Str(def)),
        ) => SettingKind::Enum {
            options: options.clone(),
            default: def,
            value: v,
        },
        _ => SettingKind::Action,
    };
    Ok(Setting {
        id: d.id.clone(),
        label: d.label.clone(),
        kind,
        scope: SCOPE_GLOBAL.into(),
        stored: inp.runtime.settings.contains_key(&d.id),
        enabled,
        disabled_reason: reason,
        enabled_when,
        hint: d.hint.clone(),
        description: None,
    })
}

/// A checked write of a global setting.
#[derive(Debug, Clone, PartialEq)]
pub enum Write {
    /// `effect.background.mode`.
    Mode(EffectMode),
    /// `effect.background.blur_strength`, as 0 to 1.
    BlurStrength(f64),
    /// A module setting: its id, what it does, the label and the value.
    Setting {
        id: String,
        label: String,
        binding: Binding,
        value: StoredValue,
    },
}

/// Checks a write of `id` with `value` against `entries` (as built now):
/// an unknown id, then a disabled setting, then a value that does not fit
/// (the wrong type, out of range, off its step, not an option), then an
/// option that cannot be chosen now (a background effect no module runs)
/// is refused.
pub fn check(
    entries: &[GlobalEntry],
    id: &str,
    value: Option<&SettingValue>,
) -> Result<Write, Refusal> {
    let Some(e) = entries.iter().find(|e| e.setting.id == id) else {
        return Err(Refusal::Unknown(format!("there is no setting {id:?}")));
    };
    if !e.setting.enabled {
        return Err(Refusal::Disabled(e.setting.disabled_reason.clone()));
    }
    let invalid = |what: String| Refusal::Invalid(format!("{id}: {what}"));
    let stored = match (&e.setting.kind, value) {
        (SettingKind::Enum { options, .. }, Some(SettingValue::Str(s))) => {
            if !options.iter().any(|(o, _)| o == s) {
                let ids: Vec<&str> = options.iter().map(|(o, _)| o.as_str()).collect();
                return Err(invalid(format!(
                    "{s:?} is not one of its options ({})",
                    ids.join(", ")
                )));
            }
            StoredValue::Str(s.clone())
        }
        (SettingKind::Int { min, max, step, .. }, Some(SettingValue::Int(v))) => {
            if v < min || v > max {
                return Err(invalid(format!("{v} is outside its range {min}..={max}")));
            }
            if (v - min) % step != 0 {
                return Err(invalid(format!(
                    "{v} is not a whole number of steps of {step} from {min}"
                )));
            }
            StoredValue::Int(*v)
        }
        (SettingKind::Bool { .. }, Some(SettingValue::Bool(b))) => StoredValue::Bool(*b),
        (kind, _) => {
            let want = match kind {
                SettingKind::Bool { .. } => "a boolean (b)",
                SettingKind::Int { .. } => "an integer",
                SettingKind::Enum { .. } => "an option id (s)",
                _ => "nothing: it cannot be set",
            };
            return Err(invalid(format!("expects {want}")));
        }
    };
    Ok(match (&e.binding, stored) {
        (Binding::Mode, StoredValue::Str(s)) => Write::Mode(
            s.parse::<EffectMode>()
                .map_err(|e| invalid(e.to_string()))?,
        ),
        (Binding::ModeOffOnly(why), StoredValue::Str(s)) => {
            match s
                .parse::<EffectMode>()
                .map_err(|e| invalid(e.to_string()))?
            {
                EffectMode::None => Write::Mode(EffectMode::None),
                _ => return Err(Refusal::Disabled(why.clone())),
            }
        }
        (Binding::BlurStrength, StoredValue::Int(v)) => Write::BlurStrength(v as f64 / 100.0),
        (binding, value) => Write::Setting {
            id: id.to_string(),
            label: e.setting.label.clone(),
            binding: binding.clone(),
            value,
        },
    })
}

/// Checks a reset of global settings `ids` (`forget` on each) against
/// `entries` (as built now), as [`check`] does a write: a reset that would
/// put a background effect other than `none` into a mode bound to
/// [`Binding::ModeOffOnly`] (the configured mode, over a stored one that
/// differs) is refused with that binding's reason, and nothing is
/// forgotten. A reset that leaves the mode as it is passes.
pub fn check_reset(
    entries: &[GlobalEntry],
    ids: &[String],
    runtime: &RuntimeState,
    config: &EffectConfig,
) -> Result<(), Refusal> {
    if !ids.iter().any(|id| id == MODE) {
        return Ok(());
    }
    let Some(Binding::ModeOffOnly(why)) = entries
        .iter()
        .find(|e| e.setting.id == MODE)
        .map(|e| &e.binding)
    else {
        return Ok(());
    };
    match runtime.effect {
        Some(stored) if stored != config.mode && config.mode != EffectMode::None => {
            Err(Refusal::Disabled(why.clone()))
        }
        _ => Ok(()),
    }
}

/// Forgets the state file's value of global setting `id`: the configured
/// one applies again. Whether there was one.
pub fn forget(runtime: &mut RuntimeState, id: &str) -> bool {
    match id {
        MODE => runtime.effect.take().is_some(),
        BLUR_STRENGTH => runtime.blur_strength.take().is_some(),
        _ => runtime.settings.remove(id).is_some(),
    }
}

#[cfg(test)]
mod tests;
