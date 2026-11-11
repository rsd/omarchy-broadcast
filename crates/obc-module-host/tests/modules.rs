//! The host against real module libraries: `examples/module-hello`,
//! `examples/module-hello-audio` and the fixture module of
//! `tests/fixtures/module-fixture`, all built by these tests with cargo (into a directory under `CARGO_TARGET_TMPDIR`) and
//! loaded with `dlopen`, as the daemon loads them.
//!
//! Each test that reads a module's log lines loads its own copy of the
//! library, so that the copies do not share their process-wide state.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use obc_backend::enhance::{
    ChromaSiting, DeviceYuv420, EnhanceRequest, EnhancerFactory, YuvLayout,
};
use obc_backend::{
    Availability, BackendError, BackendFactory, ChainFrame, ChainRequest, ColourSpec, DeviceNv12,
    EffectKind, FrameKind, FrameParams, GpuContext, HostBgr, HostNv12, LoadRequest, Params,
    StageChainFactory, StageId, StageList, Value,
};
use obc_module_host::discovery::dirs_from;
use obc_module_host::manifest::{REASON_AUDIO_IN_WORKER, REASON_VIDEO_IN_DAEMON};
use obc_module_host::{
    AudioBlock, AudioSpec, LoadKinds, LogLevel, LogSink, Manifest, Module, ModuleBackendFactory,
    ModuleChainFactory, ModuleEnhancerFactory, ModuleStatus, Registry, SettingType, Status,
    common_block_formats, scan,
};

// ---------------------------------------------------------------------------
// Building the libraries
// ---------------------------------------------------------------------------

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Builds the cdylib of the package at `dir` with `features` and copies it
/// to a file of its own, `name.so`, once per test binary. `--locked`: the
/// package's committed `Cargo.lock` is used as it is, never rewritten.
fn build(dir: &Path, lib: &str, features: &str, name: &str) -> PathBuf {
    static BUILT: Mutex<BTreeMap<String, PathBuf>> = Mutex::new(BTreeMap::new());
    let mut built = BUILT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = built.get(name) {
        return p.clone();
    }
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let target = tmp.join("module-builds");
    let mut cmd = Command::new(env!("CARGO"));
    cmd.args(["build", "--quiet", "--locked", "--manifest-path"])
        .arg(dir.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&target);
    if !features.is_empty() {
        cmd.args(["--features", features]);
    }
    let status = cmd.status().expect("run cargo");
    assert!(
        status.success(),
        "building {} failed (with --locked: if its Cargo.lock is out of date, run \
         `cargo update --workspace --manifest-path {}` and commit the lock)",
        dir.display(),
        dir.join("Cargo.toml").display()
    );
    let libs = tmp.join("module-libs");
    std::fs::create_dir_all(&libs).unwrap();
    let out = libs.join(format!("{name}.so"));
    std::fs::copy(target.join("debug").join(format!("lib{lib}.so")), &out).unwrap();
    built.insert(name.to_string(), out.clone());
    out
}

fn hello_lib() -> PathBuf {
    build(
        &repo().join("examples/module-hello"),
        "obc_module_hello",
        "",
        "hello",
    )
}

fn hello_audio_lib() -> PathBuf {
    build(
        &repo().join("examples/module-hello-audio"),
        "obc_module_hello_audio",
        "",
        "hello-audio",
    )
}

fn fixture_lib(features: &str) -> PathBuf {
    let name = if features.is_empty() {
        "fixture".to_string()
    } else {
        format!("fixture-{features}")
    };
    build(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/module-fixture"),
        "obc_module_fixture",
        features,
        &name,
    )
}

/// A copy of `lib` in `dir`, loaded apart from every other copy.
fn own_copy(lib: &Path, dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(format!("{name}.so"));
    std::fs::copy(lib, &p).unwrap();
    p
}

// ---------------------------------------------------------------------------
// Manifests and logs
// ---------------------------------------------------------------------------

fn fixture_manifest(id: &str, library: &Path, abi: u32) -> String {
    format!(
        r#"abi = {abi}
id = "{id}"
name = "Fixture"
version = "0.1.0"
licence = "MIT OR Apache-2.0"
kind = "video-stage"
library = "{}"

[[stage]]
id = "alpha"
slot = "pre-matte"
rank = 100
device = "cpu"
frames = ["host", "cuda"]
formats = ["nv12"]

[[stage]]
id = "beta"
slot = "post-matte"
rank = 100
device = "cpu"
frames = ["host", "cuda"]
formats = ["nv12"]

[[stage]]
id = "gamma"
slot = "post-composite"
rank = 100
device = "cpu"
frames = ["cuda"]
formats = ["nv12"]

[[stage]]
id = "background"
slot = "matte"
rank = 100
device = "cpu"
frames = ["cuda"]
formats = ["nv12"]

[[stage]]
id = "enhance"
slot = "pre-matte"
rank = 100
device = "cpu"
position = 0
frames = ["cuda"]
formats = ["nv12", "i420"]
"#,
        library.display()
    )
}

/// The manifest of the fixture's `audio` build (or of another library,
/// to see it refused).
fn fixture_audio_manifest(id: &str, library: &Path) -> String {
    format!(
        r#"abi = 1
id = "{id}"
name = "Audio fixture"
version = "0.1.0"
licence = "MIT OR Apache-2.0"
kind = "audio-filter"
library = "{}"

[[stage]]
id = "mic-denoise"
slot = "clean"
rank = 200
device = "cpu"
frames = ["host"]
rates = [48000]
frame_samples = [480, 960]

[[stage]]
id = "mic-dereverb"
slot = "clean"
rank = 200
device = "cpu"
position = 50
frames = ["host"]
rates = [48000]
frame_samples = [480, 960]

[[stage]]
id = "mic-enhance"
slot = "shape"
rank = 200
device = "cpu"
frames = ["host"]
rates = [48000, 16000]
frame_samples = [480]
"#,
        library.display()
    )
}

/// A manifest parsed from `text` as if it were at `dir/name.toml`.
fn manifest(text: &str, dir: &Path, name: &str) -> Manifest {
    let path = dir.join(format!("{name}.toml"));
    std::fs::write(&path, text).unwrap();
    Manifest::load(&path).unwrap()
}

/// The log lines a sink kept: module id, level, line.
type Lines = Arc<Mutex<Vec<(String, LogLevel, String)>>>;

/// A log sink that keeps every line.
fn capture() -> (LogSink, Lines) {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let keep = lines.clone();
    let sink: LogSink = Arc::new(move |id, level, msg| {
        keep.lock()
            .unwrap()
            .push((id.to_string(), level, msg.to_string()));
    });
    (sink, lines)
}

/// The last line that starts with `prefix`.
fn last_line(lines: &Lines, prefix: &str) -> String {
    lines
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|(_, _, m)| m.starts_with(prefix))
        .map(|(_, _, m)| m.clone())
        .unwrap_or_else(|| panic!("no log line starts with {prefix:?}"))
}

fn count(lines: &Lines, prefix: &str) -> usize {
    lines
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, _, m)| m.starts_with(prefix))
        .count()
}

/// An opened fixture module, with its own library copy and log.
fn fixture(config: serde_json::Value) -> (tempfile::TempDir, Module, Lines) {
    let dir = tempfile::tempdir().unwrap();
    let lib = own_copy(&fixture_lib(""), dir.path(), "fixture");
    let m = manifest(
        &fixture_manifest("org.test.fixture", &lib, 1),
        dir.path(),
        "fixture",
    );
    let (sink, lines) = capture();
    let module = Module::open(&m, &config, sink).expect("the fixture opens");
    (dir, module, lines)
}

fn s(id: &str) -> StageId {
    StageId::new(id).unwrap()
}

/// A tightly packed host NV12 image of `w` x `h`.
struct HostImage {
    y: Vec<u8>,
    uv: Vec<u8>,
    w: u32,
    h: u32,
}

