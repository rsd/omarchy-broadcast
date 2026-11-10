//! The global settings of the tree (scope `global`, D-Bus revision 8): the
//! video effects, kept for the daemon whatever the camera, as a pure model.
//!
//! [`build`] turns the effect settings (the configuration's and the state
//! file's overrides) and whether noise removal is installed into one
//! [`GlobalEntry`] per setting, in [`GROUPS`] order. They are views of the
//! existing effect settings: a value is what `Effect`, `BlurStrength` and
//! `Denoise` hold, `stored` means the state file overrides the
//! configuration's value, and the default is the configuration's value
//! (what a reset puts back).
//!
//! - `effect.background.mode`: `none`, `blur` or `replace`, always enabled
//!   (`replace` without a background image is refused by the write, as
//!   `SetEffect` refuses it).
//! - `effect.background.blur_strength`: the blur strength as a whole
//!   percentage (0 to 100), enabled only while the mode is `blur`.
//! - `effect.video.denoise`: noise removal. When the feature is known not
//!   to be installed it is greyed ([`REASON_NOT_INSTALLED`]) and published
//!   off, which is what runs; the stored choice is kept and applies once the
//!   feature is installed. When nothing is known (no SDK root), it is as
//!   before revision 8.
//! - `effect.video.enhance`: enhancement of a low-resolution camera (an
//!   upscale of the camera image to the output size). Greyed and published
//!   off, the stored choice kept, when it is known not to be installed
//!   ([`REASON_NOT_INSTALLED`]), when the active camera's capture mode is
//!   too large for it to pay ([`REASON_FULL_RESOLUTION`]), or when its
//!   frames come in a format the stage cannot read ([`REASON_FORMAT`]);
//!   enabled with no camera (it applies when a camera is used).
//! - `effect.lighting.relight`: relighting of the person. Greyed and
//!   published off, the stored choice kept, when it is known not to be
//!   installed (it needs the relighting and the green-screen features), as
//!   noise removal is.
//! - `effect.lighting.light`, `effect.lighting.direction` and
//!   `effect.lighting.brightness`: the relighting key light's colour
//!   (`warm`, `neutral`, `cool`), its direction in degrees (-90 to 90, in
//!   steps of 5) and the relit person's brightness in percent (50 to 200,
//!   in steps of 5). Enabled only while relighting is on
//!   ([`REASON_TURN_ON_RELIGHT`]), greyed [`REASON_NOT_INSTALLED`] while it
//!   is not installed.
//!
//! [`check`] validates a write as the camera settings do (unknown, then
//! disabled, then invalid) and says which existing setter carries it out.

use obc_core::config::{Config, RuntimeState, limits};
use obc_core::{EffectMode, RelightLight};
use obc_ipc::settings::SCOPE_GLOBAL;
use obc_ipc::{Condition, ConditionOp, Setting, SettingKind, SettingValue};

use super::Refusal;
use super::catalogue::Group;

/// The prefix of every global setting id.
pub const PREFIX: &str = obc_ipc::settings::GLOBAL_ID_PREFIX;
/// The background effect group.
pub const BACKGROUND: &str = "background";
/// The video group.
pub const VIDEO: &str = "video";
/// The lighting group.
pub const LIGHTING: &str = "lighting";

/// The global groups, in display order, after the camera's.
pub const GROUPS: [Group; 3] = [
    Group {
        id: BACKGROUND,
        label: "Background effect",
    },
    Group {
        id: VIDEO,
        label: "Video",
    },
    Group {
        id: LIGHTING,
        label: "Lighting",
    },
];

/// The effect mode.
pub const MODE: &str = "effect.background.mode";
/// The blur strength, in percent.
pub const BLUR_STRENGTH: &str = "effect.background.blur_strength";
/// Noise removal.
pub const DENOISE: &str = "effect.video.denoise";
/// Enhancement of a low-resolution camera.
pub const ENHANCE: &str = "effect.video.enhance";
/// Relighting.
pub const RELIGHT: &str = "effect.lighting.relight";
/// The relighting light.
pub const LIGHT: &str = "effect.lighting.light";
/// The relighting direction, in degrees.
pub const DIRECTION: &str = "effect.lighting.direction";
/// The relighting brightness, in percent.
pub const BRIGHTNESS: &str = "effect.lighting.brightness";
/// The step of the relighting direction, in degrees.
pub const DIRECTION_STEP: i64 = 5;
/// The step of the relighting brightness, in percent.
pub const BRIGHTNESS_STEP: i64 = 5;

/// Why the blur strength is greyed.
pub const REASON_CHOOSE_BLUR: &str = "Choose Blur to change this";
/// Why a feature that is not installed is greyed.
pub const REASON_NOT_INSTALLED: &str = "Not installed";
/// Why enhancement is greyed for a camera that needs none.
pub const REASON_FULL_RESOLUTION: &str = "The camera already captures at full resolution";
/// Why enhancement is greyed for a camera whose frames it cannot read.
pub const REASON_FORMAT: &str = "The camera's format cannot be enhanced";
/// Why a relighting setting is greyed while relighting is off.
pub const REASON_TURN_ON_RELIGHT: &str = "Turn on relighting to change this";

/// Whether enhancement applies to a camera's capture mode, and why not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applies {
    /// It applies.
    Yes,
    /// The mode is too large for it to pay ([`REASON_FULL_RESOLUTION`]).
    FullResolution,
    /// The mode's frames are in a format it cannot read ([`REASON_FORMAT`]).
    Format,
}

