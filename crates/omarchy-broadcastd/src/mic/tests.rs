//! The microphone's model, and its agent against a fake worker and a fake
//! service manager (never the user's).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use obc_core::config::{Config, MicGpuPolicy, RuntimeState};
use obc_core::config_d::ConfigSet;
use obc_core::modules::StoredValue;
use obc_ipc::{IpcError, MicStatus, ModuleInfo, SettingKind, SettingValue, Snapshot};
use serde_json::{Value as Json, json};

use super::agent::{
    Agent, Call, MicEvent, MicRequest, UnitError, Units, Worker, WorkerError, WorkerEvent,
};
use super::*;
use crate::controller::{Command, Note, Published};

// ---- the model ----

/// The worker's `GetSettings` document: the noise group from two modules,
/// the CPU module running the stage (G4), and an echo group.
fn worker_settings() -> Json {
    json!({"groups": [
        {"id": "noise", "label": "Noise removal", "module": "br.com.dias.maxine-afx",
         "surface": "mic", "settings": [
            {"id": "mic.noise.enabled", "label": "Noise removal", "type": "bool",
             "scope": "global", "stored": false, "enabled": false,
             "disabled_reason": "Provided by Voice (CPU)", "enabled_when": [],
             "default": true, "value": true, "stage": "mic-denoise", "role": "enable"},
            {"id": "mic.noise.strength", "label": "Strength", "type": "int",
             "scope": "global", "stored": false, "enabled": false,
             "disabled_reason": "Provided by Voice (CPU)", "enabled_when": [],
             "min": 0, "max": 100, "step": 5, "unit": "%", "default": 100, "value": 100}]},
        {"id": "noise", "label": "Noise removal", "module": "br.com.dias.voice-cpu",
         "surface": "mic", "settings": [
            {"id": "mic.noise.enabled", "label": "Noise removal", "type": "bool",
             "scope": "global", "stored": false, "enabled": true, "disabled_reason": "",
             "enabled_when": [], "default": true, "value": true, "hint": "switch"},
            {"id": "mic.noise.strength", "label": "Strength", "type": "int",
             "scope": "global", "stored": false, "enabled": true, "disabled_reason": "",
             "enabled_when": [["mic.noise.enabled", "eq", true]],
             "min": 0, "max": 100, "step": 5, "unit": "%", "default": 80, "value": 80}]},
        {"id": "voice", "label": "Studio voice", "module": "br.com.dias.maxine-afx",
         "settings": [
            {"id": "mic.voice.profile", "label": "Profile", "type": "enum",
             "scope": "global", "stored": false, "enabled": true, "disabled_reason": "",
             "enabled_when": [], "options": [["a", "A"], ["b", "B"]], "default": "a"},
            {"id": "effect.not.mine", "label": "x", "type": "bool", "enabled": true,
             "default": false}]},
    ]})
}

fn catalogue() -> Catalogue {
    Catalogue {
        known: true,
        groups: parse_groups(&worker_settings()),
        modules: Vec::new(),
        devices: vec![
            ("alsa_input.usb-a".into(), "USB A".into()),
            ("alsa_input.usb-b".into(), String::new()),
        ],
    }
}

fn groups_of(
    config: &Config,
    runtime: &RuntimeState,
    catalogue: &Catalogue,
    host_on: bool,
) -> Vec<SettingsGroup> {
    let dir = tempfile::tempdir().unwrap();
    let set = ConfigSet::defaults(&dir.path().join("config.toml"));
    groups(&Inputs {
        config,
        config_set: &set,
        runtime,
        catalogue,
        host_on,
    })
}

#[test]
fn the_worker_groups_are_one_per_id_the_active_providers() {
    let g = parse_groups(&worker_settings());
    let ids: Vec<&str> = g.iter().map(|g| g.id.as_str()).collect();
    assert_eq!(ids, ["noise", "voice"]);
    let noise = &g[0];
    assert!(noise.settings.iter().all(|s| s.enabled), "{noise:?}");
    assert_eq!(
        noise.settings[1].kind,
        SettingKind::Int {
            min: 0,
            max: 100,
            step: 5,
            default: 80,
            value: 80,
            unit: Some("%".into())
        }
    );
    assert_eq!(noise.settings[1].enabled_when.len(), 1);
    assert_eq!(noise.surface.as_deref(), Some("mic"));
    // An id that is not a microphone id is dropped; an enum without a
    // value takes its default.
    assert_eq!(g[1].settings.len(), 1);
    assert_eq!(
        g[1].settings[0].kind.value(),
        Some(SettingValue::Str("a".into()))
    );
    // Only one provider: kept as it is.
    let one = json!({"groups": [worker_settings()["groups"][0].clone()]});
    let g = parse_groups(&one);
    assert_eq!(g.len(), 1);
    assert!(!g[0].settings[0].enabled);
    assert!(parse_groups(&json!({})).is_empty());
}

