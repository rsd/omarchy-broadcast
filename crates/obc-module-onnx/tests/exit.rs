//! A process that ran a model on OpenVINO exits cleanly: the module keeps
//! ONNX Runtime's environment for the life of the process (`RUNTIME` in
//! `src/runtime.rs`), because releasing it after OpenVINO ran crashes the
//! process at exit (`SIGSEGV`, ONNX Runtime 1.29 with OpenVINO 2026.4).
//!
//! The crash happens after the test has returned, so the test runs itself
//! again in a child process (`OBC_ONNX_EXIT_CHILD=1`): the child opens the
//! module through `obc-module-host` as the daemon does, runs a frame on
//! each OpenVINO engine that loads (the CPU one must), unloads, drops the
//! instance and the module and returns; the parent checks that the child
//! exited with status 0, not by a signal.
//!
//! Needs `ORT_DYLIB_PATH` and `OBC_ONNX_MODEL_DIR`, and OpenVINO in the
//! runtime's build; skipped, with the reason printed, otherwise.

mod common;

use std::io::Read;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use common::{Log, host_chain, model_dir, nv12_of_rgb, open, runtime_env, s, test_image};
use obc_backend::Value;
use obc_module_sdk::abi::OBC_FRAME_FLAG_NEW_SOURCE;
use obc_module_sdk::serde_json::json;

const CHILD: &str = "OBC_ONNX_EXIT_CHILD";
const TEST: &str = "a_process_that_ran_openvino_exits_cleanly";
/// How long the child may run: a first OpenVINO GPU or NPU compile takes
/// tens of seconds; a child past this hangs, and is killed.
const CHILD_DEADLINE: Duration = Duration::from_secs(120);

#[test]
fn a_process_that_ran_openvino_exits_cleanly() {
    let (lib, models) = match runtime_env() {
        Ok(e) => e,
        Err(why) => {
            eprintln!("skipped: {why}");
            return;
        }
    };
    let openvino = lib
        .parent()
        .unwrap()
        .join("libonnxruntime_providers_openvino.so");
    if !openvino.is_file() {
        eprintln!(
            "skipped: OpenVINO is not in this ONNX Runtime build (no {})",
            openvino.display()
        );
        return;
    }
    if std::env::var_os(CHILD).is_some() {
        child(&models);
        return;
    }
    let out = run_child();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    eprintln!("the child's output:\n{stdout}{stderr}");
    assert!(
        stdout.contains("test result: ok. 1 passed"),
        "the child did not run the test"
    );
    assert_eq!(
        out.status.signal(),
        None,
        "the child was killed by a signal at exit: {:?}",
        out.status
    );
    assert!(out.status.success(), "the child failed: {:?}", out.status);
}

/// Runs this test again in a child process, killed past
/// [`CHILD_DEADLINE`] (which fails the test with the child's output).
fn run_child() -> Output {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([TEST, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run the test binary again");
    // Read on threads, so that a full pipe does not stall the child.
    let drain = |mut r: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = r.read_to_end(&mut buf);
            buf
        })
    };
    let stdout = drain(Box::new(child.stdout.take().unwrap()));
    let stderr = drain(Box::new(child.stderr.take().unwrap()));
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait for the child") {
            break Some(status);
        }
        if start.elapsed() > CHILD_DEADLINE {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let stdout = stdout.join().unwrap();
    let stderr = stderr.join().unwrap();
    let Some(status) = status else {
        panic!(
            "the child did not exit within {CHILD_DEADLINE:?} and was killed; its output:\n{}{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
    };
    Output {
        status,
        stdout,
        stderr,
    }
}

/// The child's part: a frame on each OpenVINO engine, then everything the
/// test made dropped, and a return (the process exits after it).
fn child(models: &std::path::Path) {
    let tmp = tempfile::tempdir().unwrap();
    let models = model_dir(models, tmp.path());
    let log = Log::default();
    let module = open(tmp.path(), json!({"model_dir": models, "threads": 2}), &log);
    module.check(&[s("background")]).unwrap();
    let (w, h) = (640usize, 360usize);
    let (rgb, iw, ih) = test_image();
    let frame = nv12_of_rgb(&rgb, iw, ih, w, h, w);
    let mut ran = Vec::new();
    for engine in ["openvino-cpu", "openvino-gpu", "openvino-npu"] {
        let mut inst = module.create_instance().unwrap();
        log.take();
        let loaded = inst.load(&host_chain(
            w as u32,
            h as u32,
            &[("mode", "blur"), ("engine", engine)],
            None,
        ));
        match loaded {
            Ok(_) => {}
            Err(e) if engine != "openvino-cpu" => {
                eprintln!("{engine}: not run: {}", e.message);
                continue;
            }
            Err(e) => panic!("{engine}: {e:?}"),
        }
        let on = log
            .take()
            .iter()
            .rev()
            .find_map(|l| l.split(" runs on ").nth(1).map(str::to_string))
            .unwrap_or_default();
        assert!(on.starts_with("OpenVINO"), "{engine} ran on {on}");
        let mut src = frame.clone();
        let mut dst = frame.clone();
        let (fi, fo) = (src.frame(OBC_FRAME_FLAG_NEW_SOURCE), dst.frame(0));
        let strength = Value::Float(1.0);
        let live = [("background", "blur_strength", &strength)];
        // SAFETY: host memory of this test, valid during the call.
        unsafe { inst.process(&fi, &fo, &live) }.unwrap();
        inst.unload();
        drop(inst);
        eprintln!("{engine}: ran on {on}");
        ran.push(engine);
    }
    assert!(ran.contains(&"openvino-cpu"), "{ran:?}");
    drop(module);
}
