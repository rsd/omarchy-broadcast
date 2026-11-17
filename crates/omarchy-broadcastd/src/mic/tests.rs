//! The microphone's model, and its agent against a fake worker and a fake
//! service manager (never the user's).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use obc_core::config::{Config, MicGpuPolicy, RuntimeState};
use obc_core::config_d::ConfigSet;
use obc_core::modules::StoredValue;
use obc_ipc::{IpcError, MicStatus, ModuleInfo, SettingKind, SettingValue, Snapshot};
use serde_json::{Value as Json, json};

use super::agent::{
    Agent, Call, HOST_POLL, MicEvent, MicRequest, PIN_RETRIES, UnitError, Units, Worker,
    WorkerError, WorkerEvent,
};
use super::*;
use crate::controller::{Command, Note, Published};

// ---- the model ----

/// The worker's `GetSettings` document: the noise group from two modules,
/// the CPU module running the stage (G4), and an echo group.
fn worker_settings() -> Json {
    json!({"groups": [
        {"id": "noise", "label": "Noise removal", "module": "br.com.dias.maxine-afx",
         "surface": "mic", "settings": [
            {"id": "mic.noise.enabled", "label": "Noise removal", "type": "bool",
             "scope": "global", "stored": false, "enabled": false,
             "disabled_reason": "Handled by Voice (CPU) instead",
             "disabled_kind": "other_provider", "enabled_when": [],
             "default": true, "value": true, "stage": "mic-denoise", "role": "enable"},
            {"id": "mic.noise.strength", "label": "Strength", "type": "int",
             "scope": "global", "stored": false, "enabled": false,
             "disabled_reason": "Handled by Voice (CPU) instead",
             "disabled_kind": "other_provider", "enabled_when": [],
             "min": 0, "max": 100, "step": 5, "unit": "%", "default": 100, "value": 100}]},
        {"id": "noise", "label": "Noise removal", "module": "br.com.dias.voice-cpu",
         "surface": "mic", "settings": [
            {"id": "mic.noise.enabled", "label": "Noise removal", "type": "bool",
             "scope": "global", "stored": false, "enabled": true, "disabled_reason": "",
             "enabled_when": [], "default": true, "value": true, "hint": "switch"},
            {"id": "mic.noise.strength", "label": "Strength", "type": "int",
             "scope": "global", "stored": false, "enabled": true, "disabled_reason": "",
             "enabled_when": [["mic.noise.enabled", "eq", true]],
             "min": 0, "max": 100, "step": 5, "unit": "%", "default": 80, "value": 80}]},
        {"id": "voice", "label": "Studio voice", "module": "br.com.dias.maxine-afx",
         "settings": [
            {"id": "mic.voice.profile", "label": "Profile", "type": "enum",
             "scope": "global", "stored": false, "enabled": true, "disabled_reason": "",
             "enabled_when": [], "options": [["a", "A"], ["b", "B"]], "default": "a"},
            {"id": "effect.not.mine", "label": "x", "type": "bool", "enabled": true,
             "default": false}]},
    ]})
}

fn catalogue() -> Catalogue {
    Catalogue {
        known: true,
        groups: parse_groups(&worker_settings()),
        modules: Vec::new(),
        devices: vec![
            ("alsa_input.usb-a".into(), "USB A".into()),
            ("alsa_input.usb-b".into(), String::new()),
        ],
        hardware_gain: None,
    }
}

/// The worker's status with a hardware gain (an MX Brio's `Headset`
/// control: +18 to +54 dB in 1 dB steps, at +40 dB).
fn status_with_gain(db: f64) -> Json {
    json!({"state": "active", "hardware_gain": {
        "control": "Headset", "db": db, "min_db": 18.0, "max_db": 54.0,
        "step_db": 1.0, "applied": false}})
}

#[test]
fn the_hardware_gain_is_read_from_the_workers_status() {
    let g = parse_hardware_gain(&status_with_gain(40.0)).unwrap();
    assert_eq!(
        g,
        HardwareGain {
            control: "Headset".into(),
            db: 40.0,
            min_db: 18.0,
            max_db: 54.0,
            step_db: 1.0,
            applied: false,
        }
    );
    assert_eq!(g.whole_db(), Some((18, 54, 1)));
    // None reported, or one that does not read.
    assert_eq!(parse_hardware_gain(&json!({"state": "active"})), None);
    assert_eq!(parse_hardware_gain(&json!({"hardware_gain": null})), None);
    for (key, bad) in [
        ("db", json!("40")),
        ("min_db", json!(-1000.0)),
        ("max_db", json!(1.0)),
        ("step_db", json!(-1.0)),
    ] {
        let mut s = status_with_gain(40.0);
        s["hardware_gain"][key] = bad;
        assert_eq!(parse_hardware_gain(&s), None, "{key}");
    }
    // A fractional range is rounded inwards, the step always 1 dB (the
    // worker rounds to the device's step).
    let frac = HardwareGain {
        min_db: -12.5,
        max_db: 33.7,
        step_db: 0.375,
        ..g.clone()
    };
    assert_eq!(frac.whole_db(), Some((-12, 33, 1)));
    let coarse = HardwareGain {
        step_db: 1.5,
        ..g.clone()
    };
    assert_eq!(coarse.whole_db(), Some((18, 54, 1)));
    // No whole dB within it: not offered.
    let narrow = HardwareGain {
        min_db: 3.2,
        max_db: 3.8,
        step_db: 0.0,
        ..g
    };
    assert_eq!(narrow.whole_db(), None);
}

#[test]
fn the_hardware_gain_is_an_input_setting_bound_to_its_config_key() {
    use obc_core::config_schema::ConfigValue as C;
    let config = Config::default();
    let runtime = RuntimeState::default();
    let mut cat = catalogue();
    cat.hardware_gain = parse_hardware_gain(&status_with_gain(40.4));
    let g = groups_of(&config, &runtime, &cat, true);
    let ids: Vec<&str> = g[0].settings.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, [ENABLED, DEVICE, HARDWARE_GAIN, USE_AS_DEFAULT]);
    let s = find(&g, HARDWARE_GAIN).unwrap();
    assert_eq!(s.label, "Hardware gain");
    assert!(!s.stored);
    assert_eq!(
        s.kind,
        SettingKind::Int {
            min: 18,
            max: 54,
            step: 1,
            default: 40,
            value: 40,
            unit: Some("dB".into()),
            display: None,
        }
    );
    assert_eq!(
        check(&g, HARDWARE_GAIN, Some(&SettingValue::Int(30))),
        Ok(Write::Config(KEY_HARDWARE_GAIN, C::Float(30.0)))
    );
    for bad in [17, 55] {
        assert!(
            matches!(
                check(&g, HARDWARE_GAIN, Some(&SettingValue::Int(bad))),
                Err(Refusal::Invalid(_))
            ),
            "{bad}"
        );
    }
    assert!(matches!(
        check(&g, HARDWARE_GAIN, Some(&SettingValue::Bool(true))),
        Err(Refusal::Invalid(_))
    ));
    assert_eq!(config_key_of(HARDWARE_GAIN), Some(KEY_HARDWARE_GAIN));
    assert!(INPUT_KEYS.contains(&KEY_HARDWARE_GAIN));
    // Stored once the managed file sets it.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::create_dir_all(dir.path().join("config.d")).unwrap();
    std::fs::write(
        dir.path().join(MANAGED_REL),
        "[mic]\nhardware_gain_db = 38.5\n",
    )
    .unwrap();
    let set = ConfigSet::load(&path).unwrap();
    let g = groups(&Inputs {
        config: &set.config,
        config_set: &set,
        runtime: &runtime,
        catalogue: &cat,
        host_on: true,
    });
    assert!(find(&g, HARDWARE_GAIN).unwrap().stored);
    // No gain reported: no setting.
    cat.hardware_gain = None;
    let g = groups_of(&config, &runtime, &cat, true);
    assert!(find(&g, HARDWARE_GAIN).is_none());
}

#[test]
fn a_key_a_later_file_sets_greys_its_setting_as_not_applicable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::create_dir_all(dir.path().join("config.d")).unwrap();
    std::fs::write(
        dir.path().join("config.d/90-local.toml"),
        "[mic]\nenabled = false\n",
    )
    .unwrap();
    let set = ConfigSet::load(&path).unwrap();
    let runtime = RuntimeState::default();
    let cat = catalogue();
    let g = groups(&Inputs {
        config: &set.config,
        config_set: &set,
        runtime: &runtime,
        catalogue: &cat,
        host_on: true,
    });
    let s = find(&g, ENABLED).unwrap();
    assert!(!s.enabled);
    assert_eq!(s.disabled_reason, "Overridden by config.d/90-local.toml");
    assert_eq!(s.disabled_kind, obc_ipc::settings::disabled::NOT_APPLICABLE);
    let d = find(&g, DEVICE).unwrap();
    assert!(d.enabled && d.disabled_kind.is_empty());
}

#[test]
fn a_coarse_device_step_refuses_no_whole_db_in_range() {
    // A 1.5 dB step from -6 dB, at 10.5 dB: every whole dB is taken (the
    // worker rounds), the one shown and the ends among them.
    let config = Config::default();
    let runtime = RuntimeState::default();
    let mut cat = catalogue();
    cat.hardware_gain = Some(HardwareGain {
        control: "Mic".into(),
        db: 10.5,
        min_db: -6.0,
        max_db: 18.0,
        step_db: 1.5,
        applied: false,
    });
    let g = groups_of(&config, &runtime, &cat, true);
    let s = find(&g, HARDWARE_GAIN).unwrap();
    let SettingKind::Int {
        min,
        max,
        step,
        value,
        ..
    } = s.kind
    else {
        panic!("{:?}", s.kind)
    };
    assert_eq!((min, max, step), (-6, 18, 1));
    for v in [min, max, value, -5, 0, 7, 17] {
        assert!(
            check(&g, HARDWARE_GAIN, Some(&SettingValue::Int(v))).is_ok(),
            "{v}"
        );
    }
}

#[test]
fn a_status_without_the_gain_is_pending() {
    assert!(gain_pending(&json!({"state": "active"})));
    assert!(!gain_pending(&json!({"hardware_gain": null})));
    assert!(!gain_pending(&status_with_gain(40.0)));
}

