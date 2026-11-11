//! The module as the audio worker sees it, over a stand-in SDK
//! (`tests/fixtures/mock-afx`: the `NvAFX_*` entry points as a delay and a
//! gain, steered by a file, logging each call), so no NVIDIA software or
//! GPU is needed: its manifest; its library opened by `obc-module-host`
//! with the audio kinds; the SDK root it finds; its probe; how the stages'
//! settings become the SDK's parameters; the combined model; the live
//! strengths; failures failing open; every effect destroyed.
//!
//! The SDK finds its feature libraries on the library search path, which
//! the dynamic linker reads at process start: a test that uses the effects
//! runs in a child process of this binary started with `LD_LIBRARY_PATH`
//! leading to its tree ([`fixture`]), as the worker's drop-in starts the
//! worker.

mod support;

use std::path::Path;

use obc_backend::{Availability, Value};
use obc_module_afx::tree::MODEL_EXT;
use obc_module_host::manifest::REASON_AUDIO_IN_WORKER;
use obc_module_host::settings::{Apply, Role};
use obc_module_host::{
    AudioBlock, Device, LoadKinds, Manifest, ModuleKind, ModuleStatus, Registry, SettingType,
    Status, common_block_formats, scan,
};
use serde_json::json;
use support::*;

/// The mock's delays and gains (its defaults).
const DENOISER: (usize, f32) = (700, 0.5);
const DEREVERB_D: (usize, f32) = (300, 0.8);
const COMBINED: (usize, f32) = (500, 0.4);
const VOICE: (usize, f32) = (1100, 0.9);
const SPEAKER_FOCUS: (usize, f32) = (400, 0.7);
const PROFILE_DELAY: usize = 200;

fn config(root: &Path) -> serde_json::Value {
    json!({ "sdk_root": root, "_host": { "compute_capability": null } })
}

/// A module over a stand-in tree.
struct Fixture {
    _dir: tempfile::TempDir,
    tree: Tree,
    module: obc_module_host::Module,
}

/// A module over a fresh stand-in tree, as the worker runs it: in a child
/// process whose library search path leads to the tree's feature
/// libraries. In the test's own process, makes the tree, runs `test` (the
/// name of the calling test) in the child and returns `None`; in the
/// child, returns the fixture over that tree.
fn fixture(test: &str) -> Option<Fixture> {
    fixture_with(test, |_| {})
}

/// [`fixture`] over a tree edited by `edit` first.
fn fixture_with(test: &str, edit: impl FnOnce(&Tree)) -> Option<Fixture> {
    if let Some(root) = child_root(test) {
        let dir = tempfile::tempdir().unwrap();
        let tree = Tree::at(&root);
        let module = open(dir.path(), config(&tree.root));
        return Some(Fixture {
            _dir: dir,
            tree,
            module,
        });
    }
    if is_child() {
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let tree = Tree::new(&dir.path().join("afx"));
    edit(&tree);
    run_child(test, &tree.root, &tree.search_path(), &[]);
    None
}

/// Runs `x` through `inst` block by block; returns the output and the
/// voice probabilities.
fn run(inst: &mut obc_module_host::AudioInstance, x: &[f32]) -> (Vec<f32>, Vec<Option<f32>>) {
    run_live(inst, x, &[])
}

/// [`run`] with the live parameters `live` on every block, as the worker
/// passes them.
fn run_live(
    inst: &mut obc_module_host::AudioInstance,
    x: &[f32],
    live: &[(&str, &str, &Value)],
) -> (Vec<f32>, Vec<Option<f32>>) {
    let mut out = vec![0.0f32; HOP];
    let mut y = Vec::new();
    let mut voice = Vec::new();
    for b in x.chunks(HOP) {
        voice.push(
            inst.process(&[b], &mut [&mut out], AudioBlock::default(), live)
                .unwrap(),
        );
        y.extend_from_slice(&out);
    }
    (y, voice)
}

/// One block through `inst` with `live`, into `out`; the output is written
/// even when a stage fails.
fn block(
    inst: &mut obc_module_host::AudioInstance,
    x: &[f32],
    out: &mut [f32],
    live: &[(&str, &str, &Value)],
) -> Result<Option<f32>, obc_module_host::ModuleError> {
    inst.process(&[x], &mut [out], AudioBlock::default(), live)
}

/// Whether `y` is `x` delayed by `delay` and scaled by `gain`.
fn assert_delayed(x: &[f32], y: &[f32], delay: usize, gain: f32) {
    assert_delayed_from(x, y, delay, gain, 0);
    assert!(y[..delay].iter().all(|v| v.abs() < 1e-6));
}

/// Whether `y` is `x` delayed by `delay` and scaled by `gain` from sample
/// `from` on.
fn assert_delayed_from(x: &[f32], y: &[f32], delay: usize, gain: f32, from: usize) {
    for i in from.max(delay)..x.len() {
        assert!(
            (y[i] - gain * x[i - delay]).abs() < 1e-5,
            "sample {i}: {} is not {gain} x {} ({delay} late)",
            y[i],
            x[i - delay]
        );
    }
}

#[test]
fn the_manifest_declares_four_gpu_stages_above_the_cpu_provider() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("maxine-afx.toml");
    std::fs::copy(crate_dir().join("maxine-afx.toml"), &path).unwrap();
    let m = Manifest::load(&path).unwrap();
    assert_eq!(m.id, ID);
    assert_eq!(m.kind, ModuleKind::AudioFilter);
    assert_eq!(m.version, env!("CARGO_PKG_VERSION"), "bump the manifest");
    assert!(m.name.contains("NVIDIA Maxine"), "{}", m.name);
    assert!(m.licence.contains("NVIDIA"), "{}", m.licence);
    assert!(m.description.as_deref().unwrap().contains("NVIDIA"));
    let ids: Vec<&str> = m.stages.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, [FOCUS, DEREVERB, DENOISE, ENHANCE]);
    for st in &m.stages {
        assert_eq!((st.rank, st.device), (300, Device::NvidiaGpu), "{}", st.id);
        assert!(st.rank > 200, "above the CPU module");
        let a = st.audio.as_ref().unwrap();
        assert_eq!(
            (a.rates.as_slice(), a.frame_samples.as_slice(), a.channels),
            (&[48_000][..], &[480][..], 1)
        );
    }
    let slots: Vec<&str> = m.stages.iter().map(|s| s.slot.as_str()).collect();
    assert_eq!(slots, ["clean", "clean", "clean", "shape"]);
    let enhance = m.stage(ENHANCE).unwrap();
    assert_eq!(enhance.conflicts, [s(DENOISE), s(DEREVERB)]);
    assert!(m.stage(DEREVERB).unwrap().position < m.stage(DENOISE).unwrap().position);
    // Speaker focus first: it conflicts with nothing, and nothing with it.
    let focus = m.stage(FOCUS).unwrap();
    assert!(focus.position < m.stage(DEREVERB).unwrap().position);
    assert!(focus.conflicts.is_empty());
    assert!(m.stages.iter().all(|st| !st.conflicts.contains(&s(FOCUS))));
    let all = [s(FOCUS), s(DEREVERB), s(DENOISE), s(ENHANCE)];
    let (rates, sizes, channels) = common_block_formats(&m, &all).unwrap();
    assert_eq!((rates[0], sizes[0], channels), (48_000, 480, 1));
}

