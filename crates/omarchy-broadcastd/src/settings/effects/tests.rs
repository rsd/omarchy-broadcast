//! The global settings with the Maxine module's description: the tree of
//! revision 8, its values, reasons and writes.

use super::*;
use crate::modules::testing;
use crate::settings::{CameraSettings, diff};
use obc_core::RelightLight;
use obc_core::modules::legacy::{BRIGHTNESS, DENOISE, DIRECTION, ENHANCE, LIGHT, RELIGHT};
use obc_ipc::valid_setting_id;

const VIDEO: &str = "video";
const LIGHTING: &str = "lighting";
/// The Maxine module's switch of the HDR look.
const HDR_LOOK: &str = "effect.video.hdr";
const REASON_NOT_INSTALLED: &str = "Not installed";
const REASON_TURN_ON_RELIGHT: &str = "Turn on relighting to change this";
/// The global settings with the Maxine module, its stages available
/// but those `unavailable` (stage, installed: `Some(false)` is not
/// installed; `None`, nothing known, offers it as `Some(true)` does).
fn with(
    config: &Config,
    runtime: &RuntimeState,
    unavailable: &[(&str, Option<bool>)],
    applies: Option<Applies>,
) -> Vec<GlobalEntry> {
    let missing: Vec<(&str, &str)> = unavailable
        .iter()
        .filter(|(_, i)| *i == Some(false))
        .map(|(s, _)| (*s, REASON_NOT_INSTALLED))
        .collect();
    let modules = testing::maxine(&missing);
    let mut runtime = runtime.clone();
    runtime.migrate_legacy_settings();
    build(&Inputs {
        config,
        runtime: &runtime,
        modules: &modules,
        enhance_applies: applies,
    })
}

fn entries(config: &Config, runtime: &RuntimeState, installed: Option<bool>) -> Vec<GlobalEntry> {
    with(config, runtime, &[("denoise", installed)], None)
}

/// The settings with enhancement `installed` and `applies` to the
/// camera (noise removal installed).
fn with_enhance(
    runtime: &RuntimeState,
    installed: Option<bool>,
    applies: Option<Applies>,
) -> Vec<GlobalEntry> {
    with(
        &Config::default(),
        runtime,
        &[("enhance", installed)],
        applies,
    )
}

/// The settings with relighting `installed` (the rest installed).
fn with_relight(runtime: &RuntimeState, installed: Option<bool>) -> Vec<GlobalEntry> {
    with(&Config::default(), runtime, &[("relight", installed)], None)
}

