//! OpenVINO on the Intel GPU with the video pipeline in system memory
//! (`_host.cuda = false`): the matte is computed on the Intel GPU, and the
//! process opens no NVIDIA device and loads no NVIDIA OpenCL driver, so a
//! runtime-suspended NVIDIA GPU stays suspended. Ignored by default:
//!
//! ```sh
//! ORT_DYLIB_PATH=... OBC_ONNX_MODEL_DIR=... \
//!   cargo test --manifest-path crates/obc-module-onnx/Cargo.toml --test intel_gpu -- --ignored
//! ```
//!
//! Its own test binary: the OpenCL loader reads its configuration once per
//! process, at the first OpenCL call, so no other test may make one first.
//! Without OpenCL for an Intel GPU (or with `OCL_ICD_*` set by the caller)
//! the module leaves the loader alone; the test then only reports.

mod common;

use common::{Log, Nv12, host_chain, model_dir, nv12_of_rgb, open, runtime_env, s, test_image};
use obc_backend::Value;
use obc_module_sdk::abi::OBC_FRAME_FLAG_NEW_SOURCE;
use obc_module_sdk::serde_json::json;

const W: usize = 1280;
const H: usize = 720;

/// The share of luma pixels the composite left as they came (within 2).
fn kept(input: &Nv12, out: &Nv12) -> f64 {
    let mut n = 0usize;
    for y in 0..input.h {
        for x in 0..input.w {
            if (i32::from(input.y(x, y)) - i32::from(out.y(x, y))).abs() <= 2 {
                n += 1;
            }
        }
    }
    n as f64 / (input.w * input.h) as f64
}

/// The NVIDIA device files this process has open.
fn nvidia_fds() -> Vec<String> {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
        .map(|p| p.display().to_string())
        .filter(|p| p.starts_with("/dev/nvidia"))
        .collect()
}

/// The NVIDIA OpenCL driver, when it is mapped into this process.
fn nvidia_opencl_mapped() -> bool {
    std::fs::read_to_string("/proc/self/maps")
        .unwrap()
        .contains("libnvidia-opencl")
}

#[test]
#[ignore = "needs an Intel GPU, ONNX Runtime with OpenVINO and the models"]
fn openvino_gpu_on_host_frames_leaves_the_nvidia_gpu_alone() {
    let (_, models) = match runtime_env() {
        Ok(e) => e,
        Err(why) => {
            eprintln!("skipped: {why}");
            return;
        }
    };
    let tmp = tempfile::tempdir().unwrap();
    let models = model_dir(&models, tmp.path());
    let log = Log::default();
    let module = open(
        tmp.path(),
        json!({"model_dir": models, "_host": {"cuda": false}}),
        &log,
    );
    module.check(&[s("background")]).unwrap();
    let lines = log.take();
    if lines
        .iter()
        .any(|l| l.starts_with("ONNX cut-out: OpenVINO GPU ") && l.contains("not in this"))
    {
        eprintln!("skipped: no OpenVINO in this ONNX Runtime build");
        return;
    }

    let mut inst = module.create_instance().unwrap();
    inst.load(&host_chain(
        W as u32,
        H as u32,
        &[("mode", "blur"), ("engine", "openvino-gpu")],
        None,
    ))
    .unwrap();
    let lines = log.take();
    for l in &lines {
        eprintln!("{l}");
    }
    let on = lines
        .iter()
        .rev()
        .find_map(|l| l.split(" runs on ").nth(1))
        .map(|r| r.split([',', '(']).next().unwrap().trim().to_string())
        .unwrap_or_else(|| panic!("no engine line in {lines:#?}"));
    assert_eq!(on, "OpenVINO GPU", "{lines:#?}");

    let (rgb, iw, ih) = test_image();
    let mut src = nv12_of_rgb(&rgb, iw, ih, W, H, W + 64);
    let input = src.clone();
    let mut dst = src.clone();
    for i in 0..30 {
        let flags = if i == 0 { OBC_FRAME_FLAG_NEW_SOURCE } else { 0 };
        let (fi, fo) = (src.frame(flags), dst.frame(0));
        let strength = Value::Float(1.0);
        // SAFETY: both frames are host memory of this test, valid during
        // the call and touched by nothing else.
        unsafe { inst.process(&fi, &fo, &[("background", "blur_strength", &strength)]) }.unwrap();
    }
    let k = kept(&input, &dst);
    eprintln!(
        "openvino-gpu on host frames: kept {:.1} % of the picture",
        100.0 * k
    );
    assert!(
        (0.03..0.97).contains(&k),
        "the person kept, the room blurred: {k}"
    );

    let fds = nvidia_fds();
    let mapped = nvidia_opencl_mapped();
    eprintln!("NVIDIA device files open: {fds:?}; NVIDIA OpenCL driver mapped: {mapped}");
    let left_alone = std::env::var_os("OCL_ICD_VENDORS").is_some()
        || std::env::var_os("OCL_ICD_FILENAMES").is_some();
    if lines
        .iter()
        .any(|l| l.contains("OpenCL drivers limited to"))
    {
        assert!(fds.is_empty(), "NVIDIA device files open: {fds:?}");
        assert!(!mapped, "the NVIDIA OpenCL driver was loaded");
    } else {
        eprintln!(
            "the OpenCL drivers were not limited (set by the caller: {left_alone}); not asserted"
        );
    }
    // Held open a moment, for an outside look at the process.
    if let Some(ms) = std::env::var("OBC_ONNX_HOLD_MS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}