#[test]
fn the_worker_opens_it_and_the_video_daemon_never_does() {
    let Some(f) = fixture("the_worker_opens_it_and_the_video_daemon_never_does") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    install(dir.path());
    let conf = config(&f.tree.root);
    let sink = obc_module_host::stderr_log();
    let mut video = Registry::new(scan(&[dir.path().to_path_buf()]));
    video.open_all(|_| conf.clone(), &sink);
    assert_eq!(
        video.status(ID),
        Some(ModuleStatus::Unavailable(REASON_AUDIO_IN_WORKER.into()))
    );
    let mut audio = Registry::new(scan(&[dir.path().to_path_buf()])).with_kinds(LoadKinds::Audio);
    audio.open_all(|_| conf.clone(), &sink);
    assert_eq!(audio.status(ID), Some(ModuleStatus::Available));
    for st in [FOCUS, DEREVERB, DENOISE, ENHANCE] {
        assert_eq!(audio.resolve(st).provider.as_deref(), Some(ID), "{st}");
    }
}

#[test]
fn the_probe_and_the_check_read_the_tree_and_touch_no_effect() {
    let Some(f) = fixture("the_probe_and_the_check_read_the_tree_and_touch_no_effect") else {
        return;
    };
    let probe = f.module.probe().unwrap();
    assert!(
        probe.iter().all(|(_, a)| *a == Availability::Installed),
        "{probe:?}"
    );
    f.module
        .check(&[s(FOCUS), s(DEREVERB), s(DENOISE), s(ENHANCE)])
        .unwrap();
    assert_eq!(f.tree.log(), Vec::<String>::new(), "no SDK call");
}