impl Applies {
    /// Why it does not apply, or `None` when it does.
    pub fn reason(self) -> Option<&'static str> {
        match self {
            Applies::Yes => None,
            Applies::FullResolution => Some(REASON_FULL_RESOLUTION),
            Applies::Format => Some(REASON_FORMAT),
        }
    }
}

/// Which effect setting a global setting is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Mode,
    BlurStrength,
    Denoise,
    Enhance,
    Relight,
    Light,
    Direction,
    Brightness,
}

impl Key {
    /// Every key, in tree order.
    pub const ALL: [Key; 8] = [
        Key::Mode,
        Key::BlurStrength,
        Key::Denoise,
        Key::Enhance,
        Key::Relight,
        Key::Light,
        Key::Direction,
        Key::Brightness,
    ];

    /// The setting id.
    pub fn id(self) -> &'static str {
        match self {
            Key::Mode => MODE,
            Key::BlurStrength => BLUR_STRENGTH,
            Key::Denoise => DENOISE,
            Key::Enhance => ENHANCE,
            Key::Relight => RELIGHT,
            Key::Light => LIGHT,
            Key::Direction => DIRECTION,
            Key::Brightness => BRIGHTNESS,
        }
    }

    /// The group id.
    pub fn group(self) -> &'static str {
        match self {
            Key::Mode | Key::BlurStrength => BACKGROUND,
            Key::Denoise | Key::Enhance => VIDEO,
            Key::Relight | Key::Light | Key::Direction | Key::Brightness => LIGHTING,
        }
    }

    /// The key of setting `id`.
    pub fn of(id: &str) -> Option<Key> {
        Key::ALL.into_iter().find(|k| k.id() == id)
    }

    /// The keys of group `group` (empty for no global group).
    pub fn of_group(group: &str) -> Vec<Key> {
        Key::ALL
            .into_iter()
            .filter(|k| k.group() == group)
            .collect()
    }

    /// Forgets the state file's override of this key: the configuration's
    /// value applies again. Whether there was one.
    pub fn forget(self, runtime: &mut RuntimeState) -> bool {
        match self {
            Key::Mode => runtime.effect.take().is_some(),
            Key::BlurStrength => runtime.blur_strength.take().is_some(),
            Key::Denoise => runtime.denoise.take().is_some(),
            Key::Enhance => runtime.enhance.take().is_some(),
            Key::Relight => runtime.relight.take().is_some(),
            Key::Light => runtime.relight_light.take().is_some(),
            Key::Direction => runtime.relight_direction.take().is_some(),
            Key::Brightness => runtime.relight_brightness.take().is_some(),
        }
    }
}

/// Whether `id` is a global setting id (or would be one: every id under
/// [`PREFIX`] is answered by this model, never by a camera's).
pub fn is_global(id: &str) -> bool {
    id.starts_with(PREFIX)
}

/// Whether `group` is a global group.
pub fn is_global_group(group: &str) -> bool {
    GROUPS.iter().any(|g| g.id == group)
}

/// What [`build`] reads.
#[derive(Debug, Clone, Copy)]
pub struct Inputs<'a> {
    /// The configuration: its `[effect]` values are the defaults.
    pub config: &'a Config,
    /// The state file: its effect values override the configuration's.
    pub runtime: &'a RuntimeState,
    /// Whether noise removal is installed: `None` when it cannot be known
    /// (no SDK root), which publishes it as before.
    pub denoise_installed: Option<bool>,
    /// Whether enhancement is installed, as `denoise_installed` is for
    /// noise removal.
    pub enhance_installed: Option<bool>,
    /// Whether enhancement applies to the active camera's capture mode
    /// (`None`: no camera, so it is enabled).
    pub enhance_applies: Option<Applies>,
    /// Whether relighting is installed (its feature and the green screen
    /// both), as `denoise_installed` is for noise removal.
    pub relight_installed: Option<bool>,
}

/// One published global setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalEntry {
    pub key: Key,
    pub setting: Setting,
}

impl GlobalEntry {
    /// Its group id.
    pub fn group(&self) -> &'static str {
        self.key.group()
    }
}

/// A blur strength (0 to 1) as the published percentage.
pub fn percent(strength: f64) -> i64 {
    // Validated to 0..=1 wherever it is set.
    (strength.clamp(0.0, 1.0) * 100.0).round() as i64
}

