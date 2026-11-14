//! Tests of the config schema.

use std::fs;
use std::str::FromStr;

use super::*;
use crate::config_d::{CONFIG_FILE, MANAGED_REL};

fn facts() -> RegistryFacts {
    RegistryFacts {
        modules: vec![
            ModuleFact {
                id: "org.example.hello".into(),
                name: "Hello tint".into(),
                forceable: true,
            },
            ModuleFact {
                id: "br.com.dias.maxine-vfx".into(),
                name: "NVIDIA Maxine".into(),
                forceable: true,
            },
        ],
        stages: vec![
            StageFact {
                id: "background".into(),
                modules: vec!["br.com.dias.maxine-vfx".into()],
            },
            StageFact {
                id: "hello".into(),
                modules: vec!["org.example.hello".into()],
            },
            StageFact {
                id: "frame-generation".into(),
                modules: vec!["br.com.dias.maxine-vfx".into()],
            },
        ],
        output_modes: vec![
            (
                "1920x1080@60".into(),
                "1920x1080 \u{b7} 60 fps (generated)".into(),
            ),
            ("1920x1080@30".into(), "1920x1080 \u{b7} 30 fps".into()),
            ("1280x720@30".into(), "1280x720 \u{b7} 30 fps".into()),
        ],
        ..RegistryFacts::default()
    }
}

fn load(files: &[(&str, &str)]) -> (tempfile::TempDir, ConfigSet) {
    let tmp = tempfile::tempdir().unwrap();
    for (rel, text) in files {
        let path = tmp.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let set = ConfigSet::load(&tmp.path().join(CONFIG_FILE)).unwrap();
    (tmp, set)
}

fn find<'a>(groups: &'a [ConfigGroup], id: &str) -> &'a ConfigRow {
    groups
        .iter()
        .flat_map(|g| &g.rows)
        .find(|r| r.id == id)
        .unwrap_or_else(|| panic!("no row {id}"))
}

#[test]
fn the_core_table_is_well_formed() {
    let group_ids: Vec<&str> = GROUPS.iter().map(|(id, _)| *id).collect();
    assert_eq!(
        group_ids,
        [
            "camera",
            "output",
            "video",
            "loopback",
            "lifecycle",
            "effect",
            "pipewire",
            "mic",
            "modules"
        ]
    );
    for (i, k) in CORE_KEYS.iter().enumerate() {
        assert!(group_ids.contains(&k.group), "{}", k.key);
        assert!(k.key.starts_with(&format!("{}.", k.group)), "{}", k.key);
        assert!(!k.label.is_empty(), "{}", k.key);
        assert!(
            !k.description.is_empty() && k.description.len() <= DESCRIPTION_MAX,
            "{}: {} bytes",
            k.key,
            k.description.len()
        );
        assert!(CORE_KEYS[..i].iter().all(|o| o.key != k.key), "{}", k.key);
        // Every core key is a key of the configuration.
        assert!(lookup(k.key, &RegistryFacts::default()).is_some());
    }
    // Enum options parse as the configuration reads them.
    let parses = |key: &str, v: &str| match key {
        "output.format" => crate::OutputFormat::from_str(v).is_ok(),
        "output.aspect" => crate::AspectPolicy::from_str(v).is_ok(),
        "output.placeholder" => crate::OutputPlaceholder::from_str(v).is_ok(),
        "effect.mode" => crate::EffectMode::from_str(v).is_ok(),
        "effect.relight_light" => crate::RelightLight::from_str(v).is_ok(),
        "effect.privacy_mode" => crate::PrivacyMode::from_str(v).is_ok(),
        "modules.profile" => crate::modules::BackendProfile::from_str(v).is_ok(),
        "mic.gpu_policy" => {
            toml::from_str::<crate::config::MicConfig>(&format!("gpu_policy = \"{v}\"")).is_ok()
        }
        "video.memory" => {
            toml::from_str::<crate::config::VideoConfig>(&format!("memory = \"{v}\"")).is_ok()
        }
        "video.battery_saver" => {
            toml::from_str::<crate::config::VideoConfig>(&format!("battery_saver = \"{v}\""))
                .is_ok()
        }
        other => panic!("enum {other} not checked"),
    };
    for k in CORE_KEYS {
        if let SpecKind::Enum(options) = k.kind {
            for (v, _) in options {
                assert!(parses(k.key, v), "{}: {v}", k.key);
            }
        }
    }
}