#[test]
fn the_input_group_comes_first_once_the_worker_is_known() {
    let config = Config::default();
    let runtime = RuntimeState::default();
    assert!(groups_of(&config, &runtime, &Catalogue::default(), false).is_empty());
    let g = groups_of(&config, &runtime, &catalogue(), false);
    let ids: Vec<&str> = g.iter().map(|g| g.id.as_str()).collect();
    assert_eq!(ids, ["input", "noise", "voice"]);
    assert!(g.iter().all(|g| g.surface.as_deref() == Some("mic")));
    let input: Vec<&str> = g[0].settings.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(input, [ENABLED, DEVICE, MAKE_DEFAULT, RESTORE_DEFAULT]);
    for s in g.iter().flat_map(|g| &g.settings) {
        assert!(obc_ipc::valid_setting_id(&s.id), "{}", s.id);
        assert_eq!(s.scope, "global");
    }
    // The device options: default, the devices (a description or the
    // name), and the value in force.
    let SettingKind::Enum { options, value, .. } = &g[0].settings[1].kind else {
        panic!("device is an enum");
    };
    assert_eq!(value, "default");
    assert_eq!(
        options
            .iter()
            .map(|(v, l)| format!("{v}={l}"))
            .collect::<Vec<_>>(),
        [
            "default=System default".to_string(),
            format!("{}=USB A", device_token("alsa_input.usb-a")),
            format!("{}=alsa_input.usb-b", device_token("alsa_input.usb-b")),
        ]
    );
    let mut gone = Config::default();
    gone.mic.device = "alsa_input.gone".into();
    let g2 = groups_of(&gone, &runtime, &catalogue(), false);
    let SettingKind::Enum { options, .. } = &g2[0].settings[1].kind else {
        panic!()
    };
    // The configured device is listed even when absent, marked so, and
    // is the value.
    assert_eq!(
        options.last().unwrap(),
        &(
            device_token("alsa_input.gone"),
            "Unavailable: alsa_input.gone".to_string()
        )
    );
    let SettingKind::Enum { value, .. } = &g2[0].settings[1].kind else {
        panic!()
    };
    assert_eq!(value, &device_token("alsa_input.gone"));
    // Many devices: at most MAX_DEVICES options, the value among them.
    let mut many = catalogue();
    many.devices = (0..40).map(|i| (format!("d{i}"), String::new())).collect();
    let g3 = groups_of(&gone, &runtime, &many, false);
    let SettingKind::Enum { options, .. } = &g3[0].settings[1].kind else {
        panic!()
    };
    assert_eq!(options.len(), MAX_DEVICES);
    assert!(
        options
            .iter()
            .any(|(v, _)| *v == device_token("alsa_input.gone"))
    );
    // The host running alone shows the group.
    assert_eq!(
        groups_of(&config, &runtime, &Catalogue::default(), true).len(),
        1
    );
}

#[test]
fn the_default_microphone_actions_follow_the_host_and_the_record() {
    let config = Config::default();
    let mut runtime = RuntimeState::default();
    let action = |g: &[SettingsGroup], id: &str| find(g, id).cloned().unwrap();
    let off = groups_of(&config, &runtime, &catalogue(), false);
    assert_eq!(action(&off, MAKE_DEFAULT).disabled_reason, REASON_HOST_OFF);
    assert_eq!(
        action(&off, RESTORE_DEFAULT).disabled_reason,
        REASON_NOT_DEFAULT
    );
    let on = groups_of(&config, &runtime, &catalogue(), true);
    assert!(action(&on, MAKE_DEFAULT).enabled);
    assert!(!action(&on, RESTORE_DEFAULT).enabled);
    runtime.mic_previous_default = Some("alsa_input.usb-a".into());
    let made = groups_of(&config, &runtime, &catalogue(), true);
    // A previous default on record may be stale (another microphone picked
    // since in the system's settings): making it the default stays offered.
    assert!(action(&made, MAKE_DEFAULT).enabled);
    assert!(action(&made, RESTORE_DEFAULT).enabled);
}

#[test]
fn stored_values_show_over_the_workers() {
    let config = Config::default();
    let mut runtime = RuntimeState::default();
    runtime
        .settings
        .insert("mic.noise.strength".into(), StoredValue::Int(40));
    // Of the wrong type: not shown.
    runtime
        .settings
        .insert("mic.noise.enabled".into(), StoredValue::Str("x".into()));
    let g = groups_of(&config, &runtime, &catalogue(), false);
    let strength = find(&g, "mic.noise.strength").unwrap();
    assert!(strength.stored);
    assert_eq!(strength.kind.value(), Some(SettingValue::Int(40)));
    let enabled = find(&g, "mic.noise.enabled").unwrap();
    assert!(!enabled.stored);
    assert_eq!(enabled.kind.value(), Some(SettingValue::Bool(true)));
    // Out of the range the worker gives now: not shown.
    runtime
        .settings
        .insert("mic.noise.strength".into(), StoredValue::Int(400));
    let g = groups_of(&config, &runtime, &catalogue(), false);
    let strength = find(&g, "mic.noise.strength").unwrap();
    assert!(!strength.stored);
    assert_eq!(strength.kind.value(), Some(SettingValue::Int(80)));
}

#[test]
fn what_the_worker_describes_is_bounded_before_it_is_published() {
    let long = "x\u{1}y ".repeat(200);
    let doc = json!({"groups": [
        {"id": "bad group", "label": "x", "settings": [
            {"id": "mic.bad.a", "label": "A", "type": "bool"}]},
        {"id": "big", "label": long, "settings": [
            {"id": "mic.big.level", "label": long, "type": "int", "min": 0, "max": 10,
             "default": 50, "value": -3, "unit": long, "hint": long,
             "enabled": false, "disabled_reason": long},
            {"id": "mic.big.inverted", "label": "x", "type": "int", "min": 5, "max": 1},
            {"id": "mic.big.mode", "label": "Mode", "type": "enum",
             "options": [["ok", long], [long, "Long"], ["has space", "S"]],
             "default": "gone", "value": "has space",
             "enabled_when": [["mic.big.level", "eq", long], ["mic.big.level", "eq", 1]]}]},
    ]});
    let g = parse_groups(&doc);
    assert_eq!(g.len(), 1, "a group id that is not plain is dropped");
    let big = &g[0];
    assert!(big.label.len() <= LABEL_MAX);
    // An inverted range does not read.
    assert_eq!(big.settings.len(), 2);
    let level = &big.settings[0];
    assert!(level.label.len() <= LABEL_MAX && !level.label.contains('\u{1}'));
    assert!(level.disabled_reason.len() <= REASON_MAX);
    assert!(level.hint.as_ref().unwrap().len() <= TAG_MAX);
    let SettingKind::Int {
        default,
        value,
        unit,
        ..
    } = &level.kind
    else {
        panic!("int")
    };
    // The default and the value out of the range: the minimum, the default.
    assert_eq!((*default, *value), (0, 0));
    assert!(unit.as_ref().unwrap().len() <= TAG_MAX);
    let mode = &big.settings[1];
    let SettingKind::Enum {
        options,
        default,
        value,
    } = &mode.kind
    else {
        panic!("enum")
    };
    // Option values that could not be written back are left out; labels
    // are cut.
    assert_eq!(options.len(), 1);
    assert!(options[0].1.len() <= OPTION_LABEL_MAX);
    assert_eq!((default.as_str(), value.as_str()), ("ok", "ok"));
    assert_eq!(mode.enabled_when.len(), 1, "a long operand is left out");
    // Too many groups: the first MAX_GROUPS.
    let many: Vec<Json> = (0..40)
        .map(|i| {
            json!({"id": format!("g{i}"), "label": "G", "settings": [
                {"id": format!("mic.g{i}.on"), "label": "On", "type": "bool"}]})
        })
        .collect();
    assert_eq!(parse_groups(&json!({ "groups": many })).len(), MAX_GROUPS);
    // Modules, devices and the status too.
    let m = parse_modules(&json!({"modules": [
        {"id": "a", "kind": "audio-filter", "name": long, "description": long, "reason": long,
         "stages": [{"id": "mic-denoise", "slot": long, "reason": long}]},
        {"id": long, "kind": "audio-filter"},
    ]}));
    assert_eq!(m.len(), 1);
    assert!(m[0].name.len() <= crate::modules::NAME_MAX);
    assert!(m[0].description.len() <= TEXT_MAX);
    assert!(m[0].reason.len() <= REASON_MAX);
    assert!(m[0].stages[0].slot.len() <= TAG_MAX);
    let d = parse_devices(&json!({"devices": [
        {"name": "alsa_input.a", "description": long},
        {"name": "x".repeat(300)}, {"name": "with\nnewline"},
    ]}));
    assert_eq!(d.len(), 1);
    assert!(d[0].1.len() <= LABEL_MAX);
    let s = status(
        true,
        Some(&json!({"state": long, "error": long, "provider": {"mic-denoise": long}})),
    );
    assert_eq!(s.state, "error");
    assert!(s.error.len() <= TEXT_MAX && !s.error.contains('\u{1}'));
    assert!(s.provider.len() <= TEXT_MAX);
}