#[test]
fn speaker_focus_not_installed_is_models_not_installed() {
    // The SDK as it is installed by default: no speaker focus feature.
    let Some(f) = fixture_with("speaker_focus_not_installed_is_models_not_installed", |t| {
        std::fs::remove_dir_all(t.root.join("features/speaker_focus")).unwrap();
    }) else {
        return;
    };
    let probe = f.module.probe().unwrap();
    let reason = |id: &str| {
        probe
            .iter()
            .find(|(s, _)| s.as_str() == id)
            .map(|(_, a)| a.clone())
            .unwrap()
    };
    assert_eq!(
        reason(FOCUS),
        Availability::NotInstalled("Models not installed".into())
    );
    for st in [DEREVERB, DENOISE, ENHANCE] {
        assert_eq!(reason(st), Availability::Installed, "{st}");
    }
    // Its library without models, or models of another GPU only.
    let dir = tempfile::tempdir().unwrap();
    let lib = f.tree.root.join("features/speaker_focus/lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("libnv_audiofx_speaker_focus.so.3"), b"").unwrap();
    let module = open(dir.path(), config(&f.tree.root));
    assert!(
        module
            .probe()
            .unwrap()
            .iter()
            .any(|(s, a)| s.as_str() == FOCUS
                && *a == Availability::NotInstalled("Models not installed".into()))
    );
    let sm = f.tree.root.join("features/speaker_focus/models/sm_120");
    std::fs::create_dir_all(&sm).unwrap();
    std::fs::write(sm.join(format!("speaker_focus_48k{MODEL_EXT}")), b"mock").unwrap();
    let mut conf = config(&f.tree.root);
    conf["_host"]["compute_capability"] = json!([8, 6]);
    let module = open(dir.path(), conf);
    assert!(
        module
            .probe()
            .unwrap()
            .iter()
            .any(|(s, a)| s.as_str() == FOCUS
                && *a == Availability::NotInstalled("Models not installed".into()))
    );
}

#[test]
fn speaker_focus_loads_its_selector_and_model_and_runs() {
    let Some(f) = fixture("speaker_focus_loads_its_selector_and_model_and_runs") else {
        return;
    };
    let mut inst = f.module.create_audio_instance().unwrap();
    let o = inst.load(&spec(&[FOCUS], &[])).unwrap();
    assert_eq!(
        (o.frame_samples, o.latency_samples, o.reused_model),
        (480, SPEAKER_FOCUS.0 as u32, false)
    );
    let log = f.tree.log();
    let at = |prefix: &str| {
        log.iter()
            .position(|l| l.starts_with(prefix))
            .unwrap_or_else(|| panic!("no {prefix:?} in {log:#?}"))
    };
    assert_eq!(log[0], "create speaker_focus");
    assert!(at("set_u32 input_sample_rate 48000 speaker_focus pre") < at("load speaker_focus 0"));
    let model = &log[at("set_str model_path")];
    assert!(
        model.contains(&format!(
            "/features/speaker_focus/models/sm_120/speaker_focus_48k{MODEL_EXT}"
        )),
        "{model}"
    );
    // No strength: no intensity_ratio, no effect version.
    assert!(f.tree.calls("set_f32").is_empty(), "{log:#?}");
    assert!(f.tree.calls("set_u32 effect_version").is_empty());
    let x = tone(HOP * 10, 0.3);
    let (y, _) = run(&mut inst, &x);
    assert_delayed(&x, &y, SPEAKER_FOCUS.0, SPEAKER_FOCUS.1);
    // First in a full chain, before the combined model; delays add up.
    let mut inst = f.module.create_audio_instance().unwrap();
    let o = inst
        .load(&spec(&[FOCUS, DEREVERB, DENOISE, ENHANCE], &[]))
        .unwrap();
    assert_eq!(
        o.latency_samples,
        (SPEAKER_FOCUS.0 + COMBINED.0 + VOICE.0) as u32
    );
    assert_eq!(
        inst.chain(),
        [s(FOCUS), s(DEREVERB), s(DENOISE), s(ENHANCE)]
    );
    inst.unload();
    assert!(
        f.tree
            .calls("destroy")
            .contains(&"destroy speaker_focus".to_string())
    );
}

#[test]
fn the_probe_names_what_is_missing() {
    // Studio voice without its models, room echo removal not installed.
    let Some(f) = fixture_with("the_probe_names_what_is_missing", |t| {
        std::fs::remove_dir_all(t.root.join("features/studio_voice/models/sm_120")).unwrap();
        std::fs::remove_dir_all(t.root.join("features/dereverb/lib")).unwrap();
    }) else {
        return;
    };
    let probe = f.module.probe().unwrap();
    let reason = |id: &str| {
        probe
            .iter()
            .find(|(s, _)| s.as_str() == id)
            .map(|(_, a)| a.clone())
            .unwrap()
    };
    assert_eq!(reason(DENOISE), Availability::Installed);
    assert_eq!(
        reason(DEREVERB),
        Availability::NotInstalled("Maxine room echo removal not installed".into())
    );
    assert_eq!(
        reason(ENHANCE),
        Availability::NotInstalled("No studio voice models installed".into())
    );
    // Models of another GPU only.
    let dir = tempfile::tempdir().unwrap();
    let mut conf = config(&f.tree.root);
    conf["_host"]["compute_capability"] = json!([8, 6]);
    let module = open(dir.path(), conf);
    let probe = module.probe().unwrap();
    assert!(probe.iter().any(|(_, a)| *a
        == Availability::NotInstalled("No noise removal models for this GPU (sm_86)".into())));
}

#[test]
fn the_configured_root_comes_first_and_a_missing_sdk_is_told() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nothing-here");
    let module = open(dir.path(), json!({ "sdk_root": missing }));
    let probe = module.probe().unwrap();
    assert!(
        probe.iter().all(|(_, a)| *a
            == Availability::NotInstalled("Maxine Audio Effects SDK not installed".into())),
        "{probe:?}"
    );
    let e = module.check(&[s(DENOISE)]).unwrap_err();
    assert_eq!(e.status, Status::Unavailable, "{e:?}");
    assert!(e.message.contains("not installed"), "{}", e.message);
    let mut inst = module.create_audio_instance().unwrap();
    let e = inst.load(&spec(&[DENOISE], &[])).unwrap_err();
    assert_eq!(e.status, Status::Unavailable, "{e:?}");
    // A configuration key the module does not know is refused.
    let e = obc_module_host::Module::open_for(
        &install(dir.path()),
        &json!({ "sdkroot": "/x" }),
        obc_module_host::stderr_log(),
        LoadKinds::Audio,
    )
    .unwrap_err();
    assert!(e.to_string().contains("sdkroot"), "{e}");
}

