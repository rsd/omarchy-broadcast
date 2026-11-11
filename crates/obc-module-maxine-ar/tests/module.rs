//! The built module library, loaded through `obc-module-host` on a machine
//! that need not have the SDK or a GPU: the manifest is valid, the library
//! loads and passes the ABI checks, the probe reads an SDK tree, the
//! settings pass the host's validation unchanged, and the configuration is
//! checked.

mod support;

use obc_backend::{Availability, StageId};
use obc_module_host::{ModuleStatus, Registry, SettingType, discovery, settings::Op};

const ID: &str = "br.com.dias.maxine-ar";

/// Whether the developer's environment names an SDK root, which takes
/// precedence over the configured one (the probe then reads that).
fn env_root_set() -> bool {
    std::env::var_os("MAXINE_AR_ROOT").is_some_and(|v| !v.is_empty())
}

#[test]
fn the_manifest_is_valid_and_declares_eye_contact_and_auto_frame() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("maxine-ar.toml");
    std::fs::copy(support::crate_dir().join("maxine-ar.toml"), &path).unwrap();
    let m = obc_module_host::Manifest::load(&path).unwrap();
    assert_eq!(m.id, ID);
    assert_eq!(m.name, "NVIDIA Maxine AR effects");
    assert_eq!(m.unloadable_reason(), None);
    let stages: Vec<(&str, &str, i32, u32, &str)> = m
        .stages
        .iter()
        .map(|s| {
            (
                s.id.as_str(),
                s.slot.as_str(),
                s.position,
                s.rank,
                s.device.as_str(),
            )
        })
        .collect();
    assert_eq!(
        stages,
        [
            ("eye-contact", "pre-matte", 10, 300, "nvidia-gpu"),
            ("auto-frame", "pre-matte", 20, 300, "nvidia-gpu")
        ]
    );
    for id in ["eye-contact", "auto-frame"] {
        let s = m.stage(id).unwrap();
        assert_eq!(
            s.frames.iter().map(|f| f.as_str()).collect::<Vec<_>>(),
            ["cuda"],
            "{id}"
        );
        assert_eq!(
            s.formats.iter().map(|f| f.as_str()).collect::<Vec<_>>(),
            ["nv12"],
            "{id}"
        );
        assert!(!s.needs_matte(), "{id}");
    }
}