#[test]
fn a_write_is_checked_and_bound_to_what_it_changes() {
    use obc_core::config_schema::ConfigValue as C;
    let config = Config::default();
    let mut runtime = RuntimeState::default();
    let g = groups_of(&config, &runtime, &catalogue(), true);
    let b = |v: bool| SettingValue::Bool(v);
    assert_eq!(
        check(&g, ENABLED, Some(&b(false))),
        Ok(Write::Config(KEY_ENABLED, C::Bool(false)))
    );
    // The device is written as its node name, whether given as the token
    // or as the node name itself.
    let devices = catalogue().devices;
    let pick = |given: &str, configured: &str| {
        check_input(
            &g,
            &devices,
            configured,
            DEVICE,
            Some(&SettingValue::Str(given.into())),
        )
    };
    let usb_a = Ok(Write::Config(KEY_DEVICE, C::Str("alsa_input.usb-a".into())));
    assert_eq!(pick(&device_token("alsa_input.usb-a"), "default"), usb_a);
    assert_eq!(pick("alsa_input.usb-a", "default"), usb_a);
    assert_eq!(
        pick("default", "default"),
        Ok(Write::Config(KEY_DEVICE, C::Str("default".into())))
    );
    assert!(matches!(pick("nope", "default"), Err(Refusal::Invalid(_))));
    assert!(matches!(
        pick("pw-000000000000", "default"),
        Err(Refusal::Invalid(_))
    ));
    assert_eq!(
        check(&g, MAKE_DEFAULT, Some(&b(true))),
        Ok(Write::MakeDefault)
    );
    assert!(matches!(
        check(&g, MAKE_DEFAULT, Some(&b(false))),
        Err(Refusal::Invalid(_))
    ));
    assert_eq!(
        check(&g, RESTORE_DEFAULT, Some(&b(true))),
        Err(Refusal::Disabled(REASON_NOT_DEFAULT.into()))
    );
    assert_eq!(
        check(&g, "mic.noise.strength", Some(&SettingValue::Int(45))),
        Ok(Write::Value(
            "mic.noise.strength".into(),
            StoredValue::Int(45)
        ))
    );
    for bad in [101, -5, 42] {
        assert!(
            matches!(
                check(&g, "mic.noise.strength", Some(&SettingValue::Int(bad))),
                Err(Refusal::Invalid(_))
            ),
            "{bad}"
        );
    }
    assert!(matches!(
        check(&g, "mic.noise.strength", Some(&b(true))),
        Err(Refusal::Invalid(_))
    ));
    assert!(matches!(
        check(&g, "mic.noise.nothing", Some(&b(true))),
        Err(Refusal::Unknown(_))
    ));
    runtime.mic_previous_default = Some("x".into());
    let g = groups_of(&config, &runtime, &catalogue(), true);
    assert_eq!(
        check(&g, RESTORE_DEFAULT, Some(&b(true))),
        Ok(Write::RestoreDefault)
    );
}

#[test]
fn the_snapshot_carries_the_config_values_pins_and_disabled_modules() {
    let mut config = Config::default();
    let mut runtime = RuntimeState::default();
    let parse = |text: String| serde_json::from_str::<Json>(&text).unwrap();
    let d = parse(snapshot(&config, &runtime, false));
    assert_eq!(
        d,
        json!({
            "version": 1, "enabled": true, "device": "default",
            "pipeline_depth": {"cpu": 1, "gpu": 2}, "idle_exit_s": 30,
            "gpu_warm_s": 30, "gpu_policy": "always", "camera_active": false, "profile": "quality",
            "pins": {}, "values": {}, "disabled_modules": [], "forced_conflicts": false,
        })
    );
    config.mic.force_conflicts = true;
    assert_eq!(
        parse(snapshot(&config, &runtime, false))["forced_conflicts"],
        json!(true)
    );
    config.mic.enabled = false;
    config.mic.device = "alsa_input.usb-a".into();
    config.mic.pipeline_depth = 3;
    config.mic.idle_exit_s = 0;
    config.mic.gpu_warm_s = 0;
    config.mic.gpu_policy = MicGpuPolicy::WithCamera;
    config
        .modules
        .pin
        .insert("mic-denoise".into(), "br.com.dias.voice-cpu".into());
    config.modules.pin.insert("background".into(), "x".into());
    config
        .modules
        .pin
        .insert("mic-dereverb".into(), String::new());
    config.modules.disabled = vec!["a".into(), "b".into()];
    config.modules.enabled.insert("b".into(), true);
    config.modules.enabled.insert("c".into(), false);
    runtime
        .settings
        .insert("mic.noise.strength".into(), StoredValue::Int(40));
    runtime
        .settings
        .insert("effect.video.denoise".into(), StoredValue::Bool(true));
    runtime
        .settings
        .insert("mic.input.enabled".into(), StoredValue::Bool(true));
    let d = parse(snapshot(&config, &runtime, true));
    assert_eq!(d["enabled"], json!(false));
    assert_eq!(d["device"], json!("alsa_input.usb-a"));
    assert_eq!(d["pipeline_depth"], json!(3));
    assert_eq!(d["idle_exit_s"], json!(0));
    assert_eq!(d["gpu_warm_s"], json!(0));
    assert_eq!(d["gpu_policy"], json!("with-camera"));
    assert_eq!(d["camera_active"], json!(true));
    assert_eq!(d["pins"], json!({"mic-denoise": "br.com.dias.voice-cpu"}));
    assert_eq!(d["values"], json!({"mic.noise.strength": 40}));
    assert_eq!(d["disabled_modules"], json!(["a", "c"]));
}

