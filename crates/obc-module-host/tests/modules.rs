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
    Availability, BackendError, BackendFactory, BackgroundFrame, ChainFrame, ChainRequest,
    ColourSpec, DeviceNv12, EffectKind, FrameKind, FrameParams, GpuContext, HostBgr, HostNv12,
    HostNv12View, LoadRequest, Params, StageChainFactory, StageId, StageList, Value,
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
    assert_eq!(
        registry.resolve("x-example-hello").provider.as_deref(),
        Some(ID)
    );
    assert_eq!(registry.order(), [s("x-example-hello")]);

    let module = registry.module(ID).unwrap().clone();
    assert_eq!(
        module.probe().unwrap(),
        [(s("x-example-hello"), Availability::Installed)]
    );
    let settings = module.settings().unwrap();
    assert_eq!(settings.groups.len(), 1);
    assert_eq!(
        (
            settings.groups[0].id.as_str(),
            settings.groups[0].label.as_str()
        ),
        ("x_example_hello", "Hello module")
    );
    assert_eq!(
        settings.enable_of("x-example-hello").unwrap().id,
        "effect.x_example_hello.enabled"
    );
    let strength = settings.param("x-example-hello", "strength").unwrap();
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

    let factory = ModuleChainFactory::new(module, &[s("x-example-hello")]).unwrap();
    assert_eq!(factory.name(), ID);
    assert_eq!(factory.kinds(), [FrameKind::Host]);
    let mut chain = factory.create().unwrap();
    let report = chain
        .load(&host_request(
            &["x-example-hello"],
            params("x-example-hello", &[("strength", Value::Int(100))]),
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
            &params("x-example-hello", &[("strength", Value::Int(0))]),
        )
        .unwrap();
    assert_eq!(img.uv, [128; 4]);
    // The example refuses CUDA frames, as its manifest says.
    let mut req = host_request(&["x-example-hello"], BTreeMap::new());
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
        r.contains("module ABI 2.0") && r.contains("module ABI 1.6"),
        "{r}"
    );
    let r = reason("org.test.minor");
    assert!(r.contains("1.99, newer than") && r.contains("1.6"), "{r}");
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
        ("", "1.6", true),
        ("abi-1-5", "1.5", true),
        ("abi-1-3", "1.3", true),
        ("abi-1-2", "1.2", true),
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