#[test]
fn the_editable_and_apply_table_is_the_decided_one() {
    let editable: Vec<&str> = CORE_KEYS
        .iter()
        .filter(|k| k.editable)
        .map(|k| k.key)
        .collect();
    assert_eq!(
        editable,
        [
            "camera.fixed_framerate",
            "output.mode",
            "output.format",
            "output.aspect",
            "output.hold_timeout_s",
            "output.input_gap_ms",
            "output.placeholder",
            "video.memory",
            "video.battery_saver",
            "lifecycle.idle_grace_s",
            "lifecycle.start_debounce_ms",
            "effect.privacy",
            "effect.privacy_mode",
            "effect.fallback_image",
            "pipewire.enabled",
            "pipewire.offer_nv12",
            "mic.enabled",
            "mic.device",
            "mic.pipeline_depth",
            "mic.idle_exit_s",
            "mic.gpu_warm_s",
            "mic.gpu_policy",
            "modules.profile",
        ]
    );
    let restart: Vec<&str> = CORE_KEYS
        .iter()
        .filter(|k| k.apply == Apply::Restart)
        .map(|k| k.key)
        .collect();
    assert_eq!(restart, ["effect.maxine_root"]);
    for k in CORE_KEYS {
        let live = k.key.starts_with("lifecycle.")
            || (k.key.starts_with("effect.") && k.key != "effect.maxine_root")
            || k.key == "camera.preference"
            || k.key == "camera.mode"
            || k.key == "output.placeholder"
            || [
                "mic.enabled",
                "mic.device",
                "mic.idle_exit_s",
                "mic.gpu_warm_s",
                "mic.force_conflicts",
            ]
            .contains(&k.key);
        assert_eq!(k.apply == Apply::Live, live, "{}", k.key);
    }
}

#[test]
fn rows_carry_value_default_source_and_override() {
    let (_tmp, set) = load(&[
        (
            "config.toml",
            "[output]\nformat = \"i420\"\n[camera]\npreference = [\"Brio\"]\n\
             [modules]\ndisabled = [\"org.example.hello\"]\n",
        ),
        (
            MANAGED_REL,
            "[output]\nhold_timeout_s = 2.5\n[modules]\nprofile = \"battery\"\n",
        ),
        (
            "config.d/90-local.toml",
            "[output]\naspect = \"letterbox\"\n[modules.pin]\nhello = \"org.example.hello\"\n",
        ),
    ]);
    let groups = config_schema(&set, &facts());
    let ids: Vec<&str> = groups.iter().map(|g| g.id.as_str()).collect();
    assert_eq!(ids, GROUPS.map(|(id, _)| id));

    let r = find(&groups, "output.format");
    assert_eq!(r.value, ConfigValue::Str("i420".into()));
    assert_eq!(r.default, Some(ConfigValue::Str("nv12".into())));
    assert_eq!(r.source, "config.toml");
    assert_eq!((r.overridden_by.as_str(), r.enabled), ("", true));
    assert!(r.editable);
    assert_eq!(r.apply, Apply::Reload);
    assert_eq!(r.kind.type_name(), "enum");

    let r = find(&groups, "output.hold_timeout_s");
    assert_eq!(r.value, ConfigValue::Float(2.5));
    assert_eq!(r.default, Some(ConfigValue::Float(3.0)));
    assert_eq!(r.source, MANAGED_REL);
    assert!(matches!(r.kind, RowKind::Float { unit: Some(ref u), .. } if u == "s"));

    let r = find(&groups, "output.aspect");
    assert_eq!(r.source, "config.d/90-local.toml");
    assert_eq!(r.overridden_by, "config.d/90-local.toml");
    assert!(!r.enabled);
    assert_eq!(r.disabled_reason, "Overridden by config.d/90-local.toml");

    let r = find(&groups, "output.mode");
    assert_eq!(r.value, ConfigValue::Str("1920x1080@30".into()));
    assert_eq!(r.default, Some(ConfigValue::Str("1920x1080@30".into())));
    assert!(r.editable);
    assert_eq!(r.source, "");
    assert_eq!(
        r.kind,
        RowKind::Enum {
            options: facts().output_modes
        }
    );

    let r = find(&groups, "camera.preference");
    assert_eq!(r.value, ConfigValue::List(vec!["Brio".into()]));
    assert_eq!(r.default, Some(ConfigValue::List(Vec::new())));

    // A path not set: empty, with no default.
    let r = find(&groups, "loopback.device");
    assert_eq!(r.value, ConfigValue::Str(String::new()));
    assert_eq!(r.default, None);
    assert_eq!(r.kind, RowKind::Path);

    let r = find(&groups, "effect.maxine_root");
    assert_eq!(r.apply, Apply::Restart);
    assert!(!r.editable);
}