/// The write of module setting `id` with `value`, as [`check`] returns
/// it.
fn written(e: &[GlobalEntry], id: &str, value: StoredValue) -> Write {
    let g = e.iter().find(|g| g.setting.id == id).unwrap();
    Write::Setting {
        id: id.into(),
        label: g.setting.label.clone(),
        binding: g.binding.clone(),
        value,
    }
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
            HDR_LOOK,
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
    }
    assert!(!is_global("camera.image.brightness"));
    let in_group = |group: &str| -> Vec<&str> {
        e.iter()
            .filter(|g| g.group == group)
            .map(|g| g.setting.id.as_str())
            .collect()
    };
    assert_eq!(in_group(LIGHTING), [RELIGHT, LIGHT, DIRECTION, BRIGHTNESS]);
    assert_eq!(in_group(BACKGROUND), [MODE, BLUR_STRENGTH]);
    assert_eq!(in_group(VIDEO), [DENOISE, ENHANCE, HDR_LOOK]);
    assert!(in_group("image").is_empty());
    let bindings: Vec<&Binding> = e.iter().map(|g| &g.binding).collect();
    let hdr = Binding::Enable(StageId::new("hdr").unwrap());
    assert_eq!(
        bindings,
        [
            &Binding::Mode,
            &Binding::BlurStrength,
            &Binding::Enable(StageId::DENOISE),
            &Binding::Enable(StageId::ENHANCE),
            &hdr,
            &Binding::Enable(StageId::RELIGHT),
            &Binding::Param(StageId::RELIGHT, Apply::Reload),
            &Binding::Param(StageId::RELIGHT, Apply::Live),
            &Binding::Param(StageId::RELIGHT, Apply::Live),
        ]
    );
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
            ("video", "Video", 3),
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
    // Stored: what the state holds (here the background, noise removal
    // and enhancement); the HDR look's switch is at its default.
    assert!(
        e.iter()
            .filter(|e| e.group() != LIGHTING && e.setting.id != HDR_LOOK)
            .all(|e| e.setting.stored)
    );
    assert!(!get(&e, HDR_LOOK).stored);
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
        Ok(written(&e, ENHANCE, StoredValue::Bool(false)))
    );
    assert!(matches!(
        check(&e, ENHANCE, Some(&SettingValue::Int(1))),
        Err(Refusal::Invalid(m)) if m.contains("expects a boolean")
    ));
    // The configuration's value is the default.
    let mut config = Config::default();
    config.effect.enhance = true;
    let e = with(
        &config,
        &RuntimeState::default(),
        &[("denoise", None), ("enhance", None), ("relight", None)],
        None,
    );
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
        Ok(written(&e, RELIGHT, StoredValue::Bool(false)))
    );
    assert_eq!(
        check(&e, LIGHT, Some(&SettingValue::Str("warm".into()))),
        Ok(written(&e, LIGHT, StoredValue::Str("warm".into())))
    );
    assert_eq!(
        check(&e, DIRECTION, Some(&SettingValue::Int(-90))),
        Ok(written(&e, DIRECTION, StoredValue::Int(-90)))
    );
    assert_eq!(
        check(&e, BRIGHTNESS, Some(&SettingValue::Int(200))),
        Ok(written(&e, BRIGHTNESS, StoredValue::Int(200)))
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
    let e = with(
        &config,
        &RuntimeState::default(),
        &[("denoise", None), ("enhance", None), ("relight", None)],
        None,
    );
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
        Ok(written(&e, DENOISE, StoredValue::Bool(false)))
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
fn forgetting_a_setting_drops_its_value() {
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
    runtime.migrate_legacy_settings();
    for id in [RELIGHT, LIGHT, DIRECTION, BRIGHTNESS] {
        assert!(forget(&mut runtime, id), "{id}");
        assert!(!forget(&mut runtime, id), "{id}");
    }
    assert!(forget(&mut runtime, ENHANCE));
    assert!(!runtime.settings.contains_key(ENHANCE));
    assert!(forget(&mut runtime, BLUR_STRENGTH));
    assert!(!forget(&mut runtime, BLUR_STRENGTH));
    assert_eq!(runtime.blur_strength, None);
    assert_eq!(runtime.effect, Some(EffectMode::Blur));
    assert!(forget(&mut runtime, MODE) && forget(&mut runtime, DENOISE));
    assert_eq!(runtime.effect, None);
    assert!(runtime.settings.is_empty());
    assert_eq!(percent(0.005), 1);
    assert_eq!(percent(0.5), 50);
}

/// While no enabled module offers the `background` stage, a mode in force
/// that is not `none` is still published, alone, so that it can be turned
/// off; `blur` and `replace` are refused with the reason.
#[test]
fn a_background_effect_no_module_runs_can_still_be_turned_off() {
    let modules = Modules::empty();
    let config = Config::default();
    let build_with = |runtime: &RuntimeState| {
        build(&Inputs {
            config: &config,
            runtime,
            modules: &modules,
            enhance_applies: None,
        })
    };
    assert!(build_with(&RuntimeState::default()).is_empty(), "off: none");
    let runtime = RuntimeState {
        effect: Some(EffectMode::Blur),
        ..RuntimeState::default()
    };
    let e = build_with(&runtime);
    let ids: Vec<&str> = e.iter().map(|e| e.setting.id.as_str()).collect();
    assert_eq!(ids, [MODE], "the mode alone");
    assert_eq!(e[0].group, BACKGROUND);
    let mode = get(&e, MODE);
    assert!(mode.enabled);
    assert_eq!(mode.kind.value(), Some(SettingValue::Str("blur".into())));
    let str_value = |s: &str| SettingValue::Str(s.into());
    assert_eq!(
        check(&e, MODE, Some(&str_value("none"))),
        Ok(Write::Mode(EffectMode::None))
    );
    for refused in ["blur", "replace"] {
        assert_eq!(
            check(&e, MODE, Some(&str_value(refused))),
            Err(Refusal::Disabled(REASON_NO_BACKGROUND.into())),
            "{refused}"
        );
    }
    assert!(matches!(
        check(&e, MODE, Some(&str_value("sepia"))),
        Err(Refusal::Invalid(_))
    ));
    assert!(matches!(
        check(&e, BLUR_STRENGTH, Some(&SettingValue::Int(50))),
        Err(Refusal::Unknown(_))
    ));
}

/// A condition's operand is published as it compares: a whole number
/// written as a float is an integer; one no setting value can be leaves
/// the setting out rather than dropping the condition (which would enable
/// it); `in` with no list holds for no value.
#[test]
fn a_condition_operand_is_never_dropped_silently() {
    assert_eq!(
        wire_json(&serde_json::json!(2.0)),
        Some(SettingValue::Int(2))
    );
    assert_eq!(wire_json(&serde_json::json!(2.5)), None);
    assert_eq!(
        wire_json(&serde_json::json!([1.0, 3])),
        Some(SettingValue::List(vec![
            SettingValue::Int(1),
            SettingValue::Int(3)
        ]))
    );
    assert!(!holds(
        ConditionOp::In,
        &SettingValue::Int(1),
        &SettingValue::Int(1)
    ));
    let json = r#"{"groups":[{"id":"x","label":"X","settings":[
        {"id":"effect.x.level","label":"Level","type":"int","min":0,"max":10,"step":1,
         "default":2,"hint":"slider"},
        {"id":"effect.x.whole","label":"Whole","type":"bool","default":false,
         "enabled_when":[["effect.x.level","eq",2.0]],"disabled_reason":"Set level 2"},
        {"id":"effect.x.half","label":"Half","type":"bool","default":false,
         "enabled_when":[["effect.x.level","eq",2.5]],"disabled_reason":"Never"}]}]}"#;
    let modules = testing::modules(vec![(
        testing::entry(
            "abi = 1\nid = \"org.example.x\"\nname = \"X\"\nversion = \"1\"\n\
             licence = \"MIT\"\nkind = \"video-stage\"\nlibrary = \"/m/libx.so\"\n\
             [[stage]]\nid = \"xstage\"\nslot = \"post-composite\"\nrank = 1\n\
             device = \"cpu\"\nframes = [\"host\"]\nformats = [\"nv12\"]\n",
            "/m/x.toml",
        ),
        Ok(crate::backend::OpenedModule {
            module: None,
            stages: Vec::new(),
            settings: testing::settings(json, &["xstage"]),
        }),
    )]);
    let config = Config::default();
    let runtime = RuntimeState::default();
    let e = build(&Inputs {
        config: &config,
        runtime: &runtime,
        modules: &modules,
        enhance_applies: None,
    });
    let ids: Vec<&str> = e.iter().map(|e| e.setting.id.as_str()).collect();
    assert_eq!(
        ids,
        ["effect.x.level", "effect.x.whole"],
        "the half one is left out"
    );
    let whole = get(&e, "effect.x.whole");
    assert!(whole.enabled, "level 2 holds");
    assert_eq!(whole.enabled_when[0].value, SettingValue::Int(2));
}
