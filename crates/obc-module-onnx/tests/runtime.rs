//! The module with a real ONNX Runtime and the real models, through
//! `obc-module-host`: the probe and the check, a `background` chain of host
//! frames loaded and run on a test image, the engines chosen, and the
//! timings of a 1080p frame.
//!
//! Needs `ORT_DYLIB_PATH` (an ONNX Runtime library) and
//! `OBC_ONNX_MODEL_DIR` (the model files, under the catalogue's names or
//! the spike's); skipped, with the reason printed, when either is unset.
//! Optional: `OBC_ONNX_TEST_IMAGE`, a PNG with a person (by default
//! `$OBC_ONNX_MODEL_DIR/../images/c2_dark.png` when it is there, else a
//! synthetic scene).
//!
//! What the engines must do depends on the runtime given; the test reads
//! it from the module's check log:
//!
//! * a build without CUDA (the CPU wheel): `auto` runs on the CPU, and an
//!   explicit `cuda` still loads, on a later engine of the automatic order
//!   (never one before it), with a notice "NVIDIA CUDA was chosen and
//!   cannot run: ... not in this ONNX Runtime build; using <engine>";
//! * a build with CUDA and `CUDA_VISIBLE_DEVICES=""`: the CUDA and TensorRT
//!   engines are reported unavailable with a reason, `auto` falls to the
//!   CPU and the blur still works, and an explicit `cuda` loads on a later
//!   engine with the same kind of notice;
//! * a build with CUDA and a GPU: `auto` picks TensorRT or CUDA: CUDA when
//!   TensorRT cannot start (TensorRT's libraries off the library path, as
//!   when only the provider's `RUNPATH` finds them), with TensorRT's reason
//!   logged; an explicit `cuda` runs on CUDA, with no notice.
//!
//! An explicit `openvino-npu` the build lacks loads on a later engine
//! (OpenVINO GPU or CPU, or the CPU) with the notice, and never on
//! TensorRT or CUDA; where the build has it, a load without a notice runs
//! on it (a notice, from a machine without the device, is accepted and
//! printed).
//!
//! One test: the module's log callback is process-wide, so the steps that
//! read it run in order.

mod common;

use common::{Log, Nv12, host_chain, model_dir, nv12_of_rgb, open, runtime_env, s, test_image};
use obc_backend::{Availability, HostBgr, Value};
use obc_module_host::Module;
use obc_module_sdk::abi::{OBC_FRAME_FLAG_NEW_SOURCE, OBC_FRAME_FLAG_REPEAT};
use obc_module_sdk::serde_json::json;

const W: usize = 1280;
const H: usize = 720;

/// Which engines the runtime and the machine have, from the check log.
#[derive(Debug)]
struct Engines {
    cuda_in_build: bool,
    cuda_hidden: bool,
}

fn engines(lines: &[String]) -> Engines {
    let cuda = lines
        .iter()
        .find(|l| l.starts_with("ONNX cut-out: NVIDIA CUDA "))
        .expect("the check logs the CUDA engine");
    Engines {
        cuda_in_build: !cuda.contains("not in this ONNX Runtime build"),
        cuda_hidden: std::env::var_os("CUDA_VISIBLE_DEVICES").is_some_and(|v| v.is_empty()),
    }
}

/// The engine a load ran on, from its log line.
fn ran_on(lines: &[String]) -> String {
    lines
        .iter()
        .rev()
        .find_map(|l| l.split(" runs on ").nth(1))
        .map(|rest| rest.split([',', '(']).next().unwrap().trim().to_string())
        .unwrap_or_else(|| panic!("no engine line in {lines:#?}"))
}