#[test]
fn the_module_rows_come_from_the_registry() {
    let (_tmp, set) = load(&[
        (
            "config.toml",
            "[modules]\ndisabled = [\"org.example.hello\"]\n",
        ),
        (MANAGED_REL, "[modules]\nprofile = \"battery\"\n"),
        (
            "config.d/90-local.toml",
            "[modules.pin]\nhello = \"org.example.hello\"\n",
        ),
    ]);
    let groups = config_schema(&set, &facts());
    let modules = groups.iter().find(|g| g.id == "modules").unwrap();
    let ids: Vec<&str> = modules.rows.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "modules.profile",
            "modules.pin.background",
            "modules.pin.hello",
            "modules.enabled.org.example.hello",
            "modules.enabled.br.com.dias.maxine-vfx",
            "modules.forced.org.example.hello",
            "modules.forced.br.com.dias.maxine-vfx",
            "modules.order",
            "modules.dirs",
            "modules.disabled",
        ]
    );
    let r = find(&groups, "modules.profile");
    assert_eq!(r.value, ConfigValue::Str("battery".into()));
    assert_eq!(r.default, Some(ConfigValue::Str("quality".into())));
    assert_eq!(r.source, MANAGED_REL);

    let r = find(&groups, "modules.pin.background");
    assert_eq!(r.value, ConfigValue::Str(String::new()));
    assert_eq!(r.default, Some(ConfigValue::Str(String::new())));
    assert_eq!(
        r.kind,
        RowKind::Enum {
            options: vec![
                (String::new(), "Automatic".into()),
                ("br.com.dias.maxine-vfx".into(), "NVIDIA Maxine".into()),
            ]
        }
    );
    assert!(r.editable && r.enabled);

    let r = find(&groups, "modules.pin.hello");
    assert_eq!(r.value, ConfigValue::Str("org.example.hello".into()));
    assert_eq!(r.disabled_reason, "Overridden by config.d/90-local.toml");
    assert!(!r.enabled);

    let r = find(&groups, "modules.enabled.org.example.hello");
    assert_eq!(r.label, "Hello tint");
    assert_eq!(r.value, ConfigValue::Bool(false));
    assert_eq!(r.default, Some(ConfigValue::Bool(true)));
    // Disabled by the list: the list's file is the source.
    assert_eq!(r.source, "config.toml");
    let r = find(&groups, "modules.enabled.br.com.dias.maxine-vfx");
    assert_eq!(r.value, ConfigValue::Bool(true));
    assert_eq!(r.source, "");
    // The force rows: off by default, a switch, editable.
    let r = find(&groups, "modules.forced.org.example.hello");
    assert_eq!(r.label, "Force Hello tint");
    assert_eq!(r.kind, RowKind::Bool);
    assert_eq!(r.value, ConfigValue::Bool(false));
    assert_eq!(r.default, Some(ConfigValue::Bool(false)));
    assert!(r.editable && r.enabled);
    assert!(r.description.contains("debugging"), "{}", r.description);
    assert!(r.description.len() <= DESCRIPTION_MAX);
    assert_eq!(
        lookup("modules.forced.org.example.hello", &facts()).map(|k| k.kind),
        Some(RowKind::Bool)
    );
    assert!(lookup("modules.forced.org.unknown.x", &facts()).is_none());
    // An audio module has no force row (`[mic] force_conflicts` instead).
    let mut audio = facts();
    audio.modules[0].forceable = false;
    let groups = config_schema(&set, &audio);
    let ids: Vec<&str> = groups
        .iter()
        .flat_map(|g| &g.rows)
        .map(|r| r.id.as_str())
        .collect();
    assert!(!ids.contains(&"modules.forced.org.example.hello"));
    assert!(ids.contains(&"modules.enabled.org.example.hello"));
    assert!(ids.contains(&"modules.forced.br.com.dias.maxine-vfx"));
    assert!(lookup("modules.forced.org.example.hello", &audio).is_none());
    assert!(lookup("modules.enabled.org.example.hello", &audio).is_some());

    // No registry: no module rows, the fixed ones stay.
    let groups = config_schema(&set, &RegistryFacts::default());
    let modules = groups.iter().find(|g| g.id == "modules").unwrap();
    assert_eq!(modules.rows.len(), 4);
}

#[test]
fn output_mode_offers_the_runtime_options_and_the_value_in_force() {
    let (_tmp, set) = load(&[
        ("config.toml", "[output]\nmode = \"1920x1080@30\"\n"),
        (MANAGED_REL, "[output]\nmode = \"1600x900@25\"\n"),
    ]);
    let groups = config_schema(&set, &facts());
    let r = find(&groups, "output.mode");
    assert_eq!(r.value, ConfigValue::Str("1600x900@25".into()));
    assert_eq!(r.source, MANAGED_REL);
    let RowKind::Enum { options } = &r.kind else {
        panic!("{:?}", r.kind)
    };
    assert_eq!(options.len(), 4);
    assert_eq!(options[3], ("1600x900@25".into(), "1600x900@25".into()));
    // Written as an enum: one of the runtime options, nothing else.
    let kind = lookup("output.mode", &facts()).unwrap().kind;
    assert_eq!(
        to_toml(
            "output.mode",
            &kind,
            &ConfigValue::Str("1280x720@30".into())
        )
        .unwrap(),
        toml::Value::String("1280x720@30".into())
    );
    let e = to_toml("output.mode", &kind, &ConfigValue::Str("800x600@30".into())).unwrap_err();
    assert!(e.contains("is not one of"), "{e}");
    assert!(to_toml("output.mode", &kind, &ConfigValue::Int(30)).is_err());
    // With no option computed, nothing is accepted.
    let kind = lookup("output.mode", &RegistryFacts::default())
        .unwrap()
        .kind;
    assert!(
        to_toml(
            "output.mode",
            &kind,
            &ConfigValue::Str("1920x1080@30".into())
        )
        .is_err()
    );
}