#[test]
fn the_settings_are_focus_noise_echo_and_studio_voice_on_the_mic_surface() {
    let Some(f) = fixture("the_settings_are_focus_noise_echo_and_studio_voice_on_the_mic_surface")
    else {
        return;
    };
    let d = f.module.settings().unwrap();
    let groups: Vec<(&str, Option<&str>)> = d
        .groups
        .iter()
        .map(|g| (g.id.as_str(), g.surface.as_deref()))
        .collect();
    assert_eq!(
        groups,
        [
            ("noise", Some("mic")),
            ("echo", Some("mic")),
            ("focus", Some("mic")),
            ("voice", Some("mic"))
        ]
    );
    let focus = d.enable_of(FOCUS).unwrap();
    assert_eq!(
        (focus.id.as_str(), focus.role),
        ("mic.focus.enabled", Some(Role::Enable))
    );
    assert!(matches!(focus.kind, SettingType::Bool { default: false }));
    assert!(focus.label.contains("other voices"), "{}", focus.label);
    assert!(
        focus.enabled_when.is_empty(),
        "composable with studio voice"
    );
    assert!(d.param(FOCUS, "strength").is_none());
    let group = d.groups.iter().find(|g| g.id == "focus").unwrap();
    assert_eq!(group.label, "Speaker focus");
    // The noise removal ids are those of the CPU provider.
    let noise = d.enable_of(DENOISE).unwrap();
    assert_eq!(
        (noise.id.as_str(), noise.role),
        ("mic.noise.enabled", Some(Role::Enable))
    );
    assert!(matches!(noise.kind, SettingType::Bool { default: true }));
    assert_eq!(
        noise.disabled_reason.as_deref(),
        Some("Studio voice already removes noise and echo")
    );
    let strength = d.param(DENOISE, "strength").unwrap();
    assert_eq!(strength.id, "mic.noise.strength");
    assert_eq!(
        strength.apply,
        Some(Apply::Live),
        "live, as the CPU provider's (one apply mode per shared id)"
    );
    assert!(matches!(
        strength.kind,
        SettingType::Int {
            min: 0,
            max: 100,
            default: 100,
            ..
        }
    ));
    let echo = d.enable_of(DEREVERB).unwrap();
    assert_eq!(echo.id, "mic.echo.enabled");
    assert!(matches!(echo.kind, SettingType::Bool { default: false }));
    let echo_strength = d.param(DEREVERB, "strength").unwrap();
    assert_eq!(echo_strength.id, "mic.echo.strength");
    assert_eq!(echo_strength.apply, Some(Apply::Live));
    assert_eq!(echo_strength.enabled_when.len(), 2, "greyed with noise on");
    assert!(
        echo_strength
            .disabled_reason
            .as_deref()
            .unwrap()
            .contains("noise removal")
    );
    let voice = d.enable_of(ENHANCE).unwrap();
    assert_eq!(voice.id, "mic.voice.enabled");
    assert!(voice.label.contains("noticeable delay"), "{}", voice.label);
    assert!(matches!(voice.kind, SettingType::Bool { default: false }));
    let profile = d.param(ENHANCE, "profile").unwrap();
    assert_eq!(profile.id, "mic.voice.profile");
    assert_eq!(profile.apply, Some(Apply::Reload), "it changes the delay");
    match &profile.kind {
        SettingType::Enum { options, default } => {
            assert_eq!(default, "passthrough");
            let values: Vec<&str> = options.iter().map(|(v, _)| v.as_str()).collect();
            assert_eq!(
                values,
                [
                    "passthrough",
                    "bright",
                    "vibrant",
                    "flat",
                    "full",
                    "thin",
                    "warm"
                ]
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn noise_removal_is_the_version_2_denoiser_with_its_parameters_in_order() {
    let Some(f) = fixture("noise_removal_is_the_version_2_denoiser_with_its_parameters_in_order")
    else {
        return;
    };
    let mut inst = f.module.create_audio_instance().unwrap();
    let o = inst.load(&spec(&[DENOISE], &[])).unwrap();
    assert_eq!(
        (o.frame_samples, o.latency_samples, o.reused_model),
        (480, DENOISER.0 as u32, false),
        "the measured delay"
    );
    let log = f.tree.log();
    let at = |prefix: &str| {
        log.iter()
            .position(|l| l.starts_with(prefix))
            .unwrap_or_else(|| panic!("no {prefix:?} in {log:#?}"))
    };
    assert_eq!(log[0], "create denoiser");
    assert!(at("set_u32 input_sample_rate 48000 denoiser pre") < at("load denoiser 0"));
    assert!(at("set_u32 effect_version 2 denoiser pre") < at("load denoiser 0"));
    let model = &log[at("set_str model_path")];
    assert!(
        model.contains(&format!(
            "/features/denoiser/models/sm_120/denoiser_v2_48k{MODEL_EXT}"
        )),
        "{model}"
    );
    // Full strength on the model, set after the load (it ignores it before).
    assert!(at("set_f32 intensity_ratio 1 denoiser post") > at("load denoiser 0"));
    // The delay was measured, then the effect reset.
    assert!(at("reset denoiser") > at("load denoiser 0"));
    // The SDK's voice activity detection is not enabled.
    assert!(f.tree.calls("set_u32 enable_vad").is_empty());
    let x = tone(48_00 * 2, 0.3);
    let (y, voice) = run(&mut inst, &x);
    assert_delayed(&x, &y, DENOISER.0, DENOISER.1);
    assert!(voice.iter().all(Option::is_none), "{voice:?}");
}

#[test]
fn a_lower_strength_mixes_in_the_input_delayed_to_match() {
    let Some(f) = fixture("a_lower_strength_mixes_in_the_input_delayed_to_match") else {
        return;
    };
    let mut inst = f.module.create_audio_instance().unwrap();
    let o = inst
        .load(&spec(&[DENOISE], &[(DENOISE, "strength", Value::Int(40))]))
        .unwrap();
    assert_eq!(o.latency_samples, DENOISER.0 as u32);
    // The model stays at full strength.
    assert!(f.tree.calls("set_f32 intensity_ratio 0.4").is_empty());
    let x = tone(4800, 0.3);
    let (y, _) = run(&mut inst, &x);
    assert_delayed(&x, &y, DENOISER.0, 0.4 * DENOISER.1 + 0.6);
    // Room echo removal alone: its model has no strength; mixed the same.
    let mut inst = f.module.create_audio_instance().unwrap();
    inst.load(&spec(
        &[DEREVERB],
        &[(DEREVERB, "strength", Value::Int(25))],
    ))
    .unwrap();
    assert!(
        f.tree
            .calls("set_f32 intensity_ratio")
            .iter()
            .all(|l| !l.contains("dereverb "))
    );
    let (y, _) = run(&mut inst, &x);
    assert_delayed(&x, &y, DEREVERB_D.0, 0.25 * DEREVERB_D.1 + 0.75);
}

#[test]
fn a_strength_is_live_and_changes_the_mix_between_blocks_with_no_reload() {
    let Some(f) = fixture("a_strength_is_live_and_changes_the_mix_between_blocks_with_no_reload")
    else {
        return;
    };
    let mut inst = f.module.create_audio_instance().unwrap();
    // As the worker loads it: the live strengths are not in the request.
    let o = inst.load(&spec(&[DENOISE], &[])).unwrap();
    assert_eq!(o.latency_samples, DENOISER.0 as u32);
    f.tree.clear_log();
    let x = tone(HOP * 12, 0.3);
    let (full, forty) = (Value::Int(100), Value::Int(40));
    let at_full = [(DENOISE, "strength", &full)];
    let at_forty = [(DENOISE, "strength", &forty)];
    let mut y = Vec::new();
    for (i, b) in x.chunks(HOP).enumerate() {
        let live: &[(&str, &str, &Value)] = if i < 4 { &at_full } else { &at_forty };
        y.extend(run_live(&mut inst, b, live).0);
    }
    // No reload, no SDK call at all: the model stays at full strength and
    // the mix changes over one block, on an input delay line that was fed
    // all along.
    assert_eq!(f.tree.log(), Vec::<String>::new());
    let (d, g) = DENOISER;
    let mix = |w: f32| w * g + (1.0 - w);
    for i in d..x.len() {
        let w = match i {
            i if i < 4 * HOP => 1.0,
            i if i < 5 * HOP => 1.0 - 0.6 * (i - 4 * HOP + 1) as f32 / HOP as f32,
            _ => 0.4,
        };
        assert!(
            (y[i] - mix(w) * x[i - d]).abs() < 1e-5,
            "sample {i}: {} is not {} x {}",
            y[i],
            mix(w),
            x[i - d]
        );
    }
    // Room echo removal's strength, live too.
    let mut inst = f.module.create_audio_instance().unwrap();
    inst.load(&spec(&[DEREVERB], &[])).unwrap();
    let quarter = Value::Int(25);
    let (y, _) = run_live(&mut inst, &x, &[(DEREVERB, "strength", &quarter)]);
    let (d, g) = DEREVERB_D;
    assert_delayed_from(&x, &y, d, 0.25 * g + 0.75, HOP);
}

#[test]
fn the_combined_model_takes_the_live_noise_strength_when_it_changes() {
    let Some(f) = fixture("the_combined_model_takes_the_live_noise_strength_when_it_changes")
    else {
        return;
    };
    let mut inst = f.module.create_audio_instance().unwrap();
    inst.load(&spec(&[DEREVERB, DENOISE], &[])).unwrap();
    f.tree.clear_log();
    let x = tone(HOP * 6, 0.3);
    let seventy = Value::Int(70);
    let echo = Value::Int(10);
    let live = [
        (DENOISE, "strength", &seventy),
        (DEREVERB, "strength", &echo),
    ];
    let (y, _) = run_live(&mut inst, &x, &live);
    // Set once, after the load, on the model; room echo removal's strength
    // is not used by it.
    assert_eq!(
        f.tree.log(),
        ["set_f32 intensity_ratio 0.7 dereverb_denoiser post"]
    );
    assert_delayed(&x, &y, COMBINED.0, 1.0 - 0.7 + 0.7 * COMBINED.1);
    assert_eq!(inst.chain(), [s(DEREVERB), s(DENOISE)]);
}

#[test]
fn a_request_of_the_same_effects_keeps_them_in_place() {
    let Some(f) = fixture("a_request_of_the_same_effects_keeps_them_in_place") else {
        return;
    };
    let mut inst = f.module.create_audio_instance().unwrap();
    inst.load(&spec(&[DENOISE], &[])).unwrap();
    let o = inst
        .load(&spec(&[DENOISE], &[(DENOISE, "strength", Value::Int(50))]))
        .unwrap();
    assert!(o.reused_model);
    assert_eq!(o.latency_samples, DENOISER.0 as u32);
    assert_eq!(f.tree.calls("create").len(), 1, "no second effect");
    let x = tone(4800, 0.3);
    let (y, _) = run(&mut inst, &x[..]);
    assert_delayed(&x, &y, DENOISER.0, 0.5 * DENOISER.1 + 0.5);
    // Another chain: a new effect, the old one destroyed.
    let o = inst.load(&spec(&[DEREVERB], &[])).unwrap();
    assert!(!o.reused_model);
    assert_eq!(
        f.tree.calls("create"),
        ["create denoiser", "create dereverb"]
    );
    assert_eq!(f.tree.calls("destroy"), ["destroy denoiser"]);
}

#[test]
fn noise_and_echo_together_run_as_the_combined_model() {
    let Some(f) = fixture("noise_and_echo_together_run_as_the_combined_model") else {
        return;
    };
    let mut inst = f.module.create_audio_instance().unwrap();
    let o = inst
        .load(&spec(
            &[DEREVERB, DENOISE],
            &[
                (DENOISE, "strength", Value::Int(70)),
                (DEREVERB, "strength", Value::Int(10)),
            ],
        ))
        .unwrap();
    assert_eq!(o.latency_samples, COMBINED.0 as u32);
    assert_eq!(f.tree.calls("create"), ["create dereverb_denoiser"]);
    assert!(
        f.tree
            .log()
            .iter()
            .any(|l| l.starts_with("set_f32 intensity_ratio 0.7 dereverb_denoiser post")),
        "noise removal's strength, on the model: {:#?}",
        f.tree.log()
    );
    let model = f.tree.calls("set_str model_path").remove(0);
    assert!(
        model.contains("/dereverb_denoiser_48k_77."),
        "the downloaded name, by pattern: {model}"
    );
    assert!(
        model.contains("/features/dereverb_denoiser/models/sm_120/"),
        "{model}"
    );
    let x = tone(4800, 0.3);
    let (y, _) = run(&mut inst, &x);
    assert_delayed(&x, &y, COMBINED.0, 1.0 - 0.7 + 0.7 * COMBINED.1);
    assert_eq!(inst.chain(), [s(DEREVERB), s(DENOISE)]);
}

#[test]
fn without_the_combined_model_the_two_run_one_after_the_other() {
    let Some(f) = fixture_with(
        "without_the_combined_model_the_two_run_one_after_the_other",
        |t| {
            std::fs::remove_dir_all(t.root.join("features/dereverb_denoiser")).unwrap();
        },
    ) else {
        return;
    };
    let mut inst = f.module.create_audio_instance().unwrap();
    let o = inst.load(&spec(&[DEREVERB, DENOISE], &[])).unwrap();
    assert_eq!(
        f.tree.calls("create"),
        ["create dereverb", "create denoiser"]
    );
    assert_eq!(o.latency_samples, (DEREVERB_D.0 + DENOISER.0) as u32);
    let x = tone(4800, 0.3);
    let (y, _) = run(&mut inst, &x);
    assert_delayed(&x, &y, DEREVERB_D.0 + DENOISER.0, DEREVERB_D.1 * DENOISER.1);
}

#[test]
fn studio_voice_takes_its_profile_after_the_load_and_no_strength() {
    let Some(f) = fixture("studio_voice_takes_its_profile_after_the_load_and_no_strength") else {
        return;
    };
    let mut inst = f.module.create_audio_instance().unwrap();
    let o = inst
        .load(&spec(
            &[ENHANCE],
            &[(ENHANCE, "profile", Value::Str("warm".into()))],
        ))
        .unwrap();
    assert_eq!(o.latency_samples, (VOICE.0 + PROFILE_DELAY) as u32);
    let log = f.tree.log();
    let at = |prefix: &str| log.iter().position(|l| l.starts_with(prefix)).unwrap();
    assert!(at("set_str mic_profile_model") < at("load studio_voice_low_latency 0"));
    assert!(
        log[at("set_str mic_profile_model")]
            .contains(&format!("studio_voice_mic_profile_48k{MODEL_EXT}"))
    );
    assert!(at("set_u32 mic_profile_id 5 studio_voice_low_latency post") > at("load"));
    assert!(f.tree.calls("set_f32").is_empty(), "no intensity: {log:#?}");
    let x = tone(4800, 0.3);
    let (y, voice) = run(&mut inst, &x);
    assert_delayed(&x, &y, VOICE.0 + PROFILE_DELAY, VOICE.1);
    assert!(voice.iter().all(Option::is_none));
    // Back to no profile, in place: -1 for the SDK, a new delay.
    let o = inst.load(&spec(&[ENHANCE], &[])).unwrap();
    assert!(o.reused_model);
    assert_eq!(o.latency_samples, VOICE.0 as u32);
    assert_eq!(f.tree.calls("set_u32 mic_profile_id 4294967295").len(), 1);
    let e = inst
        .load(&spec(
            &[ENHANCE],
            &[(ENHANCE, "profile", Value::Str("loud".into()))],
        ))
        .unwrap_err();
    assert_eq!(
        (e.status, e.stage.clone()),
        (Status::Stage, Some(s(ENHANCE)))
    );
}

#[test]
fn a_stage_that_fails_to_load_fails_alone_and_leaks_no_effect() {
    let Some(f) = fixture("a_stage_that_fails_to_load_fails_alone_and_leaks_no_effect") else {
        return;
    };
    f.tree
        .conf("fail_load=dereverb_denoiser,dereverb,studio_voice_low_latency\n");
    let mut inst = f.module.create_audio_instance().unwrap();
    // The combined model fails, then room echo removal alone.
    let e = inst.load(&spec(&[DEREVERB, DENOISE], &[])).unwrap_err();
    assert_eq!(e.status, Status::Stage, "{e:?}");
    assert_eq!(e.stage, Some(s(DEREVERB)));
    assert!(e.message.contains("(dereverb)"), "{}", e.message);
    assert!(e.message.contains("Load returned 1"), "{}", e.message);
    assert!(
        e.message.contains("maxine-afx.conf"),
        "the hint: {}",
        e.message
    );
    // As the worker does: again without the stage that failed.
    let o = inst.load(&spec(&[DENOISE], &[])).unwrap();
    assert_eq!(o.latency_samples, DENOISER.0 as u32);
    // Studio voice after a loaded chain: the chain in force is kept.
    let e = inst.load(&spec(&[ENHANCE], &[])).unwrap_err();
    assert_eq!(e.stage, Some(s(ENHANCE)));
    assert_eq!(inst.chain(), [s(DENOISE)]);
    let x = tone(4800, 0.3);
    let (y, _) = run(&mut inst, &x);
    assert!(y.iter().any(|v| *v != 0.0));
    drop(inst);
    let created = f.tree.calls("create").len();
    assert_eq!(created, 4, "{:#?}", f.tree.log());
    assert_eq!(
        f.tree.calls("destroy").len(),
        created,
        "{:#?}",
        f.tree.log()
    );
}

#[test]
fn an_effect_that_fails_on_a_block_is_replaced_by_its_delay_and_the_rest_runs() {
    let Some(f) = fixture_with(
        "an_effect_that_fails_on_a_block_is_replaced_by_its_delay_and_the_rest_runs",
        |t| {
            std::fs::remove_dir_all(t.root.join("features/dereverb_denoiser")).unwrap();
        },
    ) else {
        return;
    };
    let mut inst = f.module.create_audio_instance().unwrap();
    let o = inst.load(&spec(&[DEREVERB, DENOISE], &[])).unwrap();
    let total = DEREVERB_D.0 + DENOISER.0;
    assert_eq!(o.latency_samples, total as u32);
    let x = tone(HOP * 12, 0.3);
    let mut y = Vec::new();
    let mut out = vec![0.0f32; HOP];
    for (i, b) in x.chunks(HOP).enumerate() {
        match i {
            4 => f.tree.conf("fail_run=dereverb\n"),
            8 => f.tree.conf("fail_run=dereverb,denoiser\n"),
            _ => {}
        }
        let r = block(&mut inst, b, &mut out, &[]);
        match i {
            4 | 8 => {
                let e = r.unwrap_err();
                let want = if i == 4 { DEREVERB } else { DENOISE };
                assert_eq!((e.status, e.stage.clone()), (Status::Stage, Some(s(want))));
                assert!(e.message.contains("Run returned 1"), "{}", e.message);
            }
            _ => assert!(r.is_ok(), "block {i}: {r:?}"),
        }
        if i == 4 {
            assert_eq!(inst.chain(), [s(DENOISE)]);
            // Not destroyed on the processing thread.
            assert!(f.tree.calls("destroy").is_empty());
        }
        y.extend_from_slice(&out);
    }
    assert!(inst.chain().is_empty());
    // The output keeps the delay the chain reported, failures or not
    // (blocks that failed included): both gains until room echo removal's
    // failure reaches the denoiser's output, then the denoiser's, then, from
    // its failure on, none (the stage passes on its own input, delayed).
    let (d, g) = (DENOISER, DEREVERB_D.1);
    let from_bypass = 4 * HOP + d.0;
    let from_none = 8 * HOP;
    for i in total..x.len() {
        let gain = match i {
            i if i < from_bypass => d.1 * g,
            i if i < from_none => d.1,
            _ => 1.0,
        };
        let expect = gain * x[i - total];
        assert!(
            (y[i] - expect).abs() < 1e-5,
            "sample {i}: {} is not {expect}",
            y[i]
        );
    }
    inst.unload();
    assert_eq!(
        f.tree.calls("destroy"),
        ["destroy dereverb", "destroy denoiser"],
        "at unload"
    );
}

#[test]
fn a_failed_combined_effect_fails_both_its_stages_one_block_after_the_other() {
    let Some(f) =
        fixture("a_failed_combined_effect_fails_both_its_stages_one_block_after_the_other")
    else {
        return;
    };
    let mut inst = f.module.create_audio_instance().unwrap();
    inst.load(&spec(&[DEREVERB, DENOISE], &[])).unwrap();
    f.tree.conf("fail_run=dereverb_denoiser\n");
    let x = tone(HOP, 0.3);
    let mut out = vec![0.0f32; HOP];
    let e = block(&mut inst, &x, &mut out, &[]).unwrap_err();
    assert_eq!(e.stage, Some(s(DEREVERB)));
    assert!(
        e.message.contains("mic-dereverb and mic-denoise"),
        "{}",
        e.message
    );
    assert_eq!(inst.chain(), [s(DENOISE)]);
    let e = block(&mut inst, &x, &mut out, &[]).unwrap_err();
    assert_eq!(e.stage, Some(s(DENOISE)));
    assert!(inst.chain().is_empty());
    assert!(f.tree.calls("destroy").is_empty());
    drop(inst);
    assert_eq!(f.tree.calls("destroy"), ["destroy dereverb_denoiser"]);
}

#[test]
fn a_combined_model_that_fails_to_load_is_replaced_by_the_two_effects() {
    let Some(f) = fixture("a_combined_model_that_fails_to_load_is_replaced_by_the_two_effects")
    else {
        return;
    };
    f.tree.conf("fail_load=dereverb_denoiser\n");
    let mut inst = f.module.create_audio_instance().unwrap();
    let o = inst.load(&spec(&[DEREVERB, DENOISE], &[])).unwrap();
    assert_eq!(o.latency_samples, (DEREVERB_D.0 + DENOISER.0) as u32);
    assert_eq!(
        f.tree.calls("create"),
        [
            "create dereverb_denoiser",
            "create dereverb",
            "create denoiser"
        ]
    );
    assert_eq!(f.tree.calls("destroy"), ["destroy dereverb_denoiser"]);
    assert_eq!(inst.chain(), [s(DEREVERB), s(DENOISE)]);
    let x = tone(4800, 0.3);
    let (y, _) = run(&mut inst, &x);
    assert_delayed(&x, &y, DEREVERB_D.0 + DENOISER.0, DEREVERB_D.1 * DENOISER.1);
}

#[test]
fn a_delay_that_cannot_be_measured_fails_the_stage() {
    let Some(f) = fixture("a_delay_that_cannot_be_measured_fails_the_stage") else {
        return;
    };
    // Longer than the module looks for.
    f.tree.conf("delay.denoiser=20000\n");
    let mut inst = f.module.create_audio_instance().unwrap();
    let e = inst.load(&spec(&[DENOISE], &[])).unwrap_err();
    assert_eq!(
        (e.status, e.stage.clone()),
        (Status::Stage, Some(s(DENOISE)))
    );
    assert!(e.message.contains("could not be measured"), "{}", e.message);
    assert_eq!(f.tree.calls("destroy"), ["destroy denoiser"]);
}

#[test]
fn a_feature_library_that_cannot_be_found_fails_its_stage() {
    // The unversioned link the SDK opens is missing.
    let Some(f) = fixture_with(
        "a_feature_library_that_cannot_be_found_fails_its_stage",
        |t| {
            std::fs::remove_file(
                t.root
                    .join("features/dereverb/lib/libnv_audiofx_dereverb.so"),
            )
            .unwrap();
        },
    ) else {
        return;
    };
    let e = f.module.check(&[s(DEREVERB)]).unwrap_err();
    assert_eq!(e.status, Status::Unavailable);
    assert!(
        e.message.contains("libnv_audiofx_dereverb.so is not in"),
        "{}",
        e.message
    );
    let mut inst = f.module.create_audio_instance().unwrap();
    let e = inst.load(&spec(&[DEREVERB], &[])).unwrap_err();
    assert_eq!(e.stage, Some(s(DEREVERB)));
    // The SDK itself does not find it (a stand-in that refuses).
    f.tree.conf("fail_create=denoiser\n");
    let e = inst.load(&spec(&[DENOISE], &[])).unwrap_err();
    assert_eq!(e.stage, Some(s(DENOISE)));
    assert!(e.message.contains("effect not available"), "{}", e.message);
    assert!(f.tree.calls("destroy").is_empty());
}

#[test]
fn unload_and_drop_destroy_every_effect() {
    let Some(f) = fixture_with("unload_and_drop_destroy_every_effect", |t| {
        std::fs::remove_dir_all(t.root.join("features/dereverb_denoiser")).unwrap();
    }) else {
        return;
    };
    let mut inst = f.module.create_audio_instance().unwrap();
    // Stacked on purpose (the forced conflicts of the worker).
    inst.load(&spec(&[DEREVERB, DENOISE, ENHANCE], &[]))
        .unwrap();
    assert_eq!(f.tree.calls("create").len(), 3);
    inst.unload();
    assert_eq!(
        f.tree.calls("destroy"),
        [
            "destroy dereverb",
            "destroy denoiser",
            "destroy studio_voice_low_latency"
        ]
    );
    inst.load(&spec(&[DENOISE], &[])).unwrap();
    drop(inst);
    assert_eq!(f.tree.calls("destroy").len(), 4);
}

#[test]
fn a_new_source_resets_the_effects() {
    let Some(f) = fixture("a_new_source_resets_the_effects") else {
        return;
    };
    let mut inst = f.module.create_audio_instance().unwrap();
    inst.load(&spec(&[DENOISE], &[(DENOISE, "strength", Value::Int(50))]))
        .unwrap();
    let x = tone(HOP, 0.3);
    let mut out = vec![0.0f32; HOP];
    for _ in 0..3 {
        inst.process(&[&x], &mut [&mut out], AudioBlock::default(), &[])
            .unwrap();
    }
    f.tree.clear_log();
    let new = AudioBlock {
        pts_ns: None,
        flags: 1, /* OBC_AUDIO_FLAG_NEW_SOURCE */
    };
    inst.process(&[&x], &mut [&mut out], new, &[]).unwrap();
    assert_eq!(f.tree.calls("reset"), ["reset denoiser"]);
    // Nothing of the previous blocks comes out: the delay is silent again.
    assert!(out.iter().all(|v| *v == 0.0));
    inst.reset_temporal().unwrap();
    assert_eq!(f.tree.calls("reset").len(), 2);
}

#[test]
fn it_is_mono_48_khz_and_480_samples_only() {
    let Some(f) = fixture("it_is_mono_48_khz_and_480_samples_only") else {
        return;
    };
    let mut inst = f.module.create_audio_instance().unwrap();
    let mut stereo = spec(&[DENOISE], &[]);
    stereo.channels_in = 2;
    stereo.channels_out = 2;
    assert!(inst.load(&stereo).is_err());
    let mut rate = spec(&[DENOISE], &[]);
    rate.sample_rate = 16_000;
    assert!(inst.load(&rate).is_err());
    assert!(f.tree.log().is_empty(), "refused before the SDK");
}

#[test]
fn it_loads_runs_and_unloads_on_three_threads() {
    let Some(f) = fixture("it_loads_runs_and_unloads_on_three_threads") else {
        return;
    };
    let mut inst = f.module.create_audio_instance().unwrap();
    inst.load(&spec(&[DENOISE], &[])).unwrap();
    let x = tone(4800, 0.3);
    let mut inst = std::thread::spawn(move || {
        let (y, _) = run(&mut inst, &x);
        assert_delayed(&x, &y, DENOISER.0, DENOISER.1);
        inst
    })
    .join()
    .unwrap();
    std::thread::spawn(move || inst.unload()).join().unwrap();
    assert_eq!(f.tree.calls("destroy"), ["destroy denoiser"]);
}

/// The library search path may lead to the feature libraries through
/// links elsewhere (the tree's own unversioned links removed, a directory
/// of links on `LD_LIBRARY_PATH`), and a load failure made by the
/// environment fails its stage alone.
#[test]
fn the_search_path_may_lead_to_the_libraries_through_links() {
    const NAME: &str = "the_search_path_may_lead_to_the_libraries_through_links";
    if let Some(root) = child_root(NAME) {
        let dir = tempfile::tempdir().unwrap();
        let module = open(dir.path(), config(&root));
        module.check(&[s(DENOISE), s(DEREVERB)]).unwrap();
        let mut inst = module.create_audio_instance().unwrap();
        inst.load(&spec(&[DENOISE], &[])).unwrap();
        let e = inst.load(&spec(&[DEREVERB], &[])).unwrap_err();
        assert_eq!(e.stage, Some(s(DEREVERB)), "failed by MOCK_AFX_FAIL_LOAD");
        return;
    }
    if is_child() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let tree = Tree::new(&dir.path().join("afx"));
    let links = dir.path().join("ld");
    std::fs::create_dir_all(&links).unwrap();
    for (feature, selector, _) in FEATURES {
        let name = format!("libnv_audiofx_{selector}.so");
        let lib = tree.root.join("features").join(feature).join("lib");
        std::fs::remove_file(lib.join(&name)).unwrap();
        std::os::unix::fs::symlink(lib.join(format!("{name}.3")), links.join(&name)).unwrap();
    }
    run_child(
        NAME,
        &tree.root,
        links.as_os_str(),
        &[("MOCK_AFX_FAIL_LOAD", "dereverb")],
    );
    assert_eq!(tree.calls("create"), ["create denoiser", "create dereverb"]);
    assert_eq!(tree.calls("load"), ["load denoiser 0", "load dereverb 1"]);
}

/// Without a library search path that leads to the feature libraries (a
/// worker started without the drop-in): every stage unavailable, with a
/// reason that names the drop-in; the check and the load fail naming the
/// variable and the drop-in; no effect is created, and the working
/// directory is never changed. In this test's own process, whose
/// `LD_LIBRARY_PATH` does not name the tree.
#[test]
fn without_the_search_path_every_stage_fails_naming_the_drop_in() {
    if is_child() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let tree = Tree::new(&dir.path().join("afx"));
    let cwd = std::env::current_dir().unwrap();
    let manifests = tempfile::tempdir().unwrap();
    let module = open(manifests.path(), config(&tree.root));
    let probe = module.probe().unwrap();
    let reason = Availability::NotInstalled(obc_module_afx::REASON_NO_SEARCH_PATH.into());
    assert!(probe.iter().all(|(_, a)| *a == reason), "{probe:?}");
    assert!(obc_module_afx::REASON_NO_SEARCH_PATH.contains(DROP_IN));
    let e = module.check(&[s(DENOISE)]).unwrap_err();
    assert_eq!(e.status, Status::Unavailable);
    for stages in [&[DENOISE][..], &[DEREVERB, DENOISE], &[ENHANCE]] {
        let mut inst = module.create_audio_instance().unwrap();
        let e = inst.load(&spec(stages, &[])).unwrap_err();
        assert_eq!(e.status, Status::Stage, "{e:?}");
        assert!(
            e.message.contains("LD_LIBRARY_PATH") && e.message.contains(DROP_IN),
            "{}",
            e.message
        );
    }
    assert!(tree.calls("create").is_empty(), "{:#?}", tree.log());
    assert_eq!(std::env::current_dir().unwrap(), cwd);
}

/// The drop-in the reasons name.
const DROP_IN: &str = "omarchy-broadcast-audiod.service.d/maxine-afx.conf";