fn groups_of(
    config: &Config,
    runtime: &RuntimeState,
    catalogue: &Catalogue,
    host_on: bool,
) -> Vec<SettingsGroup> {
    let dir = tempfile::tempdir().unwrap();
    let set = ConfigSet::defaults(&dir.path().join("config.toml"));
    groups(&Inputs {
        config,
        config_set: &set,
        runtime,
        catalogue,
        host_on,
    })
}

#[test]
fn the_worker_groups_are_one_per_id_the_active_providers() {
    let g = parse_groups(&worker_settings());
    let ids: Vec<&str> = g.iter().map(|g| g.id.as_str()).collect();
    assert_eq!(ids, ["noise", "voice"]);
    let noise = &g[0];
    assert!(noise.settings.iter().all(|s| s.enabled), "{noise:?}");
    assert_eq!(
        noise.settings[1].kind,
        SettingKind::Int {
            min: 0,
            max: 100,
            step: 5,
            default: 80,
            value: 80,
            unit: Some("%".into()),
            display: None,
        }
    );
    assert_eq!(noise.settings[1].enabled_when.len(), 1);
    assert_eq!(noise.surface.as_deref(), Some("mic"));
    // An id that is not a microphone id is dropped; an enum without a
    // value takes its default.
    assert_eq!(g[1].settings.len(), 1);
    assert_eq!(
        g[1].settings[0].kind.value(),
        Some(SettingValue::Str("a".into()))
    );
    // Only one provider: kept as it is.
    let one = json!({"groups": [worker_settings()["groups"][0].clone()]});
    let g = parse_groups(&one);
    assert_eq!(g.len(), 1);
    assert!(!g[0].settings[0].enabled);
    assert!(parse_groups(&json!({})).is_empty());
}

#[test]
fn the_input_group_comes_first_once_the_worker_is_known() {
    let config = Config::default();
    let runtime = RuntimeState::default();
    assert!(groups_of(&config, &runtime, &Catalogue::default(), false).is_empty());
    let g = groups_of(&config, &runtime, &catalogue(), false);
    let ids: Vec<&str> = g.iter().map(|g| g.id.as_str()).collect();
    assert_eq!(ids, ["input", "noise", "voice"]);
    assert!(g.iter().all(|g| g.surface.as_deref() == Some("mic")));
    let input: Vec<&str> = g[0].settings.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(input, [ENABLED, DEVICE, USE_AS_DEFAULT]);
    for s in g.iter().flat_map(|g| &g.settings) {
        assert!(obc_ipc::valid_setting_id(&s.id), "{}", s.id);
        assert_eq!(s.scope, "global");
    }
    // The device options: default, the devices (a description or the
    // name), and the value in force.
    let SettingKind::Enum { options, value, .. } = &g[0].settings[1].kind else {
        panic!("device is an enum");
    };
    assert_eq!(value, "default");
    assert_eq!(
        options
            .iter()
            .map(|(v, l)| format!("{v}={l}"))
            .collect::<Vec<_>>(),
        [
            "default=System default".to_string(),
            format!("{}=USB A", device_token("alsa_input.usb-a")),
            format!("{}=alsa_input.usb-b", device_token("alsa_input.usb-b")),
        ]
    );
    let mut gone = Config::default();
    gone.mic.device = "alsa_input.gone".into();
    let g2 = groups_of(&gone, &runtime, &catalogue(), false);
    let SettingKind::Enum { options, .. } = &g2[0].settings[1].kind else {
        panic!()
    };
    // The configured device is listed even when absent, marked so, and
    // is the value.
    assert_eq!(
        options.last().unwrap(),
        &(
            device_token("alsa_input.gone"),
            "Unavailable: alsa_input.gone".to_string()
        )
    );
    let SettingKind::Enum { value, .. } = &g2[0].settings[1].kind else {
        panic!()
    };
    assert_eq!(value, &device_token("alsa_input.gone"));
    // Many devices: at most MAX_DEVICES options, the value among them.
    let mut many = catalogue();
    many.devices = (0..40).map(|i| (format!("d{i}"), String::new())).collect();
    let g3 = groups_of(&gone, &runtime, &many, false);
    let SettingKind::Enum { options, .. } = &g3[0].settings[1].kind else {
        panic!()
    };
    assert_eq!(options.len(), MAX_DEVICES);
    assert!(
        options
            .iter()
            .any(|(v, _)| *v == device_token("alsa_input.gone"))
    );
    // The host running alone shows the group.
    assert_eq!(
        groups_of(&config, &runtime, &Catalogue::default(), true).len(),
        1
    );
}

#[test]
fn the_default_microphone_switch_follows_the_host_and_the_record() {
    let config = Config::default();
    let mut runtime = RuntimeState::default();
    let switch = |g: &[SettingsGroup]| find(g, USE_AS_DEFAULT).cloned().unwrap();
    let value = |s: &Setting| s.kind.value();
    // The actions it replaced are no longer published.
    let off = groups_of(&config, &runtime, &catalogue(), false);
    assert!(find(&off, MAKE_DEFAULT).is_none() && find(&off, RESTORE_DEFAULT).is_none());
    let s = switch(&off);
    assert_eq!(s.label, "Use as default microphone");
    assert_eq!(s.kind.type_name(), "bool");
    assert_eq!(s.hint.as_deref(), Some("switch"));
    assert_eq!(s.scope, "global");
    assert_eq!(s.kind.default_value(), Some(SettingValue::Bool(false)));
    // Host off: greyed, busy, off with nothing on record.
    assert!(!s.enabled);
    assert_eq!(s.disabled_reason, REASON_HOST_OFF);
    assert_eq!(s.disabled_kind, obc_ipc::settings::disabled::BUSY);
    assert_eq!(value(&s), Some(SettingValue::Bool(false)));
    // Host on: enabled, still off.
    let on = switch(&groups_of(&config, &runtime, &catalogue(), true));
    assert!(on.enabled);
    assert_eq!(
        (on.disabled_reason.as_str(), on.disabled_kind.as_str()),
        ("", "")
    );
    assert_eq!(value(&on), Some(SettingValue::Bool(false)));
    // Made the default here (a previous default on record): on, with the
    // host on or off.
    runtime.mic_previous_default = Some("alsa_input.usb-a".into());
    let made = switch(&groups_of(&config, &runtime, &catalogue(), true));
    assert!(made.enabled);
    assert_eq!(value(&made), Some(SettingValue::Bool(true)));
    let made_off = switch(&groups_of(&config, &runtime, &catalogue(), false));
    assert!(!made_off.enabled);
    assert_eq!(value(&made_off), Some(SettingValue::Bool(true)));
    // On record with no name (the previous default was not known): on.
    runtime.mic_previous_default = Some(String::new());
    let unnamed = switch(&groups_of(&config, &runtime, &catalogue(), true));
    assert_eq!(value(&unnamed), Some(SettingValue::Bool(true)));
}

#[test]
fn the_default_microphone_switch_writes_both_ways_and_the_old_actions_still_work() {
    let config = Config::default();
    let mut runtime = RuntimeState::default();
    let b = |v: bool| SettingValue::Bool(v);
    // Nothing on record, host on.
    let g = groups_of(&config, &runtime, &catalogue(), true);
    assert_eq!(
        check(&g, USE_AS_DEFAULT, Some(&b(true))),
        Ok(Write::MakeDefault)
    );
    assert_eq!(
        check(&g, USE_AS_DEFAULT, Some(&b(false))),
        Ok(Write::Unchanged)
    );
    assert!(matches!(
        check(&g, USE_AS_DEFAULT, Some(&SettingValue::Int(1))),
        Err(Refusal::Invalid(_))
    ));
    assert_eq!(
        check(&g, MAKE_DEFAULT, Some(&b(true))),
        Ok(Write::MakeDefault)
    );
    assert_eq!(
        check(&g, RESTORE_DEFAULT, Some(&b(true))),
        Err(Refusal::Disabled(REASON_NOT_DEFAULT.into()))
    );
    // Greyed before the value is looked at, as the published action was.
    assert_eq!(
        check(&g, RESTORE_DEFAULT, Some(&b(false))),
        Err(Refusal::Disabled(REASON_NOT_DEFAULT.into()))
    );
    // Made the default here: off restores, on makes it again (harmless).
    runtime.mic_previous_default = Some("alsa_input.usb-a".into());
    let g = groups_of(&config, &runtime, &catalogue(), true);
    assert_eq!(
        check(&g, USE_AS_DEFAULT, Some(&b(false))),
        Ok(Write::RestoreDefault)
    );
    assert_eq!(
        check(&g, USE_AS_DEFAULT, Some(&b(true))),
        Ok(Write::MakeDefault)
    );
    assert_eq!(
        check(&g, RESTORE_DEFAULT, Some(&b(true))),
        Ok(Write::RestoreDefault)
    );
    assert_eq!(
        check(&g, MAKE_DEFAULT, Some(&b(true))),
        Ok(Write::MakeDefault)
    );
    // Host off: the switch refuses both ways with its reason; the old
    // restore action still restores, as it did; the old make action is
    // refused as it was.
    let g = groups_of(&config, &runtime, &catalogue(), false);
    for v in [true, false] {
        assert_eq!(
            check(&g, USE_AS_DEFAULT, Some(&b(v))),
            Err(Refusal::Disabled(REASON_HOST_OFF.into()))
        );
    }
    assert_eq!(
        check(&g, RESTORE_DEFAULT, Some(&b(true))),
        Ok(Write::RestoreDefault)
    );
    // Greyed first, whatever the value, as the published action was.
    for v in [Some(b(true)), Some(b(false)), None] {
        assert_eq!(
            check(&g, MAKE_DEFAULT, v.as_ref()),
            Err(Refusal::Disabled(REASON_HOST_OFF.into())),
            "{v:?}"
        );
    }
    // Without the input group (the worker not known, the host off): the
    // old ids are unknown, as every mic.input id.
    let none = groups_of(&config, &runtime, &Catalogue::default(), false);
    assert!(matches!(
        check(&none, MAKE_DEFAULT, Some(&b(true))),
        Err(Refusal::Unknown(_))
    ));
}

