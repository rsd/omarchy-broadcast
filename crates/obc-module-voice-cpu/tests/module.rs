//! The module as the audio worker sees it: its manifest, then its library
//! (built by cargo into a directory under `CARGO_TARGET_TMPDIR`) opened by
//! `obc-module-host` with the audio kinds, as the worker opens it.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use common::{HOP, mix, noise, rms, voice};
use obc_backend::{StageId, Value};
use obc_module_host::manifest::REASON_AUDIO_IN_WORKER;
use obc_module_host::settings::{Apply, Role};
use obc_module_host::{
    AudioBlock, AudioSpec, Device, LoadKinds, Manifest, Module, ModuleKind, ModuleStatus, Registry,
    SettingType, Status, common_block_formats, scan,
};

const ID: &str = "br.com.dias.voice-cpu";
const STAGE: &str = "mic-denoise";
const PACKAGED: &str =
    "library = \"/usr/lib/omarchy-broadcast/modules/libobc_module_voice_cpu.so\"";

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// `libobc_module_voice_cpu.so`, built once per test binary (`--locked`:
/// the workspace's `Cargo.lock` as it is).
fn library() -> PathBuf {
    static LIB: OnceLock<PathBuf> = OnceLock::new();
    LIB.get_or_init(|| {
        let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("voice-cpu-module");
        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "--quiet",
                "--locked",
                "-p",
                "obc-module-voice-cpu",
                "--manifest-path",
            ])
            .arg(crate_dir().join("../../Cargo.toml"))
            .arg("--target-dir")
            .arg(&target)
            .status()
            .expect("run cargo");
        assert!(status.success(), "building obc-module-voice-cpu failed");
        let path = target.join("debug/libobc_module_voice_cpu.so");
        assert!(path.is_file(), "{} was not built", path.display());
        path
    })
    .clone()
}

/// The shipped manifest, installed in `dir` with the built library.
fn install(dir: &Path) -> Manifest {
    let text = std::fs::read_to_string(crate_dir().join("voice-cpu.toml")).unwrap();
    let installed = text.replace(PACKAGED, &format!("library = \"{}\"", library().display()));
    assert_ne!(text, installed, "the manifest's library line moved");
    let path = dir.join("voice-cpu.toml");
    std::fs::write(&path, installed).unwrap();
    Manifest::load(&path).unwrap()
}

fn s(id: &str) -> StageId {
    StageId::new(id).unwrap()
}

fn spec(params: &[(&str, Value)]) -> AudioSpec {
    AudioSpec {
        stages: vec![s(STAGE)],
        sample_rate: 48_000,
        channels_in: 1,
        channels_out: 1,
        frame_samples: 480,
        params: params
            .iter()
            .map(|(k, v)| (s(STAGE), k.to_string(), v.clone()))
            .collect(),
    }
}

fn open(dir: &Path) -> Module {
    Module::open_for(
        &install(dir),
        &serde_json::json!({}),
        obc_module_host::stderr_log(),
        LoadKinds::Audio,
    )
    .unwrap()
}

#[test]
fn the_manifest_declares_a_cpu_mic_denoise_stage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("voice-cpu.toml");
    std::fs::copy(crate_dir().join("voice-cpu.toml"), &path).unwrap();
    let m = Manifest::load(&path).unwrap();
    assert_eq!(m.id, ID);
    assert_eq!(m.kind, ModuleKind::AudioFilter);
    assert_eq!(m.version, env!("CARGO_PKG_VERSION"), "bump the manifest");
    assert_eq!(m.stages.len(), 1);
    let st = m.stage(STAGE).unwrap();
    assert_eq!(
        (st.slot.as_str(), st.rank, st.device),
        ("clean", 200, Device::Cpu)
    );
    assert_eq!(
        st.frames.iter().map(|f| f.as_str()).collect::<Vec<_>>(),
        ["host"]
    );
    let a = st.audio.as_ref().unwrap();
    assert_eq!(
        (a.rates.as_slice(), a.frame_samples.as_slice(), a.channels),
        (&[48_000][..], &[480][..], 1)
    );
    let (rates, sizes, channels) = common_block_formats(&m, &[s(STAGE)]).unwrap();
    assert_eq!((rates[0], sizes[0], channels), (48_000, 480, 1));
}

