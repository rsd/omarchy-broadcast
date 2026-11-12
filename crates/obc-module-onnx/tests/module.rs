//! The module as the daemon sees it, without ONNX Runtime: its manifest,
//! then its library opened by `obc-module-host`, its settings and its
//! probe when no runtime is there.

mod common;

use common::{ID, Log, crate_dir, open};
use obc_backend::Availability;
use obc_module_host::settings::Apply;
use obc_module_host::{
    Device, Frames, Manifest, ModuleKind, ModuleStatus, Registry, SettingType, Slot, scan,
};
use obc_module_onnx::runtime::{self, LibrarySource, REASON_NOT_INSTALLED};
use obc_module_sdk::serde_json::json;

#[test]
fn the_manifest_declares_a_cpu_matte_stage_ranked_below_maxine() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("onnx.toml");
    std::fs::copy(crate_dir().join("onnx.toml"), &path).unwrap();
    let m = Manifest::load(&path).unwrap();
    assert_eq!(m.id, ID);
    assert_eq!(m.kind, ModuleKind::VideoStage);
    assert_eq!(m.version, env!("CARGO_PKG_VERSION"), "bump the manifest");
    assert_eq!(m.stages.len(), 1);
    let st = m.stage("background").unwrap();
    assert_eq!(
        (st.slot, st.rank, st.device),
        (Slot::Matte, 200, Device::Cpu)
    );
    // CUDA first: what the effect element hands the provider today.
    assert_eq!(st.frames, [Frames::Cuda, Frames::Host]);
    assert_eq!(
        st.formats.iter().map(|f| f.as_str()).collect::<Vec<_>>(),
        ["nv12"]
    );
    // Below the NVIDIA module's cut-out, so the quality profile keeps it.
    let maxine = crate_dir().join("../obc-module-maxine/maxine-vfx.toml");
    if maxine.is_file() {
        let mpath = dir.path().join("maxine-vfx.toml");
        std::fs::copy(&maxine, &mpath).unwrap();
        let mx = Manifest::load(&mpath).unwrap();
        assert!(mx.stage("background").unwrap().rank > st.rank);
    }
}