#[test]
fn the_frame_generation_pin_is_shown_only_with_a_choice_of_module() {
    let (_tmp, set) = load(&[]);
    let groups = config_schema(&set, &facts());
    assert!(
        !groups
            .iter()
            .flat_map(|g| g.rows.iter())
            .any(|r| r.id == "modules.pin.frame-generation")
    );
    let mut two = facts();
    two.stages[2].modules.push("org.example.hello".into());
    let groups = config_schema(&set, &two);
    let r = find(&groups, "modules.pin.frame-generation");
    assert!(r.editable);
}

#[test]
fn a_pin_to_a_module_not_offering_the_stage_is_still_an_option() {
    let (_tmp, set) = load(&[(
        "config.toml",
        "[modules.pin]\nbackground = \"org.gone.module\"\n",
    )]);
    let groups = config_schema(&set, &facts());
    let r = find(&groups, "modules.pin.background");
    let RowKind::Enum { options } = &r.kind else {
        panic!("{:?}", r.kind)
    };
    assert_eq!(options.last().unwrap().0, "org.gone.module");
    assert_eq!(r.value, ConfigValue::Str("org.gone.module".into()));
}

#[test]
fn values_convert_to_the_type_of_the_key() {
    let kind = |k: &str| lookup(k, &facts()).unwrap().kind;
    assert_eq!(
        to_toml("x", &kind("output.hold_timeout_s"), &ConfigValue::Int(2)).unwrap(),
        toml::Value::Float(2.0)
    );
    assert!(to_toml("x", &kind("output.input_gap_ms"), &ConfigValue::Float(2.0)).is_err());
    assert!(
        to_toml(
            "x",
            &kind("pipewire.enabled"),
            &ConfigValue::Str("true".into())
        )
        .is_err()
    );
    assert_eq!(
        to_toml(
            "x",
            &kind("modules.pin.background"),
            &ConfigValue::Str(String::new())
        )
        .unwrap(),
        toml::Value::String(String::new())
    );
    assert_eq!(
        to_toml("x", &RowKind::List, &ConfigValue::List(vec!["a".into()])).unwrap(),
        toml::Value::Array(vec![toml::Value::String("a".into())])
    );
    assert!(lookup("modules.enabled.org.unknown.x", &facts()).is_none());
    assert!(lookup("modules.pin.unknown", &facts()).is_none());
    assert!(lookup("version", &facts()).is_none());
}

fn options(r: &ConfigRow) -> Vec<(&str, &str)> {
    let RowKind::Enum { options } = &r.kind else {
        panic!("{:?}", r.kind)
    };
    options
        .iter()
        .map(|(v, l)| (v.as_str(), l.as_str()))
        .collect()
}