/// The status values of a module of ABI 1.3 cross the boundary as
/// [`obc_backend::StageValue`]s, after each frame; a module of ABI 1.2 (a
/// vtable that ends before the entry) and an unloaded chain report none.
#[test]
fn status_values_cross_from_a_module_of_abi_1_3_and_none_from_one_of_1_2() {
    use obc_backend::StageValue;
    let value = |name: &str, value: f64| StageValue {
        stage: s("alpha"),
        name: name.into(),
        value,
    };
    for (features, abi, reports) in [
        ("", "1.6", true),
        ("abi-1-5", "1.5", true),
        ("abi-1-2", "1.2", false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let lib = own_copy(&fixture_lib(features), dir.path(), "fixture");
        let m = manifest(
            &fixture_manifest("org.test.fixture", &lib, 1),
            dir.path(),
            "fixture",
        );
        let module = Module::open(&m, &serde_json::json!({}), obc_module_host::stderr_log())
            .expect("the fixture opens");
        assert_eq!(
            obc_module_host::library::version_text(module.library().abi_version()),
            abi
        );
        let factory = ModuleChainFactory::new(module, &[s("alpha")]).unwrap();
        let mut chain = factory.create().unwrap();
        assert!(chain.status_values().is_empty(), "{abi}: nothing loaded");
        chain
            .load(&host_request(
                &["alpha"],
                params("alpha", &[("step", Value::Int(4))]),
            ))
            .unwrap();
        let mut img = HostImage::new(4, 2, 50, 128);
        let mut f = img.frame();
        for _ in 0..2 {
            chain
                .process(ChainFrame::Host(&mut f), &BTreeMap::new())
                .unwrap();
        }
        let expected = if reports {
            vec![value("step", 4.0), value("frames", 2.0)]
        } else {
            Vec::new()
        };
        assert_eq!(chain.status_values(), expected, "{abi}");
        chain.unload();
        assert!(chain.status_values().is_empty(), "{abi}: unloaded");
    }
}

/// A load's notice (ABI 1.5) crosses from a module of ABI 1.5 until a load
/// without one; from a module of ABI 1.4 it is never read, though the SDK
/// it is built with writes it.
#[test]
fn a_load_notice_crosses_from_a_module_of_abi_1_5_and_none_from_one_of_1_4() {
    const NOTICE: &str = "Engine A was chosen and cannot run: no device; using B";
    for (features, abi, reports) in [
        ("", "1.6", true),
        ("abi-1-5", "1.5", true),
        ("abi-1-4", "1.4", false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let lib = own_copy(&fixture_lib(features), dir.path(), "fixture");
        let m = manifest(
            &fixture_manifest("org.test.fixture", &lib, 1),
            dir.path(),
            "fixture",
        );
        let module = Module::open(&m, &serde_json::json!({}), obc_module_host::stderr_log())
            .expect("the fixture opens");
        assert_eq!(
            obc_module_host::library::version_text(module.library().abi_version()),
            abi
        );
        let factory = ModuleChainFactory::new(module, &[s("alpha")]).unwrap();
        let mut chain = factory.create().unwrap();
        let report = chain
            .load(&host_request(
                &["alpha"],
                params("alpha", &[("notice", Value::Str(NOTICE.into()))]),
            ))
            .unwrap();
        assert_eq!(report.notice.as_deref(), reports.then_some(NOTICE), "{abi}");
        let report = chain
            .load(&host_request(&["alpha"], BTreeMap::new()))
            .unwrap();
        assert_eq!(report.notice, None, "{abi}");
    }
}

/// The host reads a module's status values defensively: a failed call, a
/// null or misaligned list and one longer than the bound give none; bad
/// entries (a value that is not finite, an empty or null name, a null or
/// invalid stage id) are left out and long names cut at a character
/// boundary. The fixture's `raw-status-values` answers each call with the
/// next shape (see its documentation).
#[test]
fn status_values_from_a_misbehaving_module_are_bounded_and_filtered() {
    let dir = tempfile::tempdir().unwrap();
    let lib = own_copy(&fixture_lib("raw-status-values"), dir.path(), "fixture");
    let m = manifest(
        &fixture_manifest("org.test.fixture", &lib, 1),
        dir.path(),
        "fixture",
    );
    let module = Module::open(&m, &serde_json::json!({}), obc_module_host::stderr_log())
        .expect("the fixture opens");
    let factory = ModuleChainFactory::new(module, &[s("alpha")]).unwrap();
    let mut chain = factory.create().unwrap();
    chain
        .load(&host_request(&["alpha"], BTreeMap::new()))
        .unwrap();
    let triples = |chain: &dyn obc_backend::StageChain| {
        chain
            .status_values()
            .into_iter()
            .map(|v| (v.stage.as_str().to_string(), v.name, v.value))
            .collect::<Vec<_>>()
    };

    assert_eq!(triples(&*chain), [], "a failed call");
    assert_eq!(triples(&*chain), [], "a null list");
    assert_eq!(triples(&*chain), [], "a misaligned list");
    assert_eq!(triples(&*chain), [], "65 entries, over the bound");
    let at_bound = triples(&*chain);
    assert_eq!(at_bound.len(), 64, "64 entries, at the bound");
    assert_eq!(at_bound[63], ("alpha".to_string(), "v63".to_string(), 63.0));
    let alpha = |name: &str, value: f64| ("alpha".to_string(), name.to_string(), value);
    assert_eq!(
        triples(&*chain),
        [
            alpha("zoom", 1.5),
            alpha(&"ü".repeat(32), 2.0),
            alpha(&"a".repeat(63), 2.0),
        ],
        "the bad entries are left out, the long names cut"
    );
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
    // The background stage's own parameters go with its effect.
    req.params
        .extend(params("background", &[("chair", Value::Bool(true))]));
    let report = backend.load(&gpu, &req).unwrap();
    assert_eq!(report.load_ms, 1.5);
    assert_eq!(report.warmup_ms, Some(0.5));
    assert_eq!(report.notice, None);
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
        line.contains("params=alpha.step=3;background.chair=true;background.mode=replace"),
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
    // The status values of ABI 1.3 reach the effect seam too.
    let values = backend.status_values();
    assert_eq!(
        values
            .iter()
            .map(|v| (v.stage.as_str(), v.name.as_str(), v.value))
            .collect::<Vec<_>>(),
        [("alpha", "step", 3.0), ("alpha", "frames", 1.0)]
    );

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

    // The module's notice of a load (ABI 1.5) reaches the effect seam,
    // and the next load without one has none.
    let alpha = req.params.get_mut(&s("alpha")).unwrap();
    alpha.insert(
        "notice".into(),
        Value::Str("Engine A was chosen and cannot run; using B".into()),
    );
    assert_eq!(
        backend.load(&gpu, &req).unwrap().notice.as_deref(),
        Some("Engine A was chosen and cannot run; using B")
    );
    req.params.get_mut(&s("alpha")).unwrap().remove("notice");
    assert_eq!(backend.load(&gpu, &req).unwrap().notice, None);

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
    assert!(backend.status_values().is_empty());
}

/// The replace request of the per-frame background tests: `background`
/// alone, at `w` x `h`.
fn replace_request(effect: EffectKind, w: u32, h: u32) -> LoadRequest {
    LoadRequest {
        effect,
        stages: StageList::default(),
        width: w,
        height: h,
        colour: ColourSpec::FULL_BT601,
        background: Some(Arc::new(HostBgr {
            width: w,
            height: h,
            stride: 3 * w as usize,
            data: vec![7; (3 * w * h) as usize],
        })),
        params: BTreeMap::new(),
    }
}

/// A module of ABI 1.4 that sets `instance_set_background` takes a
/// per-frame background through the effect seam, on CUDA frames with the
/// chain's handles; the host refuses, without calling the module, a frame
/// of the wrong size, a call before a load and one on a chain that does
/// not replace the background; the module's own refusal is a frame error.
#[test]
fn a_per_frame_background_reaches_a_module_of_abi_1_4() {
    let (_dir, module, lines) = fixture(serde_json::json!({}));
    let factory = ModuleBackendFactory::new(module);
    let mut backend = factory.create().unwrap();
    assert!(backend.supports_live_background());
    let gpu = fake_gpu();
    let frame = BackgroundFrame::Cuda(fake_nv12(640, 360));
    assert!(matches!(
        backend.set_background_frame(Some(frame)),
        Err(BackendError::InvalidInput(m)) if m.contains("not loaded")
    ));

    backend
        .load(&gpu, &replace_request(EffectKind::Replace, 640, 360))
        .unwrap();
    backend.set_background_frame(Some(frame)).unwrap();
    assert_eq!(
        last_line(&lines, "fixture-background"),
        "fixture-background frame=cuda:nv12:640x360:2:cuda=yes:0"
    );
    // The wrong size, or host memory on a CUDA chain: never called.
    let calls = count(&lines, "fixture-background");
    let small = BackgroundFrame::Cuda(fake_nv12(320, 180));
    assert!(matches!(
        backend.set_background_frame(Some(small)),
        Err(BackendError::InvalidInput(m)) if m.contains("output size 640x360")
    ));
    let (y, uv) = (vec![1u8; 640 * 360], vec![128u8; 640 * 180]);
    let host = BackgroundFrame::Host(HostNv12View {
        y: &y,
        y_stride: 640,
        uv: &uv,
        uv_stride: 640,
        width: 640,
        height: 360,
    });
    assert!(matches!(
        backend.set_background_frame(Some(host)),
        Err(BackendError::InvalidInput(m)) if m.contains("not host frames")
    ));
    assert_eq!(count(&lines, "fixture-background"), calls);
    // Back to the load-time image.
    backend.set_background_frame(None).unwrap();
    assert_eq!(
        last_line(&lines, "fixture-background"),
        "fixture-background none"
    );
    // A frame still runs after a refused background.
    backend
        .process(
            &gpu,
            &FrameParams::default(),
            &fake_nv12(640, 360),
            &fake_nv12(640, 360),
        )
        .unwrap();

    // A blur chain does not replace the background.
    backend
        .load(&gpu, &replace_request(EffectKind::Blur, 640, 360))
        .unwrap();
    assert!(matches!(
        backend.set_background_frame(Some(frame)),
        Err(BackendError::InvalidInput(m)) if m.contains("does not replace")
    ));
    backend.unload();
    assert!(backend.set_background_frame(None).is_err());
    assert_eq!(count(&lines, "fixture-background"), calls + 1);
}

/// On host frames the background crosses as a host NV12 frame the module
/// reads; the module's refusal fails the call and the chain keeps
/// running.
#[test]
fn a_per_frame_background_reaches_a_module_on_host_frames() {
    let (_dir, module, lines) = host_fixture();
    let factory = ModuleBackendFactory::new(module);
    let mut backend = factory.create().unwrap();
    backend
        .load_host(&replace_request(EffectKind::Replace, 4, 2))
        .unwrap();
    let mut view = |luma: u8| {
        let (y, uv) = (vec![luma; 8], vec![128u8; 4]);
        let frame = BackgroundFrame::Host(HostNv12View {
            y: &y,
            y_stride: 4,
            uv: &uv,
            uv_stride: 4,
            width: 4,
            height: 2,
        });
        backend.set_background_frame(Some(frame))
    };
    view(30).unwrap();
    assert_eq!(
        last_line(&lines, "fixture-background"),
        "fixture-background frame=host:nv12:4x2:2:cuda=no:30"
    );
    let e = view(99).unwrap_err();
    assert!(
        matches!(&e, BackendError::Run { message, .. } if message.contains("luma 99")),
        "{e:?}"
    );
    assert!(backend.is_loaded());
    let mut img = HostImage::new(4, 2, 50, 128);
    let src = img.frame();
    let mut out = HostImage::new(4, 2, 0, 0);
    let mut dst = out.frame();
    backend
        .process_host(&FrameParams::default(), &src, &mut dst)
        .unwrap();
}

/// A module of ABI 1.3 is never handed a per-frame background, even when
/// its vtable is large enough to hold the entry and sets it: the backend
/// says it shows a video background as a still.
#[test]
fn a_module_of_abi_1_3_is_never_handed_a_per_frame_background() {
    let dir = tempfile::tempdir().unwrap();
    let lib = own_copy(&fixture_lib("abi-1-3"), dir.path(), "fixture");
    let m = manifest(
        &fixture_manifest("org.test.fixture", &lib, 1),
        dir.path(),
        "fixture",
    );
    let (sink, lines) = capture();
    let module = Module::open(&m, &serde_json::json!({}), sink).expect("the fixture opens");
    assert_eq!(
        obc_module_host::library::version_text(module.library().abi_version()),
        "1.3"
    );
    assert!(!module.library().has_live_background());
    let factory = ModuleBackendFactory::new(module);
    let mut backend = factory.create().unwrap();
    assert!(!backend.supports_live_background());
    backend
        .load(&fake_gpu(), &replace_request(EffectKind::Replace, 640, 360))
        .unwrap();
    let e = backend
        .set_background_frame(Some(BackgroundFrame::Cuda(fake_nv12(640, 360))))
        .unwrap_err();
    assert_eq!(
        e,
        BackendError::Unavailable("org.test.fixture shows a video background as a still".into())
    );
    assert!(backend.set_background_frame(None).is_err());
    assert_eq!(count(&lines, "fixture-background"), 0);
}

/// The fixture with a `background` that takes CUDA and host frames, as the
/// ONNX module's does.
fn host_fixture() -> (tempfile::TempDir, Module, Lines) {
    let dir = tempfile::tempdir().unwrap();
    let lib = own_copy(&fixture_lib(""), dir.path(), "fixture");
    let cuda_only = "id = \"background\"\nslot = \"matte\"\nrank = 100\ndevice = \"cpu\"\n\
                     frames = [\"cuda\"]";
    let text = fixture_manifest("org.test.fixture", &lib, 1);
    assert!(text.contains(cuda_only));
    let text = text.replace(
        cuda_only,
        &cuda_only.replace("[\"cuda\"]", "[\"cuda\", \"host\"]"),
    );
    let m = manifest(&text, dir.path(), "fixture");
    let (sink, lines) = capture();
    let module = Module::open(&m, &serde_json::json!({}), sink).expect("the fixture opens");
    (dir, module, lines)
}

#[test]
fn the_backend_factory_carries_the_effect_on_host_frames_through_the_abi() {
    let (_dir, module, lines) = host_fixture();
    let factory = ModuleBackendFactory::new(module);
    assert_eq!(factory.kinds(), [FrameKind::Cuda, FrameKind::Host]);
    let mut backend = factory.create().unwrap();
    let bgr = HostBgr {
        width: 4,
        height: 2,
        stride: 12,
        data: (0..24).map(|i| i as u8 + 7).collect(),
    };
    let mut req = LoadRequest {
        effect: EffectKind::Replace,
        stages: StageList::new([s("beta"), s("alpha")]).unwrap(),
        width: 4,
        height: 2,
        colour: ColourSpec::FULL_BT601,
        background: Some(Arc::new(bgr)),
        params: params("alpha", &[("step", Value::Int(3))]),
    };
    let report = backend.load_host(&req).unwrap();
    assert_eq!(report.load_ms, 1.5);
    let line = last_line(&lines, "fixture-load");
    assert!(line.contains("stages=alpha,background,beta"), "{line}");
    assert!(
        line.contains("kind=host in=nv12:4x2 out=nv12:4x2"),
        "{line}"
    );
    assert!(
        line.contains("matrix=Bt601 siting=Mpeg2 full=true cuda=no"),
        "no CUDA handles: {line}"
    );
    assert!(
        line.contains("params=alpha.step=3;background.mode=replace"),
        "{line}"
    );
    assert!(line.contains("bg=host:bgr:4x2:1:cuda=no:7"), "{line}");
    assert!(backend.is_loaded());

    // The two host frames cross as they are: the source's planes and the
    // destination's, never the same memory.
    let mut src_img = HostImage::new(4, 2, 16, 128);
    let mut dst_img = HostImage::new(4, 2, 0, 0);
    let (src, mut dst) = (src_img.frame(), dst_img.frame());
    let fp = FrameParams {
        blur_strength: 0.25,
        live: params("beta", &[("x", Value::Int(1))]),
    };
    backend.process_host(&fp, &src, &mut dst).unwrap();
    let line = last_line(&lines, "fixture-process");
    assert!(
        line.contains("in=host:nv12:4x2:2:cuda=no out=host:nv12:4x2:2:cuda=no same=false"),
        "{line}"
    );
    assert!(
        line.contains("live=background.blur_strength=0.25;beta.x=1"),
        "{line}"
    );
    assert_eq!(src_img.y, [16; 8], "the source is only read");
    assert_eq!(
        backend.reset_temporal_host().unwrap(),
        StageList::from(s("beta"))
    );

    // A failure of the cut-out is never a stage failure: it fails the chain
    // and unloads it, as on CUDA frames.
    let fail = FrameParams {
        blur_strength: 0.0,
        live: params("background", &[("fail", Value::Bool(true))]),
    };
    let (src, mut dst) = (src_img.frame(), dst_img.frame());
    let e = backend.process_host(&fail, &src, &mut dst).unwrap_err();
    assert!(matches!(&e, BackendError::Run { .. }), "{e:?}");
    assert!(!backend.is_loaded());
    backend.load_host(&req).unwrap();

    // A malformed frame is refused before the module.
    let mut bad = src_img.frame();
    bad.y = std::ptr::null_mut();
    let mut dst = dst_img.frame();
    assert!(matches!(
        backend.process_host(&FrameParams::default(), &bad, &mut dst),
        Err(BackendError::InvalidInput(_))
    ));

    // One memory at a time: an instance loaded on CUDA frames refuses host
    // calls.
    req.effect = EffectKind::Blur;
    backend.load(&fake_gpu(), &req).unwrap();
    assert!(
        last_line(&lines, "fixture-load").contains("kind=cuda"),
        "a reload on CUDA frames"
    );
    let (src, mut dst) = (src_img.frame(), dst_img.frame());
    assert!(matches!(
        backend.process_host(&FrameParams::default(), &src, &mut dst),
        Err(BackendError::InvalidInput(_))
    ));
    assert!(matches!(
        backend.reset_temporal_host(),
        Err(BackendError::InvalidInput(_))
    ));
    backend.unload();
    assert!(!backend.is_loaded());
    assert_eq!(backend.reset_temporal_host(), Ok(StageList::default()));
    backend.load_host(&req).unwrap();
    assert!(last_line(&lines, "fixture-load").contains("kind=host"));
}

#[test]
fn a_cuda_only_background_takes_no_host_frames() {
    let (_dir, module, _lines) = fixture(serde_json::json!({}));
    let factory = ModuleBackendFactory::new(module);
    assert_eq!(factory.kinds(), [FrameKind::Cuda]);
    let mut backend = factory.create().unwrap();
    let req = LoadRequest {
        effect: EffectKind::Blur,
        stages: StageList::default(),
        width: 4,
        height: 2,
        colour: ColourSpec::LIMITED_BT601,
        background: None,
        params: BTreeMap::new(),
    };
    assert_eq!(
        backend.load_host(&req).unwrap_err(),
        BackendError::Unavailable("org.test.fixture takes CUDA frames only".into())
    );
    assert!(!backend.is_loaded());
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
        params: Params::new(),
    };
    e.load(&gpu, &req).unwrap();
    let line = last_line(&lines, "fixture-load");
    assert!(
        line.contains("stages=enhance kind=cuda in=i420:320x180 out=nv12:640x360"),
        "{line}"
    );
    assert!(line.contains("siting=Jpeg full=false"), "{line}");
    assert!(line.contains("params= "), "none asked for: {line}");
    // The stage's load-time parameters reach the module as the enhance
    // stage's.
    let fast = EnhanceRequest {
        params: [("method".to_string(), Value::Str("fast".into()))].into(),
        ..req.clone()
    };
    e.load(&gpu, &fast).unwrap();
    let line = last_line(&lines, "fixture-load");
    assert!(line.contains("params=enhance.method=fast "), "{line}");
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
        "1.6"
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
        "mic.x_example_hello_audio.enabled"
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

// ---------------------------------------------------------------------------
// Module ABI 1.6: version gating, kinds, namespaces and the C example
// ---------------------------------------------------------------------------

/// The value of `key=` in a log line (up to the next space).
fn field<'a>(line: &'a str, key: &str) -> &'a str {
    let start = line
        .find(&format!(" {key}="))
        .unwrap_or_else(|| panic!("no {key}= in {line:?}"))
        + key.len()
        + 2;
    line[start..].split(' ').next().unwrap_or_default()
}