#[test]
fn the_library_loads_without_the_sdk_and_describes_its_settings() {
    let dir = tempfile::tempdir().unwrap();
    let empty_root = tempfile::tempdir().unwrap();
    let module = support::open(
        dir.path(),
        serde_json::json!({
            "sdk_root": empty_root.path(),
            "_host": {"compute_capability": [12, 0]}
        }),
    );
    let probe = module.probe().unwrap();
    let ids: Vec<&str> = probe.iter().map(|(s, _)| s.as_str()).collect();
    assert_eq!(ids, ["eye-contact", "auto-frame"]);
    if !env_root_set() {
        for (_, a) in &probe {
            assert_eq!(
                *a,
                Availability::NotInstalled("Maxine AR SDK not installed".into())
            );
        }
    }

    let settings = module.settings().unwrap();
    let groups: Vec<(&str, &str, usize)> = settings
        .groups
        .iter()
        .map(|g| (g.id.as_str(), g.label.as_str(), g.settings.len()))
        .collect();
    assert_eq!(
        groups,
        [
            ("eye_contact", "Eye contact", 2),
            ("auto_frame", "Auto-framing", 2)
        ]
    );
    let enabled = settings.enable_of("eye-contact").unwrap();
    assert_eq!(enabled.id, "effect.eye_contact.enabled");
    assert_eq!(enabled.label, "Eye contact");
    assert_eq!(enabled.kind, SettingType::Bool { default: false });
    let look = settings.param("eye-contact", "look_away").unwrap();
    assert_eq!(look.id, "effect.eye_contact.look_away");
    assert_eq!(look.label, "Natural look-away");
    assert_eq!(look.kind, SettingType::Bool { default: false });
    assert_eq!(look.apply, Some(obc_module_host::settings::Apply::Reload));
    assert_eq!(look.enabled_when.len(), 1);
    let cond = &look.enabled_when[0];
    assert_eq!(
        (cond.id.as_str(), cond.op, &cond.value),
        (
            "effect.eye_contact.enabled",
            Op::Eq,
            &serde_json::json!(true)
        )
    );
    assert_eq!(
        look.disabled_reason.as_deref(),
        Some("Turn on eye contact to change this")
    );
    let auto = settings.enable_of("auto-frame").unwrap();
    assert_eq!(auto.id, "effect.auto_frame.enabled");
    assert_eq!(auto.label, "Auto-framing");
    assert_eq!(auto.kind, SettingType::Bool { default: false });
    let framing = settings.param("auto-frame", "framing").unwrap();
    assert_eq!(framing.id, "effect.auto_frame.framing");
    assert_eq!(framing.label, "Framing");
    assert_eq!(
        framing.kind,
        SettingType::Enum {
            options: [("tight", "Tight"), ("medium", "Medium"), ("wide", "Wide")]
                .map(|(v, l)| (v.to_string(), l.to_string()))
                .to_vec(),
            default: "medium".into(),
        }
    );
    assert_eq!(framing.apply, Some(obc_module_host::settings::Apply::Live));
    assert_eq!(framing.enabled_when.len(), 1);
    let cond = &framing.enabled_when[0];
    assert_eq!(
        (cond.id.as_str(), cond.op, &cond.value),
        (
            "effect.auto_frame.enabled",
            Op::Eq,
            &serde_json::json!(true)
        )
    );
    assert_eq!(
        framing.disabled_reason.as_deref(),
        Some("Turn on auto-framing to change this")
    );
    // No setting of the daemon's own background group.
    assert!(
        settings
            .settings()
            .all(|s| !s.id.starts_with("effect.background"))
    );

    // The deeper check opens the SDK: there is none.
    if !env_root_set() {
        for stage in ["eye-contact", "auto-frame"] {
            let e = module.check(&[StageId::new(stage).unwrap()]).unwrap_err();
            println!("check {stage}: {e}");
        }
    }
    // An instance can be made without the SDK: nothing is opened yet.
    let mut instance = module.create_instance().unwrap();
    instance.unload();
}

#[test]
fn the_configuration_refuses_unknown_keys_and_accepts_the_host_keys() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = support::installed_manifest(dir.path());
    let (sink, _) = support::capture();
    let e = match obc_module_host::Module::open(
        &manifest,
        &serde_json::json!({"sdk-root": "/opt"}),
        sink.clone(),
    ) {
        Ok(_) => panic!("an unknown key was accepted"),
        Err(e) => e.to_string(),
    };
    assert!(e.contains("sdk-root"), "{e}");
    for config in [
        serde_json::json!({}),
        serde_json::json!({"_host": {"compute_capability": null}}),
        serde_json::json!({"_host": {"compute_capability": [8, 6], "later": true}}),
        serde_json::json!({"sdk_root": "", "_host": {}}),
    ] {
        obc_module_host::Module::open(&manifest, &config, sink.clone())
            .unwrap_or_else(|e| panic!("{config}: {e}"));
    }
}

#[test]
fn the_registry_offers_the_stage_with_the_probes_reason() {
    let dir = tempfile::tempdir().unwrap();
    support::installed_manifest(dir.path());
    let dirs = discovery::dirs_from(None, None, &[dir.path().to_path_buf()], None);
    let mut registry = Registry::new(discovery::scan(&dirs));
    let (sink, _) = support::capture();
    let empty_root = tempfile::tempdir().unwrap();
    registry.open_all(
        |_| serde_json::json!({"sdk_root": empty_root.path()}),
        &sink,
    );
    let order: Vec<String> = registry.order().iter().map(|s| s.to_string()).collect();
    assert_eq!(order, ["eye-contact", "auto-frame"]);
    if !env_root_set() {
        assert_eq!(
            registry.status(ID),
            Some(ModuleStatus::Unavailable(
                "Maxine AR SDK not installed".into()
            ))
        );
        for stage in ["eye-contact", "auto-frame"] {
            assert_eq!(
                registry.resolve(stage).reason.as_deref(),
                Some("Maxine AR SDK not installed"),
                "{stage}"
            );
        }
    }
}