impl HostImage {
    fn new(w: u32, h: u32, luma: u8, chroma: u8) -> HostImage {
        HostImage {
            y: vec![luma; (w * h) as usize],
            uv: vec![chroma; (w * h / 2) as usize],
            w,
            h,
        }
    }

    fn frame(&mut self) -> HostNv12 {
        HostNv12 {
            y: self.y.as_mut_ptr(),
            y_pitch: self.w as usize,
            uv: self.uv.as_mut_ptr(),
            uv_pitch: self.w as usize,
            width: self.w,
            height: self.h,
        }
    }
}

fn host_request(stages: &[&str], params: BTreeMap<StageId, Params>) -> ChainRequest {
    ChainRequest {
        stages: StageList::new(stages.iter().map(|i| s(i))).unwrap(),
        params,
        width: 4,
        height: 2,
        colour: ColourSpec::LIMITED_BT601,
        kind: FrameKind::Host,
        gpu: None,
    }
}

fn params(stage: &str, pairs: &[(&str, Value)]) -> BTreeMap<StageId, Params> {
    let p: Params = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    [(s(stage), p)].into_iter().collect()
}

/// A CUDA context and stream that are never dereferenced: the fixture does
/// not touch CUDA frames.
fn fake_gpu() -> GpuContext {
    GpuContext {
        context: 0x1000 as *mut _,
        stream: 0x2000 as *mut _,
    }
}

fn fake_nv12(w: u32, h: u32) -> DeviceNv12 {
    DeviceNv12 {
        y: 0x10_0000,
        y_pitch: w as usize,
        uv: 0x20_0000,
        uv_pitch: w as usize,
        width: w,
        height: h,
    }
}

// ---------------------------------------------------------------------------
// The example module
// ---------------------------------------------------------------------------

#[test]
fn hello_is_discovered_loads_probes_describes_and_tints_a_host_frame() {
    let lib = hello_lib();
    let modules_d = tempfile::tempdir().unwrap();
    // The example's manifest, installed with the library's path.
    let shipped = std::fs::read_to_string(repo().join("examples/module-hello/hello.toml")).unwrap();
    let installed = shipped.replace(
        "library = \"target/release/libobc_module_hello.so\"",
        &format!("library = \"{}\"", lib.display()),
    );
    assert_ne!(shipped, installed, "the example's library line moved");
    std::fs::write(modules_d.path().join("hello.toml"), installed).unwrap();

    let dirs = dirs_from(None, None, &[], Some(modules_d.path().as_os_str()));
    let discovery = scan(&dirs);
    let mut registry = Registry::new(discovery);
    let (sink, _lines) = capture();
    registry.open_all(|_| serde_json::json!({}), &sink);
    const ID: &str = "org.example.hello";
    assert_eq!(registry.status(ID), Some(ModuleStatus::Available));
    let info = &registry.modules()[0];
    assert_eq!(info.stages.len(), 1);
    assert!(info.stages[0].available && info.stages[0].selected);
    assert_eq!(registry.resolve("hello").provider.as_deref(), Some(ID));
    assert_eq!(registry.order(), [s("hello")]);

    let module = registry.module(ID).unwrap().clone();
    assert_eq!(
        module.probe().unwrap(),
        [(s("hello"), Availability::Installed)]
    );
    let settings = module.settings().unwrap();
    assert_eq!(settings.groups.len(), 1);
    assert_eq!(
        (
            settings.groups[0].id.as_str(),
            settings.groups[0].label.as_str()
        ),
        ("hello", "Hello module")
    );
    assert_eq!(
        settings.enable_of("hello").unwrap().id,
        "effect.hello.enabled"
    );
    let strength = settings.param("hello", "strength").unwrap();
    assert!(matches!(
        strength.kind,
        SettingType::Int {
            min: 0,
            max: 100,
            step: 5,
            default: 50,
            ..
        }
    ));

    let factory = ModuleChainFactory::new(module, &[s("hello")]).unwrap();
    assert_eq!(factory.name(), ID);
    assert_eq!(factory.kinds(), [FrameKind::Host]);
    let mut chain = factory.create().unwrap();
    let report = chain
        .load(&host_request(
            &["hello"],
            params("hello", &[("strength", Value::Int(100))]),
        ))
        .unwrap();
    assert!(!report.reused_model);
    let mut img = HostImage::new(4, 2, 60, 128);
    let mut f = img.frame();
    chain
        .process(ChainFrame::Host(&mut f), &BTreeMap::new())
        .unwrap();
    assert_eq!(img.y, [60; 8], "luma untouched");
    assert_eq!(img.uv, [104, 152, 104, 152], "the tint is visible");
    // A live strength of 0 stops it.
    let mut img = HostImage::new(4, 2, 60, 128);
    let mut f = img.frame();
    chain
        .process(
            ChainFrame::Host(&mut f),
            &params("hello", &[("strength", Value::Int(0))]),
        )
        .unwrap();
    assert_eq!(img.uv, [128; 4]);
    // The example refuses CUDA frames, as its manifest says.
    let mut req = host_request(&["hello"], BTreeMap::new());
    req.kind = FrameKind::Cuda;
    req.gpu = Some(fake_gpu());
    assert!(matches!(
        chain.load(&req),
        Err(BackendError::InvalidInput(_))
    ));
    chain.unload();
}

// ---------------------------------------------------------------------------
// Broken modules
// ---------------------------------------------------------------------------

#[test]
fn broken_modules_are_unavailable_with_their_reasons_and_the_process_lives() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let fixture = own_copy(&fixture_lib(""), d, "fixture");
    let write =
        |name: &str, text: String| std::fs::write(d.join(format!("{name}.toml")), text).unwrap();
    write(
        "missing",
        fixture_manifest("org.test.missing", &d.join("libnone.so"), 1),
    );
    write("abi2", fixture_manifest("org.test.abi-two", &fixture, 2));
    write(
        "major",
        fixture_manifest("org.test.major", &fixture_lib("abi-major-2"), 1),
    );
    write(
        "minor",
        fixture_manifest("org.test.minor", &fixture_lib("abi-minor-newer"), 1),
    );
    write(
        "refuse",
        fixture_manifest("org.test.refuse", &fixture_lib("refuse-host"), 1),
    );
    let failing = own_copy(&fixture_lib(""), d, "failing");
    write("init", fixture_manifest("org.test.init", &failing, 1));
    let panicking = own_copy(&fixture_lib(""), d, "panicking");
    write("probe", fixture_manifest("org.test.probe", &panicking, 1));
    write(
        "typo",
        fixture_manifest("org.test.typo", &fixture, 1).replace("licence", "license"),
    );
    write(
        "audio",
        fixture_audio_manifest("org.test.audio", &fixture_lib("audio")),
    );
    write(
        "video-slots",
        fixture_manifest("org.test.video-slots", &fixture, 1)
            .replace("video-stage", "audio-filter"),
    );

    let mut registry = Registry::new(scan(&[d.to_path_buf()]));
    let (sink, _lines) = capture();
    registry.open_all(
        |id| match id {
            "org.test.init" => serde_json::json!({"fail_init": true}),
            "org.test.probe" => serde_json::json!({"panic_probe": true}),
            _ => serde_json::json!({}),
        },
        &sink,
    );
    let reason = |id: &str| match registry.status(id).unwrap() {
        ModuleStatus::Unavailable(r) => r,
        other => panic!("{id} is {other:?}"),
    };
    let r = reason("org.test.missing");
    assert!(r.contains("libnone.so is not installed"), "{r}");
    let r = reason("org.test.abi-two");
    assert!(
        r.contains("module ABI 2") && r.contains("module ABI 1"),
        "{r}"
    );
    // The library's own version: refused before its entry point (which
    // would abort the process) is called.
    let r = reason("org.test.major");
    assert!(
        r.contains("module ABI 2.0") && r.contains("module ABI 1.2"),
        "{r}"
    );
    let r = reason("org.test.minor");
    assert!(r.contains("1.99, newer than") && r.contains("1.2"), "{r}");
    let r = reason("org.test.refuse");
    assert!(r.contains("refused this daemon"), "{r}");
    let r = reason("org.test.init");
    assert_eq!(
        r,
        "The module could not start: the fixture was told to fail"
    );
    let r = reason("org.test.probe");
    assert!(
        r.starts_with("The probe failed: ") && r.contains("the probe was told to panic"),
        "{r}"
    );
    assert!(
        reason("org.test.typo").contains("license"),
        "keyed by its id"
    );
    // The video registry lists an audio module, and never opens it.
    assert_eq!(reason("org.test.audio"), REASON_AUDIO_IN_WORKER);
    let r = reason("org.test.video-slots");
    assert!(
        r.contains("must start with \"mic-\""),
        "an audio manifest of video stages is invalid: {r}"
    );
    // None of them provides anything.
    assert!(registry.module("org.test.probe").is_none());
    let alpha = registry.resolve("alpha");
    assert_eq!(alpha.provider, None);
    // The reason of the first provider by rank, then id.
    assert_eq!(
        alpha.reason.as_deref(),
        Some("The module could not start: the fixture was told to fail")
    );
}