#[test]
fn privacy_is_off_by_default_and_its_mode_offers_the_away_image_only_with_one() {
    // Nothing set: privacy off, the mode black, greyed with the reason.
    let (_tmp, set) = load(&[]);
    let groups = config_schema(&set, &facts());
    assert!(
        groups
            .iter()
            .all(|g| g.rows.iter().all(|r| r.id != "effect.on_failure")),
        "the legacy key has no row"
    );
    let p = find(&groups, "effect.privacy");
    assert_eq!(p.label, "Hide the camera when an effect can't run");
    assert_eq!(p.kind, RowKind::Bool);
    assert!(p.editable && p.enabled);
    assert_eq!(p.apply, Apply::Live);
    assert_eq!(p.value, ConfigValue::Bool(false));
    assert_eq!(p.default, Some(ConfigValue::Bool(false)));
    assert_eq!(p.source, "");
    let m = find(&groups, "effect.privacy_mode");
    assert_eq!(m.label, "Show instead of the camera");
    assert!(m.editable);
    assert_eq!(m.apply, Apply::Live);
    assert_eq!(m.value, ConfigValue::Str("black".into()));
    assert_eq!(m.default, Some(ConfigValue::Str("black".into())));
    assert_eq!(options(m), [("black", "Black"), ("freeze", "Last frame")]);
    assert!(!m.enabled);
    assert_eq!(m.disabled_reason, PRIVACY_OFF_REASON);
    let image = find(&groups, "effect.fallback_image");
    assert_eq!(image.kind, RowKind::Path);
    assert_eq!(image.label, "Away image");
    assert!(image.editable && image.enabled);
    assert_eq!(image.value, ConfigValue::Str(String::new()));
    assert_eq!(image.default, None);

    // With an image and privacy on: the away image too, between them, and
    // the mode enabled.
    let (_tmp, set) = load(&[(
        "config.toml",
        "[effect]\nprivacy = true\nfallback_image = \"/srv/away.png\"\n",
    )]);
    let groups = config_schema(&set, &facts());
    let p = find(&groups, "effect.privacy");
    assert_eq!(p.value, ConfigValue::Bool(true));
    assert_eq!(p.source, "config.toml");
    let m = find(&groups, "effect.privacy_mode");
    assert!(m.enabled, "{}", m.disabled_reason);
    assert_eq!(
        options(m),
        [
            ("black", "Black"),
            ("away", "Away image"),
            ("freeze", "Last frame")
        ]
    );
    let image = find(&groups, "effect.fallback_image");
    assert_eq!(image.value, ConfigValue::Str("/srv/away.png".into()));
    assert_eq!(image.source, "config.toml");

    // `away` in force without an image: still an option.
    let (_tmp, set) = load(&[(
        "config.toml",
        "[effect]\nprivacy = true\nprivacy_mode = \"away\"\n",
    )]);
    let groups = config_schema(&set, &facts());
    let m = find(&groups, "effect.privacy_mode");
    assert_eq!(m.value, ConfigValue::Str("away".into()));
    assert_eq!(options(m).len(), 3);
    assert_eq!(options(m)[1], ("away", "Away image"));
}

#[test]
fn the_privacy_rows_show_a_legacy_on_failure_in_force() {
    for (legacy, on, mode) in [
        ("black", true, "black"),
        ("image", true, "away"),
        ("none", false, "black"),
    ] {
        let (_tmp, set) = load(&[(
            "config.toml",
            &format!("[effect]\non_failure = \"{legacy}\"\n"),
        )]);
        let groups = config_schema(&set, &facts());
        let p = find(&groups, "effect.privacy");
        assert_eq!(p.value, ConfigValue::Bool(on), "{legacy}");
        assert_eq!(p.source, "config.toml", "{legacy}: the legacy key's file");
        let m = find(&groups, "effect.privacy_mode");
        assert_eq!(m.value, ConfigValue::Str(mode.into()), "{legacy}");
        assert_eq!(m.enabled, on, "{legacy}");
        assert!(options(m).iter().any(|(v, _)| *v == mode), "{legacy}");
    }
    // A new key set in a later file: the legacy key is ignored.
    let (_tmp, set) = load(&[
        ("config.toml", "[effect]\non_failure = \"black\"\n"),
        (MANAGED_REL, "[effect]\nprivacy = false\n"),
    ]);
    let groups = config_schema(&set, &facts());
    let p = find(&groups, "effect.privacy");
    assert_eq!(p.value, ConfigValue::Bool(false));
    assert_eq!(p.source, MANAGED_REL);
}

#[test]
fn the_privacy_rows_take_each_unset_key_from_the_legacy_on_failure() {
    // Legacy black, a mode of the panel: privacy on (the legacy file), the
    // mode of the panel, enabled.
    let (_tmp, set) = load(&[
        ("config.toml", "[effect]\non_failure = \"black\"\n"),
        (MANAGED_REL, "[effect]\nprivacy_mode = \"freeze\"\n"),
    ]);
    let groups = config_schema(&set, &facts());
    let p = find(&groups, "effect.privacy");
    assert_eq!(p.value, ConfigValue::Bool(true));
    assert_eq!(p.source, "config.toml");
    assert!(p.enabled, "{}", p.disabled_reason);
    let m = find(&groups, "effect.privacy_mode");
    assert_eq!(m.value, ConfigValue::Str("freeze".into()));
    assert_eq!(m.source, MANAGED_REL);
    assert!(m.enabled, "{}", m.disabled_reason);
    // Legacy image, privacy of the panel: the mode is the legacy file's.
    let (_tmp, set) = load(&[
        ("config.toml", "[effect]\non_failure = \"image\"\n"),
        (MANAGED_REL, "[effect]\nprivacy = true\n"),
    ]);
    let groups = config_schema(&set, &facts());
    let m = find(&groups, "effect.privacy_mode");
    assert_eq!(m.value, ConfigValue::Str("away".into()));
    assert_eq!(m.source, "config.toml");
}