#[test]
fn the_status_is_off_without_the_host_and_the_workers_with_it() {
    assert_eq!(
        status(false, Some(&json!({"state": "active"}))),
        MicStatus::default()
    );
    assert_eq!(status(true, None).state, "passthrough");
    assert_eq!(
        status(true, Some(&json!({"state": "off"}))).state,
        "passthrough"
    );
    let s = status(
        true,
        Some(&json!({
            "state": "active", "consumers": 2, "latency_ms": 20.0, "error": "",
            "provider": {"mic-denoise": "br.com.dias.voice-cpu", "mic-dereverb": "x"},
        })),
    );
    assert_eq!(
        s,
        MicStatus {
            state: "active".into(),
            consumers: 2,
            provider: "mic-denoise: br.com.dias.voice-cpu, mic-dereverb: x".into(),
            latency_ms: 20.0,
            error: String::new(),
        }
    );
    let e = status(true, Some(&json!({"state": "error", "error": "no GPU"})));
    assert_eq!((e.state.as_str(), e.error.as_str()), ("error", "no GPU"));
}

#[test]
fn the_audio_modules_are_the_workers_in_the_module_list() {
    let daemon = vec![
        ModuleInfo {
            id: "video".into(),
            kind: "video-stage".into(),
            status: "available".into(),
            ..ModuleInfo::default()
        },
        ModuleInfo {
            id: "audio".into(),
            kind: "audio-filter".into(),
            status: "unavailable".into(),
            reason: "runs in the audio worker".into(),
            ..ModuleInfo::default()
        },
    ];
    let worker = parse_modules(&json!({"modules": [
        {"id": "audio", "name": "Voice", "kind": "audio-filter", "enabled": true,
         "status": "available", "reason": "",
         "stages": [{"id": "mic-denoise", "slot": "clean", "rank": 200, "device": "cpu",
                     "available": true, "reason": "", "selected": true}]},
        {"id": "extra", "kind": "audio-filter", "status": "available"},
        {"id": "video", "kind": "video-stage", "status": "unavailable"},
    ]}));
    assert_eq!(worker.len(), 2);
    let merged = merge_modules(daemon, &worker);
    let ids: Vec<&str> = merged.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["video", "audio", "extra"]);
    assert_eq!(merged[0].status, "available");
    assert_eq!(merged[1].status, "available");
    assert_eq!(merged[1].stages[0].rank, 200);
    assert!(merged[1].stages[0].selected);
}

#[test]
fn the_devices_are_capped_and_named() {
    let d = parse_devices(&json!({"devices": [
        {"name": "a", "description": "A"}, {"name": ""}, {"description": "x"},
    ]}));
    assert_eq!(d, [("a".to_string(), "A".to_string())]);
    let many: Vec<Json> = (0..30).map(|i| json!({"name": format!("d{i}")})).collect();
    assert_eq!(
        parse_devices(&json!({ "devices": many })).len(),
        MAX_DEVICES
    );
}

// ---- the agent ----

#[derive(Default)]
struct WorkerWorld {
    running: bool,
    /// Whether a call that may activate it starts it.
    installed: bool,
    calls: Vec<Call>,
    /// The calls sent without activation.
    unactivated: Vec<&'static str>,
    /// When set, `GetModules` waits for it (a slow worker start).
    gate: Option<Arc<tokio::sync::Notify>>,
    replies: HashMap<&'static str, Result<String, WorkerError>>,
}

#[derive(Clone, Default)]
struct FakeWorker(Arc<Mutex<WorkerWorld>>);

fn kind(c: &Call) -> &'static str {
    match c {
        Call::GetStatus => "GetStatus",
        Call::GetSettings => "GetSettings",
        Call::GetModules => "GetModules",
        Call::GetDevices => "GetDevices",
        Call::ApplySnapshot(_) => "ApplySnapshot",
        Call::MakeDefault(_) => "MakeDefault",
        Call::RestoreDefault(_) => "RestoreDefault",
        Call::ResetModule(_) => "ResetModule",
    }
}

impl Worker for FakeWorker {
    async fn running(&self) -> bool {
        self.0.lock().unwrap().running
    }

    async fn call(&self, call: Call, activate: bool) -> Result<String, WorkerError> {
        assert!(!activate || call.activates(), "{call:?} cannot activate");
        let k = kind(&call);
        let gate = {
            let mut w = self.0.lock().unwrap();
            w.calls.push(call.clone());
            if !activate {
                w.unactivated.push(k);
            }
            w.gate.clone().filter(|_| k == "GetModules")
        };
        if let Some(g) = gate {
            g.notified().await;
        }
        let mut w = self.0.lock().unwrap();
        if !w.running && !(activate && w.installed) {
            return Err(WorkerError {
                unavailable: true,
                message: "no audio worker".into(),
            });
        }
        if activate {
            w.running = true;
        }
        w.replies.get(k).cloned().unwrap_or_else(|| {
            Ok(match k {
                "GetModules" => json!({"modules": []}).to_string(),
                "GetSettings" => worker_settings().to_string(),
                "GetDevices" => json!({"devices": [{"name": "alsa_input.usb-a"}]}).to_string(),
                "GetStatus" => json!({"state": "passthrough"}).to_string(),
                "MakeDefault" => json!({"previous": "alsa_input.usb-a"}).to_string(),
                "ResetModule" => "true".into(),
                _ => String::new(),
            })
        })
    }
}