#[test]
fn the_probe_maps_onto_the_manifest_and_ignores_extra_stages() {
    let (_dir, module, lines) = fixture(serde_json::json!({
        "unavailable": "beta", "omit": "gamma", "extra": true
    }));
    let probe = module.probe().unwrap();
    let by_id: BTreeMap<String, Availability> =
        probe.into_iter().map(|(s, a)| (s.to_string(), a)).collect();
    assert_eq!(by_id["alpha"], Availability::Installed);
    assert_eq!(
        by_id["beta"],
        Availability::NotInstalled("Fixture says no".into())
    );
    assert_eq!(
        by_id["gamma"],
        Availability::NotInstalled("Not reported by the module".into())
    );
    assert!(!by_id.contains_key("zeta"));
    let warned = last_line(&lines, "the probe reports the stage \"zeta\"");
    assert!(warned.contains("ignored"));
}

#[test]
fn invalid_settings_are_dropped_with_a_log_line_and_the_module_stays() {
    let (_dir, module, lines) = fixture(serde_json::json!({"bad_settings": true}));
    let settings = module.settings().unwrap();
    let ids: Vec<&str> = settings.settings().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["effect.fixture.alpha", "effect.fixture.step"]);
    let dropped: Vec<String> = lines
        .lock()
        .unwrap()
        .iter()
        .filter(|(id, level, m)| {
            id == "org.test.fixture" && *level == LogLevel::Warn && m.contains("was dropped")
        })
        .map(|(_, _, m)| m.clone())
        .collect();
    assert_eq!(dropped.len(), 2, "{dropped:?}");
    assert!(module.probe().is_ok());
}

// ---------------------------------------------------------------------------
// Failures crossing the boundary
// ---------------------------------------------------------------------------

