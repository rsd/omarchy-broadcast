//! The built module library, loaded through `obc-module-host` on a machine
//! that need not have the SDK or a GPU: the manifest is valid, the library
//! loads and passes the ABI checks, the probe reads an SDK tree and the
//! settings pass the host's validation unchanged.

mod support;

use obc_backend::{Availability, StageId};
use obc_module_host::{ModuleStatus, Registry, SettingType, discovery};

#[test]
fn the_manifest_is_valid_and_declares_the_maxine_stages() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("maxine-vfx.toml");
    std::fs::copy(support::crate_dir().join("maxine-vfx.toml"), &path).unwrap();
    let m = obc_module_host::Manifest::load(&path).unwrap();
    assert_eq!(m.id, obc_module_maxine_id());
    assert_eq!(m.unloadable_reason(), None);
    let stages: Vec<(&str, &str, i32, u32)> = m
        .stages
        .iter()
        .map(|s| (s.id.as_str(), s.slot.as_str(), s.position, s.rank))
        .collect();
    assert_eq!(
        stages,
        [
            ("enhance", "pre-matte", 0, 300),
            ("denoise", "pre-matte", 100, 300),
            ("background", "matte", 100, 300),
            ("relight", "post-matte", 100, 300),
        ]
    );
    assert!(m.stage("relight").unwrap().needs_matte());
    assert!(m.stages.iter().all(|s| s.device.as_str() == "nvidia-gpu"));
    let enhance = m.stage("enhance").unwrap();
    assert_eq!(
        enhance
            .formats
            .iter()
            .map(|f| f.as_str())
            .collect::<Vec<_>>(),
        ["nv12", "i420"]
    );
}

fn obc_module_maxine_id() -> &'static str {
    "br.com.dias.maxine-vfx"
}

#[test]
fn the_library_loads_without_the_sdk_and_describes_its_settings() {
    let dir = tempfile::tempdir().unwrap();
    let empty_root = tempfile::tempdir().unwrap();
    let (module, _lines) = support::open(
        dir.path(),
        serde_json::json!({
            "sdk_root": empty_root.path(),
            "_host": {"compute_capability": [12, 0]}
        }),
    );
    let probe = module.probe().unwrap();
    let ids: Vec<&str> = probe.iter().map(|(s, _)| s.as_str()).collect();
    assert_eq!(
        ids,
        ["enhance", "denoise", "background", "relight"],
        "manifest order"
    );
    if std::env::var_os("MAXINE_VFX_ROOT").is_none_or(|v| v.is_empty()) {
        for (s, a) in &probe {
            assert_eq!(
                *a,
                Availability::NotInstalled("Not installed".into()),
                "{s}"
            );
        }
    }

    let settings = module.settings().unwrap();
    let groups: Vec<(&str, &str, usize)> = settings
        .groups
        .iter()
        .map(|g| (g.id.as_str(), g.label.as_str(), g.settings.len()))
        .collect();
    assert_eq!(groups, [("video", "Video", 2), ("lighting", "Lighting", 4)]);
    assert_eq!(
        settings.enable_of("denoise").unwrap().id,
        "effect.video.denoise"
    );
    assert_eq!(
        settings.enable_of("enhance").unwrap().id,
        "effect.video.enhance"
    );
    assert_eq!(
        settings.enable_of("relight").unwrap().id,
        "effect.lighting.relight"
    );
    let direction = settings.param("relight", "direction").unwrap();
    assert_eq!(
        direction.kind,
        SettingType::Int {
            min: -90,
            max: 90,
            step: 5,
            default: 0,
            unit: Some("°".into())
        }
    );
    assert!(settings.param("relight", "light").is_some());
    assert!(settings.param("relight", "brightness").is_some());
    // An instance can be made without the SDK: nothing is opened yet.
    let mut instance = module.create_instance().unwrap();
    instance.unload();
}

#[test]
fn the_registry_offers_the_stages_in_the_rev8_order() {
    let dir = tempfile::tempdir().unwrap();
    let lib = support::module_library();
    support::installed_manifest(dir.path(), &lib);
    let dirs = discovery::dirs_from(None, None, &[dir.path().to_path_buf()], None);
    let mut registry = Registry::new(discovery::scan(&dirs));
    let (sink, _) = support::capture();
    let empty_root = tempfile::tempdir().unwrap();
    registry.open_all(
        |_| serde_json::json!({"sdk_root": empty_root.path()}),
        &sink,
    );
    let order: Vec<String> = registry.order().iter().map(|s| s.to_string()).collect();
    assert_eq!(order, ["enhance", "denoise", "background", "relight"]);
    if std::env::var_os("MAXINE_VFX_ROOT").is_none_or(|v| v.is_empty()) {
        assert_eq!(
            registry.status(obc_module_maxine_id()),
            Some(ModuleStatus::Unavailable("Not installed".into()))
        );
        assert_eq!(
            registry
                .resolve(StageId::DENOISE.as_str())
                .reason
                .as_deref(),
            Some("Not installed")
        );
    }
}