/// Loads a blur chain with `engine` chosen explicitly and checks the
/// outcome against `can_run` (`Some(true)`: the engine must run, with no
/// notice; `Some(false)`: the load must succeed on another engine, with the
/// notice; `None`: either, consistently). `later` are the labels of the
/// engines after the chosen one in the automatic order. Returns the
/// notice.
fn load_explicit(
    inst: &mut obc_module_host::Instance,
    log: &Log,
    engine: &str,
    label: &str,
    later: &[&str],
    can_run: Option<bool>,
) -> Option<String> {
    log.take();
    let report = inst
        .load(&host_chain(
            W as u32,
            H as u32,
            &[("mode", "blur"), ("engine", engine)],
            None,
        ))
        .unwrap_or_else(|e| panic!("engine={engine} refused the load: {e:?}"));
    let lines = log.take();
    let used = ran_on(&lines);
    eprintln!("engine={engine}: ran on {used}; notice {:?}", report.notice);
    match &report.notice {
        Some(notice) => {
            assert_ne!(can_run, Some(true), "{label} can run: {notice}");
            assert!(
                notice.starts_with(&format!("{label} was chosen and cannot run: ")),
                "{notice}"
            );
            assert!(notice.ends_with(&format!("; using {used}")), "{notice}");
            assert!(
                later.contains(&used.as_str()),
                "{label} fell to {used}, not one of {later:?}"
            );
            assert!(
                lines.iter().any(|l| l.contains(notice.as_str())),
                "the notice is logged: {lines:#?}"
            );
        }
        None => {
            assert_ne!(can_run, Some(false), "{label} cannot run, yet no notice");
            assert_eq!(used, label, "{lines:#?}");
        }
    }
    report.notice
}

/// Runs `frame` through `module`'s instance into a copy; returns the copy.
fn run(inst: &mut obc_module_host::Instance, input: &Nv12, flags: u32) -> Nv12 {
    let mut src = input.clone();
    let mut dst = input.clone();
    dst.data.fill(0);
    let (fi, fo) = (src.frame(flags), dst.frame(0));
    let strength = Value::Float(1.0);
    let live = [("background", "blur_strength", &strength)];
    // SAFETY: both frames are host memory of this test, valid during the
    // call and touched by nothing else.
    unsafe { inst.process(&fi, &fo, &live) }.unwrap();
    assert_eq!(src.data, input.data, "the input is left as it was");
    dst
}

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

/// A `w x h` NV12 frame of one colour, rows `pitch` bytes apart.
fn solid(w: usize, h: usize, pitch: usize, y: u8, (u, v): (u8, u8)) -> Nv12 {
    let mut data = vec![y; pitch * (h + h / 2)];
    for row in data[pitch * h..].chunks_mut(pitch) {
        for c in row[..w].chunks_mut(2) {
            c.copy_from_slice(&[u, v]);
        }
    }
    Nv12 { w, h, pitch, data }
}

fn median(mut v: Vec<f32>) -> f32 {
    v.sort_by(f32::total_cmp);
    v[v.len() / 2]
}