#[test]
fn a_panic_in_process_is_an_error_and_the_process_survives() {
    let (_dir, module, _lines) = fixture(serde_json::json!({}));
    let factory = ModuleChainFactory::new(module, &[s("alpha")]).unwrap();
    assert_eq!(factory.kinds(), [FrameKind::Host, FrameKind::Cuda]);
    let mut chain = factory.create().unwrap();
    chain
        .load(&host_request(&["alpha"], BTreeMap::new()))
        .unwrap();
    let mut img = HostImage::new(4, 2, 50, 128);
    let mut f = img.frame();
    chain
        .process(ChainFrame::Host(&mut f), &BTreeMap::new())
        .unwrap();
    assert_eq!(img.y, [60; 8]);

    let e = chain
        .process(
            ChainFrame::Host(&mut f),
            &params("alpha", &[("panic", Value::Bool(true))]),
        )
        .unwrap_err();
    match &e {
        BackendError::Run { code, message } => {
            assert_eq!(*code, obc_module_abi::OBC_ERR_PANIC);
            assert!(message.contains("alpha was told to panic"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    // Unloaded: frames are refused without calling the module until a load.
    let e = chain
        .process(ChainFrame::Host(&mut f), &BTreeMap::new())
        .unwrap_err();
    assert!(
        matches!(&e, BackendError::InvalidInput(m) if m.contains("loaded again")),
        "{e:?}"
    );
    chain
        .load(&host_request(&["alpha"], BTreeMap::new()))
        .unwrap();
    chain
        .process(ChainFrame::Host(&mut f), &BTreeMap::new())
        .unwrap();
    assert_eq!(img.y, [70; 8]);
}

/// A repeat of the output clock reaches a module of ABI 1.1 with
/// `OBC_FRAME_FLAG_REPEAT`; a module of ABI 1.0 never sees the flag, and
/// both process the repeat as before (the host never skips the call).
#[test]
fn a_repeat_is_flagged_for_a_module_of_abi_1_1_and_reprocessed_by_one_of_1_0() {
    for (features, abi, flagged) in [
        ("", "1.2", true),
        ("abi-1-1", "1.1", true),
        ("abi-1-0", "1.0", false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let lib = own_copy(&fixture_lib(features), dir.path(), "fixture");
        let m = manifest(
            &fixture_manifest("org.test.fixture", &lib, 1),
            dir.path(),
            "fixture",
        );
        let (sink, lines) = capture();
        let module = Module::open(&m, &serde_json::json!({}), sink).expect("the fixture opens");
        assert_eq!(
            obc_module_host::library::version_text(module.library().abi_version()),
            abi
        );
        let factory = ModuleChainFactory::new(module, &[s("alpha")]).unwrap();
        let mut chain = factory.create().unwrap();
        chain
            .load(&host_request(&["alpha"], BTreeMap::new()))
            .unwrap();
        let mut img = HostImage::new(4, 2, 50, 128);
        let mut f = img.frame();
        chain
            .process(ChainFrame::Host(&mut f), &BTreeMap::new())
            .unwrap();
        assert!(
            last_line(&lines, "fixture-process").contains(" repeat=false "),
            "{abi}: a fresh frame"
        );
        chain
            .process_repeat(ChainFrame::Host(&mut f), &BTreeMap::new())
            .unwrap();
        let line = last_line(&lines, "fixture-process");
        assert!(
            line.contains(&format!(" repeat={flagged} ")),
            "{abi}: {line}"
        );
        // The fixture processes every frame it is given: the repeat ran
        // through the chain too.
        assert_eq!(img.y, [70; 8], "{abi}");
    }
}

#[test]
fn stage_failures_cross_as_stage_errors_and_keep_the_rest_of_the_chain() {
    let (_dir, module, _lines) = fixture(serde_json::json!({}));
    let factory = ModuleChainFactory::new(module.clone(), &[s("alpha"), s("beta")]).unwrap();
    assert_eq!(factory.kinds(), [FrameKind::Host, FrameKind::Cuda]);
    let mut chain = factory.create().unwrap();
    chain
        .load(&host_request(
            &["alpha", "beta"],
            params("alpha", &[("step", Value::Int(1))]),
        ))
        .unwrap();
    let mut img = HostImage::new(4, 2, 0, 128);
    let mut f = img.frame();
    let e = chain
        .process(
            ChainFrame::Host(&mut f),
            &params("beta", &[("fail", Value::Bool(true))]),
        )
        .unwrap_err();
    assert_eq!(
        e,
        BackendError::Stage {
            stage: s("beta"),
            code: obc_module_abi::OBC_ERR_STAGE,
            message: "beta was told to fail".into()
        }
    );
    // The rest of the chain runs on.
    chain
        .process(ChainFrame::Host(&mut f), &BTreeMap::new())
        .unwrap();
    assert_eq!(img.y, [1; 8]);

    // A stage failure of a switch keeps the loaded chain.
    let mut p = params("alpha", &[("step", Value::Int(1))]);
    p.extend(params("beta", &[("fail_load", Value::Bool(true))]));
    let e = chain
        .load(&host_request(&["alpha", "beta"], p))
        .unwrap_err();
    assert!(
        matches!(&e, BackendError::Stage { stage, .. } if *stage == "beta"),
        "{e:?}"
    );
    chain
        .process(ChainFrame::Host(&mut f), &BTreeMap::new())
        .unwrap();
    assert_eq!(img.y, [2; 8], "still loaded");

    // A temporal reset drops beta.
    chain
        .load(&host_request(&["alpha", "beta"], BTreeMap::new()))
        .unwrap();
    assert_eq!(chain.reset_temporal().unwrap(), StageList::from(s("beta")));
    assert!(chain.reset_temporal().unwrap().is_empty());

    // Another kind of failure: an unknown stage.
    let factory = ModuleChainFactory::new(module.clone(), &[s("alpha")]).unwrap();
    let mut chain = factory.create().unwrap();
    let e = chain
        .load(&host_request(&["alpha", "zeta"], BTreeMap::new()))
        .unwrap_err();
    assert!(
        matches!(&e, BackendError::InvalidInput(m) if m.contains("zeta")),
        "{e:?}"
    );
    assert!(ModuleChainFactory::new(module.clone(), &[s("zeta")]).is_err());
    assert!(ModuleChainFactory::new(module.clone(), &[]).is_err());
    let cuda_only = ModuleChainFactory::new(module, &[s("alpha"), s("gamma")]).unwrap();
    assert_eq!(cuda_only.kinds(), [FrameKind::Cuda], "the kinds both take");
}

// ---------------------------------------------------------------------------
// The effect and enhance seams
// ---------------------------------------------------------------------------

/// The factory reads the settings description kept when the module was
/// opened: it never calls the module for it (that would run module code
/// outside the crash ledger's marker).
#[test]
fn the_backend_factory_never_asks_the_module_for_its_settings() {
    let count = |lines: &Lines| {
        lines
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, _, m)| m == "fixture-settings")
            .count()
    };
    let p = |v: i64| -> Params { [("step".to_string(), Value::Int(v))].into_iter().collect() };
    // Never described: no call, and every parameter is accepted.
    let (_dir, module, lines) = fixture(serde_json::json!({}));
    assert!(module.cached_settings().is_none());
    let factory = ModuleBackendFactory::new(module.clone());
    assert_eq!(count(&lines), 0);
    assert!(
        factory
            .check_params(&s("alpha"), &p(90), &Params::new())
            .is_ok()
    );
    // Described once (at open): the factory checks against that copy.
    let described = module.settings().unwrap();
    assert_eq!(count(&lines), 1);
    assert_eq!(module.cached_settings(), Some(described));
    let factory = ModuleBackendFactory::new(module.clone());
    assert_eq!(count(&lines), 1, "the factory made no module call");
    assert!(
        factory
            .check_params(&s("alpha"), &p(90), &Params::new())
            .is_err()
    );
    assert!(
        factory
            .check_params(&s("alpha"), &p(20), &Params::new())
            .is_ok()
    );
}

#[test]
fn the_backend_factory_carries_the_effect_through_the_abi() {
    let (_dir, module, lines) = fixture(serde_json::json!({}));
    // As the daemon opens a module: the description read under the crash
    // ledger's marker, kept for the factory.
    module.settings().unwrap();
    let factory = ModuleBackendFactory::new(module);
    assert_eq!(factory.name(), "org.test.fixture");
    factory.probe().unwrap();
    // Parameters are checked against the module's settings.
    let p = |k: &str, v: Value| -> Params { [(k.to_string(), v)].into_iter().collect() };
    assert!(
        factory
            .check_params(&s("alpha"), &p("step", Value::Int(20)), &Params::new())
            .is_ok()
    );
    let e = factory
        .check_params(&s("alpha"), &p("step", Value::Int(90)), &Params::new())
        .unwrap_err();
    assert!(e.contains("outside 0 to 50"), "{e}");

    let mut backend = factory.create().unwrap();
    let gpu = fake_gpu();
    let bgr = HostBgr {
        width: 4,
        height: 2,
        stride: 12,
        data: (0..24).map(|i| i as u8 + 7).collect(),
    };
    let mut req = LoadRequest {
        effect: EffectKind::Replace,
        stages: StageList::new([s("beta"), s("alpha")]).unwrap(),
        width: 640,
        height: 360,
        colour: ColourSpec::FULL_BT601,
        background: Some(Arc::new(bgr)),
        params: params("alpha", &[("step", Value::Int(3))]),
    };
    req.params
        .extend(params("gamma", &[("ignored", Value::Bool(true))]));
    let report = backend.load(&gpu, &req).unwrap();
    assert_eq!(report.load_ms, 1.5);
    assert_eq!(report.warmup_ms, Some(0.5));
    let line = last_line(&lines, "fixture-load");
    assert!(
        line.contains("stages=alpha,background,beta"),
        "the cut-out after the pre-matte stages: {line}"
    );
    assert!(
        line.contains("kind=cuda in=nv12:640x360 out=nv12:640x360"),
        "{line}"
    );
    assert!(
        line.contains("matrix=Bt601 siting=Mpeg2 full=true cuda=yes"),
        "{line}"
    );
    assert!(
        line.contains("params=alpha.step=3;background.mode=replace"),
        "{line}"
    );
    assert!(line.contains("bg=host:bgr:4x2:1:cuda=no:7"), "{line}");

    let mut fp = FrameParams {
        blur_strength: 1.5,
        live: params("beta", &[("x", Value::Int(1))]),
    };
    fp.live.extend(params("gamma", &[("y", Value::Int(2))]));
    backend
        .process(&gpu, &fp, &fake_nv12(640, 360), &fake_nv12(640, 360))
        .unwrap();
    let line = last_line(&lines, "fixture-process");
    assert!(line.contains("in=cuda:nv12:640x360:2:cuda=yes"), "{line}");
    assert!(
        line.contains("live=background.blur_strength=1;beta.x=1"),
        "clamped, the unloaded stage left out: {line}"
    );
    let timings = backend.timings().unwrap();
    assert_eq!(timings.step_ms("alpha"), Some(1.0));
    assert_eq!(timings.step_ms("beta"), Some(3.0));

    assert_eq!(
        backend.reset_temporal(&gpu).unwrap(),
        StageList::from(s("beta"))
    );

    // `none` with a stage: no cut-out, no mode, no image.
    req.effect = EffectKind::None;
    req.stages = StageList::from(s("alpha"));
    let report = backend.load(&gpu, &req).unwrap();
    assert!(report.reused_model);
    let line = last_line(&lines, "fixture-load");
    assert!(
        line.contains("stages=alpha ") && line.contains("bg=none"),
        "{line}"
    );
    assert!(!line.contains("background.mode"), "{line}");
    backend
        .process(
            &gpu,
            &FrameParams::default(),
            &fake_nv12(640, 360),
            &fake_nv12(640, 360),
        )
        .unwrap();
    assert!(!last_line(&lines, "fixture-process").contains("blur_strength"));

    // A stage failure of the load.
    req.params
        .extend(params("beta", &[("fail_load", Value::Bool(true))]));
    req.stages = StageList::new([s("alpha"), s("beta")]).unwrap();
    let e = backend.load(&gpu, &req).unwrap_err();
    assert!(
        matches!(&e, BackendError::Stage { stage, .. } if *stage == "beta"),
        "{e:?}"
    );
    // No context: refused before the module.
    let none = GpuContext {
        context: std::ptr::null_mut(),
        stream: std::ptr::null_mut(),
    };
    assert!(matches!(
        backend.load(&none, &req),
        Err(BackendError::InvalidInput(_))
    ));
    backend.unload();
    assert!(backend.timings().is_none());
}

#[test]
fn the_enhancer_factory_carries_the_enhance_stage_through_the_abi() {
    let (_dir, module, lines) = fixture(serde_json::json!({}));
    let factory = ModuleEnhancerFactory::new(module);
    assert_eq!(factory.name(), "org.test.fixture");
    let mut e = factory.create().unwrap();
    let gpu = fake_gpu();
    let req = EnhanceRequest {
        width: 320,
        height: 180,
        scale: 2,
        layout: YuvLayout::I420,
        colour: ColourSpec::LIMITED_BT601,
        chroma: ChromaSiting::Interstitial,
    };
    e.load(&gpu, &req).unwrap();
    let line = last_line(&lines, "fixture-load");
    assert!(
        line.contains("stages=enhance kind=cuda in=i420:320x180 out=nv12:640x360"),
        "{line}"
    );
    assert!(line.contains("siting=Jpeg full=false"), "{line}");
    let src = DeviceYuv420 {
        layout: YuvLayout::I420,
        y: 0x1000,
        y_pitch: 320,
        u: 0x2000,
        u_pitch: 160,
        v: 0x3000,
        v_pitch: 160,
        width: 320,
        height: 180,
    };
    e.process(&gpu, &src, &fake_nv12(640, 360)).unwrap();
    let line = last_line(&lines, "fixture-process");
    assert!(
        line.contains("in=cuda:i420:320x180:3:cuda=yes out=cuda:nv12:640x360:2"),
        "{line}"
    );
    let t = e.timings().unwrap();
    assert_eq!(
        (t.convert_in_ms, t.upscale_ms, t.convert_out_ms),
        (0.25, 1.0, 0.75)
    );
    // An invalid request is refused before the module.
    let bad = EnhanceRequest { scale: 9, ..req };
    assert!(matches!(
        e.load(&gpu, &bad),
        Err(BackendError::InvalidInput(_))
    ));
    e.unload();
}

#[test]
fn a_library_opened_twice_is_shared() {
    let (dir, module, _lines) = fixture(serde_json::json!({}));
    let m = Manifest::load(&dir.path().join("fixture.toml")).unwrap();
    let (sink, _) = capture();
    let again = Module::open(&m, &serde_json::json!({}), sink).unwrap();
    assert!(std::ptr::eq(module.library(), again.library()));
    // A module object outlives the instances made from it, and the
    // library outlives both.
    let mut instance = again.create_instance().unwrap();
    drop(again);
    instance.unload();
    assert_eq!(instance.module_id(), "org.test.fixture");
    assert!(module.create_instance().is_ok());
}

// ---------------------------------------------------------------------------
// Switches, matte stages and the library's lifetime
// ---------------------------------------------------------------------------

#[test]
fn a_failed_switch_keeps_the_previous_chain_processing() {
    let (_dir, module, _lines) = fixture(serde_json::json!({}));
    let factory = ModuleChainFactory::new(module, &[s("alpha")]).unwrap();
    let mut chain = factory.create().unwrap();
    let step = |n: i64| params("alpha", &[("step", Value::Int(n))]);
    chain.load(&host_request(&["alpha"], step(1))).unwrap();
    let mut img = HostImage::new(4, 2, 0, 128);
    let mut f = img.frame();
    chain
        .process(ChainFrame::Host(&mut f), &BTreeMap::new())
        .unwrap();
    assert_eq!(img.y, [1; 8]);

    // A switch (the same frames) that fails as a whole: the previous chain
    // runs on, as it was.
    let failing = params(
        "alpha",
        &[("step", Value::Int(5)), ("fail_load", Value::Bool(true))],
    );
    let e = chain
        .load(&host_request(&["alpha"], failing.clone()))
        .unwrap_err();
    assert!(
        matches!(&e, BackendError::Load { code, message }
            if *code == obc_module_abi::OBC_ERR_FAILED && message.contains("fail its load")),
        "{e:?}"
    );
    let mut f = img.frame();
    chain
        .process(ChainFrame::Host(&mut f), &BTreeMap::new())
        .unwrap();
    assert_eq!(img.y, [2; 8], "the previous chain, with its step of 1");

    // A reload (other frames) that fails leaves nothing loaded.
    let mut other = host_request(&["alpha"], failing);
    other.width = 8;
    assert!(chain.load(&other).is_err());
    let mut f = img.frame();
    let e = chain
        .process(ChainFrame::Host(&mut f), &BTreeMap::new())
        .unwrap_err();
    assert!(matches!(e, BackendError::InvalidInput(_)), "{e:?}");
    assert_eq!(img.y, [2; 8], "the module was not called");
}

#[test]
fn matte_stages_never_fail_open() {
    let (_dir, module, lines) = fixture(serde_json::json!({}));
    let factory = ModuleBackendFactory::new(module);
    let mut backend = factory.create().unwrap();
    let gpu = fake_gpu();
    let mut req = LoadRequest {
        effect: EffectKind::Blur,
        stages: StageList::from(s("alpha")),
        width: 640,
        height: 360,
        colour: ColourSpec::LIMITED_BT601,
        background: None,
        params: BTreeMap::new(),
    };
    backend.load(&gpu, &req).unwrap();
    assert!(backend.is_loaded());
    let frame = fake_nv12(640, 360);

    // A whole failure of a switch keeps the chain.
    req.params = params("alpha", &[("fail_load", Value::Bool(true))]);
    assert!(matches!(
        backend.load(&gpu, &req),
        Err(BackendError::Load { .. })
    ));
    assert!(backend.is_loaded(), "a failed switch keeps the chain");
    backend
        .process(&gpu, &FrameParams::default(), &frame, &frame)
        .unwrap();

    // The cut-out failing on its own is a failure of the chain, never a
    // stage failure to go on without: the instance is unloaded.
    req.params = params("background", &[("fail_load", Value::Bool(true))]);
    let e = backend.load(&gpu, &req).unwrap_err();
    assert!(
        matches!(&e, BackendError::Load { message, .. }
            if message == "background: background was told to fail its load"),
        "{e:?}"
    );
    assert!(!backend.is_loaded());
    assert!(
        backend
            .process(&gpu, &FrameParams::default(), &frame, &frame)
            .is_err()
    );

    // Likewise on a frame.
    req.params = BTreeMap::new();
    backend.load(&gpu, &req).unwrap();
    let live = FrameParams {
        blur_strength: 0.5,
        live: params("background", &[("fail", Value::Bool(true))]),
    };
    let e = backend.process(&gpu, &live, &frame, &frame).unwrap_err();
    assert!(
        matches!(&e, BackendError::Run { message, .. }
            if message == "background: background was told to fail"),
        "{e:?}"
    );
    assert!(!backend.is_loaded());

    // A stage failure that names no stage of the request: a failure of the
    // chain, and host and module agree that nothing is loaded.
    backend.load(&gpu, &req).unwrap();
    req.params = params("alpha", &[("blame", Value::Str("zeta".into()))]);
    let e = backend.load(&gpu, &req).unwrap_err();
    assert!(matches!(e, BackendError::Load { .. }), "{e:?}");
    assert!(!backend.is_loaded());
    let before = lines
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, _, m)| m.starts_with("fixture-process"))
        .count();
    assert!(
        backend
            .process(&gpu, &FrameParams::default(), &frame, &frame)
            .is_err()
    );
    let after = lines
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, _, m)| m.starts_with("fixture-process"))
        .count();
    assert_eq!(before, after, "the module was not asked");
}