#[test]
fn a_legacy_on_failure_after_the_managed_file_overrides_the_privacy_rows() {
    let (_tmp, set) = load(&[
        (MANAGED_REL, "[effect]\non_failure = \"none\"\n"),
        ("config.d/90-x.toml", "[effect]\non_failure = \"black\"\n"),
    ]);
    let groups = config_schema(&set, &facts());
    for id in ["effect.privacy", "effect.privacy_mode"] {
        let r = find(&groups, id);
        assert_eq!(r.source, "config.d/90-x.toml", "{id}");
        assert_eq!(r.overridden_by, "config.d/90-x.toml", "{id}");
        assert!(!r.enabled, "{id}");
        assert_eq!(
            r.disabled_reason, "Overridden by config.d/90-x.toml",
            "{id}"
        );
    }
    assert_eq!(
        find(&groups, "effect.privacy").value,
        ConfigValue::Bool(true)
    );
    // The panel's own keys are not overridden by a legacy key they win
    // over: privacy set by the panel stays editable; the mode, which the
    // legacy key still gives, does not.
    let (_tmp, set) = load(&[
        (MANAGED_REL, "[effect]\nprivacy = false\n"),
        ("config.d/90-x.toml", "[effect]\non_failure = \"image\"\n"),
    ]);
    let groups = config_schema(&set, &facts());
    let p = find(&groups, "effect.privacy");
    assert!(p.enabled, "{}", p.disabled_reason);
    assert_eq!(p.source, MANAGED_REL);
    let m = find(&groups, "effect.privacy_mode");
    assert_eq!(m.disabled_reason, "Overridden by config.d/90-x.toml");
}

#[test]
fn the_privacy_mode_is_written_as_one_of_the_options_the_daemon_computed() {
    let mut effect = crate::config::EffectConfig::default();
    assert_eq!(privacy_mode_options(&effect).len(), 2);
    // Without computed options, those of the defaults: no away image.
    let kind = lookup(PRIVACY_MODE, &RegistryFacts::default())
        .unwrap()
        .kind;
    let away = ConfigValue::Str("away".into());
    let e = to_toml(PRIVACY_MODE, &kind, &away).unwrap_err();
    assert!(e.contains("is not one of: black, freeze"), "{e}");
    assert!(to_toml(PRIVACY_MODE, &kind, &ConfigValue::Str("freeze".into())).is_ok());
    // With an image configured, the away image is accepted.
    effect.fallback_image = Some("/srv/away.png".into());
    let facts = RegistryFacts {
        privacy_mode_options: privacy_mode_options(&effect),
        ..facts()
    };
    let kind = lookup(PRIVACY_MODE, &facts).unwrap().kind;
    assert_eq!(
        to_toml(PRIVACY_MODE, &kind, &away).unwrap(),
        toml::Value::String("away".into())
    );
    // Every option parses as the configuration reads it.
    for (v, _) in privacy_mode_options(&effect) {
        assert!(crate::PrivacyMode::from_str(&v).is_ok(), "{v}");
    }
    // The switch is a bool; the legacy key is no row.
    let info = lookup(PRIVACY, &facts).unwrap();
    assert!(info.editable);
    assert_eq!(info.kind, RowKind::Bool);
    assert!(lookup(LEGACY_ON_FAILURE, &facts).is_none());
    // The image row is a path the panel writes (the away image).
    let info = lookup("effect.fallback_image", &facts).unwrap();
    assert!(info.editable);
    assert_eq!(info.kind, RowKind::Path);
    assert_eq!(info.apply, Apply::Live);
}

#[test]
fn an_overridden_privacy_mode_gives_the_override_as_its_reason() {
    let (_tmp, set) = load(&[(
        "config.d/90-local.toml",
        "[effect]\nprivacy_mode = \"freeze\"\n",
    )]);
    let groups = config_schema(&set, &facts());
    let m = find(&groups, "effect.privacy_mode");
    assert!(!m.enabled);
    assert_eq!(m.disabled_reason, "Overridden by config.d/90-local.toml");
}

#[test]
fn the_placeholder_row_offers_black_and_the_away_image_live() {
    let (_tmp, set) = load(&[]);
    let groups = config_schema(&set, &facts());
    let r = find(&groups, "output.placeholder");
    assert_eq!(r.label, "When there is no camera picture");
    assert!(r.editable && r.enabled);
    assert_eq!(r.apply, Apply::Live);
    assert_eq!(r.value, ConfigValue::Str("black".into()));
    assert_eq!(r.default, Some(ConfigValue::Str("black".into())));
    assert_eq!(options(r), [("black", "Black"), ("image", "Away image")]);
    let (_tmp, set) = load(&[(MANAGED_REL, "[output]\nplaceholder = \"image\"\n")]);
    let groups = config_schema(&set, &facts());
    let r = find(&groups, "output.placeholder");
    assert_eq!(r.value, ConfigValue::Str("image".into()));
    assert_eq!(r.source, MANAGED_REL);
    let kind = lookup("output.placeholder", &facts()).unwrap().kind;
    let e = to_toml(
        "output.placeholder",
        &kind,
        &ConfigValue::Str("none".into()),
    )
    .unwrap_err();
    assert!(e.contains("is not one of: black, image"), "{e}");
}

