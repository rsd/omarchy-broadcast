//! Another TensorRT already in the process's global scope: the daemon's
//! case after the Maxine video effects ran (their SDK's `libnvinfer.so.10`,
//! loaded with `RTLD_GLOBAL` by the Maxine module's preload) and the
//! cut-out then starts on TensorRT in the same process. ONNX Runtime's
//! TensorRT provider, linked to `libnvinfer.so.11`, would bind its
//! TensorRT entry points (unversioned symbols) to the global one, get no
//! builder from it and crash (`SIGSEGV`); the module rules TensorRT out
//! instead, and the load runs on CUDA with the reason. Ignored by default:
//!
//! ```sh
//! ORT_DYLIB_PATH=/usr/lib/libonnxruntime.so.1 \
//! OBC_ONNX_MODEL_DIR=/usr/share/omarchy-onnx-models \
//! OBC_FOREIGN_TENSORRT=/opt/nvidia/maxine/videofx/external/tensorrt/lib/libnvinfer.so.10.9.0 \
//!   cargo test --manifest-path crates/obc-module-onnx/Cargo.toml \
//!   --test hardware_foreign_tensorrt -- --ignored --nocapture
//! ```
//!
//! `OBC_FOREIGN_TENSORRT` defaults to the Maxine video effects SDK's
//! TensorRT (another major than the provider's); without it, or with an
//! ONNX Runtime that has no TensorRT provider, the test is skipped.

mod common;

use common::{Log, host_chain, model_dir, open, runtime_env, s};
use obc_module_sdk::serde_json::json;

const MAXINE_TENSORRT: &str =
    "/opt/nvidia/maxine/videofx/external/tensorrt/lib/libnvinfer.so.10.9.0";

#[test]
#[ignore = "needs an NVIDIA GPU, ONNX Runtime with TensorRT, the models and another TensorRT"]
fn another_tensorrt_in_the_process_rules_tensorrt_out_instead_of_crashing() {
    let (_, models) = match runtime_env() {
        Ok(e) => e,
        Err(why) => {
            eprintln!("skipped: {why}");
            return;
        }
    };
    let foreign = std::env::var("OBC_FOREIGN_TENSORRT").unwrap_or(MAXINE_TENSORRT.to_string());
    if !std::path::Path::new(&foreign).is_file() {
        eprintln!("skipped: {foreign} not found");
        return;
    }
    // As the Maxine module's preload opens its SDK's TensorRT.
    // SAFETY: TensorRT's initialisers have no preconditions. The library
    // is kept for the process, as the daemon keeps it.
    let lib = unsafe {
        libloading::os::unix::Library::open(
            Some(&foreign),
            libloading::os::unix::RTLD_LAZY | libloading::os::unix::RTLD_GLOBAL,
        )
    }
    .unwrap_or_else(|e| panic!("{foreign} cannot be loaded: {e}"));
    std::mem::forget(lib);

    let tmp = tempfile::tempdir().unwrap();
    let models = model_dir(&models, tmp.path());
    let log = Log::default();
    let module = open(tmp.path(), json!({"model_dir": models}), &log);
    module.probe().unwrap();
    module.check(&[s("background")]).unwrap();
    let lines = log.take();
    if !lines
        .iter()
        .any(|l| l.contains("NVIDIA TensorRT available in this ONNX Runtime build"))
    {
        eprintln!("skipped: this ONNX Runtime has no TensorRT provider: {lines:#?}");
        return;
    }

    let mut inst = module.create_instance().unwrap();
    let report = inst
        .load(&host_chain(
            256,
            144,
            &[("mode", "blur"), ("engine", "tensorrt")],
            None,
        ))
        .unwrap_or_else(|e| panic!("the load failed: {e:?}"));
    let lines = log.take();
    eprintln!("{lines:#?}\n{report:?}");
    let notice = report.notice.expect("TensorRT is ruled out, with a notice");
    assert!(
        notice.contains("is not the one ONNX Runtime's TensorRT provider uses"),
        "TensorRT was not ruled out for the other TensorRT: {notice}"
    );
    assert!(
        notice.starts_with("NVIDIA TensorRT was chosen and cannot run: "),
        "{notice}"
    );
    assert!(notice.contains(&foreign), "{notice}");
    assert!(notice.ends_with("; using NVIDIA CUDA"), "{notice}");
}