#[test]
fn the_factories_probe_runs_the_modules_deeper_check() {
    let (_dir, module, lines) = fixture(serde_json::json!({}));
    ModuleBackendFactory::new(module.clone()).probe().unwrap();
    assert_eq!(
        last_line(&lines, "fixture-check"),
        "fixture-check stages=background"
    );
    ModuleEnhancerFactory::new(module).probe().unwrap();
    assert_eq!(
        last_line(&lines, "fixture-check"),
        "fixture-check stages=enhance"
    );

    let (_dir, module, _lines) = fixture(serde_json::json!({"check_fails": "No runtime"}));
    assert_eq!(
        ModuleBackendFactory::new(module.clone()).probe(),
        Err(BackendError::Unavailable("No runtime".into()))
    );
    assert_eq!(
        ModuleEnhancerFactory::new(module.clone()).probe(),
        Err(BackendError::Unavailable("No runtime".into()))
    );
    // The filesystem probe still answers.
    assert!(module.probe().is_ok());
}

#[test]
fn a_library_opened_again_keeps_its_state() {
    let (dir, module, lines) = fixture(serde_json::json!({}));
    assert_eq!(last_line(&lines, "fixture-init"), "fixture-init n=1");
    // The last handle goes: the library is closed, its code stays.
    drop(module);
    let m = Manifest::load(&dir.path().join("fixture.toml")).unwrap();
    let (sink, lines) = capture();
    let again = Module::open(&m, &serde_json::json!({}), sink).unwrap();
    assert_eq!(
        last_line(&lines, "fixture-init"),
        "fixture-init n=2",
        "the statics of the first open survive, and the new host gets the log"
    );
    let factory = ModuleChainFactory::new(again, &[s("alpha")]).unwrap();
    let mut chain = factory.create().unwrap();
    chain
        .load(&host_request(&["alpha"], BTreeMap::new()))
        .unwrap();
}

