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
    }
    for (gate, reason) in [
        (GpuGate::Never, REASON_GPU_NEVER),
        (GpuGate::CameraOff, REASON_GPU_CAMERA),
    ] {
        let d = denoise(&host.settings_json(&snapshot, gate));
        assert_eq!(d["enabled"], false, "{gate:?}: {d}");
        assert_eq!(d["disabled_reason"], reason, "{gate:?}: {d}");
    }

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
