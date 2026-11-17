//! The module host on a real module library, without the worker: the
//! `GetSettings` document under each GPU gate (see `common` for the
//! fixture module).

mod common;

use common::FIXTURE_ID;
use obc_core::config::Config;
use omarchy_broadcast_audiod::host::{
    GpuGate, Host, REASON_GPU_CAMERA, REASON_GPU_NEVER, REASON_GPU_NO_RECORDER,
};
use omarchy_broadcast_audiod::snapshot::Snapshot;
use serde_json::json;

/// The `mic.fixture.denoise` switch of the settings document.
fn denoise(settings: &serde_json::Value) -> serde_json::Value {
    settings["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["settings"].as_array().unwrap())
        .find(|s| s["id"] == "mic.fixture.denoise")
        .unwrap_or_else(|| panic!("no mic.fixture.denoise: {settings}"))
        .clone()
}

/// The worker's `disabled_kind` tokens are its copy of the settings
/// channel's: each one is a kind the daemon knows, with the same name.
#[test]
fn the_worker_s_kinds_are_the_settings_channel_s() {
    use obc_ipc::settings::disabled;
    use omarchy_broadcast_audiod::host::disabled_kind as k;
    for (ours, theirs) in [
        (k::PARENT_OFF, disabled::PARENT_OFF),
        (k::NOT_APPLICABLE, disabled::NOT_APPLICABLE),
        (k::OTHER_PROVIDER, disabled::OTHER_PROVIDER),
        (k::UNAVAILABLE, disabled::UNAVAILABLE),
        (k::BUSY, disabled::BUSY),
    ] {
        assert_eq!(ours, theirs);
        assert!(disabled::KINDS.contains(&ours), "{ours}");
    }
}

/// The transient gates (nobody records, the graph not known yet) never
/// grey a GPU-only stage's settings: the user may configure it before an
/// application records. The policy gates still do; the status keeps the
/// reason of each.
#[test]
fn only_the_gpu_policy_greys_a_gpu_stage_s_settings() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("fixture.so");
    std::fs::copy(common::fixture_lib(), &lib).unwrap();
    let modules = dir.path().join("modules");
    std::fs::create_dir_all(&modules).unwrap();
    std::fs::write(
        modules.join("fixture.toml"),
        common::fixture_manifest(&lib, "").replace("device = \"cpu\"", "device = \"nvidia-gpu\""),
    )
    .unwrap();
    let snapshot = Snapshot::parse(
        &json!({ "version": 1, "enabled": true, "values": { "mic.fixture.gain": 100 } })
            .to_string(),
    )
    .unwrap();
    let host = Host::load(
        Config::default(),
        &[modules],
        None,
        &snapshot,
        obc_module_host::stderr_log(),
    );

    for gate in [GpuGate::Allowed, GpuGate::NoRecorder, GpuGate::Pending] {
        let d = denoise(&host.settings_json(&snapshot, gate));
        assert_eq!(d["enabled"], true, "{gate:?}: {d}");
        assert_eq!(d["disabled_kind"], "", "{gate:?}: {d}");
    }
    for (gate, reason) in [
        (GpuGate::Never, REASON_GPU_NEVER),
        (GpuGate::CameraOff, REASON_GPU_CAMERA),
    ] {
        let d = denoise(&host.settings_json(&snapshot, gate));
        assert_eq!(d["enabled"], false, "{gate:?}: {d}");
        assert_eq!(d["disabled_reason"], reason, "{gate:?}: {d}");
        assert_eq!(d["disabled_kind"], "not_applicable", "{gate:?}: {d}");
    }
    // Processing off as well: the reason is that, for now.
    let off = Snapshot::parse(&json!({ "version": 1, "enabled": false }).to_string()).unwrap();
    let d = denoise(&host.settings_json(&off, GpuGate::Never));
    assert_eq!(d["enabled"], false, "{d}");
    assert_eq!(d["disabled_kind"], "busy", "{d}");

    // The plan (and so the status) keeps the transient reason.
    let plan = host.plan(&snapshot, GpuGate::NoRecorder);
    let stage = plan.stages.iter().find(|s| s.id == "mic-denoise").unwrap();
    assert_eq!(stage.provider, None);
    assert_eq!(stage.reason, REASON_GPU_NO_RECORDER);
    assert_eq!(
        host.settings_json(&snapshot, GpuGate::NoRecorder)["groups"][0]["module"],
        FIXTURE_ID
    );
}