#[test]
fn a_registry_open_fills_the_settings_cache_the_factory_checks_against() {
    let dir = tempfile::tempdir().unwrap();
    let lib = own_copy(&fixture_lib(""), dir.path(), "registry");
    std::fs::write(
        dir.path().join("fixture.toml"),
        fixture_manifest("org.test.fixture", &lib, 1),
    )
    .unwrap();
    let mut registry = Registry::new(scan(&[dir.path().to_path_buf()]));
    let (sink, lines) = capture();
    registry.open_all(|_| serde_json::json!({}), &sink);
    let module = registry.module("org.test.fixture").unwrap().clone();
    assert!(module.cached_settings().is_some());
    let described = lines
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, _, m)| m == "fixture-settings")
        .count();
    assert_eq!(described, 1);
    let p = |v: i64| -> Params { [("step".to_string(), Value::Int(v))].into_iter().collect() };
    let factory = ModuleBackendFactory::new(module);
    assert!(
        factory
            .check_params(&s("alpha"), &p(90), &Params::new())
            .is_err()
    );
}

// ---------------------------------------------------------------------------
// Audio modules (ABI 1.2)
// ---------------------------------------------------------------------------

/// One mono block of `n` samples, ramping from 0.
fn ramp(n: usize) -> Vec<f32> {
    (0..n).map(|i| i as f32 / n as f32 - 0.5).collect()
}

fn audio_spec(stages: &[&str], params: &[(&str, &str, Value)]) -> AudioSpec {
    AudioSpec {
        stages: stages.iter().map(|i| s(i)).collect(),
        sample_rate: 48_000,
        channels_in: 1,
        channels_out: 1,
        frame_samples: 480,
        params: params
            .iter()
            .map(|(st, k, v)| (s(st), k.to_string(), v.clone()))
            .collect(),
    }
}

/// An audio registry over `dir`, opened, with its log.
fn audio_registry(dir: &Path) -> (Registry, Lines) {
    let mut registry = Registry::new(scan(&[dir.to_path_buf()])).with_kinds(LoadKinds::Audio);
    let (sink, lines) = capture();
    registry.open_all(|_| serde_json::json!({}), &sink);
    (registry, lines)
}

#[test]
fn an_audio_module_loads_only_through_the_audio_kinds_and_runs_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let lib = own_copy(&fixture_lib("audio"), dir.path(), "audio");
    std::fs::write(
        dir.path().join("audio.toml"),
        fixture_audio_manifest("org.test.audio", &lib),
    )
    .unwrap();
    let m = Manifest::load(&dir.path().join("audio.toml")).unwrap();

    // The default (video) host never opens it.
    let (sink, _) = capture();
    assert_eq!(
        Module::open(&m, &serde_json::json!({}), sink).unwrap_err(),
        REASON_AUDIO_IN_WORKER
    );
    let mut video = Registry::new(scan(&[dir.path().to_path_buf()]));
    let (sink, _) = capture();
    video.open_all(|_| serde_json::json!({}), &sink);
    assert_eq!(
        video.status("org.test.audio"),
        Some(ModuleStatus::Unavailable(REASON_AUDIO_IN_WORKER.into()))
    );
    assert!(video.stage_ids().is_empty());

    // The audio host does.
    let (registry, lines) = audio_registry(dir.path());
    assert_eq!(
        registry.status("org.test.audio"),
        Some(ModuleStatus::Available)
    );
    assert_eq!(
        registry.order(),
        [s("mic-dereverb"), s("mic-denoise"), s("mic-enhance")],
        "clean before shape, then the position"
    );
    let module = registry.module("org.test.audio").unwrap().clone();
    assert_eq!(
        obc_module_host::library::version_text(module.library().abi_version()),
        "1.2"
    );
    assert!(module.library().has_audio());
    assert!(module.matte_stages().is_empty());
    let settings = module.settings().unwrap();
    assert_eq!(settings.groups[0].surface.as_deref(), Some("mic"));
    assert_eq!(
        settings.enable_of("mic-denoise").unwrap().id,
        "mic.fixture.denoise"
    );
    assert_eq!(
        settings.param("mic-enhance", "gain_pct").unwrap().id,
        "mic.fixture.gain"
    );
    let stages = [s("mic-denoise"), s("mic-enhance")];
    assert_eq!(
        common_block_formats(module.manifest(), &stages).unwrap(),
        (vec![48_000], vec![480], 1)
    );
    // A video instance of it is still an instance; an audio one of a video
    // module is refused (below). An audio instance:
    let mut inst = module.create_audio_instance().unwrap();
    assert!(!inst.is_loaded());
    let input = ramp(480);
    let mut out = vec![0.0f32; 480];
    let e = inst
        .process(&[&input], &mut [&mut out], AudioBlock::default(), &[])
        .unwrap_err();
    assert_eq!(e.status, Status::State);

    let outcome = inst
        .load(&audio_spec(
            &["mic-denoise", "mic-enhance"],
            &[("mic-enhance", "gain_pct", Value::Int(200))],
        ))
        .unwrap();
    assert_eq!(
        (
            outcome.frame_samples,
            outcome.latency_samples,
            outcome.reused_model
        ),
        (480, 20, false)
    );
    assert_eq!(inst.frame_samples(), Some(480));
    assert_eq!(inst.latency_samples(), Some(20));
    let line = last_line(&lines, "fixture-audio-load");
    assert!(
        line.contains("stages=mic-denoise,mic-enhance rate=48000 in=1 out=1 frame=480")
            && line.contains("mic-enhance.gain_pct=200"),
        "{line}"
    );
    let block = AudioBlock {
        pts_ns: Some(1_000),
        flags: 0xff,
    };
    let voice = inst
        .process(&[&input], &mut [&mut out], block, &[])
        .unwrap();
    assert_eq!(voice, Some(0.5));
    let doubled: Vec<f32> = input.iter().map(|x| x * 2.0).collect();
    assert_eq!(out, doubled);
    let line = last_line(&lines, "fixture-audio-process");
    assert!(
        line.contains("n=1 flags=3 pts=Some(1000)"),
        "unknown flags cleared: {line}"
    );
    // A live gain.
    let half = Value::Int(50);
    inst.process(
        &[&input],
        &mut [&mut out],
        AudioBlock::default(),
        &[("mic-enhance", "gain_pct", &half)],
    )
    .unwrap();
    let halved: Vec<f32> = input.iter().map(|x| x * 0.5).collect();
    assert_eq!(out, halved);

    // Blocks of another shape are refused before the module is called.
    let short = vec![0.0f32; 240];
    let e = inst
        .process(&[&short], &mut [&mut out], AudioBlock::default(), &[])
        .unwrap_err();
    assert_eq!(e.status, Status::Invalid);
    let e = inst
        .process(
            &[&input, &input],
            &mut [&mut out],
            AudioBlock::default(),
            &[],
        )
        .unwrap_err();
    assert_eq!(e.status, Status::Invalid);

    // A stage that fails on a block is dropped; the next runs without it.
    let yes = Value::Bool(true);
    let e = inst
        .process(
            &[&input],
            &mut [&mut out],
            AudioBlock::default(),
            &[("mic-enhance", "fail", &yes)],
        )
        .unwrap_err();
    assert_eq!(
        (e.status, e.stage.clone()),
        (Status::Stage, Some(s("mic-enhance")))
    );
    assert_eq!(inst.chain(), [s("mic-denoise")]);
    inst.process(&[&input], &mut [&mut out], AudioBlock::default(), &[])
        .unwrap();
    assert_eq!(out, input, "a pass-through without the gain");

    // A switch that adds a stage failing its load keeps the previous chain:
    // the failure names the added stage, read against the request.
    let e = inst
        .load(&audio_spec(
            &["mic-denoise", "mic-enhance"],
            &[("mic-enhance", "fail_load", Value::Bool(true))],
        ))
        .unwrap_err();
    assert_eq!(
        (e.status, e.stage.clone()),
        (Status::Stage, Some(s("mic-enhance")))
    );
    assert!(inst.is_loaded());
    assert_eq!(inst.chain(), [s("mic-denoise")]);
    inst.process(&[&input], &mut [&mut out], AudioBlock::default(), &[])
        .unwrap();
    assert_eq!(out, input, "the previous chain still runs");

    // A spec the manifest does not declare is refused before the module is
    // called, the chain kept.
    for (what, spec) in [
        ("a video stage", audio_spec(&["mic-denoise", "alpha"], &[])),
        (
            "an undeclared rate",
            AudioSpec {
                sample_rate: 44_100,
                ..audio_spec(&["mic-denoise"], &[])
            },
        ),
        (
            "an undeclared block size",
            AudioSpec {
                frame_samples: 256,
                ..audio_spec(&["mic-denoise"], &[])
            },
        ),
        (
            "undeclared input channels",
            AudioSpec {
                channels_in: 2,
                ..audio_spec(&["mic-denoise"], &[])
            },
        ),
        (
            "undeclared output channels",
            AudioSpec {
                channels_out: 2,
                ..audio_spec(&["mic-denoise"], &[])
            },
        ),
    ] {
        let before = count(&lines, "fixture-audio-load");
        let e = inst.load(&spec).unwrap_err();
        assert_eq!(e.status, Status::Invalid, "{what}: {e}");
        assert_eq!(count(&lines, "fixture-audio-load"), before, "{what}");
        assert_eq!(inst.chain(), [s("mic-denoise")], "{what}");
        assert!(inst.is_loaded(), "{what}");
    }

    // A failed switch keeps the chain; a module's own block size is taken.
    let e = inst
        .load(&audio_spec(
            &["mic-denoise"],
            &[("mic-denoise", "fail_load", Value::Bool(true))],
        ))
        .unwrap_err();
    assert_eq!(e.stage, Some(s("mic-denoise")));
    assert!(
        inst.chain().is_empty(),
        "the host no longer counts on the stage"
    );
    let outcome = inst
        .load(&audio_spec(
            &["mic-denoise"],
            &[("mic-denoise", "frame_samples", Value::Int(960))],
        ))
        .unwrap();
    assert_eq!(outcome.frame_samples, 960);
    let big = ramp(960);
    let mut big_out = vec![0.0f32; 960];
    inst.process(&[&big], &mut [&mut big_out], AudioBlock::default(), &[])
        .unwrap();
    assert_eq!(big_out, big);
    // A block size the manifest does not declare for every stage unloads.
    let e = inst
        .load(&audio_spec(
            &["mic-denoise", "mic-enhance"],
            &[("mic-denoise", "frame_samples", Value::Int(960))],
        ))
        .unwrap_err();
    assert_eq!(e.status, Status::Failed);
    assert!(e.message.contains("960 samples"), "{e}");
    assert!(!inst.is_loaded());

    // A panic unloads and poisons until the next load.
    inst.load(&audio_spec(&["mic-denoise"], &[])).unwrap();
    let e = inst
        .process(
            &[&input],
            &mut [&mut out],
            AudioBlock::default(),
            &[("mic-denoise", "panic", &yes)],
        )
        .unwrap_err();
    assert_eq!(e.status, Status::Panic);
    assert!(!inst.is_loaded());
    let e = inst
        .process(&[&input], &mut [&mut out], AudioBlock::default(), &[])
        .unwrap_err();
    assert!(e.message.contains("panicked earlier"), "{e}");
    inst.load(&audio_spec(&["mic-denoise"], &[])).unwrap();
    inst.process(&[&input], &mut [&mut out], AudioBlock::default(), &[])
        .unwrap();
    inst.unload();
    assert!(!inst.is_loaded());
}