/// The standing compatibility test of every later host: a module built for
/// ABI 1.5 (and each older minor version) is handed nothing newer than its
/// version. The fixture logs, raw, the `struct_size`s the host set in the
/// request, the load report and the frames, the request's constants, the
/// value kinds of the parameters and the frame flags; the host's view of
/// the vtable has only the entries of the version. A later minor version
/// that hands a module something new adds its expectation here.
#[test]
fn a_module_of_an_older_minor_is_handed_nothing_newer_than_its_version() {
    use obc_module_abi::{
        OBC_CHAIN_REQUEST_SIZE_1_0, OBC_FORMAT_NV12, OBC_FRAME_FLAG_REPEAT, OBC_FRAME_HOST,
        OBC_FRAME_SIZE_1_0, OBC_LOAD_REPORT_SIZE_1_0, OBC_LOAD_REPORT_SIZE_1_5, OBC_VALUE_INT,
    };
    use obc_module_host::gating;
    for minor in (0..=5u16).rev() {
        let features = format!("abi-1-{minor}");
        let dir = tempfile::tempdir().unwrap();
        let lib = own_copy(&fixture_lib(&features), dir.path(), "fixture");
        let m = manifest(
            &fixture_manifest("org.test.fixture", &lib, 1),
            dir.path(),
            "fixture",
        );
        let (sink, lines) = capture();
        let module = Module::open(&m, &serde_json::json!({}), sink).expect("the fixture opens");
        assert_eq!(module.library().abi_minor(), minor);
        // The entries of the version only (the 1.3 fixture sets the
        // per-frame background of 1.4 in a vtable large enough for it).
        assert_eq!(
            module.library().has_live_background(),
            minor >= 4,
            "1.{minor}"
        );
        assert!(!module.library().has_audio(), "1.{minor}");

        let factory = ModuleChainFactory::new(module, &[s("alpha")]).unwrap();
        let mut chain = factory.create().unwrap();
        chain
            .load(&host_request(
                &["alpha"],
                params("alpha", &[("step", Value::Int(1))]),
            ))
            .unwrap();
        let line = last_line(&lines, "fixture-abi load");
        let report_size = if minor >= 5 {
            OBC_LOAD_REPORT_SIZE_1_5
        } else {
            OBC_LOAD_REPORT_SIZE_1_0
        };
        assert_eq!(
            field(&line, "request_size"),
            OBC_CHAIN_REQUEST_SIZE_1_0.to_string(),
            "1.{minor}: {line}"
        );
        assert_eq!(
            field(&line, "report_size"),
            report_size.to_string(),
            "1.{minor}: {line}"
        );
        assert_eq!(field(&line, "kind"), OBC_FRAME_HOST.to_string());
        assert_eq!(field(&line, "format"), OBC_FORMAT_NV12.to_string());
        assert_eq!(field(&line, "values"), OBC_VALUE_INT.to_string());

        let mut img = HostImage::new(4, 2, 50, 128);
        let mut f = img.frame();
        chain
            .process(ChainFrame::Host(&mut f), &BTreeMap::new())
            .unwrap();
        chain
            .process_repeat(ChainFrame::Host(&mut f), &BTreeMap::new())
            .unwrap();
        let line = last_line(&lines, "fixture-abi process");
        let known = gating::frame_flags(minor);
        for flags in field(&line, "flags").split('/') {
            let flags: u32 = flags.parse().unwrap();
            assert_eq!(flags & !known, 0, "1.{minor}: {line}");
        }
        let input: u32 = field(&line, "flags")
            .split('/')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            input & OBC_FRAME_FLAG_REPEAT != 0,
            minor >= 1,
            "1.{minor}: the repeat is marked from 1.1: {line}"
        );
        assert_eq!(
            field(&line, "size"),
            format!("{OBC_FRAME_SIZE_1_0}/{OBC_FRAME_SIZE_1_0}"),
            "1.{minor}: {line}"
        );
        // Status values (1.3) are asked of 1.3 and later only.
        assert_eq!(chain.status_values().is_empty(), minor < 3, "1.{minor}");
        chain.unload();
    }
}