#[test]
fn the_settings_are_the_model_and_the_engine_of_the_cut_out() {
    let dir = tempfile::tempdir().unwrap();
    let module = open(dir.path(), json!({}), &Log::default());
    let d = module.settings().unwrap();
    let groups: Vec<(&str, &str)> = d
        .groups
        .iter()
        .map(|g| (g.id.as_str(), g.label.as_str()))
        .collect();
    assert_eq!(groups, [("onnx", "Open-model cut-out")]);
    assert!(
        d.enable_of("background").is_none(),
        "the daemon's mode is the switch"
    );
    let model = d.param("background", "model").unwrap();
    assert_eq!(model.id, "effect.onnx.model");
    assert_eq!(model.apply, Some(Apply::Reload));
    match &model.kind {
        SettingType::Enum { options, default } => {
            assert_eq!(default, "mediapipe-landscape");
            let values: Vec<&str> = options.iter().map(|(v, _)| v.as_str()).collect();
            assert_eq!(
                values,
                [
                    "mediapipe-landscape",
                    "pphumanseg-v2-mobile",
                    "pphumanseg-v2-lite",
                    "modnet"
                ]
            );
        }
        other => panic!("{other:?}"),
    }
    let engine = d.param("background", "engine").unwrap();
    assert_eq!(engine.id, "effect.onnx.engine");
    assert_eq!(engine.apply, Some(Apply::Reload));
    match &engine.kind {
        SettingType::Enum { options, default } => {
            assert_eq!(default, "auto");
            assert_eq!(options.len(), 7);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn without_a_runtime_the_stage_is_unavailable_with_the_reason() {
    if std::env::var_os("ORT_DYLIB_PATH").is_some_and(|v| !v.is_empty()) {
        eprintln!("skipped: ORT_DYLIB_PATH is set, and it wins over the configured library");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nowhere/libonnxruntime.so");
    let module = open(dir.path(), json!({"ort_library": missing}), &Log::default());
    let probe = module.probe().unwrap();
    match &probe[0].1 {
        Availability::NotInstalled(r) => {
            assert!(
                r.starts_with("ONNX Runtime not installed (libonnxruntime.so not found"),
                "{r}"
            );
            assert!(r.contains(&missing.display().to_string()), "{r}");
        }
        other => panic!("{other:?}"),
    }
    // From the system path, when this machine has none: the documented
    // reason, word for word, and the module is listed with it.
    if runtime::find_library(&LibrarySource::System, &runtime::loader_dirs()).is_err() {
        let other = tempfile::tempdir().unwrap();
        let manifest = common::install(other.path());
        let mut reg = Registry::new(scan(&[other.path().to_path_buf()]));
        reg.open_all(|_| json!({}), &Log::default().sink());
        match reg.status(ID) {
            Some(ModuleStatus::Unavailable(r)) => assert!(r.contains(REASON_NOT_INSTALLED), "{r}"),
            other => panic!("{other:?}"),
        }
        // The same library file: never copied again while it is mapped.
        let module =
            obc_module_host::Module::open(&manifest, &json!({}), Log::default().sink()).unwrap();
        assert_eq!(
            module.probe().unwrap()[0].1,
            Availability::NotInstalled(REASON_NOT_INSTALLED.into())
        );
    } else {
        eprintln!("the system has an ONNX Runtime: the system-path case is not checked");
    }
}

#[test]
fn a_bad_configuration_makes_the_module_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = common::install(dir.path());
    let e = obc_module_host::Module::open(&manifest, &json!({"threads": 0}), Log::default().sink())
        .unwrap_err();
    assert!(e.contains("threads"), "{e}");
}

/// With `_host.cuda = false` the module makes no CUDA call: `auto` skips
/// TensorRT and CUDA with the reason, an explicit `cuda` or `tensorrt`
/// fails the load with it, and the CPU runs. Needs the runtime and models
/// of the environment (`ORT_DYLIB_PATH`, `OBC_ONNX_MODEL_DIR`).
#[test]
fn with_cuda_off_in_the_host_configuration_no_cuda_engine_is_tried() {
    const OFF: &str = "CUDA is off: the video pipeline runs in system memory";
    let Ok((_, models)) = common::runtime_env() else {
        eprintln!("skipped: ORT_DYLIB_PATH and OBC_ONNX_MODEL_DIR are not both set");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let model_dir = common::model_dir(&models, dir.path());
    let log = Log::default();
    let module = open(
        dir.path(),
        json!({"model_dir": model_dir, "_host": {"cuda": false}}),
        &log,
    );
    module.check(&[common::s("background")]).unwrap();
    let lines = log.take();
    for engine in ["NVIDIA TensorRT", "NVIDIA CUDA"] {
        let line = lines
            .iter()
            .find(|l| l.starts_with(&format!("ONNX cut-out: {engine} ")))
            .unwrap_or_else(|| panic!("no {engine} line in {lines:#?}"));
        assert!(line.contains(OFF), "{line}");
    }
    let mut inst = module.create_instance().unwrap();
    for engine in ["cuda", "tensorrt"] {
        let e = inst
            .load(&common::host_chain(
                1280,
                720,
                &[("mode", "blur"), ("engine", engine)],
                None,
            ))
            .unwrap_err();
        assert!(e.message.contains(OFF), "{engine}: {e:?}");
        assert!(!inst.is_loaded());
    }
    log.take();
    inst.load(&common::host_chain(
        1280,
        720,
        &[("mode", "blur"), ("engine", "auto")],
        None,
    ))
    .unwrap();
    let lines = log.take();
    assert!(
        lines.iter().all(|l| !l.contains("runs on NVIDIA")),
        "{lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("NVIDIA TensorRT unavailable") && l.contains(OFF)),
        "{lines:#?}"
    );
}