#[test]
fn a_worker_setting_keeps_a_known_kind_only_while_greyed() {
    let doc = |enabled: bool, kind: &str| {
        json!({"groups": [{"id": "noise", "label": "Noise", "settings": [
            {"id": "mic.noise.strength", "label": "Strength", "type": "int",
             "min": 0, "max": 100, "enabled": enabled, "disabled_reason": "Why",
             "disabled_kind": kind}]}]})
    };
    let kind = |enabled: bool, k: &str| {
        parse_groups(&doc(enabled, k))[0].settings[0]
            .disabled_kind
            .clone()
    };
    for k in obc_ipc::settings::disabled::KINDS {
        assert_eq!(kind(false, k), k);
        // Enabled: no kind, whatever the worker says.
        assert_eq!(kind(true, k), "");
    }
    // Greyed with a kind this version does not know, or none (an older
    // worker): `unavailable`, never "" (every greyed setting has a kind);
    // enabled, still none.
    assert_eq!(
        kind(false, "later"),
        obc_ipc::settings::disabled::UNAVAILABLE
    );
    assert_eq!(kind(false, ""), obc_ipc::settings::disabled::UNAVAILABLE);
    assert_eq!(kind(true, "later"), "");
    let mut old = doc(false, "x");
    old["groups"][0]["settings"][0]
        .as_object_mut()
        .unwrap()
        .remove("disabled_kind");
    assert_eq!(
        parse_groups(&old)[0].settings[0].disabled_kind,
        obc_ipc::settings::disabled::UNAVAILABLE
    );
    old["groups"][0]["settings"][0]["enabled"] = json!(true);
    assert_eq!(parse_groups(&old)[0].settings[0].disabled_kind, "");
}

#[test]
fn stored_values_show_over_the_workers() {
    let config = Config::default();
    let mut runtime = RuntimeState::default();
    runtime
        .settings
        .insert("mic.noise.strength".into(), StoredValue::Int(40));
    // Of the wrong type: not shown.
    runtime
        .settings
        .insert("mic.noise.enabled".into(), StoredValue::Str("x".into()));
    let g = groups_of(&config, &runtime, &catalogue(), false);
    let strength = find(&g, "mic.noise.strength").unwrap();
    assert!(strength.stored);
    assert_eq!(strength.kind.value(), Some(SettingValue::Int(40)));
    let enabled = find(&g, "mic.noise.enabled").unwrap();
    assert!(!enabled.stored);
    assert_eq!(enabled.kind.value(), Some(SettingValue::Bool(true)));
    // Out of the range the worker gives now: not shown.
    runtime
        .settings
        .insert("mic.noise.strength".into(), StoredValue::Int(400));
    let g = groups_of(&config, &runtime, &catalogue(), false);
    let strength = find(&g, "mic.noise.strength").unwrap();
    assert!(!strength.stored);
    assert_eq!(strength.kind.value(), Some(SettingValue::Int(80)));
}

#[test]
fn what_the_worker_describes_is_bounded_before_it_is_published() {
    let long = "x\u{1}y ".repeat(200);
    let doc = json!({"groups": [
        {"id": "bad group", "label": "x", "settings": [
            {"id": "mic.bad.a", "label": "A", "type": "bool"}]},
        {"id": "big", "label": long, "settings": [
            {"id": "mic.big.level", "label": long, "type": "int", "min": 0, "max": 10,
             "default": 50, "value": -3, "unit": long, "hint": long,
             "enabled": false, "disabled_reason": long},
            {"id": "mic.big.inverted", "label": "x", "type": "int", "min": 5, "max": 1},
            {"id": "mic.big.mode", "label": "Mode", "type": "enum",
             "options": [["ok", long], [long, "Long"], ["has space", "S"]],
             "default": "gone", "value": "has space",
             "enabled_when": [["mic.big.level", "eq", long], ["mic.big.level", "eq", 1]]}]},
    ]});
    let g = parse_groups(&doc);
    assert_eq!(g.len(), 1, "a group id that is not plain is dropped");
    let big = &g[0];
    assert!(big.label.len() <= LABEL_MAX);
    // An inverted range does not read.
    assert_eq!(big.settings.len(), 2);
    let level = &big.settings[0];
    assert!(level.label.len() <= LABEL_MAX && !level.label.contains('\u{1}'));
    assert!(level.disabled_reason.len() <= REASON_MAX);
    assert!(level.hint.as_ref().unwrap().len() <= TAG_MAX);
    let SettingKind::Int {
        default,
        value,
        unit,
        ..
    } = &level.kind
    else {
        panic!("int")
    };
    // The default and the value out of the range: the minimum, the default.
    assert_eq!((*default, *value), (0, 0));
    assert!(unit.as_ref().unwrap().len() <= TAG_MAX);
    let mode = &big.settings[1];
    let SettingKind::Enum {
        options,
        default,
        value,
    } = &mode.kind
    else {
        panic!("enum")
    };
    // Option values that could not be written back are left out; labels
    // are cut.
    assert_eq!(options.len(), 1);
    assert!(options[0].1.len() <= OPTION_LABEL_MAX);
    assert_eq!((default.as_str(), value.as_str()), ("ok", "ok"));
    assert_eq!(mode.enabled_when.len(), 1, "a long operand is left out");
    // Too many groups: the first MAX_GROUPS.
    let many: Vec<Json> = (0..40)
        .map(|i| {
            json!({"id": format!("g{i}"), "label": "G", "settings": [
                {"id": format!("mic.g{i}.on"), "label": "On", "type": "bool"}]})
        })
        .collect();
    assert_eq!(parse_groups(&json!({ "groups": many })).len(), MAX_GROUPS);
    // Modules, devices and the status too.
    let m = parse_modules(&json!({"modules": [
        {"id": "a", "kind": "audio-filter", "name": long, "description": long, "reason": long,
         "stages": [{"id": "mic-denoise", "slot": long, "reason": long}]},
        {"id": long, "kind": "audio-filter"},
    ]}));
    assert_eq!(m.len(), 1);
    assert!(m[0].name.len() <= crate::modules::NAME_MAX);
    assert!(m[0].description.len() <= TEXT_MAX);
    assert!(m[0].reason.len() <= REASON_MAX);
    assert!(m[0].stages[0].slot.len() <= TAG_MAX);
    let d = parse_devices(&json!({"devices": [
        {"name": "alsa_input.a", "description": long},
        {"name": "x".repeat(300)}, {"name": "with\nnewline"},
    ]}));
    assert_eq!(d.len(), 1);
    assert!(d[0].1.len() <= LABEL_MAX);
    let s = status(
        true,
        Some(&json!({"state": long, "error": long, "provider": {"mic-denoise": long}})),
    );
    assert_eq!(s.state, "error");
    assert!(s.error.len() <= TEXT_MAX && !s.error.contains('\u{1}'));
    assert!(s.provider.len() <= TEXT_MAX);
}

#[test]
fn a_write_is_checked_and_bound_to_what_it_changes() {
    use obc_core::config_schema::ConfigValue as C;
    let config = Config::default();
    let mut runtime = RuntimeState::default();
    let g = groups_of(&config, &runtime, &catalogue(), true);
    let b = |v: bool| SettingValue::Bool(v);
    assert_eq!(
        check(&g, ENABLED, Some(&b(false))),
        Ok(Write::Config(KEY_ENABLED, C::Bool(false)))
    );
    // The device is written as its node name, whether given as the token
    // or as the node name itself.
    let devices = catalogue().devices;
    let pick = |given: &str, configured: &str| {
        check_input(
            &g,
            &devices,
            configured,
            DEVICE,
            Some(&SettingValue::Str(given.into())),
        )
    };
    let usb_a = Ok(Write::Config(KEY_DEVICE, C::Str("alsa_input.usb-a".into())));
    assert_eq!(pick(&device_token("alsa_input.usb-a"), "default"), usb_a);
    assert_eq!(pick("alsa_input.usb-a", "default"), usb_a);
    assert_eq!(
        pick("default", "default"),
        Ok(Write::Config(KEY_DEVICE, C::Str("default".into())))
    );
    assert!(matches!(pick("nope", "default"), Err(Refusal::Invalid(_))));
    assert!(matches!(
        pick("pw-000000000000", "default"),
        Err(Refusal::Invalid(_))
    ));
    assert_eq!(
        check(&g, MAKE_DEFAULT, Some(&b(true))),
        Ok(Write::MakeDefault)
    );
    assert!(matches!(
        check(&g, MAKE_DEFAULT, Some(&b(false))),
        Err(Refusal::Invalid(_))
    ));
    assert_eq!(
        check(&g, RESTORE_DEFAULT, Some(&b(true))),
        Err(Refusal::Disabled(REASON_NOT_DEFAULT.into()))
    );
    assert_eq!(
        check(&g, "mic.noise.strength", Some(&SettingValue::Int(45))),
        Ok(Write::Value(
            "mic.noise.strength".into(),
            StoredValue::Int(45)
        ))
    );
    for bad in [101, -5, 42] {
        assert!(
            matches!(
                check(&g, "mic.noise.strength", Some(&SettingValue::Int(bad))),
                Err(Refusal::Invalid(_))
            ),
            "{bad}"
        );
    }
    assert!(matches!(
        check(&g, "mic.noise.strength", Some(&b(true))),
        Err(Refusal::Invalid(_))
    ));
    assert!(matches!(
        check(&g, "mic.noise.nothing", Some(&b(true))),
        Err(Refusal::Unknown(_))
    ));
    runtime.mic_previous_default = Some("x".into());
    let g = groups_of(&config, &runtime, &catalogue(), true);
    assert_eq!(
        check(&g, RESTORE_DEFAULT, Some(&b(true))),
        Ok(Write::RestoreDefault)
    );
}