#[derive(Default)]
struct UnitWorld {
    active: HashMap<String, bool>,
    calls: Vec<String>,
    fail: Option<UnitError>,
}

#[derive(Clone, Default)]
struct FakeUnits(Arc<Mutex<UnitWorld>>);

impl Units for FakeUnits {
    async fn active(&self, unit: &str) -> Result<bool, UnitError> {
        let u = self.0.lock().unwrap();
        Ok(u.active.get(unit).copied().unwrap_or(false))
    }

    async fn enable_now(&self, unit: &str) -> Result<(), UnitError> {
        let mut u = self.0.lock().unwrap();
        u.calls.push(format!("enable --now {unit}"));
        if let Some(e) = u.fail.clone() {
            return Err(e);
        }
        u.active.insert(unit.into(), true);
        Ok(())
    }

    async fn disable_now(&self, unit: &str) -> Result<(), UnitError> {
        let mut u = self.0.lock().unwrap();
        u.calls.push(format!("disable --now {unit}"));
        u.active.insert(unit.into(), false);
        Ok(())
    }
}

/// An agent running on its own task, with what it sends.
struct Rig {
    worker: FakeWorker,
    units: FakeUnits,
    requests: tokio::sync::mpsc::UnboundedSender<MicRequest>,
    events: tokio::sync::mpsc::UnboundedSender<WorkerEvent>,
    commands: std::sync::mpsc::Receiver<Command>,
    notes: tokio::sync::mpsc::UnboundedReceiver<Note>,
    dir: tempfile::TempDir,
}