/// Since ABI 1.6 an `audio-filter` module may leave the video entries
/// null: it loads in the audio worker and runs blocks. A `video-stage`
/// module still needs them.
#[test]
fn an_audio_module_may_leave_the_video_entries_null() {
    let dir = tempfile::tempdir().unwrap();
    let lib = own_copy(&fixture_lib("audio-null-video"), dir.path(), "audio");
    std::fs::write(
        dir.path().join("audio.toml"),
        fixture_audio_manifest("org.test.audio", &lib),
    )
    .unwrap();
    let (registry, _lines) = audio_registry(dir.path());
    assert_eq!(
        registry.status("org.test.audio"),
        Some(ModuleStatus::Available)
    );
    let module = registry.module("org.test.audio").unwrap().clone();
    let mut inst = module.create_audio_instance().unwrap();
    inst.load(&audio_spec(&["mic-denoise"], &[])).unwrap();
    let input = ramp(480);
    let mut out = vec![0.0f32; 480];
    inst.process(&[&input], &mut [&mut out], AudioBlock::default(), &[])
        .unwrap();
    assert_eq!(out, input);

    // The same library as a video module is refused before its init.
    let other = tempfile::tempdir().unwrap();
    let video = own_copy(&lib, other.path(), "video");
    let m = manifest(
        &fixture_manifest("org.test.video", &video, 1),
        other.path(),
        "video",
    );
    let e = Module::open(&m, &serde_json::json!({}), obc_module_host::stderr_log()).unwrap_err();
    assert_eq!(
        e,
        "The module's vtable lacks required entries: instance_load, instance_process"
    );
}