#[test]
fn the_snapshot_carries_the_config_values_pins_and_disabled_modules() {
    let mut config = Config::default();
    let mut runtime = RuntimeState::default();
    let parse = |text: String| serde_json::from_str::<Json>(&text).unwrap();
    let d = parse(snapshot(&config, &runtime, false, 0));
    assert_eq!(
        d,
        json!({
            "version": 1, "enabled": true, "device": "default",
            "pipeline_depth": {"cpu": 1, "gpu": 2}, "idle_exit_s": 30,
            "gpu_warm_s": 30, "gpu_policy": "always", "camera_active": false, "profile": "quality",
            "pins": {}, "values": {}, "disabled_modules": [], "forced_conflicts": false,
            "hardware_gain_db": null, "hardware_gain_serial": 0,
            "previous_in_force": null,
        })
    );
    runtime.mic_previous_in_force = Some("alsa_input.pci-dmic".into());
    assert_eq!(
        parse(snapshot(&config, &runtime, false, 0))["previous_in_force"],
        json!("alsa_input.pci-dmic")
    );
    runtime.mic_previous_in_force = None;
    config.mic.hardware_gain_db = Some(38.5);
    assert_eq!(
        parse(snapshot(&config, &runtime, false, 0))["hardware_gain_db"],
        json!(38.5)
    );
    assert_eq!(
        parse(snapshot(&config, &runtime, false, 7))["hardware_gain_serial"],
        json!(7)
    );
    config.mic.force_conflicts = true;
    assert_eq!(
        parse(snapshot(&config, &runtime, false, 0))["forced_conflicts"],
        json!(true)
    );
    config.mic.enabled = false;
    config.mic.device = "alsa_input.usb-a".into();
    config.mic.pipeline_depth = 3;
    config.mic.idle_exit_s = 0;
    config.mic.gpu_warm_s = 0;
    config.mic.gpu_policy = MicGpuPolicy::WithCamera;
    config
        .modules
        .pin
        .insert("mic-denoise".into(), "br.com.dias.voice-cpu".into());
    config.modules.pin.insert("background".into(), "x".into());
    config
        .modules
        .pin
        .insert("mic-dereverb".into(), String::new());
    config.modules.disabled = vec!["a".into(), "b".into()];
    config.modules.enabled.insert("b".into(), true);
    config.modules.enabled.insert("c".into(), false);
    runtime
        .settings
        .insert("mic.noise.strength".into(), StoredValue::Int(40));
    runtime
        .settings
        .insert("effect.video.denoise".into(), StoredValue::Bool(true));
    runtime
        .settings
        .insert("mic.input.enabled".into(), StoredValue::Bool(true));
    let d = parse(snapshot(&config, &runtime, true, 0));
    assert_eq!(d["enabled"], json!(false));
    assert_eq!(d["device"], json!("alsa_input.usb-a"));
    assert_eq!(d["pipeline_depth"], json!(3));
    assert_eq!(d["idle_exit_s"], json!(0));
    assert_eq!(d["gpu_warm_s"], json!(0));
    assert_eq!(d["gpu_policy"], json!("with-camera"));
    assert_eq!(d["camera_active"], json!(true));
    assert_eq!(d["pins"], json!({"mic-denoise": "br.com.dias.voice-cpu"}));
    assert_eq!(d["values"], json!({"mic.noise.strength": 40}));
    assert_eq!(d["disabled_modules"], json!(["a", "c"]));
}

#[test]
fn the_status_is_off_without_the_host_and_the_workers_with_it() {
    assert_eq!(
        status(false, Some(&json!({"state": "active"}))),
        MicStatus::default()
    );
    assert_eq!(status(true, None).state, "passthrough");
    assert_eq!(
        status(true, Some(&json!({"state": "off"}))).state,
        "passthrough"
    );
    let s = status(
        true,
        Some(&json!({
            "state": "active", "consumers": 2, "latency_ms": 20.0, "error": "",
            "provider": {"mic-denoise": "br.com.dias.voice-cpu", "mic-dereverb": "x"},
        })),
    );
    assert_eq!(
        s,
        MicStatus {
            state: "active".into(),
            consumers: 2,
            provider: "mic-denoise: br.com.dias.voice-cpu, mic-dereverb: x".into(),
            latency_ms: 20.0,
            error: String::new(),
        }
    );
    let e = status(true, Some(&json!({"state": "error", "error": "no GPU"})));
    assert_eq!((e.state.as_str(), e.error.as_str()), ("error", "no GPU"));
}

#[test]
fn the_audio_modules_are_the_workers_in_the_module_list() {
    let daemon = vec![
        ModuleInfo {
            id: "video".into(),
            kind: "video-stage".into(),
            status: "available".into(),
            ..ModuleInfo::default()
        },
        ModuleInfo {
            id: "audio".into(),
            kind: "audio-filter".into(),
            status: "unavailable".into(),
            reason: "runs in the audio worker".into(),
            ..ModuleInfo::default()
        },
    ];
    let worker = parse_modules(&json!({"modules": [
        {"id": "audio", "name": "Voice", "kind": "audio-filter", "enabled": true,
         "status": "available", "reason": "",
         "stages": [{"id": "mic-denoise", "slot": "clean", "rank": 200, "device": "cpu",
                     "available": true, "reason": "", "selected": true}]},
        {"id": "extra", "kind": "audio-filter", "status": "available"},
        {"id": "video", "kind": "video-stage", "status": "unavailable"},
    ]}));
    assert_eq!(worker.len(), 2);
    let merged = merge_modules(daemon, &worker);
    let ids: Vec<&str> = merged.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["video", "audio", "extra"]);
    assert_eq!(merged[0].status, "available");
    assert_eq!(merged[1].status, "available");
    assert_eq!(merged[1].stages[0].rank, 200);
    assert!(merged[1].stages[0].selected);
}

#[test]
fn the_devices_are_capped_and_named() {
    let d = parse_devices(&json!({"devices": [
        {"name": "a", "description": "A"}, {"name": ""}, {"description": "x"},
    ]}));
    assert_eq!(d, [("a".to_string(), "A".to_string())]);
    let many: Vec<Json> = (0..30).map(|i| json!({"name": format!("d{i}")})).collect();
    assert_eq!(
        parse_devices(&json!({ "devices": many })).len(),
        MAX_DEVICES
    );
}

// ---- the agent ----

#[derive(Default)]
struct WorkerWorld {
    running: bool,
    /// Whether a call that may activate it starts it.
    installed: bool,
    calls: Vec<Call>,
    /// The calls sent without activation.
    unactivated: Vec<&'static str>,
    /// When set, `GetModules` waits for it (a slow worker start).
    gate: Option<Arc<tokio::sync::Notify>>,
    replies: HashMap<&'static str, Result<String, WorkerError>>,
}

#[derive(Clone, Default)]
struct FakeWorker(Arc<Mutex<WorkerWorld>>);

fn kind(c: &Call) -> &'static str {
    match c {
        Call::GetStatus => "GetStatus",
        Call::GetSettings => "GetSettings",
        Call::GetModules => "GetModules",
        Call::GetDevices => "GetDevices",
        Call::ApplySnapshot(_) => "ApplySnapshot",
        Call::MakeDefault(_) => "MakeDefault",
        Call::RestoreDefault(_) => "RestoreDefault",
        Call::Pin(_) => "Pin",
        Call::ResetModule(_) => "ResetModule",
    }
}

impl Worker for FakeWorker {
    async fn running(&self) -> bool {
        self.0.lock().unwrap().running
    }

    async fn call(&self, call: Call, activate: bool) -> Result<String, WorkerError> {
        assert!(!activate || call.activates(), "{call:?} cannot activate");
        let k = kind(&call);
        let gate = {
            let mut w = self.0.lock().unwrap();
            w.calls.push(call.clone());
            if !activate {
                w.unactivated.push(k);
            }
            w.gate.clone().filter(|_| k == "GetModules")
        };
        if let Some(g) = gate {
            g.notified().await;
        }
        let mut w = self.0.lock().unwrap();
        if !w.running && !(activate && w.installed) {
            return Err(WorkerError {
                unavailable: true,
                unknown_method: false,
                message: "no audio worker".into(),
            });
        }
        if activate {
            w.running = true;
        }
        w.replies.get(k).cloned().unwrap_or_else(|| {
            Ok(match k {
                "GetModules" => json!({"modules": []}).to_string(),
                "GetSettings" => worker_settings().to_string(),
                "GetDevices" => json!({"devices": [{"name": "alsa_input.usb-a"}]}).to_string(),
                "GetStatus" => json!({"state": "passthrough"}).to_string(),
                "MakeDefault" => json!({"previous": "alsa_input.usb-a"}).to_string(),
                "ResetModule" => "true".into(),
                _ => String::new(),
            })
        })
    }
}

#[derive(Default)]
struct UnitWorld {
    active: HashMap<String, bool>,
    calls: Vec<String>,
    fail: Option<UnitError>,
}

#[derive(Clone, Default)]
struct FakeUnits(Arc<Mutex<UnitWorld>>);

impl Units for FakeUnits {
    async fn active(&self, unit: &str) -> Result<bool, UnitError> {
        let u = self.0.lock().unwrap();
        Ok(u.active.get(unit).copied().unwrap_or(false))
    }

    async fn enable_now(&self, unit: &str) -> Result<(), UnitError> {
        let mut u = self.0.lock().unwrap();
        u.calls.push(format!("enable --now {unit}"));
        if let Some(e) = u.fail.clone() {
            return Err(e);
        }
        u.active.insert(unit.into(), true);
        Ok(())
    }

    async fn disable_now(&self, unit: &str) -> Result<(), UnitError> {
        let mut u = self.0.lock().unwrap();
        u.calls.push(format!("disable --now {unit}"));
        u.active.insert(unit.into(), false);
        Ok(())
    }
}

/// An agent running on its own task, with what it sends.
struct Rig {
    worker: FakeWorker,
    units: FakeUnits,
    requests: tokio::sync::mpsc::UnboundedSender<MicRequest>,
    events: tokio::sync::mpsc::UnboundedSender<WorkerEvent>,
    commands: std::sync::mpsc::Receiver<Command>,
    notes: tokio::sync::mpsc::UnboundedReceiver<Note>,
    dir: tempfile::TempDir,
}

impl Rig {
    fn start(worker: FakeWorker, units: FakeUnits, queued: Vec<MicRequest>) -> Rig {
        Self::start_polling(worker, units, queued, HOST_POLL)
    }