#[test]
fn the_mic_rows_carry_their_values_and_take_writes() {
    let (_tmp, set) = load(&[(
        "config.toml",
        "[mic]\nenabled = false\ngpu_policy = \"never\"\n",
    )]);
    let groups = config_schema(&set, &facts());
    let mic = groups.iter().find(|g| g.id == "mic").unwrap();
    let ids: Vec<&str> = mic.rows.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "mic.enabled",
            "mic.device",
            "mic.pipeline_depth",
            "mic.idle_exit_s",
            "mic.gpu_warm_s",
            "mic.gpu_policy",
            "mic.force_conflicts"
        ]
    );
    assert_eq!(find(&groups, "mic.enabled").value, ConfigValue::Bool(false));
    assert_eq!(
        find(&groups, "mic.enabled").default,
        Some(ConfigValue::Bool(true))
    );
    assert_eq!(
        find(&groups, "mic.device").value,
        ConfigValue::Str("default".into())
    );
    assert_eq!(find(&groups, "mic.idle_exit_s").value, ConfigValue::Int(30));
    assert_eq!(find(&groups, "mic.gpu_warm_s").value, ConfigValue::Int(30));
    assert_eq!(
        find(&groups, "mic.force_conflicts").value,
        ConfigValue::Bool(false)
    );
    assert!(!find(&groups, "mic.force_conflicts").editable);
    assert_eq!(
        find(&groups, "mic.gpu_policy").value,
        ConfigValue::Str("never".into())
    );
    let info = lookup("mic.gpu_policy", &RegistryFacts::default()).unwrap();
    assert!(
        to_toml(
            "mic.gpu_policy",
            &info.kind,
            &ConfigValue::Str("with-camera".into())
        )
        .is_ok()
    );
    assert!(
        to_toml(
            "mic.gpu_policy",
            &info.kind,
            &ConfigValue::Str("sometimes".into())
        )
        .is_err()
    );
}

#[test]
fn the_video_memory_row_states_the_memory_in_force() {
    use crate::config::{MemoryChoice, resolve_memory};
    let (_tmp, set) = load(&[(MANAGED_REL, "[video]\nmemory = \"system\"\n")]);
    let groups = config_schema(&set, &facts());
    let video = groups.iter().find(|g| g.id == "video").unwrap();
    assert_eq!(
        video.rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        ["video.memory", "video.battery_saver"]
    );
    let r = find(&groups, VIDEO_MEMORY);
    assert_eq!(r.value, ConfigValue::Str("system".into()));
    assert_eq!(r.default, Some(ConfigValue::Str("auto".into())));
    assert_eq!(r.source, MANAGED_REL);
    assert!(r.editable && r.enabled);
    assert_eq!(r.apply, Apply::Reload);
    let RowKind::Enum { options } = &r.kind else {
        panic!("{:?}", r.kind)
    };
    assert_eq!(
        options.iter().map(|(v, _)| v.as_str()).collect::<Vec<_>>(),
        ["auto", "cuda", "system"]
    );
    // Without the daemon's word, the fixed text.
    assert!(
        r.description.contains("next camera start"),
        "{}",
        r.description
    );
    assert!(!r.description.starts_with("Now:"), "{}", r.description);

    // With it, the memory in force and why.
    let facts = RegistryFacts {
        video_memory: Some(resolve_memory(
            MemoryChoice::Auto,
            Some("no NVIDIA driver is loaded"),
        )),
        ..facts()
    };
    let groups = config_schema(&set, &facts);
    assert_eq!(
        find(&groups, VIDEO_MEMORY).description,
        "Now: system memory, no NVIDIA driver is loaded. Applies at the next camera start."
    );
    // Other rows keep theirs.
    assert_eq!(
        find(&groups, "output.format").description,
        core_key("output.format").unwrap().description
    );
}

#[test]
fn a_video_memory_write_is_checked_against_its_options() {
    let info = lookup(VIDEO_MEMORY, &RegistryFacts::default()).unwrap();
    assert!(info.editable);
    assert_eq!(info.apply, Apply::Reload);
    assert_eq!(
        to_toml(VIDEO_MEMORY, &info.kind, &ConfigValue::Str("cuda".into())),
        Ok(toml::Value::String("cuda".into()))
    );
    let e = to_toml(VIDEO_MEMORY, &info.kind, &ConfigValue::Str("gpu".into())).unwrap_err();
    assert!(
        e.contains("\"gpu\" is not one of: auto, cuda, system"),
        "{e}"
    );
}