/// A manifest's `abi_minor` (since ABI 1.6) is checked against the
/// library's version, and a newer one than the host's is listed with the
/// version it needs, even with keys of that version.
#[test]
fn the_manifest_abi_minor_is_checked_against_the_library_and_the_host() {
    let dir = tempfile::tempdir().unwrap();
    let lib = own_copy(&fixture_lib("abi-1-4"), dir.path(), "fixture");
    let text = fixture_manifest("org.test.fixture", &lib, 1);
    let needs = |n: u32| text.replacen("abi = 1\n", &format!("abi = 1\nabi_minor = {n}\n"), 1);
    let m = manifest(&needs(5), dir.path(), "fixture");
    assert_eq!(m.abi_minor, 5);
    let e = Module::open(&m, &serde_json::json!({}), obc_module_host::stderr_log()).unwrap_err();
    assert_eq!(
        e,
        "The library is built for module ABI 1.4, older than the module ABI 1.5 its manifest \
         needs"
    );
    let m = manifest(&needs(4), dir.path(), "fixture");
    Module::open(&m, &serde_json::json!({}), obc_module_host::stderr_log())
        .expect("a library of the manifest's minor version opens");

    // Newer than this host: listed with the version it needs, never opened.
    let later = tempfile::tempdir().unwrap();
    std::fs::write(
        later.path().join("later.toml"),
        needs(99).replacen("abi_minor = 99\n", "abi_minor = 99\nlater_key = 1\n", 1),
    )
    .unwrap();
    let mut registry = Registry::new(scan(&[later.path().to_path_buf()]));
    let (sink, _) = capture();
    registry.open_all(|_| serde_json::json!({}), &sink);
    assert_eq!(
        registry.status("org.test.fixture"),
        Some(ModuleStatus::Unavailable(format!(
            "Needs module ABI 1.99; this daemon has module ABI 1.{}",
            obc_module_abi::OBC_MODULE_ABI_MINOR
        )))
    );
}