/// The global settings (see the module docs).
pub fn build(inp: &Inputs<'_>) -> Vec<GlobalEntry> {
    let config = &inp.config.effect;
    let effect = inp.runtime.effect_over(config);
    let runtime = inp.runtime;
    let setting = |key: Key, label: &str, kind: SettingKind, stored: bool, hint: &str| Setting {
        id: key.id().into(),
        label: label.into(),
        kind,
        scope: SCOPE_GLOBAL.into(),
        stored,
        enabled: true,
        disabled_reason: String::new(),
        enabled_when: Vec::new(),
        hint: Some(hint.into()),
    };
    let mode = setting(
        Key::Mode,
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
        Key::BlurStrength,
        "Blur strength",
        SettingKind::Int {
            min: 0,
            max: 100,
            step: 1,
            default: percent(config.blur_strength),
            value: percent(effect.blur_strength),
            unit: Some("%".into()),
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
    let missing = inp.denoise_installed == Some(false);
    let mut denoise = setting(
        Key::Denoise,
        "Noise removal",
        SettingKind::Bool {
            default: config.denoise,
            // What runs: nothing while it is not installed.
            value: effect.denoise && !missing,
        },
        runtime.denoise.is_some(),
        "switch",
    );
    if missing {
        denoise.enabled = false;
        denoise.disabled_reason = REASON_NOT_INSTALLED.into();
    }
    let enhance_missing = inp.enhance_installed == Some(false);
    let refused = inp.enhance_applies.and_then(Applies::reason);
    let needless = refused.is_some();
    let mut enhance = setting(
        Key::Enhance,
        "Enhance low-resolution camera",
        SettingKind::Bool {
            default: config.enhance,
            // What runs: nothing while it is not installed or not needed.
            value: effect.enhance && !enhance_missing && !needless,
        },
        runtime.enhance.is_some(),
        "switch",
    );
    if enhance_missing || needless {
        enhance.enabled = false;
        enhance.disabled_reason = if enhance_missing {
            REASON_NOT_INSTALLED
        } else {
            refused.unwrap_or(REASON_FULL_RESOLUTION)
        }
        .into();
    }
    let relight_missing = inp.relight_installed == Some(false);
    let mut relight = setting(
        Key::Relight,
        "Relighting",
        SettingKind::Bool {
            default: config.relight,
            // What runs: nothing while it is not installed.
            value: effect.relight && !relight_missing,
        },
        runtime.relight.is_some(),
        "switch",
    );
    if relight_missing {
        relight.enabled = false;
        relight.disabled_reason = REASON_NOT_INSTALLED.into();
    }
    // The settings of the light: enabled only while relighting runs.
    let lit = |mut s: Setting| {
        s.enabled_when = vec![Condition {
            id: RELIGHT.into(),
            op: ConditionOp::Eq,
            value: SettingValue::Bool(true),
        }];
        if relight_missing {
            s.enabled = false;
            s.disabled_reason = REASON_NOT_INSTALLED.into();
        } else if !effect.relight {
            s.enabled = false;
            s.disabled_reason = REASON_TURN_ON_RELIGHT.into();
        }
        s
    };
    let light = lit(setting(
        Key::Light,
        "Light",
        SettingKind::Enum {
            options: [
                (RelightLight::Warm, "Warm"),
                (RelightLight::Neutral, "Neutral"),
                (RelightLight::Cool, "Cool"),
            ]
            .iter()
            .map(|(l, label)| (l.as_str().to_string(), (*label).to_string()))
            .collect(),
            default: config.relight_light.as_str().into(),
            value: effect.relight_light.as_str().into(),
        },
        runtime.relight_light.is_some(),
        "segmented",
    ));
    let direction = lit(setting(
        Key::Direction,
        "Light direction",
        SettingKind::Int {
            min: limits::MIN_RELIGHT_DIRECTION.into(),
            max: limits::MAX_RELIGHT_DIRECTION.into(),
            step: DIRECTION_STEP,
            default: config.relight_direction.into(),
            value: effect.relight_direction.into(),
            unit: Some("°".into()),
        },
        runtime.relight_direction.is_some(),
        "slider",
    ));
    let brightness = lit(setting(
        Key::Brightness,
        "Brightness",
        SettingKind::Int {
            min: limits::MIN_RELIGHT_BRIGHTNESS.into(),
            max: limits::MAX_RELIGHT_BRIGHTNESS.into(),
            step: BRIGHTNESS_STEP,
            default: config.relight_brightness.into(),
            value: effect.relight_brightness.into(),
            unit: Some("%".into()),
        },
        runtime.relight_brightness.is_some(),
        "slider",
    ));
    [
        (Key::Mode, mode),
        (Key::BlurStrength, blur),
        (Key::Denoise, denoise),
        (Key::Enhance, enhance),
        (Key::Relight, relight),
        (Key::Light, light),
        (Key::Direction, direction),
        (Key::Brightness, brightness),
    ]
    .into_iter()
    .map(|(key, setting)| GlobalEntry { key, setting })
    .collect()
}

/// What a valid write of a global setting does: the existing setter it is.
#[derive(Debug, Clone, PartialEq)]
pub enum Write {
    /// `SetEffect`.
    Mode(EffectMode),
    /// `SetBlurStrength`, 0 to 1.
    BlurStrength(f64),
    /// `SetDenoise`.
    Denoise(bool),
    /// Enhancement on or off (no method of its own).
    Enhance(bool),
    /// Relighting on or off (no method of its own).
    Relight(bool),
    /// The relighting light (no method of its own).
    Light(RelightLight),
    /// The relighting direction, in degrees (no method of its own).
    Direction(i32),
    /// The relighting brightness, in percent (no method of its own).
    Brightness(u32),
}

/// Validates `value` for global setting `id` among `entries`: unknown,
/// then disabled, then invalid (`None`: a D-Bus type no setting takes).
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
    match (&e.setting.kind, value) {
        (SettingKind::Enum { options, .. }, Some(SettingValue::Str(s))) => {
            let write = if !options.iter().any(|(o, _)| o == s) {
                None
            } else if e.key == Key::Light {
                s.parse::<RelightLight>().ok().map(Write::Light)
            } else {
                s.parse::<EffectMode>().ok().map(Write::Mode)
            };
            write.ok_or_else(|| {
                let ids: Vec<&str> = options.iter().map(|(o, _)| o.as_str()).collect();
                invalid(format!(
                    "{s:?} is not one of its options ({})",
                    ids.join(", ")
                ))
            })
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
            // Within the range, so each fits its type.
            Ok(match e.key {
                Key::Direction => Write::Direction(*v as i32),
                Key::Brightness => Write::Brightness(*v as u32),
                _ => Write::BlurStrength(*v as f64 / 100.0),
            })
        }
        (SettingKind::Bool { .. }, Some(SettingValue::Bool(b))) => Ok(match e.key {
            Key::Enhance => Write::Enhance(*b),
            Key::Relight => Write::Relight(*b),
            _ => Write::Denoise(*b),
        }),
        (kind, _) => {
            let want = match kind {
                SettingKind::Bool { .. } => "a boolean (b)",
                SettingKind::Int { .. } => "an integer",
                _ => "an option id (s)",
            };
            Err(invalid(format!("expects {want}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{CameraSettings, diff};
    use obc_ipc::valid_setting_id;

    fn entries(
        config: &Config,
        runtime: &RuntimeState,
        installed: Option<bool>,
    ) -> Vec<GlobalEntry> {
        build(&Inputs {
            config,
            runtime,
            denoise_installed: installed,
            enhance_installed: None,
            enhance_applies: None,
            relight_installed: None,
        })
    }

    /// The settings with enhancement `installed` and `applies` to the
    /// camera (noise removal installed).
    fn with_enhance(
        runtime: &RuntimeState,
        installed: Option<bool>,
        applies: Option<Applies>,
    ) -> Vec<GlobalEntry> {
        build(&Inputs {
            config: &Config::default(),
            runtime,
            denoise_installed: Some(true),
            enhance_installed: installed,
            enhance_applies: applies,
            relight_installed: Some(true),
        })
    }

    /// The settings with relighting `installed` (the rest installed).
    fn with_relight(runtime: &RuntimeState, installed: Option<bool>) -> Vec<GlobalEntry> {
        build(&Inputs {
            config: &Config::default(),
            runtime,
            denoise_installed: Some(true),
            enhance_installed: Some(true),
            enhance_applies: None,
            relight_installed: installed,
        })
    }

    fn get<'a>(e: &'a [GlobalEntry], id: &str) -> &'a Setting {
        &e.iter().find(|e| e.setting.id == id).unwrap().setting
    }

    fn tree(global: Vec<GlobalEntry>) -> CameraSettings {
        CameraSettings {
            global,
            ..CameraSettings::none()
        }
    }

    #[test]
    fn the_effects_are_three_global_groups_with_valid_ids() {
        let e = entries(&Config::default(), &RuntimeState::default(), None);
        let ids: Vec<&str> = e.iter().map(|e| e.setting.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                MODE,
                BLUR_STRENGTH,
                DENOISE,
                ENHANCE,
                RELIGHT,
                LIGHT,
                DIRECTION,
                BRIGHTNESS
            ]
        );
        for g in &e {
            assert!(valid_setting_id(&g.setting.id), "{}", g.setting.id);
            assert!(is_global(&g.setting.id));
            assert_eq!(g.setting.scope, SCOPE_GLOBAL);
            assert!(!g.setting.stored);
            assert_eq!(Key::of(&g.setting.id), Some(g.key));
        }
        assert!(!is_global("camera.image.brightness"));
        assert!(is_global_group(BACKGROUND) && is_global_group(VIDEO));
        assert!(is_global_group(LIGHTING));
        assert_eq!(
            Key::of_group(LIGHTING),
            [Key::Relight, Key::Light, Key::Direction, Key::Brightness]
        );
        assert!(!is_global_group("image"));
        assert_eq!(Key::of_group(BACKGROUND), [Key::Mode, Key::BlurStrength]);
        assert_eq!(Key::of_group(VIDEO), [Key::Denoise, Key::Enhance]);
        assert!(Key::of_group("image").is_empty());
        // No camera: only the global groups, in order.
        let groups = tree(e).groups();
        let shown: Vec<(&str, &str, usize)> = groups
            .iter()
            .map(|g| (g.id.as_str(), g.label.as_str(), g.settings.len()))
            .collect();
        assert_eq!(
            shown,
            [
                ("background", "Background effect", 2),
                ("video", "Video", 2),
                ("lighting", "Lighting", 4)
            ]
        );
        assert!(groups.iter().all(|g| g.is_global()));
    }

    #[test]
    fn the_values_and_defaults_are_the_effect_settings() {
        let mut config = Config::default();
        config.effect.mode = EffectMode::Blur;
        config.effect.blur_strength = 0.335;
        let e = entries(&config, &RuntimeState::default(), None);
        let mode = get(&e, MODE);
        assert_eq!(mode.label, "Effect");
        assert_eq!(mode.hint.as_deref(), Some("segmented"));
        assert!(mode.enabled);
        let SettingKind::Enum {
            options,
            default,
            value,
        } = &mode.kind
        else {
            panic!("{mode:?}")
        };
        let options: Vec<(&str, &str)> = options
            .iter()
            .map(|(v, l)| (v.as_str(), l.as_str()))
            .collect();
        assert_eq!(
            options,
            [("none", "Off"), ("blur", "Blur"), ("replace", "Replace")]
        );
        assert_eq!((default.as_str(), value.as_str()), ("blur", "blur"));
        let blur = get(&e, BLUR_STRENGTH);
        assert_eq!(
            blur.kind,
            SettingKind::Int {
                min: 0,
                max: 100,
                step: 1,
                default: 34,
                value: 34,
                unit: Some("%".into())
            }
        );
        assert_eq!(blur.hint.as_deref(), Some("slider"));
        assert!(blur.enabled && blur.disabled_reason.is_empty());
        // The state file overrides, and says so.
        let runtime = RuntimeState {
            effect: Some(EffectMode::None),
            blur_strength: Some(1.0),
            denoise: Some(true),
            enhance: Some(true),
            ..RuntimeState::default()
        };
        let e = entries(&config, &runtime, Some(true));
        assert_eq!(
            get(&e, MODE).kind.value(),
            Some(SettingValue::Str("none".into()))
        );
        assert_eq!(
            get(&e, BLUR_STRENGTH).kind.value(),
            Some(SettingValue::Int(100))
        );
        assert_eq!(
            get(&e, BLUR_STRENGTH).kind.default_value(),
            Some(SettingValue::Int(34))
        );
        let denoise = get(&e, DENOISE);
        assert_eq!(denoise.label, "Noise removal");
        assert_eq!(denoise.hint.as_deref(), Some("switch"));
        assert_eq!(
            denoise.kind,
            SettingKind::Bool {
                default: false,
                value: true
            }
        );
        assert!(
            e.iter()
                .filter(|e| e.group() != LIGHTING)
                .all(|e| e.setting.stored)
        );
        assert!(
            e.iter()
                .filter(|e| e.group() == LIGHTING)
                .all(|e| !e.setting.stored)
        );
    }

    #[test]
    fn the_blur_strength_is_enabled_only_with_blur() {
        let e = entries(&Config::default(), &RuntimeState::default(), None);
        let blur = get(&e, BLUR_STRENGTH);
        assert!(!blur.enabled);
        assert_eq!(blur.disabled_reason, REASON_CHOOSE_BLUR);
        assert_eq!(
            blur.enabled_when,
            [Condition {
                id: MODE.into(),
                op: ConditionOp::Eq,
                value: SettingValue::Str("blur".into())
            }]
        );
        assert_eq!(
            check(&e, BLUR_STRENGTH, Some(&SettingValue::Int(40))),
            Err(Refusal::Disabled(REASON_CHOOSE_BLUR.into()))
        );
        let runtime = RuntimeState {
            effect: Some(EffectMode::Blur),
            ..RuntimeState::default()
        };
        let e = entries(&Config::default(), &runtime, None);
        assert!(get(&e, BLUR_STRENGTH).enabled);
        assert_eq!(
            check(&e, BLUR_STRENGTH, Some(&SettingValue::Int(40))),
            Ok(Write::BlurStrength(0.4))
        );
    }

    #[test]
    fn noise_removal_not_installed_is_greyed_and_off_with_the_choice_kept() {
        let runtime = RuntimeState {
            denoise: Some(true),
            ..RuntimeState::default()
        };
        let config = Config::default();
        let e = entries(&config, &runtime, Some(false));
        let d = get(&e, DENOISE);
        assert!(!d.enabled);
        assert_eq!(d.disabled_reason, REASON_NOT_INSTALLED);
        assert_eq!(d.kind.value(), Some(SettingValue::Bool(false)));
        assert!(d.stored, "the choice is kept");
        assert_eq!(
            check(&e, DENOISE, Some(&SettingValue::Bool(true))),
            Err(Refusal::Disabled(REASON_NOT_INSTALLED.into()))
        );
        // Installed, or unknown (no root): as before revision 8.
        for installed in [Some(true), None] {
            let e = entries(&config, &runtime, installed);
            let d = get(&e, DENOISE);
            assert!(d.enabled && d.disabled_reason.is_empty(), "{installed:?}");
            assert_eq!(d.kind.value(), Some(SettingValue::Bool(true)));
        }
        // Becoming installed is a delta, not a structural change.
        let old = tree(entries(&config, &runtime, Some(false)));
        let new = tree(entries(&config, &runtime, Some(true)));
        let delta = diff(&old, &new).unwrap();
        assert!(!delta.structural);
        assert_eq!(delta.changes.len(), 1);
        let c = &delta.changes[0];
        assert_eq!(c.id, DENOISE);
        assert_eq!(
            (c.value.clone(), c.enabled),
            (Some(SettingValue::Bool(true)), true)
        );
    }

    #[test]
    fn enhance_is_a_switch_after_noise_removal() {
        let runtime = RuntimeState {
            enhance: Some(true),
            ..RuntimeState::default()
        };
        let e = with_enhance(&runtime, Some(true), Some(Applies::Yes));
        let en = get(&e, ENHANCE);
        assert_eq!(en.label, "Enhance low-resolution camera");
        assert_eq!(en.hint.as_deref(), Some("switch"));
        assert_eq!(en.scope, SCOPE_GLOBAL);
        assert!(en.enabled && en.disabled_reason.is_empty() && en.stored);
        assert_eq!(
            en.kind,
            SettingKind::Bool {
                default: false,
                value: true
            }
        );
        assert_eq!(
            check(&e, ENHANCE, Some(&SettingValue::Bool(false))),
            Ok(Write::Enhance(false))
        );
        assert!(matches!(
            check(&e, ENHANCE, Some(&SettingValue::Int(1))),
            Err(Refusal::Invalid(m)) if m.contains("expects a boolean")
        ));
        // The configuration's value is the default.
        let mut config = Config::default();
        config.effect.enhance = true;
        let e = build(&Inputs {
            config: &config,
            runtime: &RuntimeState::default(),
            denoise_installed: None,
            enhance_installed: None,
            enhance_applies: None,
            relight_installed: None,
        });
        let en = get(&e, ENHANCE);
        assert_eq!(
            en.kind,
            SettingKind::Bool {
                default: true,
                value: true
            }
        );
        assert!(!en.stored);
        for r in [REASON_NOT_INSTALLED, REASON_FULL_RESOLUTION, REASON_FORMAT] {
            assert!(r.len() <= 96, "{r}");
        }
    }

    #[test]
    fn enhance_is_greyed_and_off_when_not_installed_or_not_needed() {
        let runtime = RuntimeState {
            enhance: Some(true),
            ..RuntimeState::default()
        };
        for (installed, applies, reason) in [
            (Some(false), Some(Applies::Yes), REASON_NOT_INSTALLED),
            (
                Some(false),
                Some(Applies::FullResolution),
                REASON_NOT_INSTALLED,
            ),
            (Some(false), Some(Applies::Format), REASON_NOT_INSTALLED),
            (Some(false), None, REASON_NOT_INSTALLED),
            (
                Some(true),
                Some(Applies::FullResolution),
                REASON_FULL_RESOLUTION,
            ),
            (None, Some(Applies::FullResolution), REASON_FULL_RESOLUTION),
            (Some(true), Some(Applies::Format), REASON_FORMAT),
            (None, Some(Applies::Format), REASON_FORMAT),
        ] {
            let e = with_enhance(&runtime, installed, applies);
            let en = get(&e, ENHANCE);
            assert!(!en.enabled, "{installed:?} {applies:?}");
            assert_eq!(en.disabled_reason, reason);
            assert_eq!(en.kind.value(), Some(SettingValue::Bool(false)));
            assert!(en.stored, "the choice is kept");
            assert_eq!(
                check(&e, ENHANCE, Some(&SettingValue::Bool(true))),
                Err(Refusal::Disabled(reason.into()))
            );
        }
        // No camera, or one it applies to: enabled, the choice in force.
        for (installed, applies) in [(Some(true), None), (None, None), (None, Some(Applies::Yes))] {
            let e = with_enhance(&runtime, installed, applies);
            let en = get(&e, ENHANCE);
            assert!(en.enabled && en.disabled_reason.is_empty());
            assert_eq!(en.kind.value(), Some(SettingValue::Bool(true)));
        }
    }

    #[test]
    fn enhance_applicability_changes_are_deltas() {
        let runtime = RuntimeState {
            enhance: Some(true),
            ..RuntimeState::default()
        };
        // Another camera (one at full resolution, then a small one).
        let old = tree(with_enhance(
            &runtime,
            Some(true),
            Some(Applies::FullResolution),
        ));
        let new = tree(with_enhance(&runtime, Some(true), Some(Applies::Yes)));
        let delta = diff(&old, &new).unwrap();
        assert!(!delta.structural);
        assert_eq!(delta.changes.len(), 1);
        let c = &delta.changes[0];
        assert_eq!(c.id, ENHANCE);
        assert_eq!(
            (c.value.clone(), c.enabled, c.disabled_reason.as_str()),
            (Some(SettingValue::Bool(true)), true, "")
        );
        let back = diff(&new, &old).unwrap();
        assert_eq!(back.changes.len(), 1);
        assert_eq!(back.changes[0].disabled_reason, REASON_FULL_RESOLUTION);
        // Becoming installed too.
        let missing = tree(with_enhance(&runtime, Some(false), Some(Applies::Yes)));
        let delta = diff(&missing, &new).unwrap();
        assert!(!delta.structural);
        assert_eq!(delta.changes.len(), 1);
        assert_eq!(delta.changes[0].id, ENHANCE);
    }

    #[test]
    fn relighting_is_a_switch_with_three_settings_of_its_light() {
        let e = with_relight(&RuntimeState::default(), Some(true));
        let r = get(&e, RELIGHT);
        assert_eq!(r.label, "Relighting");
        assert_eq!(r.hint.as_deref(), Some("switch"));
        assert!(r.enabled && !r.stored);
        assert_eq!(
            r.kind,
            SettingKind::Bool {
                default: false,
                value: false
            }
        );
        let light = get(&e, LIGHT);
        assert_eq!(light.label, "Light");
        assert_eq!(light.hint.as_deref(), Some("segmented"));
        let SettingKind::Enum {
            options,
            default,
            value,
        } = &light.kind
        else {
            panic!("{light:?}")
        };
        let options: Vec<(&str, &str)> = options
            .iter()
            .map(|(v, l)| (v.as_str(), l.as_str()))
            .collect();
        assert_eq!(
            options,
            [("warm", "Warm"), ("neutral", "Neutral"), ("cool", "Cool")]
        );
        assert_eq!((default.as_str(), value.as_str()), ("neutral", "neutral"));
        let dir = get(&e, DIRECTION);
        assert_eq!(
            (dir.label.as_str(), dir.hint.as_deref()),
            ("Light direction", Some("slider"))
        );
        assert_eq!(
            dir.kind,
            SettingKind::Int {
                min: -90,
                max: 90,
                step: 5,
                default: 0,
                value: 0,
                unit: Some("°".into())
            }
        );
        let b = get(&e, BRIGHTNESS);
        assert_eq!(
            (b.label.as_str(), b.hint.as_deref()),
            ("Brightness", Some("slider"))
        );
        assert_eq!(
            b.kind,
            SettingKind::Int {
                min: 50,
                max: 200,
                step: 5,
                default: 100,
                value: 100,
                unit: Some("%".into())
            }
        );
        // Off: the three are greyed until it is on.
        for id in [LIGHT, DIRECTION, BRIGHTNESS] {
            let s = get(&e, id);
            assert_eq!(s.scope, SCOPE_GLOBAL);
            assert!(!s.enabled, "{id}");
            assert_eq!(s.disabled_reason, REASON_TURN_ON_RELIGHT);
            assert_eq!(
                s.enabled_when,
                [Condition {
                    id: RELIGHT.into(),
                    op: ConditionOp::Eq,
                    value: SettingValue::Bool(true)
                }]
            );
        }
        assert_eq!(
            check(&e, DIRECTION, Some(&SettingValue::Int(-30))),
            Err(Refusal::Disabled(REASON_TURN_ON_RELIGHT.into()))
        );
        // On, with the state file's values.
        let runtime = RuntimeState {
            relight: Some(true),
            relight_light: Some(RelightLight::Cool),
            relight_direction: Some(-45),
            relight_brightness: Some(150),
            ..RuntimeState::default()
        };
        let e = with_relight(&runtime, Some(true));
        for id in [RELIGHT, LIGHT, DIRECTION, BRIGHTNESS] {
            let s = get(&e, id);
            assert!(
                s.enabled && s.disabled_reason.is_empty() && s.stored,
                "{id}"
            );
        }
        assert_eq!(
            get(&e, LIGHT).kind.value(),
            Some(SettingValue::Str("cool".into()))
        );
        assert_eq!(
            get(&e, DIRECTION).kind.value(),
            Some(SettingValue::Int(-45))
        );
        assert_eq!(
            get(&e, BRIGHTNESS).kind.value(),
            Some(SettingValue::Int(150))
        );
        assert_eq!(
            check(&e, RELIGHT, Some(&SettingValue::Bool(false))),
            Ok(Write::Relight(false))
        );
        assert_eq!(
            check(&e, LIGHT, Some(&SettingValue::Str("warm".into()))),
            Ok(Write::Light(RelightLight::Warm))
        );
        assert_eq!(
            check(&e, DIRECTION, Some(&SettingValue::Int(-90))),
            Ok(Write::Direction(-90))
        );
        assert_eq!(
            check(&e, BRIGHTNESS, Some(&SettingValue::Int(200))),
            Ok(Write::Brightness(200))
        );
        for (id, v, why) in [
            (
                LIGHT,
                Some(SettingValue::Str("blur".into())),
                "is not one of its options (warm, neutral, cool)",
            ),
            (
                LIGHT,
                Some(SettingValue::Str("Warm".into())),
                "is not one of its options",
            ),
            (LIGHT, Some(SettingValue::Int(1)), "expects an option id"),
            (
                DIRECTION,
                Some(SettingValue::Int(95)),
                "outside its range -90..=90",
            ),
            (DIRECTION, Some(SettingValue::Int(-91)), "outside its range"),
            (
                DIRECTION,
                Some(SettingValue::Int(-32)),
                "not a whole number of steps of 5 from -90",
            ),
            (
                BRIGHTNESS,
                Some(SettingValue::Int(45)),
                "outside its range 50..=200",
            ),
            (
                BRIGHTNESS,
                Some(SettingValue::Int(101)),
                "not a whole number of steps of 5",
            ),
            (RELIGHT, Some(SettingValue::Int(1)), "expects a boolean"),
        ] {
            match check(&e, id, v.as_ref()) {
                Err(Refusal::Invalid(m)) => assert!(m.contains(why) && m.starts_with(id), "{m}"),
                other => panic!("{id} {v:?}: {other:?}"),
            }
        }
        // The configuration's values are the defaults.
        let mut config = Config::default();
        config.effect.relight = true;
        config.effect.relight_light = RelightLight::Warm;
        config.effect.relight_direction = 30;
        config.effect.relight_brightness = 80;
        let e = build(&Inputs {
            config: &config,
            runtime: &RuntimeState::default(),
            denoise_installed: None,
            enhance_installed: None,
            enhance_applies: None,
            relight_installed: None,
        });
        assert_eq!(
            get(&e, RELIGHT).kind.default_value(),
            Some(SettingValue::Bool(true))
        );
        assert_eq!(
            get(&e, LIGHT).kind.default_value(),
            Some(SettingValue::Str("warm".into()))
        );
        assert_eq!(
            get(&e, DIRECTION).kind.default_value(),
            Some(SettingValue::Int(30))
        );
        assert_eq!(
            get(&e, BRIGHTNESS).kind.default_value(),
            Some(SettingValue::Int(80))
        );
        assert!(get(&e, DIRECTION).enabled);
        assert!(REASON_TURN_ON_RELIGHT.len() <= 96);
    }

    #[test]
    fn relighting_not_installed_is_greyed_and_off_with_the_choices_kept() {
        let runtime = RuntimeState {
            relight: Some(true),
            relight_direction: Some(20),
            ..RuntimeState::default()
        };
        let e = with_relight(&runtime, Some(false));
        let r = get(&e, RELIGHT);
        assert!(!r.enabled && r.stored, "the choice is kept");
        assert_eq!(r.disabled_reason, REASON_NOT_INSTALLED);
        assert_eq!(r.kind.value(), Some(SettingValue::Bool(false)));
        for id in [RELIGHT, LIGHT, DIRECTION, BRIGHTNESS] {
            let s = get(&e, id);
            assert!(!s.enabled, "{id}");
            assert_eq!(s.disabled_reason, REASON_NOT_INSTALLED, "{id}");
        }
        assert_eq!(get(&e, DIRECTION).kind.value(), Some(SettingValue::Int(20)));
        assert_eq!(
            check(&e, RELIGHT, Some(&SettingValue::Bool(true))),
            Err(Refusal::Disabled(REASON_NOT_INSTALLED.into()))
        );
        assert_eq!(
            check(&e, BRIGHTNESS, Some(&SettingValue::Int(100))),
            Err(Refusal::Disabled(REASON_NOT_INSTALLED.into()))
        );
        // Unknown (no root): as installed.
        let e = with_relight(&runtime, None);
        assert!(get(&e, RELIGHT).enabled && get(&e, DIRECTION).enabled);
        // Becoming installed is a delta of the four, not a structural change.
        let old = tree(with_relight(&runtime, Some(false)));
        let new = tree(with_relight(&runtime, Some(true)));
        let delta = diff(&old, &new).unwrap();
        assert!(!delta.structural);
        let ids: Vec<&str> = delta.changes.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, [RELIGHT, LIGHT, DIRECTION, BRIGHTNESS]);
        assert!(delta.changes.iter().all(|c| c.enabled));
    }

    #[test]
    fn turning_relighting_on_is_one_delta_with_the_settings_it_enables() {
        let old = tree(with_relight(&RuntimeState::default(), Some(true)));
        let runtime = RuntimeState {
            relight: Some(true),
            ..RuntimeState::default()
        };
        let new = tree(with_relight(&runtime, Some(true)));
        let delta = diff(&old, &new).unwrap();
        assert!(!delta.structural);
        let ids: Vec<&str> = delta.changes.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, [RELIGHT, LIGHT, DIRECTION, BRIGHTNESS]);
        assert!(delta.changes.iter().all(|c| c.enabled));
    }

    #[test]
    fn writes_are_checked_unknown_then_disabled_then_invalid() {
        let runtime = RuntimeState {
            effect: Some(EffectMode::Blur),
            ..RuntimeState::default()
        };
        let e = entries(&Config::default(), &runtime, Some(true));
        assert!(matches!(
            check(&e, "effect.background.nope", Some(&SettingValue::Int(1))),
            Err(Refusal::Unknown(_))
        ));
        assert_eq!(
            check(&e, MODE, Some(&SettingValue::Str("replace".into()))),
            Ok(Write::Mode(EffectMode::Replace))
        );
        for (id, v, why) in [
            (
                MODE,
                Some(SettingValue::Str("Blur".into())),
                "is not one of its options",
            ),
            (MODE, Some(SettingValue::Bool(true)), "expects an option id"),
            (
                BLUR_STRENGTH,
                Some(SettingValue::Int(101)),
                "outside its range 0..=100",
            ),
            (
                BLUR_STRENGTH,
                Some(SettingValue::Int(-1)),
                "outside its range",
            ),
            (BLUR_STRENGTH, None, "expects an integer"),
            (DENOISE, Some(SettingValue::Int(1)), "expects a boolean"),
        ] {
            match check(&e, id, v.as_ref()) {
                Err(Refusal::Invalid(m)) => assert!(m.contains(why) && m.starts_with(id), "{m}"),
                other => panic!("{id} {v:?}: {other:?}"),
            }
        }
        assert_eq!(
            check(&e, BLUR_STRENGTH, Some(&SettingValue::Int(0))),
            Ok(Write::BlurStrength(0.0))
        );
        assert_eq!(
            check(&e, DENOISE, Some(&SettingValue::Bool(false))),
            Ok(Write::Denoise(false))
        );
    }

    #[test]
    fn a_mode_change_is_one_delta_with_the_strength_it_enables() {
        let config = Config::default();
        let old = tree(entries(&config, &RuntimeState::default(), None));
        let runtime = RuntimeState {
            effect: Some(EffectMode::Blur),
            ..RuntimeState::default()
        };
        let new = tree(entries(&config, &runtime, None));
        let delta = diff(&old, &new).unwrap();
        assert!(!delta.structural);
        let ids: Vec<&str> = delta.changes.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, [MODE, BLUR_STRENGTH]);
        assert!(delta.changes[0].stored && delta.changes[1].enabled);
        assert_eq!(diff(&new, &new.clone()), None);
        // A new configuration default is structural.
        let mut config2 = Config::default();
        config2.effect.blur_strength = 0.9;
        let moved = tree(entries(&config2, &runtime, None));
        assert!(diff(&new, &moved).unwrap().structural);
        // Global settings appearing are structural too.
        assert!(diff(&CameraSettings::none(), &new).unwrap().structural);
    }

    #[test]
    fn forgetting_a_key_drops_its_override() {
        let mut runtime = RuntimeState {
            effect: Some(EffectMode::Blur),
            blur_strength: Some(0.2),
            denoise: Some(true),
            enhance: Some(false),
            relight: Some(true),
            relight_light: Some(RelightLight::Warm),
            relight_direction: Some(10),
            relight_brightness: Some(90),
            ..RuntimeState::default()
        };
        for k in [Key::Relight, Key::Light, Key::Direction, Key::Brightness] {
            assert!(k.forget(&mut runtime), "{k:?}");
            assert!(!k.forget(&mut runtime), "{k:?}");
        }
        assert_eq!(
            (
                runtime.relight,
                runtime.relight_light,
                runtime.relight_direction,
                runtime.relight_brightness
            ),
            (None, None, None, None)
        );
        assert!(Key::Enhance.forget(&mut runtime));
        assert_eq!(runtime.enhance, None);
        assert!(Key::BlurStrength.forget(&mut runtime));
        assert!(!Key::BlurStrength.forget(&mut runtime));
        assert_eq!(runtime.blur_strength, None);
        assert_eq!(runtime.effect, Some(EffectMode::Blur));
        assert!(Key::Mode.forget(&mut runtime) && Key::Denoise.forget(&mut runtime));
        assert_eq!((runtime.effect, runtime.denoise), (None, None));
        assert_eq!(percent(0.005), 1);
        assert_eq!(percent(0.5), 50);
    }
}