#[test]
fn audio_modules_without_the_audio_abi_and_video_modules_in_the_worker_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let write =
        |name: &str, text: String| std::fs::write(d.join(format!("{name}.toml")), text).unwrap();
    // An audio manifest over a video-only library of ABI 1.2, and over one
    // of ABI 1.1.
    write(
        "no-entries",
        fixture_audio_manifest("org.test.no-entries", &fixture_lib("")),
    );
    write(
        "old",
        fixture_audio_manifest("org.test.old", &fixture_lib("abi-1-1")),
    );
    write(
        "video",
        fixture_manifest("org.test.video", &fixture_lib(""), 1),
    );
    let (registry, _) = audio_registry(d);
    let reason = |id: &str| match registry.status(id).unwrap() {
        ModuleStatus::Unavailable(r) => r,
        other => panic!("{id} is {other:?}"),
    };
    assert_eq!(
        reason("org.test.no-entries"),
        "The module's vtable lacks the entries an audio-filter module needs: \
         audio_instance_load, audio_instance_process"
    );
    assert_eq!(
        reason("org.test.old"),
        "The library is built for module ABI 1.1; an audio-filter module needs module ABI 1.2 \
         or later"
    );
    assert_eq!(reason("org.test.video"), REASON_VIDEO_IN_DAEMON);

    // An audio instance of a video module is refused.
    let (_dir, video, _) = fixture(serde_json::json!({}));
    let e = video.create_audio_instance().unwrap_err();
    assert_eq!(e.status, Status::Unsupported);

    // A video instance of an audio module is refused.
    let dir = tempfile::tempdir().unwrap();
    let lib = own_copy(&fixture_lib("audio"), dir.path(), "audio");
    std::fs::write(
        dir.path().join("audio.toml"),
        fixture_audio_manifest("org.test.audio", &lib),
    )
    .unwrap();
    let (registry, _) = audio_registry(dir.path());
    let audio = registry.module("org.test.audio").unwrap();
    let e = audio.create_instance().unwrap_err();
    assert_eq!(e.status, Status::Unsupported);
    assert!(e.message.contains("audio-filter"), "{e}");
    // A first load whose stage fails names it, read against the request,
    // and leaves the instance unloaded.
    let mut inst = audio.create_audio_instance().unwrap();
    let e = inst
        .load(&audio_spec(
            &["mic-denoise", "mic-enhance"],
            &[("mic-enhance", "fail_load", Value::Bool(true))],
        ))
        .unwrap_err();
    assert_eq!(
        (e.status, e.stage.clone()),
        (Status::Stage, Some(s("mic-enhance")))
    );
    assert!(!inst.is_loaded());
    assert!(inst.chain().is_empty());
}

/// The environment variables the audio fixture reads.
const FIXTURE_ENV: [&str; 4] = [
    "OBC_FIXTURE_AUDIO_FAIL_LOAD",
    "OBC_FIXTURE_AUDIO_FAIL_AT",
    "OBC_FIXTURE_AUDIO_ABORT_AT",
    "OBC_FIXTURE_AUDIO_SLEEP_MS",
];

/// Set by the parent of [`audio_fixture_child`]: the library to load.
const CHILD_LIB: &str = "OBC_TEST_AUDIO_CHILD_LIB";