/// The namespaces of ABI 1.6: a stage id or settings group outside the
/// registry and not of the vendor form is warned about and kept; a
/// configuration key starting with `_` that the host does not define is
/// not passed, with a warning; a status value named `obc.*` is dropped,
/// logged once per instance.
#[test]
fn ids_outside_the_namespaces_are_warned_about_and_reserved_names_kept_from_modules() {
    let (_dir, module, lines) = fixture(serde_json::json!({"_debug": true, "_host": {}}));
    let warnings = |what: &str| {
        lines
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, level, m)| *level == LogLevel::Warn && m.contains(what))
            .count()
    };
    for id in ["alpha", "beta", "gamma"] {
        assert_eq!(
            warnings(&format!("the stage id {id:?} is not a well-known stage id")),
            1,
            "{id}"
        );
    }
    assert_eq!(warnings("the stage id \"background\""), 0);
    assert_eq!(warnings("the stage id \"enhance\""), 0);
    assert_eq!(
        warnings("the configuration key \"_debug\" was not passed to the module"),
        1
    );
    assert_eq!(warnings("\"_host\""), 0);
    // The group, once, at the first read of the settings.
    module.settings().unwrap();
    module.settings().unwrap();
    assert_eq!(
        warnings("the settings group \"fixture\" is not a well-known group"),
        1
    );

    let factory = ModuleChainFactory::new(module, &[s("alpha")]).unwrap();
    let mut chain = factory.create().unwrap();
    chain
        .load(&host_request(
            &["alpha"],
            params("alpha", &[("value_name", Value::Str("obc.zoom".into()))]),
        ))
        .unwrap();
    let mut img = HostImage::new(4, 2, 50, 128);
    for _ in 0..2 {
        let mut f = img.frame();
        chain
            .process(ChainFrame::Host(&mut f), &BTreeMap::new())
            .unwrap();
        let names: Vec<String> = chain.status_values().into_iter().map(|v| v.name).collect();
        assert_eq!(names, ["step", "frames"]);
    }
    assert_eq!(warnings("names starting with `obc.` are reserved"), 1);
    // A name of the module's own crosses.
    chain
        .load(&host_request(
            &["alpha"],
            params("alpha", &[("value_name", Value::Str("zoom".into()))]),
        ))
        .unwrap();
    let mut f = img.frame();
    chain
        .process(ChainFrame::Host(&mut f), &BTreeMap::new())
        .unwrap();
    let names: Vec<String> = chain.status_values().into_iter().map(|v| v.name).collect();
    assert_eq!(names, ["step", "frames", "zoom"]);
}