#[test]
fn with_onnx_runtime_and_the_models() {
    let (lib, models) = match runtime_env() {
        Ok(e) => e,
        Err(why) => {
            eprintln!("skipped: {why}");
            return;
        }
    };
    eprintln!("ONNX Runtime: {}", lib.display());
    let tmp = tempfile::tempdir().unwrap();
    let models = model_dir(&models, tmp.path());
    let log = Log::default();
    let module: Module = open(tmp.path(), json!({"model_dir": models, "threads": 4}), &log);

    // The probe (files only) and the check (the runtime opened, its
    // providers listed): one line per engine.
    let probe = module.probe().unwrap();
    assert_eq!(probe[0].1, Availability::Installed, "{probe:?}");
    module.check(&[s("background")]).unwrap();
    let lines = log.take();
    for label in [
        "NVIDIA TensorRT",
        "NVIDIA CUDA",
        "OpenVINO NPU",
        "OpenVINO GPU",
        "OpenVINO on the CPU",
    ] {
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with(&format!("ONNX cut-out: {label} "))),
            "no line for {label}: {lines:#?}"
        );
    }
    let eng = engines(&lines);
    eprintln!("{eng:?}");
    let npu_in_build = lines
        .iter()
        .any(|l| l.contains("OpenVINO NPU available in this ONNX Runtime build"));
    if eng.cuda_in_build && eng.cuda_hidden {
        assert!(
            lines
                .iter()
                .any(|l| l.contains("NVIDIA CUDA unavailable: CUDA not available")),
            "{lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("NVIDIA TensorRT unavailable:")),
            "{lines:#?}"
        );
    }

    let (rgb, iw, ih) = test_image();
    let frame = nv12_of_rgb(&rgb, iw, ih, W, H, W + 64);

    // Blur, the default model and engine.
    let mut inst = module.create_instance().unwrap();
    let report = inst
        .load(&host_chain(W as u32, H as u32, &[("mode", "blur")], None))
        .unwrap();
    assert!(!report.reused_model);
    assert!(report.warmup_ms.is_some());
    let lines = log.take();
    let engine = ran_on(&lines);
    eprintln!("auto ran on {engine}; load {report:?}");
    if eng.cuda_in_build && !eng.cuda_hidden {
        assert!(
            engine == "NVIDIA TensorRT" || engine == "NVIDIA CUDA",
            "auto with a GPU: {engine}"
        );
        if engine == "NVIDIA CUDA" {
            let why = lines
                .iter()
                .find(|l| l.contains("NVIDIA TensorRT unavailable: "))
                .unwrap_or_else(|| panic!("auto on CUDA gives no TensorRT reason: {lines:#?}"));
            eprintln!("{why}");
        }
    } else {
        if npu_in_build {
            // A build with OpenVINO: `auto` falls to it, not to the NVIDIA
            // engines, when they are out.
            assert!(!engine.starts_with("NVIDIA"), "{lines:#?}");
        } else {
            assert_eq!(engine, "CPU", "{lines:#?}");
        }
        if eng.cuda_in_build {
            // The hidden GPU: both NVIDIA engines refused, with a reason.
            assert!(
                lines
                    .iter()
                    .any(|l| l.contains("NVIDIA TensorRT unavailable: ")),
                "{lines:#?}"
            );
            assert!(
                lines
                    .iter()
                    .any(|l| l.contains("NVIDIA CUDA unavailable: CUDA not available")),
                "{lines:#?}"
            );
        }
    }
    let out = run(&mut inst, &frame, OBC_FRAME_FLAG_NEW_SOURCE);
    let k = kept(&frame, &out);
    eprintln!("blur kept {:.1} % of the picture", 100.0 * k);
    assert!(
        (0.03..0.97).contains(&k),
        "the person kept, the room blurred: {k}"
    );
    let steps: Vec<String> = inst.timings().into_iter().map(|(s, _)| s).collect();
    assert_eq!(steps, ["convert-in", "mask", "composite", "convert-out"]);
    // A repeat is composited again from the kept matte: the same picture.
    let again = run(&mut inst, &frame, OBC_FRAME_FLAG_REPEAT);
    assert_eq!(again.data, out.data);
    inst.reset_temporal().unwrap();

    // Replace, as a switch: the session is reused; a mid-grey image shows
    // where the room was.
    let bg = HostBgr {
        width: 64,
        height: 36,
        stride: 64 * 3,
        data: vec![128; 64 * 3 * 36],
    };
    let report = inst
        .load(&host_chain(
            W as u32,
            H as u32,
            &[("mode", "replace")],
            Some(bg),
        ))
        .unwrap();
    assert!(report.reused_model, "{report:?}");
    let out = run(&mut inst, &frame, 0);
    let grey = (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .filter(|&(x, y)| (i32::from(out.y(x, y)) - 126).abs() <= 1)
        .count() as f64
        / (W * H) as f64;
    eprintln!("replace: {:.1} % of the picture is the image", 100.0 * grey);
    assert!(grey > 0.03, "{grey}");

    // A per-frame background (ABI 1.4) shows where the room was, in
    // place of the image; `None` returns to the image; a frame of another
    // size is refused and leaves the one in force.
    let share = |out: &Nv12, luma: i32| {
        (0..H)
            .flat_map(|y| (0..W).map(move |x| (x, y)))
            .filter(|&(x, y)| (i32::from(out.y(x, y)) - luma).abs() <= 1)
            .count() as f64
            / (W * H) as f64
    };
    let mut live = solid(W, H, W + 32, 60, (100, 150));
    let f = live.frame(0);
    // SAFETY: host memory of this test, valid during the call.
    unsafe { inst.set_background(Some(&f)) }.unwrap();
    live.data.fill(0); // copied: the frame is valid only during the call
    let out = run(&mut inst, &frame, 0);
    let (shown, still) = (share(&out, 60), share(&out, 126));
    eprintln!(
        "per-frame background: {:.1} % of the picture",
        100.0 * shown
    );
    assert!(
        (shown - grey).abs() < 0.02 && still < 0.01,
        "{shown} {still}"
    );
    let mut small = solid(W - 2, H, W, 30, (128, 128));
    let f = small.frame(0);
    // SAFETY: as above.
    let e = unsafe { inst.set_background(Some(&f)) }.unwrap_err();
    assert_eq!(e.status, obc_module_host::Status::Invalid, "{e:?}");
    let out = run(&mut inst, &frame, 0);
    assert!(share(&out, 60) > 0.03, "the previous background stays");
    // SAFETY: no frame.
    unsafe { inst.set_background(None) }.unwrap();
    let out = run(&mut inst, &frame, 0);
    assert!((share(&out, 126) - grey).abs() < 0.02 && share(&out, 60) < 0.01);

    // A switch to an engine this build lacks does not fail: it loads on a
    // later engine, with a notice, and the new chain runs.
    if !npu_in_build {
        let notice = load_explicit(
            &mut inst,
            &log,
            "openvino-npu",
            "OpenVINO NPU",
            &["OpenVINO GPU", "OpenVINO on the CPU", "CPU"],
            Some(false),
        )
        .unwrap();
        assert!(
            notice.contains("not in this ONNX Runtime build"),
            "{notice}"
        );
        assert!(inst.is_loaded());
        let out = run(&mut inst, &frame, 0);
        let k = kept(&frame, &out);
        assert!(
            (0.03..0.97).contains(&k),
            "the blur chain runs after the switch: {k}"
        );
    }

    // A frame that fails drops the chain: the frames after it fail too
    // (a matte never fails open), until a load.
    {
        let mut small = nv12_of_rgb(&rgb, iw, ih, W - 2, H, W + 64);
        let mut dst = small.clone();
        let (fi, fo) = (small.frame(0), dst.frame(0));
        // SAFETY: host memory of this test, valid during the call.
        let e = unsafe { inst.process(&fi, &fo, &[]) }.unwrap_err();
        eprintln!("a frame of another size: {}", e.message);
        // A matte stage's failure is the whole chain's.
        assert_eq!(e.status, obc_module_host::Status::Failed, "{e:?}");
        assert!(e.message.starts_with("background: "), "{e:?}");
        let mut src = frame.clone();
        let mut dst = frame.clone();
        let (fi, fo) = (src.frame(0), dst.frame(0));
        // SAFETY: as above.
        assert!(unsafe { inst.process(&fi, &fo, &[]) }.is_err());
        assert!(unsafe { inst.process(&fi, &fo, &[]) }.is_err());
        assert!(!inst.is_loaded());
    }
    log.take();

    // Every model of the catalogue on the CPU, on host frames.
    for m in obc_module_onnx::catalogue::MODELS {
        if !models.join(m.file).exists() {
            eprintln!("{}: no file, not run", m.id);
            continue;
        }
        let report = inst
            .load(&host_chain(
                W as u32,
                H as u32,
                &[("mode", "blur"), ("model", m.id), ("engine", "cpu")],
                None,
            ))
            .unwrap();
        let out = run(&mut inst, &frame, OBC_FRAME_FLAG_NEW_SOURCE);
        let k = kept(&frame, &out);
        eprintln!(
            "{}: load {:.0} ms, kept {:.1} %",
            m.id,
            report.load_ms,
            100.0 * k
        );
        assert!((0.03..0.97).contains(&k), "{}: {k}", m.id);
    }

    // A switch under `auto` whose TensorRT engine is not built yet runs on
    // CUDA for now (no build holds the stream); one that is cached runs on
    // TensorRT. An explicit `tensorrt` builds, and says the feed pauses.
    if engine == "NVIDIA TensorRT" {
        assert!(inst.is_loaded());
        log.take();
        inst.load(&host_chain(
            W as u32,
            H as u32,
            &[("mode", "blur"), ("model", "pphumanseg-v2-lite")],
            None,
        ))
        .unwrap();
        let lines = log.take();
        let on = ran_on(&lines);
        let skipped = lines
            .iter()
            .any(|l| l.contains("TensorRT engine not built yet; using NVIDIA CUDA for now"));
        eprintln!("auto switch to pphumanseg-v2-lite: on {on}, TensorRT skipped: {skipped}");
        if skipped {
            assert_eq!(on, "NVIDIA CUDA", "{lines:#?}");
        } else {
            assert_eq!(on, "NVIDIA TensorRT", "{lines:#?}");
            assert!(
                !lines
                    .iter()
                    .any(|l| l.contains("preparing the TensorRT engine")),
                "a cache hit builds nothing: {lines:#?}"
            );
        }
        run(&mut inst, &frame, OBC_FRAME_FLAG_NEW_SOURCE);
        if skipped {
            let t0 = std::time::Instant::now();
            inst.load(&host_chain(
                W as u32,
                H as u32,
                &[
                    ("mode", "blur"),
                    ("model", "pphumanseg-v2-lite"),
                    ("engine", "tensorrt"),
                ],
                None,
            ))
            .unwrap();
            let lines = log.take();
            eprintln!(
                "explicit tensorrt switch: built in {:.1} s",
                t0.elapsed().as_secs_f32()
            );
            assert_eq!(ran_on(&lines), "NVIDIA TensorRT");
            assert!(
                lines.iter().any(|l| l.contains("the feed pauses")),
                "{lines:#?}"
            );
        }
    }

    // An explicit engine that cannot run does not fail the load: it runs on
    // a later engine of the automatic order, and the notice says why.
    let cuda_later = ["OpenVINO NPU", "OpenVINO GPU", "OpenVINO on the CPU", "CPU"];
    if eng.cuda_in_build && !eng.cuda_hidden {
        let notice = load_explicit(
            &mut inst,
            &log,
            "cuda",
            "NVIDIA CUDA",
            &cuda_later,
            Some(true),
        );
        assert_eq!(notice, None);
    } else {
        let notice = load_explicit(
            &mut inst,
            &log,
            "cuda",
            "NVIDIA CUDA",
            &cuda_later,
            Some(false),
        )
        .unwrap();
        let why = if eng.cuda_in_build {
            "CUDA not available"
        } else {
            "not in this ONNX Runtime build"
        };
        assert!(notice.contains(why), "{notice}");
        assert!(!notice.contains("; using NVIDIA"), "{notice}");
    }
    let npu_later = ["OpenVINO GPU", "OpenVINO on the CPU", "CPU"];
    if npu_in_build {
        // Whether the device is there is not known from the build alone:
        // no notice means it ran on the NPU; a notice is checked as above.
        load_explicit(
            &mut inst,
            &log,
            "openvino-npu",
            "OpenVINO NPU",
            &npu_later,
            None,
        );
    } else {
        let notice = load_explicit(
            &mut inst,
            &log,
            "openvino-npu",
            "OpenVINO NPU",
            &npu_later,
            Some(false),
        )
        .unwrap();
        assert!(
            notice.contains("not in this ONNX Runtime build"),
            "{notice}"
        );
    }
    inst.unload();

    // The timings of a 1080p frame, MediaPipe on the CPU (4 threads).
    let frame = nv12_of_rgb(&rgb, iw, ih, 1920, 1080, 1920);
    let mut inst = module.create_instance().unwrap();
    inst.load(&host_chain(
        1920,
        1080,
        &[("mode", "blur"), ("engine", "cpu")],
        None,
    ))
    .unwrap();
    let mut steps: Vec<Vec<f32>> = vec![Vec::new(); 4];
    let mut total = Vec::new();
    for i in 0..60 {
        run(
            &mut inst,
            &frame,
            if i == 0 { OBC_FRAME_FLAG_NEW_SOURCE } else { 0 },
        );
        if i >= 10 {
            let t = inst.timings();
            total.push(t.iter().map(|(_, ms)| ms).sum::<f32>());
            for (k, (_, ms)) in t.into_iter().enumerate() {
                steps[k].push(ms);
            }
        }
    }
    let names = ["convert-in", "mask", "composite", "convert-out"];
    let medians: Vec<String> = names
        .iter()
        .zip(steps)
        .map(|(n, v)| format!("{n} {:.2} ms", median(v)))
        .collect();
    let total = median(total);
    eprintln!(
        "1080p, MediaPipe on the CPU: {} ; the module's steps together {total:.2} ms (median)",
        medians.join(", ")
    );
    assert!(total < 100.0, "{total} ms a frame");

    // A video background at 1080p: the frame handed in before each frame
    // (its copy, timed here), and the composite over it.
    let bg = HostBgr {
        width: 64,
        height: 36,
        stride: 64 * 3,
        data: vec![128; 64 * 3 * 36],
    };
    inst.load(&host_chain(
        1920,
        1080,
        &[("mode", "replace"), ("engine", "cpu")],
        Some(bg),
    ))
    .unwrap();
    let mut live = solid(1920, 1080, 1920, 60, (100, 150));
    let (mut set, mut still, mut over) = (Vec::new(), Vec::new(), Vec::new());
    for i in 0..60 {
        let flags = if i == 0 { OBC_FRAME_FLAG_NEW_SOURCE } else { 0 };
        run(&mut inst, &frame, flags);
        let composite = |inst: &obc_module_host::Instance| inst.timings()[2].1;
        let c0 = composite(&inst);
        let f = live.frame(0);
        let t = std::time::Instant::now();
        // SAFETY: host memory of this test, valid during the call.
        unsafe { inst.set_background(Some(&f)) }.unwrap();
        let ms = t.elapsed().as_secs_f32() * 1e3;
        run(&mut inst, &frame, OBC_FRAME_FLAG_REPEAT);
        let c1 = composite(&inst);
        // SAFETY: no frame.
        unsafe { inst.set_background(None) }.unwrap();
        if i >= 10 {
            set.push(ms);
            still.push(c0);
            over.push(c1);
        }
    }
    eprintln!(
        "1080p video background: set_background {:.2} ms; composite over it {:.2} ms, over the image {:.2} ms (median)",
        median(set),
        median(over),
        median(still)
    );

    // The same on the engine `auto` chose, when it is not the CPU.
    if engine != "CPU" {
        let mut inst = module.create_instance().unwrap();
        let report = inst
            .load(&host_chain(1920, 1080, &[("mode", "blur")], None))
            .unwrap();
        eprintln!("1080p on {}: load {report:?}", ran_on(&log.take()));
        let mut mask = Vec::new();
        let mut total = Vec::new();
        for i in 0..60 {
            run(
                &mut inst,
                &frame,
                if i == 0 { OBC_FRAME_FLAG_NEW_SOURCE } else { 0 },
            );
            if i >= 10 {
                let t = inst.timings();
                mask.push(t[1].1);
                total.push(t.iter().map(|(_, ms)| ms).sum::<f32>());
            }
        }
        eprintln!(
            "1080p, MediaPipe on {engine}: mask {:.2} ms, the module's steps together {:.2} ms (median)",
            median(mask),
            median(total)
        );
    }
}