/// Runs only as a child of
/// [`the_audio_fixture_fails_and_aborts_as_its_environment_says`] (a no-op
/// otherwise): loads the audio fixture at `$OBC_TEST_AUDIO_CHILD_LIB` and
/// runs five blocks, printing what each call gave.
#[test]
fn audio_fixture_child() {
    let Some(lib) = std::env::var_os(CHILD_LIB) else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let m = manifest(
        &fixture_audio_manifest("org.test.child", Path::new(&lib)),
        dir.path(),
        "child",
    );
    let module = Module::open_for(
        &m,
        &serde_json::json!({}),
        obc_module_host::stderr_log(),
        LoadKinds::Audio,
    )
    .unwrap();
    let mut inst = module.create_audio_instance().unwrap();
    match inst.load(&audio_spec(&["mic-denoise", "mic-enhance"], &[])) {
        Ok(_) => eprintln!("child: load ok"),
        Err(e) => {
            eprintln!(
                "child: load err status={:?} stage={:?} {e}",
                e.status,
                e.stage.as_ref().map(|s| s.as_str())
            );
            return;
        }
    }
    let input = ramp(480);
    let mut out = vec![0.0f32; 480];
    for n in 1..=5 {
        let start = std::time::Instant::now();
        let r = inst.process(&[&input], &mut [&mut out], AudioBlock::default(), &[]);
        let ms = start.elapsed().as_millis();
        match r {
            Ok(_) => eprintln!("child: block {n} ok {ms}ms"),
            Err(e) => eprintln!("child: block {n} err {e}"),
        }
    }
}

#[test]
fn the_audio_fixture_fails_and_aborts_as_its_environment_says() {
    let lib = fixture_lib("audio");
    let run = |env: &[(&str, &str)]| {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            "audio_fixture_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_LIB, &lib);
        for k in FIXTURE_ENV {
            cmd.env_remove(k);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        (
            out.status,
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };

    let (status, err) = run(&[]);
    assert!(status.success(), "{err}");
    assert!(
        err.contains("child: load ok") && err.contains("child: block 5 ok"),
        "{err}"
    );

    let (status, err) = run(&[("OBC_FIXTURE_AUDIO_FAIL_LOAD", "mic-enhance")]);
    assert!(status.success(), "{err}");
    assert!(
        err.contains(
            "child: load err status=Stage stage=Some(\"mic-enhance\") \
             mic-enhance: mic-enhance was told to fail its load"
        ),
        "{err}"
    );
    let (_, err) = run(&[("OBC_FIXTURE_AUDIO_FAIL_LOAD", "chain")]);
    assert!(
        err.contains(
            "child: load err status=Failed stage=None the audio chain was told to fail its load"
        ),
        "{err}"
    );

    let (status, err) = run(&[("OBC_FIXTURE_AUDIO_FAIL_AT", "2")]);
    assert!(status.success(), "{err}");
    assert!(
        err.contains("child: block 1 ok")
            && err.contains("child: block 2 err mic-denoise: mic-denoise was told to fail")
            && err.contains("child: block 3 ok"),
        "{err}"
    );

    let (_, err) = run(&[("OBC_FIXTURE_AUDIO_SLEEP_MS", "30")]);
    let slow = err
        .lines()
        .filter_map(|l| l.strip_prefix("child: block "))
        .filter_map(|l| l.split(' ').nth(2)?.strip_suffix("ms")?.parse::<u64>().ok())
        .filter(|ms| *ms >= 30)
        .count();
    assert_eq!(slow, 5, "{err}");

    // A crash of native code: the process dies on block 3, by SIGABRT.
    let (status, err) = run(&[("OBC_FIXTURE_AUDIO_ABORT_AT", "3")]);
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(status.signal(), Some(6), "{status:?}: {err}");
    assert!(
        err.contains("child: block 2 ok")
            && err.contains("fixture-audio-abort n=3")
            && !err.contains("child: block 3"),
        "{err}"
    );
}

#[test]
fn hello_audio_is_discovered_in_the_worker_and_shapes_the_voice() {
    let lib = hello_audio_lib();
    let modules_d = tempfile::tempdir().unwrap();
    let shipped =
        std::fs::read_to_string(repo().join("examples/module-hello-audio/hello-audio.toml"))
            .unwrap();
    let installed = shipped.replace(
        "library = \"target/release/libobc_module_hello_audio.so\"",
        &format!("library = \"{}\"", lib.display()),
    );
    assert_ne!(shipped, installed, "the example's library line moved");
    std::fs::write(modules_d.path().join("hello-audio.toml"), installed).unwrap();
    const ID: &str = "org.example.hello-audio";

    // Listed, never opened, by the video daemon's registry.
    let mut video = Registry::new(scan(&[modules_d.path().to_path_buf()]));
    let (sink, _) = capture();
    video.open_all(|_| serde_json::json!({}), &sink);
    assert_eq!(
        video.status(ID),
        Some(ModuleStatus::Unavailable(REASON_AUDIO_IN_WORKER.into()))
    );

    let (registry, _lines) = audio_registry(modules_d.path());
    assert_eq!(registry.status(ID), Some(ModuleStatus::Available));
    assert_eq!(
        registry.resolve("mic-enhance").provider.as_deref(),
        Some(ID)
    );
    let module = registry.module(ID).unwrap().clone();
    let settings = module.settings().unwrap();
    assert_eq!(settings.groups.len(), 1);
    assert_eq!(settings.groups[0].surface.as_deref(), Some("mic"));
    assert_eq!(
        settings.enable_of("mic-enhance").unwrap().id,
        "mic.hello.enabled"
    );
    assert!(matches!(
        settings.param("mic-enhance", "gain_db").unwrap().kind,
        SettingType::Int {
            min: -12,
            max: 12,
            default: 0,
            ..
        }
    ));
    let (rates, sizes, channels) =
        common_block_formats(module.manifest(), &[s("mic-enhance")]).unwrap();
    assert_eq!((rates[0], sizes[0], channels), (48_000, 480, 1));

    let mut inst = module.create_audio_instance().unwrap();
    let outcome = inst
        .load(&audio_spec(
            &["mic-enhance"],
            &[("mic-enhance", "gain_db", Value::Int(6))],
        ))
        .unwrap();
    assert_eq!((outcome.frame_samples, outcome.latency_samples), (480, 0));
    // +6 dB is a gain of 10^(6/20), about 1.995.
    let input = vec![0.1f32; 480];
    let mut out = vec![0.0f32; 480];
    inst.process(&[&input], &mut [&mut out], AudioBlock::default(), &[])
        .unwrap();
    let gain = 10f32.powf(6.0 / 20.0);
    assert!(
        out.iter().all(|x| (x - 0.1 * gain).abs() < 1e-6),
        "{:?}",
        &out[..4]
    );
    // A live 0 dB is the identity.
    let zero = Value::Int(0);
    inst.process(
        &[&input],
        &mut [&mut out],
        AudioBlock::default(),
        &[("mic-enhance", "gain_db", &zero)],
    )
    .unwrap();
    assert_eq!(out, input);
    // The test tone: a 1 kHz sine at the level asked, added to silence.
    inst.load(&audio_spec(
        &["mic-enhance"],
        &[
            ("mic-enhance", "tone", Value::Bool(true)),
            ("mic-enhance", "tone_level_pct", Value::Int(50)),
        ],
    ))
    .unwrap();
    let silence = vec![0.0f32; 480];
    inst.process(&[&silence], &mut [&mut out], AudioBlock::default(), &[])
        .unwrap();
    let peak = out.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    assert!((peak - 0.5).abs() < 1e-3, "peak {peak}");
    // 1 kHz at 48 kHz: 48 samples a period, so sample 12 is the crest.
    assert!(
        out[0].abs() < 1e-6 && (out[12] - 0.5).abs() < 1e-4,
        "{:?}",
        &out[..13]
    );
    // The phase carries on across blocks: 480 is 10 whole periods.
    inst.process(&[&silence], &mut [&mut out], AudioBlock::default(), &[])
        .unwrap();
    assert!(out[0].abs() < 1e-4 && (out[12] - 0.5).abs() < 1e-4);
}