/// The reserved names hold a module of ABI 1.6 or later only: a module of
/// ABI 1.5 is passed a configuration key starting with `_` and keeps a
/// status value named `obc.*`, as a host of 1.5 did, each with a warning
/// (the value's logged once per instance).
#[test]
fn reserved_names_are_passed_to_a_module_of_abi_1_5_with_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    let lib = own_copy(&fixture_lib("abi-1-5"), dir.path(), "fixture");
    let m = manifest(
        &fixture_manifest("org.test.fixture", &lib, 1),
        dir.path(),
        "fixture",
    );
    let (sink, lines) = capture();
    let module = Module::open(&m, &serde_json::json!({"_debug": true, "_host": {}}), sink)
        .expect("the fixture opens");
    assert_eq!(module.library().abi_minor(), 5);
    let warnings = |what: &str| {
        lines
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, level, m)| *level == LogLevel::Warn && m.contains(what))
            .count()
    };
    assert_eq!(
        warnings(
            "the configuration key \"_debug\" starts with `_`, which is reserved for the host: \
             it is passed to this module of ABI 1.5"
        ),
        1
    );
    assert_eq!(warnings("was not passed to the module"), 0);
    assert_eq!(warnings("\"_host\""), 0);

    let factory = ModuleChainFactory::new(module, &[s("alpha")]).unwrap();
    let mut chain = factory.create().unwrap();
    chain
        .load(&host_request(
            &["alpha"],
            params("alpha", &[("value_name", Value::Str("obc.zoom".into()))]),
        ))
        .unwrap();
    let mut img = HostImage::new(4, 2, 50, 128);
    for _ in 0..2 {
        let mut f = img.frame();
        chain
            .process(ChainFrame::Host(&mut f), &BTreeMap::new())
            .unwrap();
        let names: Vec<String> = chain.status_values().into_iter().map(|v| v.name).collect();
        assert_eq!(names, ["step", "frames", "obc.zoom"]);
    }
    assert_eq!(
        warnings("it is kept from this module of ABI 1.5, but dropped from a module of ABI 1.6"),
        1
    );
    assert_eq!(warnings("was dropped"), 0);
}