    /// [`Rig::start`], the host's state read every `every`.
    fn start_polling(
        worker: FakeWorker,
        units: FakeUnits,
        queued: Vec<MicRequest>,
        every: Duration,
    ) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let (published, notes) = Published::new(Snapshot::empty());
        let (ctx, commands) = std::sync::mpsc::channel();
        let (requests, rrx) = tokio::sync::mpsc::unbounded_channel();
        let (events, erx) = tokio::sync::mpsc::unbounded_channel();
        for r in queued {
            requests.send(r).unwrap();
        }
        let agent = Agent::new(
            worker.clone(),
            units.clone(),
            ctx,
            published,
            Some(dir.path().join("state").join(SNAPSHOT_FILE)),
        )
        .with_host_poll(every);
        tokio::spawn(agent.run(rrx, erx));
        Rig {
            worker,
            units,
            requests,
            events,
            commands,
            notes,
            dir,
        }
    }

    /// The next event the agent sends the controller.
    async fn next(&self) -> MicEvent {
        // Up to 10 s: a loaded machine is slow, never wrong.
        for _ in 0..5000 {
            match self.commands.try_recv() {
                Ok(Command::Mic(e)) => return e,
                Ok(other) => panic!("not a Mic command: {other:?}"),
                Err(_) => tokio::time::sleep(Duration::from_millis(2)).await,
            }
        }
        panic!("no event from the agent")
    }

    /// Events until `f` matches one, which is returned.
    async fn until(&self, f: impl Fn(&MicEvent) -> bool) -> MicEvent {
        loop {
            let e = self.next().await;
            if f(&e) {
                return e;
            }
        }
    }

    fn calls(&self) -> Vec<&'static str> {
        self.worker
            .0
            .lock()
            .unwrap()
            .calls
            .iter()
            .map(kind)
            .collect()
    }

    fn snapshot_file(&self) -> Option<String> {
        std::fs::read_to_string(self.dir.path().join("state").join(SNAPSHOT_FILE)).ok()
    }

    /// Lets the agent run what is queued.
    /// Waits (up to 10 s) until `f` holds.
    async fn wait_for(&self, what: &str, f: impl Fn(&Rig) -> bool) {
        for _ in 0..5000 {
            if f(self) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("timed out waiting for {what}");
    }

    async fn settle(&self) {
        for _ in 0..20 {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
}

fn reply() -> (
    crate::controller::Reply,
    tokio::sync::oneshot::Receiver<Result<(), IpcError>>,
) {
    tokio::sync::oneshot::channel()
}

#[tokio::test]
async fn at_start_the_snapshot_is_written_and_the_worker_asked_what_it_offers() {
    let worker = FakeWorker::default();
    worker.0.lock().unwrap().installed = true;
    let rig = Rig::start(
        worker,
        FakeUnits::default(),
        vec![
            MicRequest::Snapshot("{\"version\":1,\"n\":1}".into()),
            MicRequest::Snapshot("{\"version\":1,\"n\":2}".into()),
        ],
    );
    let e = rig.until(|e| matches!(e, MicEvent::Catalogue(_))).await;
    let MicEvent::Catalogue(c) = e else { panic!() };
    assert!(c.known);
    assert_eq!(c.groups.len(), 2);
    assert_eq!(c.devices.len(), 1);
    // The snapshots queued together: one written, the last; the worker was
    // not running, so none pushed (and it was not started for it).
    assert_eq!(
        rig.snapshot_file().as_deref(),
        Some("{\"version\":1,\"n\":2}")
    );
    let calls = rig.calls();
    assert_eq!(calls[0], "GetModules", "{calls:?}");
    assert!(!calls.contains(&"ApplySnapshot"), "{calls:?}");
    // Running now: a snapshot is pushed, and the settings read again.
    rig.requests
        .send(MicRequest::Snapshot("{\"version\":1,\"n\":3}".into()))
        .unwrap();
    rig.wait_for("the push and the settings read again", |r| {
        let calls = r.calls();
        calls
            .iter()
            .position(|c| *c == "ApplySnapshot")
            .is_some_and(|at| calls[at..].contains(&"GetSettings"))
    })
    .await;
    assert_eq!(
        rig.snapshot_file().as_deref(),
        Some("{\"version\":1,\"n\":3}")
    );
}

#[tokio::test]
async fn without_a_worker_installed_the_microphone_is_off_and_unknown() {
    let rig = Rig::start(FakeWorker::default(), FakeUnits::default(), Vec::new());
    let e = rig.next().await;
    assert!(
        matches!(&e, MicEvent::Status(s) if *s == MicStatus::default()),
        "{e:?}"
    );
    rig.settle().await;
    assert!(rig.commands.try_recv().is_err(), "no catalogue");
}

#[tokio::test]
async fn mic_on_is_refused_while_an_adr_0014_unit_runs() {
    let units = FakeUnits::default();
    units
        .0
        .lock()
        .unwrap()
        .active
        .insert("omarchy-broadcast-mic@deepfilter.service".into(), true);
    let rig = Rig::start(FakeWorker::default(), units, Vec::new());
    let (tx, rx) = reply();
    rig.requests
        .send(MicRequest::Host {
            on: true,
            previous: None,
            reply: tx,
        })
        .unwrap();
    let e = rig.until(|e| matches!(e, MicEvent::HostDone { .. })).await;
    let MicEvent::HostDone { result, reply, .. } = e else {
        panic!()
    };
    let _ = reply.send(result);
    match rx.await.unwrap() {
        Err(IpcError::Unavailable(m)) => {
            assert!(
                m.contains("omarchy-broadcast-mic@deepfilter.service"),
                "{m}"
            )
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
    assert!(rig.units.0.lock().unwrap().calls.is_empty());
}

#[tokio::test]
async fn mic_on_and_off_enable_and_disable_the_host_restoring_the_default_first() {
    let worker = FakeWorker::default();
    worker.0.lock().unwrap().installed = true;
    let rig = Rig::start(worker, FakeUnits::default(), Vec::new());
    let (tx, _rx) = reply();
    rig.requests
        .send(MicRequest::Host {
            on: true,
            previous: None,
            reply: tx,
        })
        .unwrap();
    // The status before the reply: the controller answers after it.
    let mut seen_on = false;
    loop {
        match rig.next().await {
            MicEvent::Status(s) if s.state != "off" => seen_on = true,
            MicEvent::HostDone { result, .. } => {
                assert!(result.is_ok(), "{result:?}");
                break;
            }
            _ => {}
        }
    }
    assert!(seen_on);
    let (tx, _rx) = reply();
    rig.requests
        .send(MicRequest::Host {
            on: false,
            previous: Some("alsa_input.usb-a".into()),
            reply: tx,
        })
        .unwrap();
    let e = rig.until(|e| matches!(e, MicEvent::HostDone { .. })).await;
    let MicEvent::HostDone {
        restored, result, ..
    } = e
    else {
        panic!()
    };
    assert!(restored);
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        rig.units.0.lock().unwrap().calls,
        [
            "enable --now omarchy-broadcast-mic.service",
            "disable --now omarchy-broadcast-mic.service"
        ]
    );
    let w = rig.worker.0.lock().unwrap();
    assert!(
        w.calls
            .contains(&Call::RestoreDefault("alsa_input.usb-a".into()))
    );
}

#[tokio::test]
async fn a_service_manager_refusal_is_the_reply() {
    let units = FakeUnits::default();
    units.0.lock().unwrap().fail = Some(UnitError::Unavailable("no systemd".into()));
    let rig = Rig::start(FakeWorker::default(), units, Vec::new());
    let (tx, _rx) = reply();
    rig.requests
        .send(MicRequest::Host {
            on: true,
            previous: None,
            reply: tx,
        })
        .unwrap();
    let e = rig.until(|e| matches!(e, MicEvent::HostDone { .. })).await;
    let MicEvent::HostDone { result, .. } = e else {
        panic!()
    };
    assert!(
        matches!(&result, Err(IpcError::Unavailable(m)) if m == "no systemd"),
        "{result:?}"
    );
}

#[tokio::test]
async fn make_default_hands_back_the_previous_default() {
    let worker = FakeWorker::default();
    worker.0.lock().unwrap().installed = true;
    let rig = Rig::start(worker, FakeUnits::default(), Vec::new());
    let (tx, _rx) = reply();
    rig.requests
        .send(MicRequest::MakeDefault {
            target: String::new(),
            reply: tx,
        })
        .unwrap();
    let e = rig
        .until(|e| matches!(e, MicEvent::DefaultMade { .. }))
        .await;
    let MicEvent::DefaultMade {
        previous, in_force, ..
    } = e
    else {
        panic!()
    };
    // An older worker: no source in force told.
    assert_eq!((previous.as_str(), in_force), ("alsa_input.usb-a", None));
    // A newer one tells the real source in force then (`null`, or empty:
    // none).
    for (reply_doc, want) in [
        (
            json!({"previous": "alsa_input.usb-a", "in_force": "alsa_input.pci-dmic"}),
            Some("alsa_input.pci-dmic"),
        ),
        (
            json!({"previous": "alsa_input.usb-a", "in_force": null}),
            None,
        ),
        (
            json!({"previous": "alsa_input.usb-a", "in_force": ""}),
            None,
        ),
    ] {
        rig.worker
            .0
            .lock()
            .unwrap()
            .replies
            .insert("MakeDefault", Ok(reply_doc.to_string()));
        let (tx, _rx) = reply();
        rig.requests
            .send(MicRequest::MakeDefault {
                target: String::new(),
                reply: tx,
            })
            .unwrap();
        let e = rig
            .until(|e| matches!(e, MicEvent::DefaultMade { .. }))
            .await;
        let MicEvent::DefaultMade { in_force, .. } = e else {
            panic!()
        };
        assert_eq!(in_force.as_deref(), want, "{reply_doc}");
    }
    // A worker that cannot: the controller is told why, with the reply
    // (so that it knows the request is answered), nothing is recorded.
    rig.worker.0.lock().unwrap().replies.insert(
        "MakeDefault",
        Err(WorkerError {
            unavailable: true,
            unknown_method: false,
            message: "no PipeWire".into(),
        }),
    );
    let (tx, _rx) = reply();
    rig.requests
        .send(MicRequest::MakeDefault {
            target: "x".into(),
            reply: tx,
        })
        .unwrap();
    let e = rig
        .until(|e| matches!(e, MicEvent::DefaultNotMade { .. }))
        .await;
    match e {
        MicEvent::DefaultNotMade {
            error: IpcError::Unavailable(m),
            ..
        } => assert!(m.contains("no PipeWire"), "{m}"),
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

/// The targets of the `Pin` calls made so far.
fn pinned(r: &Rig) -> Vec<String> {
    r.worker
        .0
        .lock()
        .unwrap()
        .calls
        .iter()
        .filter_map(|c| match c {
            Call::Pin(t) => Some(t.clone()),
            _ => None,
        })
        .collect()
}

fn set_host(units: &FakeUnits, on: bool) {
    units.0.lock().unwrap().active.insert(HOST_UNIT.into(), on);
}

#[tokio::test]
async fn a_repin_pins_only_while_the_host_runs_never_makes_the_default_and_records_nothing() {
    let worker = FakeWorker::default();
    worker.0.lock().unwrap().installed = true;
    let units = FakeUnits::default();
    let rig = Rig::start(worker, units.clone(), Vec::new());
    rig.until(|e| matches!(e, MicEvent::Catalogue(_))).await;
    // The host is off: nothing is asked.
    rig.requests
        .send(MicRequest::Repin {
            target: "alsa_input.usb-b".into(),
        })
        .unwrap();
    rig.settle().await;
    assert!(pinned(&rig).is_empty());
    // On: the worker is asked to pin, with the target (empty: the
    // default), whether or not ours is the default; never `MakeDefault`,
    // and no event comes of it.
    set_host(&units, true);
    for target in ["alsa_input.usb-b", ""] {
        rig.requests
            .send(MicRequest::Repin {
                target: target.into(),
            })
            .unwrap();
    }
    rig.wait_for("both re-pins", |r| pinned(r).len() == 2).await;
    assert_eq!(pinned(&rig), ["alsa_input.usb-b", ""]);
    rig.settle().await;
    assert!(!rig.calls().contains(&"MakeDefault"));
    while let Ok(e) = rig.commands.try_recv() {
        assert!(
            !matches!(e, Command::Mic(MicEvent::DefaultMade { .. })),
            "{e:?}"
        );
    }
    // A worker that cannot (a device not connected): only logged, the
    // agent goes on.
    rig.worker.0.lock().unwrap().replies.insert(
        "Pin",
        Err(WorkerError {
            unavailable: false,
            unknown_method: false,
            message: "x is not a microphone PipeWire knows now".into(),
        }),
    );
    rig.requests
        .send(MicRequest::Repin { target: "x".into() })
        .unwrap();
    rig.wait_for("the failing re-pin", |r| pinned(r).len() == 3)
        .await;
}

#[tokio::test]
async fn the_device_chosen_is_pinned_when_the_host_comes_on() {
    let worker = FakeWorker::default();
    worker.0.lock().unwrap().installed = true;
    let units = FakeUnits::default();
    let rig = Rig::start_polling(worker, units.clone(), Vec::new(), Duration::from_millis(50));
    rig.until(|e| matches!(e, MicEvent::Catalogue(_))).await;
    rig.requests
        .send(MicRequest::Repin {
            target: "alsa_input.usb-b".into(),
        })
        .unwrap();
    rig.settle().await;
    assert!(pinned(&rig).is_empty());
    // `mic on`: pinned once the host runs.
    let (reply, mut rx) = reply();
    rig.requests
        .send(MicRequest::Host {
            on: true,
            previous: None,
            reply,
        })
        .unwrap();
    rig.until(|e| matches!(e, MicEvent::HostDone { .. })).await;
    rig.wait_for("the pin on `mic on`", |r| pinned(r).len() == 1)
        .await;
    assert_eq!(pinned(&rig), ["alsa_input.usb-b"]);
    assert!(rx.try_recv().is_err(), "answered by the controller");
    // The polls of a host that stays on pin nothing more.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(pinned(&rig).len(), 1);
    // Off, then on by hand (systemctl): the poll sees it and pins again.
    set_host(&units, false);
    tokio::time::sleep(Duration::from_millis(150)).await;
    set_host(&units, true);
    rig.wait_for("the pin on a host seen on", |r| pinned(r).len() == 2)
        .await;
    assert_eq!(pinned(&rig), ["alsa_input.usb-b", "alsa_input.usb-b"]);
}

#[tokio::test]
async fn at_start_with_the_host_running_the_device_chosen_is_pinned_once() {
    let worker = FakeWorker::default();
    worker.0.lock().unwrap().installed = true;
    let units = FakeUnits::default();
    set_host(&units, true);
    let rig = Rig::start(
        worker,
        units,
        vec![
            MicRequest::Snapshot("{\"version\":1}".into()),
            MicRequest::Repin {
                target: "alsa_input.usb-b".into(),
            },
        ],
    );
    // The status goes out first (no worker start holds it up).
    let e = rig.next().await;
    assert!(matches!(e, MicEvent::Status(_)), "{e:?}");
    rig.until(|e| matches!(e, MicEvent::Catalogue(_))).await;
    rig.settle().await;
    assert_eq!(pinned(&rig), ["alsa_input.usb-b"]);
}

#[tokio::test]
async fn a_restore_pins_the_device_chosen_again_and_the_default_stays_unpinned() {
    let worker = FakeWorker::default();
    worker.0.lock().unwrap().installed = true;
    let units = FakeUnits::default();
    set_host(&units, true);
    let rig = Rig::start(worker, units, Vec::new());
    rig.until(|e| matches!(e, MicEvent::Catalogue(_))).await;
    let restore = |rig: &Rig| {
        let (reply, _rx) = reply();
        rig.requests
            .send(MicRequest::RestoreDefault {
                previous: "alsa_input.usb-a".into(),
                reply,
            })
            .unwrap();
    };
    rig.requests
        .send(MicRequest::Repin {
            target: "alsa_input.usb-b".into(),
        })
        .unwrap();
    rig.wait_for("the first pin", |r| pinned(r).len() == 1)
        .await;
    restore(&rig);
    let e = rig
        .until(|e| matches!(e, MicEvent::DefaultRestored { .. }))
        .await;
    assert!(matches!(
        e,
        MicEvent::DefaultRestored { result: Ok(()), .. }
    ));
    rig.wait_for("the pin after the restore", |r| pinned(r).len() == 2)
        .await;
    let calls = rig.calls();
    let restored = calls.iter().rposition(|c| *c == "RestoreDefault").unwrap();
    assert_eq!(calls[restored + 1..], ["Pin"]);
    // A restore that fails pins nothing.
    rig.worker.0.lock().unwrap().replies.insert(
        "RestoreDefault",
        Err(WorkerError {
            unavailable: false,
            unknown_method: false,
            message: "no".into(),
        }),
    );
    restore(&rig);
    rig.until(|e| matches!(e, MicEvent::DefaultRestored { .. }))
        .await;
    rig.settle().await;
    assert_eq!(pinned(&rig).len(), 2);
    rig.worker
        .0
        .lock()
        .unwrap()
        .replies
        .remove("RestoreDefault");
    // `default`: unpinned by the restore, and left so.
    rig.requests
        .send(MicRequest::Repin {
            target: String::new(),
        })
        .unwrap();
    rig.wait_for("the unpin", |r| pinned(r).len() == 3).await;
    restore(&rig);
    rig.until(|e| matches!(e, MicEvent::DefaultRestored { .. }))
        .await;
    rig.settle().await;
    assert_eq!(pinned(&rig), ["alsa_input.usb-b", "alsa_input.usb-b", ""]);
}

#[tokio::test]
async fn a_pin_that_fails_is_tried_again_at_each_poll_a_bounded_number_of_times() {
    let worker = FakeWorker::default();
    worker.0.lock().unwrap().installed = true;
    worker.0.lock().unwrap().replies.insert(
        "Pin",
        Err(WorkerError {
            unavailable: false,
            unknown_method: false,
            message: "Omarchy Broadcast Mic is not running".into(),
        }),
    );
    let units = FakeUnits::default();
    set_host(&units, true);
    let rig = Rig::start_polling(worker, units, Vec::new(), Duration::from_millis(50));
    rig.until(|e| matches!(e, MicEvent::Catalogue(_))).await;
    rig.requests
        .send(MicRequest::Repin {
            target: "alsa_input.usb-b".into(),
        })
        .unwrap();
    // Tried at once, then at each poll.
    let tries = 1 + PIN_RETRIES as usize;
    rig.wait_for("the retries", |r| pinned(r).len() == tries)
        .await;
    // Then only logged: no more tries.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(pinned(&rig).len(), tries);
    // A pin due afresh is tried at once, and again at the polls.
    rig.requests
        .send(MicRequest::Repin {
            target: "alsa_input.usb-a".into(),
        })
        .unwrap();
    rig.wait_for("the new pin's tries", |r| pinned(r).len() == tries + 2)
        .await;
    // Once the mic host's capture node is there, the retry succeeds and
    // the tries end.
    rig.worker.0.lock().unwrap().replies.remove("Pin");
    rig.wait_for("the pin that succeeds", |r| pinned(r).len() == tries + 3)
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(pinned(&rig).len(), tries + 3);
}

#[tokio::test]
async fn a_pin_that_failed_waits_for_the_next_poll() {
    let worker = FakeWorker::default();
    worker.0.lock().unwrap().installed = true;
    worker.0.lock().unwrap().replies.insert(
        "Pin",
        Err(WorkerError {
            unavailable: false,
            unknown_method: false,
            message: "Omarchy Broadcast Mic is not running".into(),
        }),
    );
    let units = FakeUnits::default();
    set_host(&units, true);
    // No poll comes in this test.
    let rig = Rig::start_polling(worker, units, Vec::new(), Duration::from_secs(3600));
    rig.until(|e| matches!(e, MicEvent::Catalogue(_))).await;
    rig.requests
        .send(MicRequest::Repin {
            target: "alsa_input.usb-b".into(),
        })
        .unwrap();
    rig.wait_for("the first try", |r| pinned(r).len() == 1)
        .await;
    // A worker's status, or the host's state read for a request, is no
    // retry.
    rig.events
        .send(WorkerEvent::Status(
            json!({"state": "passthrough"}).to_string(),
        ))
        .unwrap();
    let (r, _rx) = reply();
    rig.requests
        .send(MicRequest::Host {
            on: true,
            previous: None,
            reply: r,
        })
        .unwrap();
    rig.until(|e| matches!(e, MicEvent::HostDone { .. })).await;
    rig.settle().await;
    assert_eq!(pinned(&rig).len(), 1);
}

#[tokio::test]
async fn a_new_capture_node_is_pinned_again_between_two_polls() {
    let worker = FakeWorker::default();
    {
        let mut w = worker.0.lock().unwrap();
        w.installed = true;
        w.replies.insert(
            "GetStatus",
            Ok(json!({"state": "passthrough", "capture_serial": 1}).to_string()),
        );
    }
    let units = FakeUnits::default();
    set_host(&units, true);
    // The poll never comes in this test: only the worker's status tells.
    let rig = Rig::start_polling(worker, units, Vec::new(), Duration::from_secs(3600));
    rig.until(|e| matches!(e, MicEvent::Catalogue(_))).await;
    rig.requests
        .send(MicRequest::Repin {
            target: "alsa_input.usb-b".into(),
        })
        .unwrap();
    rig.wait_for("the pin", |r| pinned(r).len() == 1).await;
    let status = |serial: Option<u64>| {
        let mut v = json!({"state": "passthrough"});
        if let Some(n) = serial {
            v["capture_serial"] = json!(n);
        }
        rig.events.send(WorkerEvent::Status(v.to_string())).unwrap();
    };
    // The worker running: its first status's serial was seen already.
    rig.events.send(WorkerEvent::Owner(true)).unwrap();
    rig.settle().await;
    status(Some(1));
    rig.settle().await;
    assert_eq!(pinned(&rig).len(), 1);
    // The mic host restarted (systemctl restart, within one poll): its
    // capture node goes and comes back, each a move of the serial.
    status(Some(2));
    rig.wait_for("the pin of a capture node gone", |r| pinned(r).len() == 2)
        .await;
    status(Some(3));
    rig.wait_for("the pin of the new capture node", |r| pinned(r).len() == 3)
        .await;
    assert_eq!(pinned(&rig), ["alsa_input.usb-b"; 3]);
    // A worker that does not tell it: nothing more.
    status(None);
    rig.settle().await;
    assert_eq!(pinned(&rig).len(), 3);
    // The worker gone and back (PipeWire restarted while it did not run):
    // its first serial is a new capture node to pin.
    rig.events.send(WorkerEvent::Owner(false)).unwrap();
    rig.settle().await;
    status(Some(1));
    rig.wait_for("the pin after the worker came back", |r| {
        pinned(r).len() == 4
    })
    .await;
    // With the host off, nothing is pinned.
    set_host(&rig.units, false);
    rig.requests
        .send(MicRequest::Repin {
            target: "alsa_input.usb-b".into(),
        })
        .unwrap();
    rig.settle().await;
    status(Some(7));
    rig.settle().await;
    assert_eq!(pinned(&rig).len(), 4);
}

#[tokio::test]
async fn a_worker_without_pin_is_asked_to_make_the_default_only_while_ours_is_made_default() {
    let unknown = || {
        Err(WorkerError {
            unavailable: false,
            unknown_method: true,
            message: "Unknown method Pin".into(),
        })
    };
    let worker = FakeWorker::default();
    worker.0.lock().unwrap().installed = true;
    worker.0.lock().unwrap().replies.insert("Pin", unknown());
    let units = FakeUnits::default();
    set_host(&units, true);
    let rig = Rig::start_polling(worker, units, Vec::new(), Duration::from_millis(50));
    rig.until(|e| matches!(e, MicEvent::Catalogue(_))).await;
    let made = |r: &Rig| -> Vec<String> {
        r.worker
            .0
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter_map(|c| match c {
                Call::MakeDefault(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    };
    // Ours not made the default here: given up, never retried, and never
    // `MakeDefault` (it would make ours the default).
    rig.requests
        .send(MicRequest::Repin {
            target: "alsa_input.usb-b".into(),
        })
        .unwrap();
    rig.wait_for("the pin tried", |r| pinned(r).len() == 1)
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(pinned(&rig).len(), 1);
    assert!(made(&rig).is_empty());
    // Made the default: the pin moves with `MakeDefault`, as before `Pin`.
    let (r, _rx) = reply();
    rig.requests
        .send(MicRequest::MakeDefault {
            target: String::new(),
            reply: r,
        })
        .unwrap();
    rig.until(|e| matches!(e, MicEvent::DefaultMade { .. }))
        .await;
    rig.requests
        .send(MicRequest::Repin {
            target: "alsa_input.usb-a".into(),
        })
        .unwrap();
    rig.wait_for("the old way", |r| made(r).len() == 2).await;
    assert_eq!(made(&rig), ["", "alsa_input.usb-a"]);
    // Restored: given up again.
    let (r, _rx) = reply();
    rig.requests
        .send(MicRequest::RestoreDefault {
            previous: "alsa_input.usb-a".into(),
            reply: r,
        })
        .unwrap();
    rig.until(|e| matches!(e, MicEvent::DefaultRestored { .. }))
        .await;
    rig.wait_for("the pin after the restore", |r| pinned(r).len() == 3)
        .await;
    rig.requests
        .send(MicRequest::Repin {
            target: String::new(),
        })
        .unwrap();
    rig.wait_for("the last pin tried", |r| pinned(r).len() == 4)
        .await;
    rig.settle().await;
    assert_eq!(made(&rig).len(), 2);
    // Any other failure of `Pin` never falls back.
    rig.worker.0.lock().unwrap().replies.insert(
        "Pin",
        Err(WorkerError {
            unavailable: false,
            unknown_method: false,
            message: "no mic host".into(),
        }),
    );
    let (r, _rx) = reply();
    rig.requests
        .send(MicRequest::MakeDefault {
            target: String::new(),
            reply: r,
        })
        .unwrap();
    rig.until(|e| matches!(e, MicEvent::DefaultMade { .. }))
        .await;
    rig.requests
        .send(MicRequest::Repin {
            target: "alsa_input.usb-b".into(),
        })
        .unwrap();
    rig.wait_for("a failing pin", |r| pinned(r).len() == 5)
        .await;
    rig.settle().await;
    assert_eq!(made(&rig).len(), 3, "only the MakeDefault asked for");
}

#[tokio::test]
async fn the_workers_signals_become_the_status_and_mic_stats() {
    let worker = FakeWorker::default();
    let units = FakeUnits::default();
    units
        .0
        .lock()
        .unwrap()
        .active
        .insert(HOST_UNIT.into(), true);
    let mut rig = Rig::start(worker, units, Vec::new());
    let e = rig.next().await;
    assert!(
        matches!(&e, MicEvent::Status(s) if s.state == "passthrough"),
        "{e:?}"
    );
    // Stats while not active: not relayed.
    rig.events.send(WorkerEvent::Stats(1, 2, 0.5)).unwrap();
    rig.worker.0.lock().unwrap().running = true;
    rig.events.send(WorkerEvent::Owner(true)).unwrap();
    rig.events
        .send(WorkerEvent::Status(
            json!({"state": "active", "consumers": 1, "latency_ms": 10.0}).to_string(),
        ))
        .unwrap();
    let e = rig
        .until(|e| matches!(e, MicEvent::Status(s) if s.state == "active"))
        .await;
    let MicEvent::Status(s) = e else { panic!() };
    assert_eq!((s.consumers, s.latency_ms), (1, 10.0));
    rig.events.send(WorkerEvent::Stats(3, 4, 0.9)).unwrap();
    // The events are handled in order: the first Stats (not active) went
    // before this one.
    let mut stats = Vec::new();
    for _ in 0..5000 {
        while let Ok(n) = rig.notes.try_recv() {
            if let Note::MicStats(o, m, v) = n {
                stats.push((o, m, v));
            }
        }
        if !stats.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(stats, [(3, 4, 0.9)]);
    // The worker gone: passthrough while the host runs.
    rig.worker.0.lock().unwrap().running = false;
    rig.events.send(WorkerEvent::Owner(false)).unwrap();
    let e = rig.until(|e| matches!(e, MicEvent::Status(_))).await;
    assert!(
        matches!(&e, MicEvent::Status(s) if s.state == "passthrough"),
        "{e:?}"
    );
    // ResetModule starts the worker (a user's request).
    rig.requests
        .send(MicRequest::ResetModule("audio".into()))
        .unwrap();
    rig.wait_for("ResetModule", |r| {
        r.worker
            .0
            .lock()
            .unwrap()
            .calls
            .contains(&Call::ResetModule("audio".into()))
    })
    .await;
}

#[tokio::test]
async fn a_change_of_the_hardware_gain_alone_is_a_new_catalogue() {
    let worker = FakeWorker::default();
    worker.0.lock().unwrap().installed = true;
    let rig = Rig::start(worker, FakeUnits::default(), Vec::new());
    let e = rig.until(|e| matches!(e, MicEvent::Catalogue(_))).await;
    let MicEvent::Catalogue(c) = e else { panic!() };
    assert_eq!(c.hardware_gain, None);
    let gain_of = |e: &MicEvent| match e {
        MicEvent::Catalogue(c) => c.hardware_gain.as_ref().map(|g| g.db),
        _ => None,
    };
    rig.events
        .send(WorkerEvent::Status(status_with_gain(40.0).to_string()))
        .unwrap();
    let e = rig.until(|e| gain_of(e).is_some()).await;
    assert_eq!(gain_of(&e), Some(40.0));
    // The gain moved (another writer, or the worker applying the config):
    // nothing else changed, and the controller hears of it all the same.
    rig.events
        .send(WorkerEvent::Status(status_with_gain(30.0).to_string()))
        .unwrap();
    let e = rig.until(|e| gain_of(e) == Some(30.0)).await;
    let MicEvent::Catalogue(c) = e else { panic!() };
    assert_eq!(c.groups.len(), 2);
    // The worker starting again: its gain pending (left out of the
    // status), the gain known stands; the next one it reports follows.
    rig.events
        .send(WorkerEvent::Status(json!({"state": "active"}).to_string()))
        .unwrap();
    rig.events
        .send(WorkerEvent::Status(status_with_gain(31.0).to_string()))
        .unwrap();
    let e = rig.until(|e| matches!(e, MicEvent::Catalogue(_))).await;
    assert_eq!(
        gain_of(&e),
        Some(31.0),
        "the pending status dropped the gain"
    );
    // No gain reported any more (another device): gone from the catalogue.
    rig.events
        .send(WorkerEvent::Status(
            json!({"state": "active", "hardware_gain": null}).to_string(),
        ))
        .unwrap();
    rig.until(|e| matches!(e, MicEvent::Catalogue(c) if c.hardware_gain.is_none()))
        .await;
}

#[tokio::test]
async fn mic_off_behind_an_inflight_make_default_still_restores() {
    let worker = FakeWorker::default();
    worker.0.lock().unwrap().installed = true;
    let rig = Rig::start(worker, FakeUnits::default(), Vec::new());
    // "Use as default microphone", then `mic off` queued before the
    // controller heard of the first: it had no previous default to send.
    let (tx, _rx) = reply();
    rig.requests
        .send(MicRequest::MakeDefault {
            target: String::new(),
            reply: tx,
        })
        .unwrap();
    let (tx, _rx2) = reply();
    rig.requests
        .send(MicRequest::Host {
            on: false,
            previous: None,
            reply: tx,
        })
        .unwrap();
    let e = rig.until(|e| matches!(e, MicEvent::HostDone { .. })).await;
    let MicEvent::HostDone {
        restored, result, ..
    } = e
    else {
        panic!()
    };
    assert!(restored && result.is_ok(), "{result:?}");
    let calls = rig.worker.0.lock().unwrap().calls.clone();
    assert!(
        calls.contains(&Call::RestoreDefault("alsa_input.usb-a".into())),
        "{calls:?}"
    );
    // Nothing on record at all: restored all the same (the worker acts
    // only if the Omarchy Broadcast Mic is the default).
    let (tx, _rx3) = reply();
    rig.requests
        .send(MicRequest::Host {
            on: false,
            previous: None,
            reply: tx,
        })
        .unwrap();
    rig.until(|e| matches!(e, MicEvent::HostDone { .. })).await;
    let calls = rig.worker.0.lock().unwrap().calls.clone();
    assert_eq!(calls.last(), Some(&Call::RestoreDefault(String::new())));
}

#[tokio::test]
async fn a_make_with_ours_the_default_already_keeps_the_previous_default_known_here() {
    let worker = FakeWorker::default();
    worker.0.lock().unwrap().installed = true;
    let rig = Rig::start(worker, FakeUnits::default(), Vec::new());
    let make = |doc: serde_json::Value| {
        rig.worker
            .0
            .lock()
            .unwrap()
            .replies
            .insert("MakeDefault", Ok(doc.to_string()));
        let (tx, _rx) = reply();
        rig.requests
            .send(MicRequest::MakeDefault {
                target: String::new(),
                reply: tx,
            })
            .unwrap();
    };
    let made = || async {
        let e = rig
            .until(|e| matches!(e, MicEvent::DefaultMade { .. }))
            .await;
        let MicEvent::DefaultMade {
            previous, already, ..
        } = e
        else {
            panic!()
        };
        (previous, already)
    };
    let off = || async {
        let (tx, _rx) = reply();
        rig.requests
            .send(MicRequest::Host {
                on: false,
                previous: None,
                reply: tx,
            })
            .unwrap();
        rig.until(|e| matches!(e, MicEvent::HostDone { .. })).await;
        rig.worker.0.lock().unwrap().calls.last().cloned()
    };
    make(json!({"previous": "alsa_input.cmedia", "in_force": "alsa_input.dmic"}));
    assert_eq!(made().await, ("alsa_input.cmedia".into(), false));
    // Again, ours the configured default already: the worker names the pin
    // in force; told to the controller, but the one known here stays.
    make(json!({"previous": "alsa_input.c930e", "in_force": null, "already": true}));
    assert_eq!(made().await, ("alsa_input.c930e".into(), true));
    // `mic off` before the controller's record reached the request: the
    // first one is restored.
    assert_eq!(
        off().await,
        Some(Call::RestoreDefault("alsa_input.cmedia".into()))
    );
    // Nothing known here: `already` records what the worker names.
    make(json!({"previous": "alsa_input.c930e", "already": true}));
    made().await;
    assert_eq!(
        off().await,
        Some(Call::RestoreDefault("alsa_input.c930e".into()))
    );
}

#[tokio::test]
async fn following_a_running_worker_never_activates_it() {
    let worker = FakeWorker::default();
    {
        let mut w = worker.0.lock().unwrap();
        w.installed = true;
        w.running = true;
    }
    let rig = Rig::start(worker, FakeUnits::default(), Vec::new());
    rig.until(|e| matches!(e, MicEvent::Catalogue(_))).await;
    rig.wait_for("the status read", |r| r.calls().contains(&"GetStatus"))
        .await;
    rig.worker.0.lock().unwrap().unactivated.clear();
    let before = rig.calls().len();
    rig.events
        .send(WorkerEvent::Status(json!({"state": "active"}).to_string()))
        .unwrap();
    rig.requests
        .send(MicRequest::Snapshot("{\"version\":1}".into()))
        .unwrap();
    rig.wait_for("the follow-up reads", |r| {
        let n = r.calls()[before..]
            .iter()
            .filter(|c| **c == "GetSettings")
            .count();
        n >= 2
    })
    .await;
    let w = rig.worker.0.lock().unwrap();
    let follow: Vec<&str> = w.calls[before..].iter().map(kind).collect();
    assert_eq!(
        w.unactivated, follow,
        "every follow-up call without activation"
    );
}

#[tokio::test]
async fn the_host_state_is_told_before_a_slow_catalogue() {
    let worker = FakeWorker::default();
    let gate = Arc::new(tokio::sync::Notify::new());
    {
        let mut w = worker.0.lock().unwrap();
        w.installed = true;
        w.gate = Some(gate.clone());
    }
    let units = FakeUnits::default();
    units
        .0
        .lock()
        .unwrap()
        .active
        .insert(HOST_UNIT.into(), true);
    let rig = Rig::start(worker, units, Vec::new());
    // The worker has not answered GetModules yet.
    let e = rig.next().await;
    assert!(
        matches!(&e, MicEvent::Status(s) if s.state == "passthrough"),
        "{e:?}"
    );
    gate.notify_one();
    rig.until(|e| matches!(e, MicEvent::Catalogue(_))).await;
}

// ---- the device options ----

/// The node name of a USB microphone, 53 bytes: past the CLI's 32-byte
/// bound on an option value.
const LONG_NAME: &str = "alsa_input.usb-K66_K66_20190805V001-00.analog-stereo";

#[test]
fn a_long_node_name_is_offered_as_a_short_stable_token() {
    assert!(LONG_NAME.len() > 32);
    // FNV-1a is fixed: the same name always has the same token.
    assert_eq!(device_token(""), "pw-cbf29ce48422");
    assert_eq!(device_token(LONG_NAME), device_token(LONG_NAME));
    let token = device_token(LONG_NAME);
    assert_eq!(token.len(), 3 + 12);
    assert!(token.starts_with("pw-"));
    assert!(
        token[3..]
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    );
    // The worker's long name is offered (not dropped), and what the
    // daemon publishes fits the CLI's 32-byte option bounds.
    let devices = parse_devices(&json!({"devices": [
        {"name": LONG_NAME, "description": "K66 Analog Stereo"},
        {"name": "x".repeat(DEVICE_NAME_MAX + 1)},
    ]}));
    assert_eq!(devices.len(), 1);
    let mut config = Config::default();
    config.mic.device = LONG_NAME.into();
    let mut known = catalogue();
    known.devices = devices;
    let g = groups_of(&config, &RuntimeState::default(), &known, true);
    let SettingKind::Enum { options, value, .. } = &g[0].settings[1].kind else {
        panic!("device is an enum");
    };
    assert_eq!(value, &token);
    assert_eq!(
        options,
        &[
            ("default".to_string(), "System default".to_string()),
            (token.clone(), "K66 Analog Stereo".to_string()),
        ]
    );
    assert!(options.iter().all(|(v, l)| v.len() <= 32 && l.len() <= 32));

    // Set by token, and by the node name (the CLI's `mic device NAME`).
    use obc_core::config_schema::ConfigValue as C;
    let want = Ok(Write::Config(KEY_DEVICE, C::Str(LONG_NAME.into())));
    for given in [token.as_str(), LONG_NAME] {
        assert_eq!(
            check_input(
                &g,
                &known.devices,
                LONG_NAME,
                DEVICE,
                Some(&SettingValue::Str(given.into()))
            ),
            want
        );
    }
    // The node name of a device not listed and not configured is refused.
    assert!(matches!(
        check_input(
            &g,
            &known.devices,
            "default",
            DEVICE,
            Some(&SettingValue::Str("alsa_input.other".into()))
        ),
        Err(Refusal::Invalid(_))
    ));
}

#[test]
fn a_configured_device_that_is_absent_is_listed_unavailable() {
    let entries = device_entries(&catalogue().devices, LONG_NAME);
    let last = entries.last().unwrap();
    assert_eq!(last.name, LONG_NAME);
    assert_eq!(last.token, device_token(LONG_NAME));
    assert!(last.label.starts_with("Unavailable: alsa_input.usb-K66"));
    assert!(last.label.len() <= LABEL_MAX);
    // Nothing configured (default): no extra entry.
    assert_eq!(device_entries(&catalogue().devices, "default").len(), 3);
    // Present: its own label, not marked.
    let present = device_entries(&catalogue().devices, "alsa_input.usb-a");
    assert_eq!(present.len(), 3);
    assert_eq!(
        find_device(&present, "alsa_input.usb-a").unwrap().label,
        "USB A"
    );
}

#[test]
fn colliding_tokens_are_settled_in_listing_order() {
    let devices: Vec<(String, String)> = ["a", "b", "c"]
        .iter()
        .map(|n| (n.to_string(), n.to_uppercase()))
        .collect();
    // A hash that is the same for every name: the later ones' tokens grow,
    // then take a counter, and all are distinct.
    let entries = device_entries_with(&devices, "default", |_| 0x0123_4567_89ab_cdef);
    let tokens: Vec<&str> = entries.iter().map(|e| e.token.as_str()).collect();
    assert_eq!(
        tokens,
        [
            "default",
            "pw-0123456789ab",
            "pw-0123456789abc",
            "pw-0123456789abcd"
        ]
    );
    // The same list gives the same tokens, and each token finds its device.
    assert_eq!(
        entries,
        device_entries_with(&devices, "default", |_| 0x0123_4567_89ab_cdef)
    );
    for e in &entries {
        assert_eq!(find_device(&entries, &e.token), Some(e));
        assert!(e.token.len() <= 32);
    }
    // Past the extensions of the hash, a counter.
    let many: Vec<(String, String)> = (0..8).map(|i| (format!("n{i}"), String::new())).collect();
    let entries = device_entries_with(&many, "default", |_| 0x0123_4567_89ab_cdef);
    let mut tokens: Vec<&str> = entries.iter().map(|e| e.token.as_str()).collect();
    tokens.sort_unstable();
    tokens.dedup();
    assert_eq!(tokens.len(), entries.len());
    assert!(entries.iter().all(|e| e.token.len() <= 32));
    assert!(entries[6].token.ends_with("-2"));
    assert!(entries.last().unwrap().token.ends_with("-4"));
}

#[test]
fn the_configured_device_survives_the_option_cap() {
    let many: Vec<(String, String)> = (0..40).map(|i| (format!("d{i}"), String::new())).collect();
    // Listed, but past the cap: kept, with its own label.
    let entries = device_entries(&many, "d30");
    assert_eq!(entries.len(), MAX_DEVICES);
    assert_eq!(entries.last().unwrap().name, "d30");
    assert_eq!(entries.last().unwrap().label, "d30");
    // Absent: kept, marked.
    let entries = device_entries(&many, "gone");
    assert_eq!(entries.len(), MAX_DEVICES);
    assert_eq!(entries.last().unwrap().label, "Unavailable: gone");
    // Within the cap: the order is the worker's.
    let entries = device_entries(&many, "d2");
    assert_eq!(entries[3].name, "d2");
}
