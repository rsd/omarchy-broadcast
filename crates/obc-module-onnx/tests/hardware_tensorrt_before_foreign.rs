//! The reverse of `hardware_foreign_tensorrt`: the cut-out starts on
//! TensorRT first, then another TensorRT enters the process's global scope
//! (the Maxine video effects run after the cut-out, in the same daemon),
//! and the cut-out starts again. ONNX Runtime's TensorRT provider is
//! loaded by then and bound to its own TensorRT, so it is not ruled out
//! and runs on TensorRT again. Ignored by default:
//!
//! ```sh
//! ORT_DYLIB_PATH=/usr/lib/libonnxruntime.so.1 \
//! OBC_ONNX_MODEL_DIR=/usr/share/omarchy-onnx-models \
//! OBC_FOREIGN_TENSORRT=/opt/nvidia/maxine/videofx/external/tensorrt/lib/libnvinfer.so.10.9.0 \
//!   cargo test --manifest-path crates/obc-module-onnx/Cargo.toml \
//!   --test hardware_tensorrt_before_foreign -- --ignored --nocapture
//! ```
//!
//! `OBC_FOREIGN_TENSORRT` defaults to the Maxine video effects SDK's
//! TensorRT; without it, or where TensorRT does not start at all, the test
//! is skipped.

mod common;

use common::{Log, host_chain, model_dir, open, runtime_env, s};
use obc_module_sdk::serde_json::json;

const MAXINE_TENSORRT: &str =
    "/opt/nvidia/maxine/videofx/external/tensorrt/lib/libnvinfer.so.10.9.0";

#[test]
#[ignore = "needs an NVIDIA GPU, ONNX Runtime with TensorRT, the models and another TensorRT"]
fn a_tensorrt_started_before_another_one_is_loaded_starts_again() {
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
    let tmp = tempfile::tempdir().unwrap();
    let models = model_dir(&models, tmp.path());
    let log = Log::default();
    let module = open(tmp.path(), json!({"model_dir": models}), &log);
    module.probe().unwrap();
    module.check(&[s("background")]).unwrap();
    log.take();
    let chain = host_chain(256, 144, &[("mode", "blur"), ("engine", "tensorrt")], None);

    let mut inst = module.create_instance().unwrap();
    let report = inst.load(&chain).unwrap();
    if report.notice.is_some() {
        eprintln!("skipped: TensorRT does not start here: {:?}", report.notice);
        return;
    }
    drop(inst);
    log.take();

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

    let mut inst = module.create_instance().unwrap();
    let report = inst
        .load(&chain)
        .unwrap_or_else(|e| panic!("the second load failed: {e:?}"));
    let lines = log.take();
    eprintln!("{lines:#?}\n{report:?}");
    assert_eq!(report.notice, None, "{lines:#?}");
    assert!(
        lines
            .iter()
            .any(|l| l.contains(" runs on NVIDIA TensorRT ")),
        "{lines:#?}"
    );
}