impl Rig {
    fn start(worker: FakeWorker, units: FakeUnits, queued: Vec<MicRequest>) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let (published, notes) = Published::new(Snapshot::empty());
        let (ctx, commands) = std::sync::mpsc::channel();
        let (requests, rrx) = tokio::sync::mpsc::unbounded_channel();
        let (events, erx) = tokio::sync::mpsc::unbounded_channel();
        for r in queued {
            requests.send(r).unwrap();
        }
        let agent = Agent::new(
            worker.clone(),
            units.clone(),
            ctx,
            published,
            Some(dir.path().join("state").join(SNAPSHOT_FILE)),
        );
        tokio::spawn(agent.run(rrx, erx));
        Rig {
            worker,
            units,
            requests,
            events,
            commands,
            notes,
            dir,
        }
    }

    /// The next event the agent sends the controller.
    async fn next(&self) -> MicEvent {
        // Up to 10 s: a loaded machine is slow, never wrong.
        for _ in 0..5000 {
            match self.commands.try_recv() {
                Ok(Command::Mic(e)) => return e,
                Ok(other) => panic!("not a Mic command: {other:?}"),
                Err(_) => tokio::time::sleep(Duration::from_millis(2)).await,
            }
        }
        panic!("no event from the agent")
    }

    /// Events until `f` matches one, which is returned.
    async fn until(&self, f: impl Fn(&MicEvent) -> bool) -> MicEvent {
        loop {
            let e = self.next().await;
            if f(&e) {
                return e;
            }
        }
    }

    fn calls(&self) -> Vec<&'static str> {
        self.worker
            .0
            .lock()
            .unwrap()
            .calls
            .iter()
            .map(kind)
            .collect()
    }

    fn snapshot_file(&self) -> Option<String> {
        std::fs::read_to_string(self.dir.path().join("state").join(SNAPSHOT_FILE)).ok()
    }

    /// Lets the agent run what is queued.
    /// Waits (up to 10 s) until `f` holds.
    async fn wait_for(&self, what: &str, f: impl Fn(&Rig) -> bool) {
        for _ in 0..5000 {
            if f(self) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("timed out waiting for {what}");
    }

    async fn settle(&self) {
        for _ in 0..20 {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
}

fn reply() -> (
    crate::controller::Reply,
    tokio::sync::oneshot::Receiver<Result<(), IpcError>>,
) {
    tokio::sync::oneshot::channel()
}

#[tokio::test]
async fn at_start_the_snapshot_is_written_and_the_worker_asked_what_it_offers() {
    let worker = FakeWorker::default();
    worker.0.lock().unwrap().installed = true;
    let rig = Rig::start(
        worker,
        FakeUnits::default(),
        vec![
            MicRequest::Snapshot("{\"version\":1,\"n\":1}".into()),
            MicRequest::Snapshot("{\"version\":1,\"n\":2}".into()),
        ],
    );
    let e = rig.until(|e| matches!(e, MicEvent::Catalogue(_))).await;
    let MicEvent::Catalogue(c) = e else { panic!() };
    assert!(c.known);
    assert_eq!(c.groups.len(), 2);
    assert_eq!(c.devices.len(), 1);
    // The snapshots queued together: one written, the last; the worker was
    // not running, so none pushed (and it was not started for it).
    assert_eq!(
        rig.snapshot_file().as_deref(),
        Some("{\"version\":1,\"n\":2}")
    );
    let calls = rig.calls();
    assert_eq!(calls[0], "GetModules", "{calls:?}");
    assert!(!calls.contains(&"ApplySnapshot"), "{calls:?}");
    // Running now: a snapshot is pushed, and the settings read again.
    rig.requests
        .send(MicRequest::Snapshot("{\"version\":1,\"n\":3}".into()))
        .unwrap();
    rig.wait_for("the push and the settings read again", |r| {
        let calls = r.calls();
        calls
            .iter()
            .position(|c| *c == "ApplySnapshot")
            .is_some_and(|at| calls[at..].contains(&"GetSettings"))
    })
    .await;
    assert_eq!(
        rig.snapshot_file().as_deref(),
        Some("{\"version\":1,\"n\":3}")
    );
}

#[tokio::test]
async fn without_a_worker_installed_the_microphone_is_off_and_unknown() {
    let rig = Rig::start(FakeWorker::default(), FakeUnits::default(), Vec::new());
    let e = rig.next().await;
    assert!(
        matches!(&e, MicEvent::Status(s) if *s == MicStatus::default()),
        "{e:?}"
    );
    rig.settle().await;
    assert!(rig.commands.try_recv().is_err(), "no catalogue");
}

#[tokio::test]
async fn mic_on_is_refused_while_an_adr_0014_unit_runs() {
    let units = FakeUnits::default();
    units
        .0
        .lock()
        .unwrap()
        .active
        .insert("omarchy-broadcast-mic@deepfilter.service".into(), true);
    let rig = Rig::start(FakeWorker::default(), units, Vec::new());
    let (tx, rx) = reply();
    rig.requests
        .send(MicRequest::Host {
            on: true,
            previous: None,
            reply: tx,
        })
        .unwrap();
    let e = rig.until(|e| matches!(e, MicEvent::HostDone { .. })).await;
    let MicEvent::HostDone { result, reply, .. } = e else {
        panic!()
    };
    let _ = reply.send(result);
    match rx.await.unwrap() {
        Err(IpcError::Unavailable(m)) => {
            assert!(
                m.contains("omarchy-broadcast-mic@deepfilter.service"),
                "{m}"
            )
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
    assert!(rig.units.0.lock().unwrap().calls.is_empty());
}

#[tokio::test]
async fn mic_on_and_off_enable_and_disable_the_host_restoring_the_default_first() {
    let worker = FakeWorker::default();
    worker.0.lock().unwrap().installed = true;
    let rig = Rig::start(worker, FakeUnits::default(), Vec::new());
    let (tx, _rx) = reply();
    rig.requests
        .send(MicRequest::Host {
            on: true,
            previous: None,
            reply: tx,
        })
        .unwrap();
    // The status before the reply: the controller answers after it.
    let mut seen_on = false;
    loop {
        match rig.next().await {
            MicEvent::Status(s) if s.state != "off" => seen_on = true,
            MicEvent::HostDone { result, .. } => {
                assert!(result.is_ok(), "{result:?}");
                break;
            }
            _ => {}
        }
    }
    assert!(seen_on);
    let (tx, _rx) = reply();
    rig.requests
        .send(MicRequest::Host {
            on: false,
            previous: Some("alsa_input.usb-a".into()),
            reply: tx,
        })
        .unwrap();
    let e = rig.until(|e| matches!(e, MicEvent::HostDone { .. })).await;
    let MicEvent::HostDone {
        restored, result, ..
    } = e
    else {
        panic!()
    };
    assert!(restored);
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        rig.units.0.lock().unwrap().calls,
        [
            "enable --now omarchy-broadcast-mic.service",
            "disable --now omarchy-broadcast-mic.service"
        ]
    );
    let w = rig.worker.0.lock().unwrap();
    assert!(
        w.calls
            .contains(&Call::RestoreDefault("alsa_input.usb-a".into()))
    );
}

#[tokio::test]
async fn a_service_manager_refusal_is_the_reply() {
    let units = FakeUnits::default();
    units.0.lock().unwrap().fail = Some(UnitError::Unavailable("no systemd".into()));
    let rig = Rig::start(FakeWorker::default(), units, Vec::new());
    let (tx, _rx) = reply();
    rig.requests
        .send(MicRequest::Host {
            on: true,
            previous: None,
            reply: tx,
        })
        .unwrap();
    let e = rig.until(|e| matches!(e, MicEvent::HostDone { .. })).await;
    let MicEvent::HostDone { result, .. } = e else {
        panic!()
    };
    assert!(
        matches!(&result, Err(IpcError::Unavailable(m)) if m == "no systemd"),
        "{result:?}"
    );
}

#[tokio::test]
async fn make_default_hands_back_the_previous_default() {
    let worker = FakeWorker::default();
    worker.0.lock().unwrap().installed = true;
    let rig = Rig::start(worker, FakeUnits::default(), Vec::new());
    let (tx, _rx) = reply();
    rig.requests
        .send(MicRequest::MakeDefault {
            target: String::new(),
            reply: tx,
        })
        .unwrap();
    let e = rig
        .until(|e| matches!(e, MicEvent::DefaultMade { .. }))
        .await;
    let MicEvent::DefaultMade { previous, .. } = e else {
        panic!()
    };
    assert_eq!(previous, "alsa_input.usb-a");
    // A worker that cannot: the reply says why, nothing is recorded.
    rig.worker.0.lock().unwrap().replies.insert(
        "MakeDefault",
        Err(WorkerError {
            unavailable: true,
            message: "no PipeWire".into(),
        }),
    );
    let (tx, rx) = reply();
    rig.requests
        .send(MicRequest::MakeDefault {
            target: "x".into(),
            reply: tx,
        })
        .unwrap();
    match rx.await.unwrap() {
        Err(IpcError::Unavailable(m)) => assert!(m.contains("no PipeWire"), "{m}"),
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

#[tokio::test]
async fn a_repin_asks_the_worker_only_while_the_host_runs_and_records_nothing() {
    let worker = FakeWorker::default();
    worker.0.lock().unwrap().installed = true;
    let units = FakeUnits::default();
    let rig = Rig::start(worker, units.clone(), Vec::new());
    rig.until(|e| matches!(e, MicEvent::Catalogue(_))).await;
    let made = |r: &Rig| -> Vec<String> {
        r.worker
            .0
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter_map(|c| match c {
                Call::MakeDefault(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    };
    // The host is off: nothing is asked.
    rig.requests
        .send(MicRequest::Repin {
            target: "alsa_input.usb-b".into(),
        })
        .unwrap();
    rig.settle().await;
    assert!(made(&rig).is_empty());
    // On: the worker is asked, with the target (empty: the previous
    // default), and no event comes of it.
    units
        .0
        .lock()
        .unwrap()
        .active
        .insert(HOST_UNIT.into(), true);
    for target in ["alsa_input.usb-b", ""] {
        rig.requests
            .send(MicRequest::Repin {
                target: target.into(),
            })
            .unwrap();
    }
    rig.wait_for("both re-pins", |r| made(r).len() == 2).await;
    assert_eq!(made(&rig), ["alsa_input.usb-b", ""]);
    rig.settle().await;
    while let Ok(e) = rig.commands.try_recv() {
        assert!(
            !matches!(e, Command::Mic(MicEvent::DefaultMade { .. })),
            "{e:?}"
        );
    }
    // A worker that cannot: only logged, the agent goes on.
    rig.worker.0.lock().unwrap().replies.insert(
        "MakeDefault",
        Err(WorkerError {
            unavailable: false,
            message: "no such node".into(),
        }),
    );
    rig.requests
        .send(MicRequest::Repin { target: "x".into() })
        .unwrap();
    rig.wait_for("the failing re-pin", |r| made(r).len() == 3)
        .await;
}

#[tokio::test]
async fn the_workers_signals_become_the_status_and_mic_stats() {
    let worker = FakeWorker::default();
    let units = FakeUnits::default();
    units
        .0
        .lock()
        .unwrap()
        .active
        .insert(HOST_UNIT.into(), true);
    let mut rig = Rig::start(worker, units, Vec::new());
    let e = rig.next().await;
    assert!(
        matches!(&e, MicEvent::Status(s) if s.state == "passthrough"),
        "{e:?}"
    );
    // Stats while not active: not relayed.
    rig.events.send(WorkerEvent::Stats(1, 2, 0.5)).unwrap();
    rig.worker.0.lock().unwrap().running = true;
    rig.events.send(WorkerEvent::Owner(true)).unwrap();
    rig.events
        .send(WorkerEvent::Status(
            json!({"state": "active", "consumers": 1, "latency_ms": 10.0}).to_string(),
        ))
        .unwrap();
    let e = rig
        .until(|e| matches!(e, MicEvent::Status(s) if s.state == "active"))
        .await;
    let MicEvent::Status(s) = e else { panic!() };
    assert_eq!((s.consumers, s.latency_ms), (1, 10.0));
    rig.events.send(WorkerEvent::Stats(3, 4, 0.9)).unwrap();
    // The events are handled in order: the first Stats (not active) went
    // before this one.
    let mut stats = Vec::new();
    for _ in 0..5000 {
        while let Ok(n) = rig.notes.try_recv() {
            if let Note::MicStats(o, m, v) = n {
                stats.push((o, m, v));
            }
        }
        if !stats.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(stats, [(3, 4, 0.9)]);
    // The worker gone: passthrough while the host runs.
    rig.worker.0.lock().unwrap().running = false;
    rig.events.send(WorkerEvent::Owner(false)).unwrap();
    let e = rig.until(|e| matches!(e, MicEvent::Status(_))).await;
    assert!(
        matches!(&e, MicEvent::Status(s) if s.state == "passthrough"),
        "{e:?}"
    );
    // ResetModule starts the worker (a user's request).
    rig.requests
        .send(MicRequest::ResetModule("audio".into()))
        .unwrap();
    rig.wait_for("ResetModule", |r| {
        r.worker
            .0
            .lock()
            .unwrap()
            .calls
            .contains(&Call::ResetModule("audio".into()))
    })
    .await;
}

#[tokio::test]
async fn mic_off_behind_an_inflight_make_default_still_restores() {
    let worker = FakeWorker::default();
    worker.0.lock().unwrap().installed = true;
    let rig = Rig::start(worker, FakeUnits::default(), Vec::new());
    // "Use as default microphone", then `mic off` queued before the
    // controller heard of the first: it had no previous default to send.
    let (tx, _rx) = reply();
    rig.requests
        .send(MicRequest::MakeDefault {
            target: String::new(),
            reply: tx,
        })
        .unwrap();
    let (tx, _rx2) = reply();
    rig.requests
        .send(MicRequest::Host {
            on: false,
            previous: None,
            reply: tx,
        })
        .unwrap();
    let e = rig.until(|e| matches!(e, MicEvent::HostDone { .. })).await;
    let MicEvent::HostDone {
        restored, result, ..
    } = e
    else {
        panic!()
    };
    assert!(restored && result.is_ok(), "{result:?}");
    let calls = rig.worker.0.lock().unwrap().calls.clone();
    assert!(
        calls.contains(&Call::RestoreDefault("alsa_input.usb-a".into())),
        "{calls:?}"
    );
    // Nothing on record at all: restored all the same (the worker acts
    // only if the Omarchy Broadcast Mic is the default).
    let (tx, _rx3) = reply();
    rig.requests
        .send(MicRequest::Host {
            on: false,
            previous: None,
            reply: tx,
        })
        .unwrap();
    rig.until(|e| matches!(e, MicEvent::HostDone { .. })).await;
    let calls = rig.worker.0.lock().unwrap().calls.clone();
    assert_eq!(calls.last(), Some(&Call::RestoreDefault(String::new())));
}

#[tokio::test]
async fn following_a_running_worker_never_activates_it() {
    let worker = FakeWorker::default();
    {
        let mut w = worker.0.lock().unwrap();
        w.installed = true;
        w.running = true;
    }
    let rig = Rig::start(worker, FakeUnits::default(), Vec::new());
    rig.until(|e| matches!(e, MicEvent::Catalogue(_))).await;
    rig.wait_for("the status read", |r| r.calls().contains(&"GetStatus"))
        .await;
    rig.worker.0.lock().unwrap().unactivated.clear();
    let before = rig.calls().len();
    rig.events
        .send(WorkerEvent::Status(json!({"state": "active"}).to_string()))
        .unwrap();
    rig.requests
        .send(MicRequest::Snapshot("{\"version\":1}".into()))
        .unwrap();
    rig.wait_for("the follow-up reads", |r| {
        let n = r.calls()[before..]
            .iter()
            .filter(|c| **c == "GetSettings")
            .count();
        n >= 2
    })
    .await;
    let w = rig.worker.0.lock().unwrap();
    let follow: Vec<&str> = w.calls[before..].iter().map(kind).collect();
    assert_eq!(
        w.unactivated, follow,
        "every follow-up call without activation"
    );
}

#[tokio::test]
async fn the_host_state_is_told_before_a_slow_catalogue() {
    let worker = FakeWorker::default();
    let gate = Arc::new(tokio::sync::Notify::new());
    {
        let mut w = worker.0.lock().unwrap();
        w.installed = true;
        w.gate = Some(gate.clone());
    }
    let units = FakeUnits::default();
    units
        .0
        .lock()
        .unwrap()
        .active
        .insert(HOST_UNIT.into(), true);
    let rig = Rig::start(worker, units, Vec::new());
    // The worker has not answered GetModules yet.
    let e = rig.next().await;
    assert!(
        matches!(&e, MicEvent::Status(s) if s.state == "passthrough"),
        "{e:?}"
    );
    gate.notify_one();
    rig.until(|e| matches!(e, MicEvent::Catalogue(_))).await;
}

// ---- the device options ----

/// The node name of a USB microphone, 53 bytes: past the CLI's 32-byte
/// bound on an option value.
const LONG_NAME: &str = "alsa_input.usb-K66_K66_20190805V001-00.analog-stereo";

#[test]
fn a_long_node_name_is_offered_as_a_short_stable_token() {
    assert!(LONG_NAME.len() > 32);
    // FNV-1a is fixed: the same name always has the same token.
    assert_eq!(device_token(""), "pw-cbf29ce48422");
    assert_eq!(device_token(LONG_NAME), device_token(LONG_NAME));
    let token = device_token(LONG_NAME);
    assert_eq!(token.len(), 3 + 12);
    assert!(token.starts_with("pw-"));
    assert!(
        token[3..]
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    );
    // The worker's long name is offered (not dropped), and what the
    // daemon publishes fits the CLI's 32-byte option bounds.
    let devices = parse_devices(&json!({"devices": [
        {"name": LONG_NAME, "description": "K66 Analog Stereo"},
        {"name": "x".repeat(DEVICE_NAME_MAX + 1)},
    ]}));
    assert_eq!(devices.len(), 1);
    let mut config = Config::default();
    config.mic.device = LONG_NAME.into();
    let mut known = catalogue();
    known.devices = devices;
    let g = groups_of(&config, &RuntimeState::default(), &known, true);
    let SettingKind::Enum { options, value, .. } = &g[0].settings[1].kind else {
        panic!("device is an enum");
    };
    assert_eq!(value, &token);
    assert_eq!(
        options,
        &[
            ("default".to_string(), "System default".to_string()),
            (token.clone(), "K66 Analog Stereo".to_string()),
        ]
    );
    assert!(options.iter().all(|(v, l)| v.len() <= 32 && l.len() <= 32));

    // Set by token, and by the node name (the CLI's `mic device NAME`).
    use obc_core::config_schema::ConfigValue as C;
    let want = Ok(Write::Config(KEY_DEVICE, C::Str(LONG_NAME.into())));
    for given in [token.as_str(), LONG_NAME] {
        assert_eq!(
            check_input(
                &g,
                &known.devices,
                LONG_NAME,
                DEVICE,
                Some(&SettingValue::Str(given.into()))
            ),
            want
        );
    }
    // The node name of a device not listed and not configured is refused.
    assert!(matches!(
        check_input(
            &g,
            &known.devices,
            "default",
            DEVICE,
            Some(&SettingValue::Str("alsa_input.other".into()))
        ),
        Err(Refusal::Invalid(_))
    ));
}

#[test]
fn a_configured_device_that_is_absent_is_listed_unavailable() {
    let entries = device_entries(&catalogue().devices, LONG_NAME);
    let last = entries.last().unwrap();
    assert_eq!(last.name, LONG_NAME);
    assert_eq!(last.token, device_token(LONG_NAME));
    assert!(last.label.starts_with("Unavailable: alsa_input.usb-K66"));
    assert!(last.label.len() <= LABEL_MAX);
    // Nothing configured (default): no extra entry.
    assert_eq!(device_entries(&catalogue().devices, "default").len(), 3);
    // Present: its own label, not marked.
    let present = device_entries(&catalogue().devices, "alsa_input.usb-a");
    assert_eq!(present.len(), 3);
    assert_eq!(
        find_device(&present, "alsa_input.usb-a").unwrap().label,
        "USB A"
    );
}

#[test]
fn colliding_tokens_are_settled_in_listing_order() {
    let devices: Vec<(String, String)> = ["a", "b", "c"]
        .iter()
        .map(|n| (n.to_string(), n.to_uppercase()))
        .collect();
    // A hash that is the same for every name: the later ones' tokens grow,
    // then take a counter, and all are distinct.
    let entries = device_entries_with(&devices, "default", |_| 0x0123_4567_89ab_cdef);
    let tokens: Vec<&str> = entries.iter().map(|e| e.token.as_str()).collect();
    assert_eq!(
        tokens,
        [
            "default",
            "pw-0123456789ab",
            "pw-0123456789abc",
            "pw-0123456789abcd"
        ]
    );
    // The same list gives the same tokens, and each token finds its device.
    assert_eq!(
        entries,
        device_entries_with(&devices, "default", |_| 0x0123_4567_89ab_cdef)
    );
    for e in &entries {
        assert_eq!(find_device(&entries, &e.token), Some(e));
        assert!(e.token.len() <= 32);
    }
    // Past the extensions of the hash, a counter.
    let many: Vec<(String, String)> = (0..8).map(|i| (format!("n{i}"), String::new())).collect();
    let entries = device_entries_with(&many, "default", |_| 0x0123_4567_89ab_cdef);
    let mut tokens: Vec<&str> = entries.iter().map(|e| e.token.as_str()).collect();
    tokens.sort_unstable();
    tokens.dedup();
    assert_eq!(tokens.len(), entries.len());
    assert!(entries.iter().all(|e| e.token.len() <= 32));
    assert!(entries[6].token.ends_with("-2"));
    assert!(entries.last().unwrap().token.ends_with("-4"));
}

#[test]
fn the_configured_device_survives_the_option_cap() {
    let many: Vec<(String, String)> = (0..40).map(|i| (format!("d{i}"), String::new())).collect();
    // Listed, but past the cap: kept, with its own label.
    let entries = device_entries(&many, "d30");
    assert_eq!(entries.len(), MAX_DEVICES);
    assert_eq!(entries.last().unwrap().name, "d30");
    assert_eq!(entries.last().unwrap().label, "d30");
    // Absent: kept, marked.
    let entries = device_entries(&many, "gone");
    assert_eq!(entries.len(), MAX_DEVICES);
    assert_eq!(entries.last().unwrap().label, "Unavailable: gone");
    // Within the cap: the order is the worker's.
    let entries = device_entries(&many, "d2");
    assert_eq!(entries[3].name, "d2");
}