/// A stage another one that is on turns off: its switch is greyed with the
/// conflict, kind `busy` (it comes back when the other one is turned off),
/// and the kind of a stage no module can run is `unavailable`.
#[test]
fn a_conflict_greys_the_switch_as_busy_and_a_missing_provider_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("fixture.so");
    std::fs::copy(common::fixture_lib(), &lib).unwrap();
    let modules = dir.path().join("modules");
    std::fs::create_dir_all(&modules).unwrap();
    std::fs::write(
        modules.join("fixture.toml"),
        common::fixture_manifest(&lib, "conflicts = [\"mic-denoise\"]"),
    )
    .unwrap();
    let snapshot = Snapshot::parse(&json!({ "version": 1, "enabled": true }).to_string()).unwrap();
    let host = Host::load(
        Config::default(),
        std::slice::from_ref(&modules),
        None,
        &snapshot,
        obc_module_host::stderr_log(),
    );
    let d = denoise(&host.settings_json(&snapshot, GpuGate::Allowed));
    assert_eq!(d["enabled"], false, "{d}");
    assert_eq!(d["disabled_kind"], "busy", "{d}");
    assert!(
        d["disabled_reason"]
            .as_str()
            .unwrap()
            .starts_with("Cannot run together with"),
        "{d}"
    );

    // Forced: no conflict, enabled.
    let forced = Snapshot::parse(
        &json!({ "version": 1, "enabled": true, "forced_conflicts": true }).to_string(),
    )
    .unwrap();
    let d = denoise(&host.settings_json(&forced, GpuGate::Allowed));
    assert_eq!(
        (d["enabled"].clone(), d["disabled_kind"].clone()),
        (json!(true), json!(""))
    );

    // A requirement no module meets: no provider, the registry's reason.
    std::fs::write(
        modules.join("fixture.toml"),
        common::fixture_manifest(&lib, "").replace(
            "id = \"mic-denoise\"\n",
            "id = \"mic-denoise\"\nrequires = [\"mic-absent\"]\n",
        ),
    )
    .unwrap();
    let host = Host::load(
        Config::default(),
        &[modules],
        None,
        &snapshot,
        obc_module_host::stderr_log(),
    );
    let d = denoise(&host.settings_json(&snapshot, GpuGate::Allowed));
    assert_eq!(d["enabled"], false, "{d}");
    assert_eq!(d["disabled_kind"], "unavailable", "{d}");
}

/// A condition that does not hold greys a setting with the module's
/// reason, kind `parent_off`; it is enabled, kind `""`, once it holds.
#[test]
fn a_condition_that_does_not_hold_is_parent_off() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("hello.so");
    std::fs::copy(common::hello_lib(), &lib).unwrap();
    let modules = dir.path().join("modules");
    std::fs::create_dir_all(&modules).unwrap();
    std::fs::write(modules.join("hello.toml"), common::hello_manifest(&lib)).unwrap();
    let gain = |settings: &serde_json::Value| {
        settings["groups"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["settings"].as_array().unwrap())
            .find(|s| s["id"] == "mic.x_example_hello_audio.gain")
            .unwrap_or_else(|| panic!("no gain: {settings}"))
            .clone()
    };
    let snapshot = |on: bool| {
        Snapshot::parse(
            &json!({
                "version": 1,
                "enabled": true,
                "values": { "mic.x_example_hello_audio.enabled": on },
            })
            .to_string(),
        )
        .unwrap()
    };
    let host = Host::load(
        Config::default(),
        &[modules],
        None,
        &snapshot(false),
        obc_module_host::stderr_log(),
    );
    let off = gain(&host.settings_json(&snapshot(false), GpuGate::Allowed));
    assert_eq!(off["enabled"], false, "{off}");
    assert_eq!(off["disabled_kind"], "parent_off", "{off}");
    assert_eq!(
        off["disabled_reason"],
        "Turn on the voice gain to change this"
    );
    let on = gain(&host.settings_json(&snapshot(true), GpuGate::Allowed));
    assert_eq!(
        (on["enabled"].clone(), on["disabled_kind"].clone()),
        (json!(true), json!(""))
    );
}

/// A stage another module runs: the other's settings of it are greyed,
/// `Handled by <name> instead`, kind `other_provider`.
#[test]
fn a_stage_another_module_runs_is_other_provider() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("fixture.so");
    std::fs::copy(common::fixture_lib(), &lib).unwrap();
    let modules = dir.path().join("modules");
    std::fs::create_dir_all(&modules).unwrap();
    std::fs::write(
        modules.join("fixture.toml"),
        common::fixture_manifest(&lib, ""),
    )
    .unwrap();
    std::fs::write(
        modules.join("second.toml"),
        common::fixture_manifest(&lib, "")
            .replace(FIXTURE_ID, "org.example.second-audio")
            .replace("Audio fixture", "Second fixture")
            .replace("rank = 200", "rank = 100"),
    )
    .unwrap();
    let snapshot = Snapshot::parse(&json!({ "version": 1, "enabled": true }).to_string()).unwrap();
    let host = Host::load(
        Config::default(),
        &[modules],
        None,
        &snapshot,
        obc_module_host::stderr_log(),
    );
    let doc = host.settings_json(&snapshot, GpuGate::Allowed);
    let of = |module: &str| {
        doc["groups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["module"] == module)
            .and_then(|g| {
                g["settings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|s| s["id"] == "mic.fixture.denoise")
                    .cloned()
            })
            .unwrap_or_else(|| panic!("no denoise of {module}: {doc}"))
    };
    let runs = of(FIXTURE_ID);
    assert_eq!(
        (runs["enabled"].clone(), runs["disabled_kind"].clone()),
        (json!(true), json!(""))
    );
    let other = of("org.example.second-audio");
    assert_eq!(other["enabled"], false, "{other}");
    assert_eq!(other["disabled_kind"], "other_provider", "{other}");
    assert_eq!(other["disabled_reason"], "Handled by Audio fixture instead");
}