/// The C example, `examples/module-hello-c`, built with `cc` from the
/// header alone into a temporary directory; `None` when there is no `cc`.
fn hello_c_lib() -> Option<PathBuf> {
    static BUILT: Mutex<Option<PathBuf>> = Mutex::new(None);
    let mut built = BUILT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = built.as_ref() {
        return Some(p.clone());
    }
    let libs = Path::new(env!("CARGO_TARGET_TMPDIR")).join("module-libs");
    std::fs::create_dir_all(&libs).unwrap();
    let out = libs.join("libobc_module_hello_c.so");
    let result = Command::new("cc")
        .args([
            "-std=c11", "-Wall", "-Wextra", "-Werror", "-shared", "-fPIC", "-I",
        ])
        .arg(repo().join("crates/obc-module-abi/include"))
        .arg("-o")
        .arg(&out)
        .arg(repo().join("examples/module-hello-c/hello.c"))
        .output();
    let output = match result {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => panic!("run cc: {e}"),
        Ok(o) => o,
    };
    assert!(
        output.status.success(),
        "cc rejected examples/module-hello-c/hello.c:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    *built = Some(out.clone());
    Some(out)
}

/// The C example, built from the header alone, is discovered with its
/// shipped manifest, probed, described, and tints a host NV12 frame:
/// instance, load, process, unload, destroy. Its ids are of the vendor
/// form, so the host warns about none.
#[test]
fn the_c_example_built_from_the_header_alone_loads_and_tints_a_host_frame() {
    let Some(lib) = hello_c_lib() else {
        eprintln!("note: no `cc` on PATH; the C example was not built or loaded");
        return;
    };
    const ID: &str = "org.example.hello-c";
    const STAGE: &str = "x-example-hello-c";
    let modules_d = tempfile::tempdir().unwrap();
    std::fs::copy(
        repo().join("examples/module-hello-c/hello-c.toml"),
        modules_d.path().join("hello-c.toml"),
    )
    .unwrap();
    std::fs::copy(&lib, modules_d.path().join("libobc_module_hello_c.so")).unwrap();
    let mut registry = Registry::new(scan(&[modules_d.path().to_path_buf()]));
    let (sink, lines) = capture();
    registry.open_all(|_| serde_json::json!({"_host": {}}), &sink);
    assert_eq!(registry.status(ID), Some(ModuleStatus::Available));
    assert_eq!(registry.resolve(STAGE).provider.as_deref(), Some(ID));

    let module = registry.module(ID).unwrap().clone();
    assert_eq!(module.manifest().abi_minor, 6);
    assert_eq!(
        obc_module_host::library::version_text(module.library().abi_version()),
        format!("1.{}", obc_module_abi::OBC_MODULE_ABI_MINOR)
    );
    assert_eq!(
        module.probe().unwrap(),
        [(s(STAGE), Availability::Installed)]
    );
    let settings = module.settings().unwrap();
    assert_eq!(settings.groups.len(), 1);
    assert_eq!(settings.groups[0].id, "x_example_hello_c");
    assert_eq!(
        settings.enable_of(STAGE).unwrap().id,
        "effect.x_example_hello_c.enabled"
    );
    assert!(matches!(
        settings.param(STAGE, "strength").unwrap().kind,
        SettingType::Int {
            min: 0,
            max: 100,
            step: 5,
            default: 50,
            ..
        }
    ));
    let warned: Vec<String> = lines
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, level, _)| *level <= LogLevel::Warn)
        .map(|(_, _, m)| m.clone())
        .collect();
    assert!(warned.is_empty(), "{warned:?}");

    let factory = ModuleChainFactory::new(module, &[s(STAGE)]).unwrap();
    assert_eq!(factory.kinds(), [FrameKind::Host]);
    let mut chain = factory.create().unwrap();
    chain
        .load(&host_request(
            &[STAGE],
            params(STAGE, &[("strength", Value::Int(100))]),
        ))
        .unwrap();
    let mut img = HostImage::new(4, 2, 60, 128);
    let mut f = img.frame();
    chain
        .process(ChainFrame::Host(&mut f), &BTreeMap::new())
        .unwrap();
    assert_eq!(img.y, [60; 8], "luma untouched");
    assert_eq!(img.uv, [104, 152, 104, 152], "the tint of module-hello");
    let mut img = HostImage::new(4, 2, 60, 128);
    let mut f = img.frame();
    chain
        .process(
            ChainFrame::Host(&mut f),
            &params(STAGE, &[("strength", Value::Int(0))]),
        )
        .unwrap();
    assert_eq!(img.uv, [128; 4], "a live strength of 0");
    // CUDA frames are refused, as the manifest says.
    let mut req = host_request(&[STAGE], BTreeMap::new());
    req.kind = FrameKind::Cuda;
    req.gpu = Some(fake_gpu());
    assert!(chain.load(&req).is_err());
    chain.unload();
    // Destroys the instance, then the module object.
    drop(chain);
    drop(factory);
    drop(registry);
}