#[test]
fn the_output_mode_row_mentions_frame_generation_only_where_it_can_run() {
    let (_tmp, set) = load(&[]);
    let groups = config_schema(&set, &facts());
    let full = find(&groups, OUTPUT_MODE).description.clone();
    assert!(full.contains("frame generation"), "{full}");
    let facts = RegistryFacts {
        no_frame_generation: true,
        ..facts()
    };
    let groups = config_schema(&set, &facts);
    assert_eq!(
        find(&groups, OUTPUT_MODE).description,
        "Size and frame rate of the virtual camera."
    );
    assert!(full.starts_with(&find(&groups, OUTPUT_MODE).description));
}

#[test]
fn the_battery_saver_row_offers_the_modes_and_states_the_saver_now() {
    use crate::battery_saver::{
        BATTERY_SAVER_NOTICE, BatterySaver, PowerProfile, PowerSource, saver_state,
    };
    let (_tmp, set) = load(&[]);
    let groups = config_schema(&set, &facts());
    let r = find(&groups, BATTERY_SAVER);
    assert_eq!(r.label, "Battery saver");
    assert_eq!(r.value, ConfigValue::Str("auto".into()));
    assert_eq!(r.default, Some(ConfigValue::Str("auto".into())));
    assert!(r.editable && r.enabled, "{r:?}");
    assert_eq!(r.disabled_reason, "");
    assert_eq!(r.notice, "");
    assert_eq!(r.apply, Apply::Reload);
    let RowKind::Enum { options } = &r.kind else {
        panic!("{:?}", r.kind)
    };
    assert_eq!(
        options
            .iter()
            .map(|(v, l)| (v.as_str(), l.as_str()))
            .collect::<Vec<_>>(),
        [
            ("off", "Off"),
            ("auto", "On battery or power saver"),
            ("on_battery", "On battery"),
            ("power_profile", "Follow the power profile"),
            ("always", "Always"),
        ]
    );
    // Without the daemon's word, the fixed text.
    assert_eq!(r.description, core_key(BATTERY_SAVER).unwrap().description);
    assert!(r.description.contains("next camera start"));

    // With it, whether it is active and why; and the notice.
    let facts = RegistryFacts {
        battery_saver: Some(saver_state(
            BatterySaver::OnBattery,
            &PowerSource::Battery,
            &PowerProfile::Unavailable,
        )),
        battery_saver_notice: BATTERY_SAVER_NOTICE.into(),
        ..facts()
    };
    let groups = config_schema(&set, &facts);
    let r = find(&groups, BATTERY_SAVER);
    assert_eq!(
        r.description,
        "Keeps the NVIDIA GPU asleep by processing video in system memory. Now: active, on \
         battery."
    );
    assert_eq!(r.notice, "Applies when the camera restarts");
    // No other row carries it.
    assert!(
        groups
            .iter()
            .flat_map(|g| &g.rows)
            .filter(|r| r.id != BATTERY_SAVER)
            .all(|r| r.notice.is_empty())
    );
}

#[test]
fn the_battery_saver_row_is_greyed_unless_video_memory_is_automatic() {
    for (memory, enabled) in [("auto", true), ("cuda", false), ("system", false)] {
        let (_tmp, set) = load(&[(
            MANAGED_REL,
            &format!("[video]\nmemory = \"{memory}\"\nbattery_saver = \"always\"\n"),
        )]);
        let groups = config_schema(&set, &facts());
        let r = find(&groups, BATTERY_SAVER);
        assert_eq!(r.value, ConfigValue::Str("always".into()));
        assert_eq!(r.enabled, enabled, "{memory}");
        assert!(r.editable, "{memory}: still written");
        assert_eq!(
            r.disabled_reason,
            if enabled {
                ""
            } else {
                "Set video processing to automatic to use this"
            },
            "{memory}"
        );
    }
    // An override's reason stays first.
    let (_tmp, set) = load(&[
        (MANAGED_REL, "[video]\nmemory = \"cuda\"\n"),
        (
            "config.d/90-local.toml",
            "[video]\nbattery_saver = \"off\"\n",
        ),
    ]);
    let r = find(&config_schema(&set, &facts()), BATTERY_SAVER).clone();
    assert!(!r.enabled);
    assert_eq!(r.disabled_reason, "Overridden by config.d/90-local.toml");
}

#[test]
fn a_battery_saver_write_is_checked_against_its_options() {
    let info = lookup(BATTERY_SAVER, &RegistryFacts::default()).unwrap();
    assert!(info.editable);
    assert_eq!(info.apply, Apply::Reload);
    assert_eq!(
        to_toml(
            BATTERY_SAVER,
            &info.kind,
            &ConfigValue::Str("power_profile".into())
        ),
        Ok(toml::Value::String("power_profile".into()))
    );
    let e = to_toml(BATTERY_SAVER, &info.kind, &ConfigValue::Str("on".into())).unwrap_err();
    assert!(
        e.contains("\"on\" is not one of: off, auto, on_battery, power_profile, always"),
        "{e}"
    );
}