#[test]
fn the_worker_finds_it_and_the_video_daemon_never_opens_it() {
    let dir = tempfile::tempdir().unwrap();
    install(dir.path());
    let sink = obc_module_host::stderr_log();
    let mut video = Registry::new(scan(&[dir.path().to_path_buf()]));
    video.open_all(|_| serde_json::json!({}), &sink);
    assert_eq!(
        video.status(ID),
        Some(ModuleStatus::Unavailable(REASON_AUDIO_IN_WORKER.into()))
    );
    let mut audio = Registry::new(scan(&[dir.path().to_path_buf()])).with_kinds(LoadKinds::Audio);
    audio.open_all(|_| serde_json::json!({}), &sink);
    assert_eq!(audio.status(ID), Some(ModuleStatus::Available));
    assert_eq!(audio.resolve(STAGE).provider.as_deref(), Some(ID));
}

#[test]
fn the_settings_are_noise_removal_and_the_engine_on_the_mic_surface() {
    let dir = tempfile::tempdir().unwrap();
    let module = open(dir.path());
    let d = module.settings().unwrap();
    let groups: Vec<(&str, Option<&str>)> = d
        .groups
        .iter()
        .map(|g| (g.id.as_str(), g.surface.as_deref()))
        .collect();
    assert_eq!(groups, [("noise", Some("mic")), ("cpu", Some("mic"))]);
    let enabled = d.enable_of(STAGE).unwrap();
    assert_eq!(enabled.id, "mic.noise.enabled");
    assert_eq!(enabled.role, Some(Role::Enable));
    assert!(matches!(enabled.kind, SettingType::Bool { default: true }));
    let strength = d.param(STAGE, "strength").unwrap();
    assert_eq!(strength.id, "mic.noise.strength");
    assert_eq!(strength.apply, Some(Apply::Live));
    assert!(matches!(
        strength.kind,
        SettingType::Int {
            min: 0,
            max: 100,
            default: 100,
            ..
        }
    ));
    let engine = d.param(STAGE, "engine").unwrap();
    assert_eq!(engine.id, "mic.cpu.engine");
    assert_eq!(engine.apply, Some(Apply::Reload));
    match &engine.kind {
        SettingType::Enum { options, default } => {
            assert_eq!(default, "deepfilternet");
            let values: Vec<&str> = options.iter().map(|(v, _)| v.as_str()).collect();
            assert_eq!(values, ["deepfilternet", "rnnoise"]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_engine_reloads_and_the_strength_changes_live() {
    let dir = tempfile::tempdir().unwrap();
    let module = open(dir.path());
    let mut inst = module.create_audio_instance().unwrap();

    // DeepFilterNet by default: 30 ms of delay, no voice probability.
    let o = inst.load(&spec(&[])).unwrap();
    assert_eq!(
        (o.frame_samples, o.latency_samples, o.reused_model),
        (480, 1440, false)
    );
    let clean = voice(2.0, 3);
    let x = mix(&clean, &noise(clean.len(), 5), 5.0);
    let mut out = vec![0.0f32; HOP];
    let mut y = Vec::new();
    for b in x.as_chunks::<HOP>().0.iter() {
        let v = inst
            .process(&[b], &mut [&mut out], AudioBlock::default(), &[])
            .unwrap();
        assert_eq!(v, None);
        y.extend_from_slice(&out);
    }
    // The pauses of the last second, quieter.
    let tail = |v: &[f32], d: usize| {
        let idx: Vec<usize> = (48_000..clean.len() - 2000)
            .filter(|&i| {
                clean[i.saturating_sub(HOP)..i + HOP]
                    .iter()
                    .all(|c| *c == 0.0)
            })
            .collect();
        (idx.iter()
            .map(|&i| f64::from(v[i + d]).powi(2))
            .sum::<f64>()
            / idx.len() as f64)
            .sqrt()
    };
    assert!(
        tail(&y, 1440) < 0.1 * tail(&x, 0),
        "noise left in the pauses"
    );

    // The same engine again: a switch that keeps the model.
    let o = inst
        .load(&spec(&[("engine", Value::Str("deepfilternet".into()))]))
        .unwrap();
    assert!(o.reused_model);

    // RNNoise: a reload, 10 ms of delay, a voice probability.
    let o = inst
        .load(&spec(&[("engine", Value::Str("rnnoise".into()))]))
        .unwrap();
    assert_eq!((o.latency_samples, o.reused_model), (480, false));
    let mut last = Vec::new();
    for (n, b) in x.as_chunks::<HOP>().0.iter().enumerate() {
        // Live: 0 % from block 100 on, a delayed passthrough.
        let zero = Value::Int(0);
        let live: Vec<(&str, &str, &Value)> = if n >= 100 {
            vec![(STAGE, "strength", &zero)]
        } else {
            Vec::new()
        };
        let v = inst
            .process(&[b], &mut [&mut out], AudioBlock::default(), &live)
            .unwrap();
        assert!(v.is_some_and(|p| (0.0..=1.0).contains(&p)), "{v:?}");
        if n == 150 {
            last = out.clone();
        }
    }
    assert_eq!(
        last,
        x[149 * HOP..150 * HOP],
        "0 % is the input one block late"
    );
    assert!(rms(&last) > 0.0);
}

#[test]
fn it_is_mono_48_khz_and_480_samples_only() {
    let dir = tempfile::tempdir().unwrap();
    let module = open(dir.path());
    let mut inst = module.create_audio_instance().unwrap();
    let mut stereo = spec(&[]);
    stereo.channels_in = 2;
    stereo.channels_out = 2;
    assert!(inst.load(&stereo).is_err(), "stereo loaded");
    let mut rate = spec(&[]);
    rate.sample_rate = 16_000;
    assert!(inst.load(&rate).is_err(), "16 kHz loaded");
    let mut size = spec(&[]);
    size.frame_samples = 960;
    assert!(inst.load(&size).is_err(), "960 samples loaded");
    let e = inst
        .load(&spec(&[("engine", Value::Str("speex".into()))]))
        .unwrap_err();
    assert_eq!(e.status, Status::Invalid, "{e}");
}

#[test]
fn it_loads_on_one_thread_and_processes_on_another() {
    let dir = tempfile::tempdir().unwrap();
    let module = open(dir.path());
    for engine in ["deepfilternet", "rnnoise"] {
        let mut inst = module.create_audio_instance().unwrap();
        inst.load(&spec(&[("engine", Value::Str(engine.into()))]))
            .unwrap();
        let x = mix(&voice(1.0, 3), &noise(48_000, 5), 5.0);
        // As the worker does: the loaded instance moves to its thread.
        let y = std::thread::spawn(move || {
            let mut out = vec![0.0f32; HOP];
            let mut y = Vec::new();
            for b in x.as_chunks::<HOP>().0.iter() {
                inst.process(&[b], &mut [&mut out], AudioBlock::default(), &[])
                    .unwrap();
                y.extend_from_slice(&out);
            }
            y
        })
        .join()
        .unwrap();
        assert!(y.iter().all(|v| v.is_finite()) && rms(&y) > 0.0, "{engine}");
    }
}

#[test]
fn a_new_source_resets_the_state() {
    let dir = tempfile::tempdir().unwrap();
    let module = open(dir.path());
    let x = mix(&voice(1.0, 3), &noise(48_000, 5), 5.0);
    let blocks: Vec<&[f32; HOP]> = x.as_chunks::<HOP>().0.iter().collect();
    for engine in ["deepfilternet", "rnnoise"] {
        let run = |warm: bool, flags: u32| {
            let mut inst = module.create_audio_instance().unwrap();
            inst.load(&spec(&[("engine", Value::Str(engine.into()))]))
                .unwrap();
            let mut out = vec![0.0f32; HOP];
            if warm {
                for b in &blocks[..60] {
                    inst.process(&[*b], &mut [&mut out], AudioBlock::default(), &[])
                        .unwrap();
                }
            }
            let block = AudioBlock {
                pts_ns: None,
                flags,
            };
            let mut y = Vec::new();
            for (i, b) in blocks[20..40].iter().enumerate() {
                let blk = if i == 0 { block } else { AudioBlock::default() };
                inst.process(&[*b], &mut [&mut out], blk, &[]).unwrap();
                y.extend_from_slice(&out);
            }
            y
        };
        let new_source = obc_module_sdk::abi::OBC_AUDIO_FLAG_NEW_SOURCE;
        let fresh = run(false, 0);
        assert_eq!(run(true, new_source), fresh, "{engine}: state carried over");
        assert_ne!(run(true, 0), fresh, "{engine}: the test cannot tell");
    }
}
