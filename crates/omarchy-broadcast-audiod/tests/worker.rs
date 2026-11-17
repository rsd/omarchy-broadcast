//! The audio worker end to end: its binary, real module libraries and a
//! fake mic plugin driving the shared-memory rings (see `common`).

mod common;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{Env, FIXTURE_ID, Out, Plugin, PrivateBus};
use obc_audio::proto::RefuseReason;
use omarchy_broadcast_audiod::engine::{Engine, Exit, Shared};
use omarchy_broadcast_audiod::host::{GpuGate, Host};
use omarchy_broadcast_audiod::listener;
use omarchy_broadcast_audiod::mixer::{Mixer, Reading};
use omarchy_broadcast_audiod::pipewire::{
    AlsaSource, AlsaSources, Consumers, PipeWireControl, PipeWireError, PreviousDefault,
    SourceDevice,
};
use omarchy_broadcast_audiod::snapshot::Snapshot;
use serde_json::json;

const SEC: Duration = Duration::from_secs(1);

fn processed(out: &[Out]) -> Vec<&Out> {
    out.iter().filter(|o| !o.raw()).collect()
}

/// The fixture with every stage on, `mic-enhance` at `gain_pct`.
fn fixture_snapshot(gain_pct: i64, extra: serde_json::Value) -> serde_json::Value {
    let mut s = json!({
        "version": 1,
        "enabled": true,
        "idle_exit_s": 0,
        "values": { "mic.fixture.gain": gain_pct },
    });
    if let (Some(s), Some(e)) = (s.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            s.insert(k.clone(), v.clone());
        }
    }
    s
}

#[test]
fn blocks_come_back_in_order_processed_with_their_seq_and_the_latency_published() {
    let mut env = Env::new();
    env.fixture("");
    env.snapshot(fixture_snapshot(200, json!({})));
    let w = env.spawn();
    let mut p = Plugin::new(&env.socket_path);
    p.connect(5 * SEC).unwrap();
    assert_eq!(p.accept().pipeline_depth, 1, "a CPU chain: depth 1");
    assert!(
        p.run_until(5 * SEC, |o| processed(o).len() >= 50),
        "no processed blocks:\n{}",
        w.log_text()
    );
    p.assert_in_order();
    // Raw while the chain loads and warms up (30 samples: one block).
    assert!(p.out[0].raw(), "the first block is raw");
    let first = p.out.iter().position(|o| !o.raw()).unwrap();
    assert!(
        p.out[first..].iter().all(|o| !o.raw()),
        "no raw block once processing"
    );
    // The three stages ran as one ordered request (clean before shape,
    // then by position), and gave twice the input.
    w.wait_log(
        "fixture-audio-load stages=mic-dereverb,mic-denoise,mic-enhance",
        SEC,
    );
    for o in processed(&p.out) {
        assert!(
            o.is_input_times(2.0),
            "block {} is not twice its input",
            o.seq
        );
    }
    // 10 samples per stage.
    assert_eq!(p.rings().latency_samples(), 30);
}

#[test]
fn a_second_plugin_is_refused_while_one_is_connected() {
    let mut env = Env::new();
    env.fixture("");
    env.snapshot(fixture_snapshot(100, json!({})));
    let w = env.spawn();
    let mut a = Plugin::new(&env.socket_path);
    a.connect(5 * SEC).unwrap();
    let mut b = Plugin::new(&env.socket_path);
    match b.connect(5 * SEC) {
        Err(obc_audio::client::ConnectError::Refused(r)) => {
            assert_eq!(r.reason, RefuseReason::Busy);
            assert!(r.text.contains("already serves"), "{}", r.text);
        }
        other => panic!("not refused: {other:?}\n{}", w.log_text()),
    }
    // The first keeps being served.
    assert!(a.run_until(5 * SEC, |o| processed(o).len() >= 10));
}

#[test]
fn a_reload_switches_chains_without_a_gap() {
    let bus = PrivateBus::start();
    let mut env = Env::new();
    env.bus = Some(bus.address.clone());
    env.hello();
    let base = json!({
        "version": 1, "idle_exit_s": 0,
        "values": { "mic.x_example_hello_audio.enabled": true, "mic.x_example_hello_audio.gain": -6 },
    });
    env.snapshot(base.clone());
    let w = env.spawn();
    bus.wait_for_worker(5 * SEC);
    let mut p = Plugin::new(&env.socket_path);
    p.connect(5 * SEC).unwrap();
    let g6 = 10f32.powf(-6.0 / 20.0);
    assert!(p.run_until(5 * SEC, |o| processed(o).len() >= 20));
    let first = p.out.iter().position(|o| !o.raw()).unwrap();
    assert!(p.out[first].is_input_times(g6));

    // A live parameter: no reload.
    let mut live = base.clone();
    live["values"]["mic.x_example_hello_audio.gain"] = json!(0);
    bus.call("ApplySnapshot", Some(&live.to_string())).unwrap();
    let n = p.out.len();
    assert!(p.run_until(5 * SEC, |o| o.len() > n + 10
        && o.last().unwrap().is_input_times(1.0)));
    assert_eq!(
        w.log_text().matches("chain loaded").count(),
        1,
        "{}",
        w.log_text()
    );

    // A load-time parameter (the test tone): a new chain loads while the
    // old one keeps processing, then takes over.
    let mut tone = live.clone();
    tone["values"]["mic.x_example_hello_audio.tone"] = json!(true);
    bus.call("ApplySnapshot", Some(&tone.to_string())).unwrap();
    // The switch happens on a block: audio keeps flowing meanwhile.
    let end = std::time::Instant::now() + 5 * SEC;
    while !w.log_text().contains("switched to the new chain") {
        assert!(
            std::time::Instant::now() < end,
            "no switch:\n{}",
            w.log_text()
        );
        p.run(Duration::from_millis(50));
    }
    p.run(Duration::from_millis(300));
    p.assert_in_order();
    assert!(
        p.out[first..].iter().all(|o| !o.raw()),
        "a raw block during the switch"
    );
    let last = p.out.last().unwrap();
    assert!(!last.is_input_times(1.0), "the tone is on after the switch");
    // Never persisted.
    let on_disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(env.snapshot_path()).unwrap()).unwrap();
    assert_eq!(on_disk, base);
}

#[test]
fn a_stage_over_its_deadline_fails_open_after_fifty_blocks() {
    let mut env = Env::new();
    env.fixture("");
    env.env
        .push(("OBC_FIXTURE_AUDIO_SLEEP_MS".into(), "14".into()));
    env.snapshot(fixture_snapshot(200, json!({})));
    let w = env.spawn();
    let mut p = Plugin::new(&env.socket_path);
    p.connect(5 * SEC).unwrap();
    p.run(Duration::from_secs(3));
    w.wait_log(
        "failed open: Failed open after 50 blocks in a row over its deadline",
        SEC,
    );
    // Raw (passthrough) since: the audio goes on.
    let tail = &p.out[p.out.len() - 20..];
    assert!(
        tail.iter().all(Out::raw),
        "still processed after failing open"
    );
    assert!(tail.iter().all(|o| o.is_input_times(1.0)));
    p.assert_in_order();
}

#[test]
fn a_stage_error_fails_that_stage_open_and_the_rest_runs_on() {
    let mut env = Env::new();
    env.fixture("");
    env.env
        .push(("OBC_FIXTURE_AUDIO_FAIL_AT".into(), "20".into()));
    env.snapshot(fixture_snapshot(200, json!({})));
    let w = env.spawn();
    let mut p = Plugin::new(&env.socket_path);
    p.connect(5 * SEC).unwrap();
    assert!(p.run_until(5 * SEC, |o| processed(o).len() >= 60));
    // Block 20 of the chain failed its first stage, mic-dereverb.
    w.wait_log(
        "stage mic-dereverb of org.example.fixture-audio failed open",
        SEC,
    );
    let ok = processed(&p.out);
    // Only the failing block passes through unchanged; the rest is
    // processed by the stages left (mic-enhance's gain among them).
    let unchanged = ok.iter().filter(|o| o.is_input_times(1.0)).count();
    assert!(unchanged <= 1, "{unchanged} blocks passed unprocessed");
    assert!(ok.last().unwrap().is_input_times(2.0));
    let first = p.out.iter().position(|o| !o.raw()).unwrap();
    assert!(p.out[first..].iter().all(|o| !o.raw()));
}

#[test]
fn after_kill_9_a_restarted_worker_processes_again() {
    let mut env = Env::new();
    env.fixture("");
    env.snapshot(fixture_snapshot(200, json!({})));
    let mut w = env.spawn();
    let mut p = Plugin::new(&env.socket_path);
    p.connect(5 * SEC).unwrap();
    assert!(p.run_until(5 * SEC, |o| processed(o).len() >= 20));
    w.kill9();
    // The plugin sees the hang-up and plays raw by its own path.
    p.run(Duration::from_millis(500));
    assert!(!p.connected());
    let own = p.raw_own;
    p.run(Duration::from_millis(200));
    assert!(p.raw_own > own, "the plugin keeps playing raw");
    // systemd starts the worker again on the next connect.
    let w2 = env.spawn();
    p.connect(5 * SEC).unwrap();
    assert!(
        p.run_until(5 * SEC, |o| o
            .iter()
            .filter(|x| x.conn == 2 && !x.raw())
            .count()
            >= 20),
        "{}",
        w2.log_text()
    );
    p.assert_in_order();
    for o in processed(&p.out) {
        assert!(o.is_input_times(2.0));
    }
}

#[test]
fn the_worker_exits_when_idle() {
    let mut env = Env::new();
    env.fixture("");
    env.snapshot(fixture_snapshot(100, json!({ "idle_exit_s": 1 })));
    // Started directly: it binds the socket itself.
    let socket = env.sockets.path().join("direct.sock");
    let mut w = env.spawn_direct(&socket);
    let mut p = Plugin::new(&socket);
    p.connect(5 * SEC).unwrap();
    assert!(p.run_until(5 * SEC, |o| processed(o).len() >= 10));
    // No more audio: the worker hangs up and exits 0.
    std::thread::sleep(Duration::from_millis(1500));
    let status = w.wait_exit(3 * SEC);
    assert!(status.success(), "{status:?}\n{}", w.log_text());
    assert!(w.log_text().contains("idle: exiting"));
    p.run(Duration::from_millis(100));
    assert!(!p.connected(), "the plugin saw the hang-up");
    assert!(!socket.exists(), "the socket it bound is removed");
    assert!(
        ledger(&env)["running"].get(FIXTURE_ID).is_none(),
        "an idle exit leaves no crash marker: {}",
        ledger(&env)
    );
    // Activated with nothing to do (bus activation, no plugin): it exits
    // after the grace period too.
    let mut w2 = env.spawn();
    assert!(w2.wait_exit(4 * SEC).success());
}

#[test]
fn a_module_that_crashes_twice_is_disabled_and_the_mic_stays_live() {
    let mut env = Env::new();
    env.fixture("");
    env.env
        .push(("OBC_FIXTURE_AUDIO_ABORT_AT".into(), "30".into()));
    env.snapshot(fixture_snapshot(200, json!({})));
    let mut p = Plugin::new(&env.socket_path);
    for round in 1..=2 {
        let mut w = env.spawn();
        if let Err(e) = p.connect(5 * SEC) {
            panic!("round {round}: {e}\n{}", w.log_text());
        }
        p.run(Duration::from_secs(3));
        assert!(!p.connected(), "round {round}: the worker crashed");
        let status = w.wait_exit(3 * SEC);
        assert!(!status.success(), "round {round}: {status:?}");
        // Raw by the plugin's own path meanwhile.
        p.run(Duration::from_millis(100));
    }
    assert!(p.raw_own > 0);
    let w = env.spawn();
    w.wait_log(
        omarchy_broadcast_audiod::host::REASON_CRASHED_WORKER,
        3 * SEC,
    );
    p.connect(5 * SEC).unwrap();
    p.run(Duration::from_secs(1));
    assert!(p.connected(), "{}", w.log_text());
    let third: Vec<&Out> = p.out.iter().filter(|o| o.conn == 3).collect();
    assert!(third.len() > 50);
    assert!(third.iter().all(|o| o.raw()), "no module runs: raw");
    let ledger: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(env.ledger_path()).unwrap()).unwrap();
    assert!(ledger["disabled"][FIXTURE_ID].is_string(), "{ledger}");
    p.assert_in_order();
}

#[test]
fn a_missing_or_corrupt_snapshot_gives_the_defaults() {
    for corrupt in [false, true] {
        let mut env = Env::new();
        env.fixture("");
        if corrupt {
            std::fs::write(env.snapshot_path(), "{\"version\": 1, \"enabled\": ").unwrap();
        }
        let w = env.spawn();
        let mut p = Plugin::new(&env.socket_path);
        p.connect(5 * SEC).unwrap();
        assert!(
            p.run_until(5 * SEC, |o| processed(o).len() >= 10),
            "{}",
            w.log_text()
        );
        let note = if corrupt {
            "cannot be used"
        } else {
            "no audio settings at"
        };
        w.wait_log(note, SEC);
        // The defaults: processing on, the fixture's gain 100 %.
        assert!(processed(&p.out).last().unwrap().is_input_times(1.0));
    }
}

#[test]
fn the_control_interface_lists_describes_and_applies() {
    let bus = PrivateBus::start();
    let mut env = Env::new();
    env.bus = Some(bus.address.clone());
    env.fixture("conflicts = [\"mic-denoise\", \"mic-dereverb\"]");
    env.manifest("video", &common::video_manifest());
    let base = fixture_snapshot(100, json!({}));
    env.snapshot(base.clone());
    let w = env.spawn();
    bus.wait_for_worker(5 * SEC);

    let modules: serde_json::Value =
        serde_json::from_str(&bus.call("GetModules", None).unwrap()).unwrap();
    let list = modules["modules"].as_array().unwrap();
    let fixture = list.iter().find(|m| m["id"] == FIXTURE_ID).unwrap();
    assert_eq!(fixture["kind"], "audio-filter");
    assert_eq!(fixture["status"], "available", "{fixture}");
    assert_eq!(fixture["stages"].as_array().unwrap().len(), 3);
    let video = list
        .iter()
        .find(|m| m["id"] == "org.example.video")
        .unwrap();
    assert_eq!(video["status"], "unavailable");
    assert_eq!(
        video["reason"],
        "Video stage modules run in the video daemon"
    );

    // The mic-enhance stage turns noise and echo removal off; their switch
    // is greyed with the reason.
    let settings: serde_json::Value =
        serde_json::from_str(&bus.call("GetSettings", None).unwrap()).unwrap();
    let group = &settings["groups"][0];
    assert_eq!(group["surface"], "mic");
    assert_eq!(group["module"], FIXTURE_ID);
    let denoise = group["settings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "mic.fixture.denoise")
        .unwrap();
    assert_eq!(denoise["enabled"], false, "{denoise}");
    assert!(
        denoise["disabled_reason"]
            .as_str()
            .unwrap()
            .starts_with("Cannot run together with"),
        "{denoise}"
    );
    assert_eq!(denoise["disabled_kind"], "busy", "{denoise}");
    assert_eq!(denoise["value"], true);
    assert_eq!(denoise["scope"], "global");

    let mut p = Plugin::new(&env.socket_path);
    p.connect(5 * SEC).unwrap();
    assert!(p.run_until(5 * SEC, |o| processed(o).len() >= 5));
    w.wait_log("fixture-audio-load stages=mic-enhance ", SEC);
    let status = bus.status();
    assert_eq!(status["state"], "active", "{status}");
    assert_eq!(status["connected"], true);
    assert_eq!(status["provider"]["mic-enhance"], FIXTURE_ID);
    let denoise = status["stages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "mic-denoise")
        .unwrap();
    assert_eq!(denoise["state"], "off");

    // Forced: the three run together.
    let forced = fixture_snapshot(100, json!({ "forced_conflicts": true }));
    bus.call("ApplySnapshot", Some(&forced.to_string()))
        .unwrap();
    p.run(Duration::from_millis(500));
    w.wait_log("stages=mic-dereverb,mic-denoise,mic-enhance", 2 * SEC);

    // Processing off: raw, state off.
    let off = fixture_snapshot(100, json!({ "enabled": false }));
    bus.call("ApplySnapshot", Some(&off.to_string())).unwrap();
    p.run(Duration::from_millis(300));
    assert!(p.out.last().unwrap().raw());
    assert_eq!(bus.status()["state"], "off");

    // A snapshot that is not valid is refused; PipeWire is not there yet.
    let e = bus
        .call("ApplySnapshot", Some("{\"version\": 7}"))
        .unwrap_err();
    assert!(e.contains("InvalidArgs"), "{e}");
    let e = bus.call("GetDevices", None).unwrap_err();
    assert!(e.contains("NotSupported"), "{e}");
    let e = bus.call("MakeDefault", Some("x")).unwrap_err();
    assert!(e.contains("NotSupported"), "{e}");
    let e = bus.call("Pin", Some("x")).unwrap_err();
    assert!(e.contains("NotSupported"), "{e}");
    // Nothing persisted.
    let on_disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(env.snapshot_path()).unwrap()).unwrap();
    assert_eq!(on_disk, base);
}

#[test]
fn the_pipeline_depth_follows_the_devices_unless_overridden() {
    for (overridden, want) in [(None, 2), (Some(3), 3)] {
        let mut env = Env::new();
        let lib = env.dir.path().join("fixture.so");
        std::fs::copy(common::fixture_lib(), &lib).unwrap();
        let manifest = common::fixture_manifest(&lib, "").replacen(
            "device = \"cpu\"",
            "device = \"nvidia-gpu\"",
            1,
        );
        env.manifest("fixture", &manifest);
        let extra = match overridden {
            Some(d) => json!({ "pipeline_depth": d }),
            None => json!({}),
        };
        env.snapshot(fixture_snapshot(100, extra));
        let w = env.spawn();
        let mut p = Plugin::new(&env.socket_path);
        p.connect(5 * SEC).unwrap();
        assert_eq!(p.accept().pipeline_depth, want, "{}", w.log_text());
        assert!(p.run_until(5 * SEC, |o| processed(o).len() >= 5));
    }
}

/// The fixture with every stage on a GPU, and the example module (CPU) for
/// `mic-enhance`.
fn gpu_fixture_and_hello(env: &mut Env) {
    let lib = env.dir.path().join("fixture.so");
    std::fs::copy(common::fixture_lib(), &lib).unwrap();
    let manifest =
        common::fixture_manifest(&lib, "").replace("device = \"cpu\"", "device = \"nvidia-gpu\"");
    env.manifest("fixture", &manifest);
    env.hello();
}

fn stage<'a>(status: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    status["stages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == id)
        .unwrap_or_else(|| panic!("no stage {id}: {status}"))
}

/// `never` skips GPU providers (the CPU one runs, a GPU-only stage is off
/// with the reason); `with-camera` decides at the session's start and
/// keeps the answer until the plugin reconnects; `always` uses the GPU.
#[test]
fn the_gpu_policy_is_enforced_and_with_camera_decided_per_session() {
    use omarchy_broadcast_audiod::host::{REASON_GPU_CAMERA, REASON_GPU_NEVER};
    let bus = PrivateBus::start();
    let mut env = Env::new();
    env.bus = Some(bus.address.clone());
    gpu_fixture_and_hello(&mut env);
    let snap = |mut extra: serde_json::Value| {
        extra["values"] =
            json!({ "mic.fixture.gain": 100, "mic.x_example_hello_audio.enabled": true });
        fixture_snapshot(100, extra)
    };
    env.snapshot(snap(json!({ "gpu_policy": "never" })));
    let w = env.spawn();
    bus.wait_for_worker(5 * SEC);
    let s = bus.status();
    assert_eq!(
        stage(&s, "mic-enhance")["provider"],
        common::HELLO_ID,
        "{s}"
    );
    let denoise = stage(&s, "mic-denoise");
    assert_eq!(denoise["state"], "off", "{s}");
    assert_eq!(denoise["reason"], REASON_GPU_NEVER, "{s}");

    let mut p = Plugin::new(&env.socket_path);
    p.connect(5 * SEC).unwrap();
    assert!(
        p.run_until(5 * SEC, |o| processed(o).len() >= 5),
        "{}",
        w.log_text()
    );
    assert_eq!(bus.status()["provider"]["mic-enhance"], common::HELLO_ID);
    // A CPU-only chain: depth 1.
    assert_eq!(p.accept().pipeline_depth, 1);

    // The policy changes mid-session: at once, with the camera off now.
    let camera_off = snap(json!({ "gpu_policy": "with-camera", "camera_active": false }));
    bus.call("ApplySnapshot", Some(&camera_off.to_string()))
        .unwrap();
    p.run(Duration::from_millis(300));
    let s = bus.status();
    assert_eq!(stage(&s, "mic-denoise")["reason"], REASON_GPU_CAMERA, "{s}");
    // The camera comes on mid-session: no switch.
    let camera_on = snap(json!({ "gpu_policy": "with-camera", "camera_active": true }));
    bus.call("ApplySnapshot", Some(&camera_on.to_string()))
        .unwrap();
    p.run(Duration::from_millis(500));
    let s = bus.status();
    assert_eq!(stage(&s, "mic-denoise")["reason"], REASON_GPU_CAMERA, "{s}");
    assert_eq!(s["provider"]["mic-enhance"], common::HELLO_ID, "{s}");

    // The next session starts with the camera on: the GPU module runs.
    p.disconnect();
    p.connect(5 * SEC).unwrap();
    let mut ok = false;
    for _ in 0..50 {
        p.run(Duration::from_millis(100));
        let s = bus.status();
        if s["provider"]["mic-denoise"] == FIXTURE_ID && s["state"] == "active" {
            ok = true;
            break;
        }
    }
    assert!(ok, "{}\n{}", bus.status(), w.log_text());
    assert_eq!(bus.status()["provider"]["mic-enhance"], FIXTURE_ID);

    // Never again: back to the CPU module, without a gap.
    bus.call(
        "ApplySnapshot",
        Some(&snap(json!({ "gpu_policy": "never" })).to_string()),
    )
    .unwrap();
    let mut ok = false;
    for _ in 0..50 {
        p.run(Duration::from_millis(100));
        if bus.status()["provider"]["mic-enhance"] == common::HELLO_ID {
            ok = true;
            break;
        }
    }
    assert!(ok, "{}", bus.status());
    p.assert_in_order();
}

/// A module the crash ledger disabled is enabled again by `ResetModule`:
/// its entry is cleared and it runs; an unknown module is refused.
#[test]
fn reset_module_enables_a_crash_disabled_module_again() {
    let bus = PrivateBus::start();
    let mut env = Env::new();
    env.bus = Some(bus.address.clone());
    env.fixture("");
    env.snapshot(fixture_snapshot(200, json!({})));
    std::fs::write(
        env.ledger_path(),
        json!({ "disabled": { FIXTURE_ID: obc_module_host::REASON_CRASHED } }).to_string(),
    )
    .unwrap();
    let w = env.spawn();
    bus.wait_for_worker(5 * SEC);
    let mut p = Plugin::new(&env.socket_path);
    p.connect(5 * SEC).unwrap();
    p.run(Duration::from_millis(500));
    assert!(p.out.iter().all(|o| o.raw()), "disabled: raw");
    let modules: serde_json::Value =
        serde_json::from_str(&bus.call("GetModules", None).unwrap()).unwrap();
    let fixture = modules["modules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == FIXTURE_ID)
        .unwrap()
        .clone();
    assert_eq!(fixture["enabled"], false, "{fixture}");

    assert!(bus.call_bool("ResetModule", FIXTURE_ID).unwrap());
    assert!(
        p.run_until(5 * SEC, |o| processed(o).len() >= 5),
        "{}",
        w.log_text()
    );
    let ledger: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(env.ledger_path()).unwrap()).unwrap();
    assert!(ledger["disabled"].get(FIXTURE_ID).is_none(), "{ledger}");
    // Again: it was not disabled.
    assert!(!bus.call_bool("ResetModule", FIXTURE_ID).unwrap());
    let e = bus
        .call_bool("ResetModule", "org.example.none")
        .unwrap_err();
    assert!(e.contains("InvalidArgs"), "{e}");
    p.assert_in_order();
}

/// A manifest of the fixture with one stage, `mic-denoise`, on the CPU.
fn denoise_only_manifest(env: &Env) -> String {
    let lib = env.dir.path().join("fixture.so");
    std::fs::copy(common::fixture_lib(), &lib).unwrap();
    format!(
        r#"abi = 1
id = "{FIXTURE_ID}"
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
frame_samples = [480]
"#,
        lib.display()
    )
}

fn ledger(env: &Env) -> serde_json::Value {
    std::fs::read_to_string(env.ledger_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(json!({}))
}

/// The chain in use fails (its only stage) while the new chain warms up:
/// the new one takes over at once, and processing goes on.
#[test]
fn a_chain_that_fails_while_the_next_warms_up_hands_over_to_it() {
    let bus = PrivateBus::start();
    let mut env = Env::new();
    env.bus = Some(bus.address.clone());
    env.manifest("fixture", &denoise_only_manifest(&env));
    env.hello();
    // The fixture's block 40 fails its only stage: its group fails open.
    env.env
        .push(("OBC_FIXTURE_AUDIO_FAIL_AT".into(), "40".into()));
    let base = json!({
        "version": 1, "idle_exit_s": 0,
        "values": { "mic.x_example_hello_audio.enabled": false, "mic.x_example_hello_audio.gain": -6 },
    });
    env.snapshot(base.clone());
    let w = env.spawn();
    bus.wait_for_worker(5 * SEC);
    let mut p = Plugin::new(&env.socket_path);
    p.connect(5 * SEC).unwrap();
    // One block at a time until the first processed one: the block before
    // it was the chain's first (its warm-up, raw).
    let end = std::time::Instant::now() + 5 * SEC;
    while !p.out.iter().any(|o| !o.raw()) {
        assert!(std::time::Instant::now() < end, "{}", w.log_text());
        assert!(p.step(SEC), "no answer:\n{}", w.log_text());
        std::thread::sleep(Duration::from_millis(5));
    }
    // The fixture has run 2 blocks; on to 39.
    for _ in 2..39 {
        assert!(p.step(SEC));
    }
    assert!(!p.out.last().unwrap().raw());
    // A new chain (the example module added): loaded while no block comes,
    // so it warms up on the very block the fixture fails.
    let mut more = base.clone();
    more["values"]["mic.x_example_hello_audio.enabled"] = json!(true);
    bus.call("ApplySnapshot", Some(&more.to_string())).unwrap();
    let end = std::time::Instant::now() + 5 * SEC;
    while w.log_text().matches("chain loaded").count() < 2 {
        assert!(std::time::Instant::now() < end, "{}", w.log_text());
        std::thread::sleep(Duration::from_millis(20));
    }
    let failed_at = p.out.len();
    for _ in 0..10 {
        assert!(p.step(SEC));
    }
    w.wait_log("failed open", SEC);
    w.wait_log("the chain in use failed: switched to the new chain", SEC);
    let g6 = 10f32.powf(-6.0 / 20.0);
    let after = &p.out[failed_at + 1..];
    assert!(
        after.iter().all(|o| !o.raw() && o.is_input_times(g6)),
        "processing did not resume with the new chain:\n{}",
        w.log_text()
    );
    p.assert_in_order();
}

/// A module that never returns from a block: the watchdog aborts the
/// worker, whose crash marker for the module stays (a strike at the next
/// start); the plugin plays raw meanwhile.
#[test]
fn a_module_hanging_in_a_block_aborts_the_worker_with_a_strike() {
    let mut env = Env::new();
    env.fixture("");
    env.env
        .push(("OBC_FIXTURE_AUDIO_SLEEP_MS".into(), "60000".into()));
    env.snapshot(fixture_snapshot(100, json!({})));
    let mut w = env.spawn();
    let mut p = Plugin::new(&env.socket_path);
    p.connect(5 * SEC).unwrap();
    let started = std::time::Instant::now();
    p.run(Duration::from_secs(6));
    let status = w.wait_exit(5 * SEC);
    assert!(!status.success(), "{status:?}");
    assert!(
        w.log_text().contains("aborting the audio worker"),
        "{}",
        w.log_text()
    );
    assert!(started.elapsed() < Duration::from_secs(11));
    assert!(!p.connected());
    assert!(p.raw_own > 0, "the plugin played raw by its own path");
    assert!(
        ledger(&env)["running"].get(FIXTURE_ID).is_some(),
        "{}",
        ledger(&env)
    );
    // The next start counts it.
    env.env.clear();
    let w2 = env.spawn();
    let end = std::time::Instant::now() + 5 * SEC;
    while ledger(&env)["strikes"][FIXTURE_ID] != 1 {
        assert!(
            std::time::Instant::now() < end,
            "{}\n{}",
            ledger(&env),
            w2.log_text()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// SIGTERM while a chain runs: a clean exit that leaves no crash marker.
#[test]
fn a_clean_stop_leaves_no_crash_marker() {
    let mut env = Env::new();
    env.fixture("");
    env.snapshot(fixture_snapshot(100, json!({})));
    let mut w = env.spawn();
    let mut p = Plugin::new(&env.socket_path);
    p.connect(5 * SEC).unwrap();
    assert!(p.run_until(5 * SEC, |o| processed(o).len() >= 10));
    assert!(ledger(&env)["running"].get(FIXTURE_ID).is_some());
    // SAFETY: a signal to our own child.
    unsafe {
        libc::kill(w.child.id() as i32, libc::SIGTERM);
    }
    let status = w.wait_exit(10 * SEC);
    assert!(status.success(), "{status:?}\n{}", w.log_text());
    assert!(
        ledger(&env)["running"].get(FIXTURE_ID).is_none(),
        "{}",
        ledger(&env)
    );
}

/// The plan comes to need a deeper pipeline than the connection has (the
/// GPU policy allows the GPU module now): the worker drops the connection
/// and the plugin connects again with the new depth.
#[test]
fn a_deeper_pipeline_is_negotiated_again() {
    let bus = PrivateBus::start();
    let mut env = Env::new();
    env.bus = Some(bus.address.clone());
    gpu_fixture_and_hello(&mut env);
    let snap = |mut extra: serde_json::Value| {
        extra["values"] =
            json!({ "mic.fixture.gain": 100, "mic.x_example_hello_audio.enabled": true });
        fixture_snapshot(100, extra)
    };
    env.snapshot(snap(json!({ "gpu_policy": "never" })));
    let w = env.spawn();
    bus.wait_for_worker(5 * SEC);
    let mut p = Plugin::new(&env.socket_path);
    p.connect(5 * SEC).unwrap();
    assert_eq!(p.accept().pipeline_depth, 1);
    assert!(p.run_until(5 * SEC, |o| processed(o).len() >= 5));
    bus.call(
        "ApplySnapshot",
        Some(&snap(json!({ "gpu_policy": "always" })).to_string()),
    )
    .unwrap();
    p.run(Duration::from_secs(1));
    assert!(!p.connected(), "{}", w.log_text());
    w.wait_log("needs a pipeline depth of 2", SEC);
    // The app still records while the plugin connects again: no flip to
    // passthrough.
    let s = bus.status();
    assert_eq!(s["connected"], true, "{s}");
    assert_ne!(s["state"], "passthrough", "{s}");
    p.connect(5 * SEC).unwrap();
    assert_eq!(p.accept().pipeline_depth, 2);
    assert!(
        p.run_until(5 * SEC, |o| o
            .iter()
            .filter(|x| x.conn == 2 && !x.raw())
            .count()
            >= 5),
        "{}",
        w.log_text()
    );
    assert_eq!(bus.status()["provider"]["mic-denoise"], FIXTURE_ID);
    assert_eq!(bus.status()["pipeline_depth"], 2);
    p.assert_in_order();
}

/// With the bus: D-Bus calls keep the worker up; once they stop it
/// releases its name and exits 0.
#[test]
fn calls_keep_the_worker_up_and_it_exits_idle_after_them() {
    let bus = PrivateBus::start();
    let mut env = Env::new();
    env.bus = Some(bus.address.clone());
    env.fixture("");
    env.snapshot(fixture_snapshot(100, json!({ "idle_exit_s": 1 })));
    let mut w = env.spawn();
    bus.wait_for_worker(5 * SEC);
    for _ in 0..8 {
        std::thread::sleep(Duration::from_millis(300));
        bus.call("GetStatus", None)
            .unwrap_or_else(|e| panic!("{e}\n{}", w.log_text()));
    }
    let status = w.wait_exit(5 * SEC);
    assert!(status.success(), "{status:?}\n{}", w.log_text());
    assert!(w.log_text().contains("idle: exiting"));
    assert!(bus.call("GetStatus", None).is_err(), "the name is gone");
}

/// With no plugin connected nothing records: `passthrough`, with no
/// provider and no latency, before the first recorder and after the last
/// one (the chain that ran last stays loaded); `off` when processing is off.
#[test]
fn the_state_is_passthrough_while_no_plugin_is_connected() {
    let bus = PrivateBus::start();
    let mut env = Env::new();
    env.bus = Some(bus.address.clone());
    env.fixture("");
    env.snapshot(fixture_snapshot(100, json!({})));
    let w = env.spawn();
    bus.wait_for_worker(5 * SEC);
    let idle = |s: &serde_json::Value| {
        assert_eq!(s["state"], "passthrough", "{s}");
        assert_eq!(s["connected"], false, "{s}");
        assert_eq!(s["provider"], json!({}), "{s}");
        assert_eq!(s["latency_ms"], 0.0, "{s}");
        assert_eq!(s["latency_samples"], 0, "{s}");
        assert_eq!(s["stages"].as_array().unwrap().len(), 3, "{s}");
    };
    idle(&bus.status());

    let mut p = Plugin::new(&env.socket_path);
    p.connect(5 * SEC).unwrap();
    assert!(
        p.run_until(5 * SEC, |o| processed(o).len() >= 5),
        "{}",
        w.log_text()
    );
    let s = bus.status();
    assert_eq!(s["state"], "active", "{s}");
    assert_eq!(s["provider"]["mic-enhance"], FIXTURE_ID, "{s}");
    assert!(s["latency_ms"].as_f64().unwrap() > 0.0, "{s}");

    p.disconnect();
    let mut gone = false;
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        if bus.status()["connected"] == false {
            gone = true;
            break;
        }
    }
    assert!(gone, "{}\n{}", bus.status(), w.log_text());
    idle(&bus.status());

    let off = fixture_snapshot(100, json!({ "enabled": false }));
    bus.call("ApplySnapshot", Some(&off.to_string())).unwrap();
    let mut off = false;
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        if bus.status()["state"] == "off" {
            off = true;
            break;
        }
    }
    assert!(off, "{}", bus.status());
}

// ---- the warm grace: the engine in process, with fake consumers ----

/// What [`FakePipeWire`] reports: the consumers, and whether they are
/// pending (PipeWire connected, its graph not synced yet).
type FakeConsumers = Arc<Mutex<(Option<Consumers>, bool)>>;

/// A PipeWire whose consumers the test sets.
struct FakePipeWire(FakeConsumers);

impl PipeWireControl for FakePipeWire {
    fn devices(&self) -> Result<Vec<SourceDevice>, PipeWireError> {
        Err(PipeWireError::Unavailable("fake".into()))
    }

    fn make_default(&self, _target: &str) -> Result<PreviousDefault, PipeWireError> {
        Err(PipeWireError::Unavailable("fake".into()))
    }

    fn restore_default(
        &self,
        _previous: &str,
        _in_force: Option<&str>,
    ) -> Result<(), PipeWireError> {
        Err(PipeWireError::Unavailable("fake".into()))
    }

    fn pin(&self, _target: &str, _in_force: Option<&str>) -> Result<(), PipeWireError> {
        Err(PipeWireError::Unavailable("fake".into()))
    }

    fn consumers(&self) -> Option<Consumers> {
        self.0.lock().unwrap().0
    }

    fn consumers_pending(&self) -> bool {
        self.0.lock().unwrap().1
    }
}

fn consumers_of(real: Option<u32>) -> Option<Consumers> {
    real.map(|real| Consumers { real, monitors: 0 })
}

/// The engine run in this process over the GPU fixture and the example
/// module (see [`gpu_fixture_and_hello`]), its consumers set by the test.
struct InProcess {
    shared: Arc<Shared>,
    consumers: FakeConsumers,
    /// The modules' log lines.
    lines: Arc<Mutex<Vec<String>>>,
    engine: Option<std::thread::JoinHandle<Exit>>,
    socket: std::path::PathBuf,
    started: Instant,
    _env: Env,
}

impl Drop for InProcess {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        if let Some(e) = self.engine.take() {
            let _ = e.join();
        }
    }
}

/// The snapshot of the warm tests: every stage on, `gpu_warm_s` as given;
/// `values` in `extra` are set over those.
fn warm_snapshot(extra: serde_json::Value) -> Snapshot {
    let mut values = json!({ "mic.fixture.gain": 100, "mic.x_example_hello_audio.enabled": true });
    if let Some(v) = extra.get("values").and_then(|v| v.as_object()) {
        for (k, x) in v {
            values[k] = x.clone();
        }
    }
    let mut s = fixture_snapshot(100, extra);
    s["values"] = values;
    Snapshot::parse(&s.to_string()).unwrap()
}

impl InProcess {
    /// Starts it with `real` recorders (`None`: unknown), `pending` or not.
    fn start(snapshot: Snapshot, (real, pending): (Option<u32>, bool)) -> InProcess {
        let mut env = Env::new();
        gpu_fixture_and_hello(&mut env);
        let lines: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink: obc_module_host::LogSink = {
            let lines = lines.clone();
            Arc::new(move |_, _, msg| lines.lock().unwrap().push(msg.to_string()))
        };
        let host = Host::load(
            obc_core::config::Config::default(),
            std::slice::from_ref(&env.modules),
            Some(&env.ledger_path()),
            &snapshot,
            sink,
        );
        let consumers: FakeConsumers = Arc::new(Mutex::new((consumers_of(real), pending)));
        let shared = Arc::new(Shared::new(
            host,
            snapshot,
            Box::new(FakePipeWire(consumers.clone())),
        ));
        let socket = env.sockets.path().join("in-process.sock");
        let engine = Engine::new(shared.clone(), listener::bind(&socket).unwrap());
        let engine = std::thread::spawn(move || engine.run());
        InProcess {
            shared,
            consumers,
            lines,
            engine: Some(engine),
            socket,
            started: Instant::now(),
            _env: env,
        }
    }

    fn set_consumers(&self, real: Option<u32>, pending: bool) {
        *self.consumers.lock().unwrap() = (consumers_of(real), pending);
    }

    fn gate(&self) -> GpuGate {
        *self.shared.gpu_gate.lock().unwrap()
    }

    /// The module planned for `mic-denoise` (the GPU fixture's only).
    fn denoise_provider(&self) -> Option<String> {
        self.shared
            .status
            .borrow()
            .stages
            .iter()
            .find(|s| s.id == "mic-denoise")
            .and_then(|s| s.provider.clone())
    }

    /// Loads of the GPU chain (the fixture's three stages together).
    fn gpu_loads(&self) -> usize {
        self.lines
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.starts_with("fixture-audio-load stages=mic-dereverb,mic-denoise"))
            .count()
    }

    /// Loads of any chain of the GPU fixture.
    fn fixture_loads(&self) -> usize {
        self.lines
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.starts_with("fixture-audio-load "))
            .count()
    }

    /// Runs the plugin (connected or not) until `cond`, at most `timeout`.
    fn run_until(&self, p: &mut Plugin, timeout: Duration, cond: impl Fn(&Self) -> bool) -> bool {
        let end = Instant::now() + timeout;
        while !cond(self) {
            if Instant::now() > end {
                return false;
            }
            p.run(Duration::from_millis(20));
        }
        true
    }

    /// A recorder: connects the plugin and runs until the GPU chain
    /// processes.
    fn record(&self, p: &mut Plugin) {
        self.set_consumers(Some(1), false);
        p.connect(5 * SEC).unwrap();
        assert!(
            self.run_until(p, 5 * SEC, |s| s.gpu_loads() == 1
                && s.denoise_provider().as_deref() == Some(FIXTURE_ID)),
            "the GPU chain did not load: {:?}",
            self.lines.lock().unwrap()
        );
        assert!(p.run_until(5 * SEC, |o| processed(o).len() >= 5));
        assert_eq!(self.gate(), GpuGate::Allowed);
    }

    /// Whether the engine has exited (when it did).
    fn wait_exit(&mut self, timeout: Duration) -> Option<(Exit, Duration)> {
        let end = Instant::now() + timeout;
        while !self.engine.as_ref().unwrap().is_finished() {
            if Instant::now() > end {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let at = self.started.elapsed();
        Some((self.engine.take().unwrap().join().unwrap(), at))
    }
}

/// The last recorder leaves: the GPU plan stays for `gpu_warm_s`, and a
/// recorder back within it finds the chain loaded (no reload); after it the
/// plan goes to the CPU.
#[test]
fn the_gpu_chain_stays_warm_for_the_grace_after_the_last_recorder() {
    let w = InProcess::start(warm_snapshot(json!({ "gpu_warm_s": 3 })), (Some(0), false));
    let mut p = Plugin::new(&w.socket);
    // Nobody records yet: CPU.
    assert_eq!(w.gate(), GpuGate::NoRecorder);
    w.record(&mut p);

    // Well inside the grace, even on a slow machine.
    w.set_consumers(Some(0), false);
    p.run(Duration::from_millis(500));
    assert_eq!(w.gate(), GpuGate::Allowed, "held within the grace");
    assert_eq!(w.denoise_provider().as_deref(), Some(FIXTURE_ID));

    // Back within the grace: the same chain goes on.
    w.set_consumers(Some(1), false);
    let n = p.out.len();
    assert!(p.run_until(5 * SEC, |o| processed(&o[n..]).len() >= 20));
    assert_eq!(w.gpu_loads(), 1, "reloaded: {:?}", w.lines.lock().unwrap());
    assert!(p.out[n..].iter().all(|o| !o.raw()), "a raw block");

    // Gone again: CPU after the grace, counted from the moment it left.
    w.set_consumers(Some(0), false);
    let left = Instant::now();
    assert!(w.run_until(&mut p, 5 * SEC, |s| s.gate() == GpuGate::NoRecorder));
    let held = left.elapsed();
    assert!(held >= Duration::from_millis(2900), "held {held:?}");
    assert!(w.run_until(&mut p, 2 * SEC, |s| s.denoise_provider().is_none()));
    p.assert_in_order();
}

/// `gpu_warm_s = 0`: the GPU plan goes at once (as before the grace); a
/// worker that never served a recorder is never warm.
#[test]
fn no_grace_at_zero_or_without_a_recorder() {
    let w = InProcess::start(warm_snapshot(json!({ "gpu_warm_s": 0 })), (Some(0), false));
    let mut p = Plugin::new(&w.socket);
    w.record(&mut p);
    w.set_consumers(Some(0), false);
    assert!(w.run_until(&mut p, SEC, |s| s.gate() == GpuGate::NoRecorder));

    let w = InProcess::start(warm_snapshot(json!({ "gpu_warm_s": 30 })), (None, true));
    let mut p = Plugin::new(&w.socket);
    p.connect(5 * SEC).unwrap();
    p.run(Duration::from_millis(300));
    assert_eq!(w.gate(), GpuGate::Pending);
    w.set_consumers(Some(0), false);
    assert!(w.run_until(&mut p, SEC, |s| s.gate() == GpuGate::NoRecorder));
    p.run(Duration::from_millis(300));
    assert_eq!(w.gpu_loads(), 0);
}

/// The policy closes the GPU at once, also within the grace.
#[test]
fn the_gpu_policy_closes_at_once_within_the_grace() {
    for (policy, want) in [
        (json!({ "gpu_policy": "never" }), GpuGate::Never),
        (
            json!({ "gpu_policy": "with-camera", "camera_active": false }),
            GpuGate::CameraOff,
        ),
    ] {
        let w = InProcess::start(warm_snapshot(json!({ "gpu_warm_s": 30 })), (Some(0), false));
        let mut p = Plugin::new(&w.socket);
        w.record(&mut p);
        w.set_consumers(Some(0), false);
        p.run(Duration::from_millis(500));
        assert_eq!(w.gate(), GpuGate::Allowed, "held");
        let mut extra = policy.clone();
        extra["gpu_warm_s"] = json!(30);
        w.shared.apply_snapshot(warm_snapshot(extra));
        assert!(
            w.run_until(&mut p, SEC, |s| s.gate() == want),
            "{policy}: {:?}",
            w.gate()
        );
        assert!(w.run_until(&mut p, 2 * SEC, |s| s.denoise_provider().is_none()));
    }
}

/// The gate open only because the consumers were not known (no PipeWire)
/// is not a recorder: when PipeWire reports none, the GPU plan goes at
/// once.
#[test]
fn unknown_consumers_are_not_a_recorder() {
    let w = InProcess::start(warm_snapshot(json!({ "gpu_warm_s": 30 })), (None, false));
    let mut p = Plugin::new(&w.socket);
    p.connect(5 * SEC).unwrap();
    assert!(
        w.run_until(&mut p, 5 * SEC, |s| s.gpu_loads() == 1
            && s.denoise_provider().as_deref() == Some(FIXTURE_ID)),
        "the GPU chain did not load: {:?}",
        w.lines.lock().unwrap()
    );
    assert_eq!(w.gate(), GpuGate::Allowed);
    w.set_consumers(Some(0), false);
    assert!(w.run_until(&mut p, SEC, |s| s.gate() == GpuGate::NoRecorder));
    assert!(w.run_until(&mut p, 2 * SEC, |s| s.denoise_provider().is_none()));
}

/// A change of what the chain loads (here a stage off) within the grace
/// ends it at once: no new GPU chain loads while nobody records.
#[test]
fn a_load_change_within_the_grace_ends_it() {
    let w = InProcess::start(warm_snapshot(json!({ "gpu_warm_s": 30 })), (Some(0), false));
    let mut p = Plugin::new(&w.socket);
    w.record(&mut p);
    w.set_consumers(Some(0), false);
    p.run(Duration::from_millis(300));
    assert_eq!(w.gate(), GpuGate::Allowed, "held");
    w.shared.apply_snapshot(warm_snapshot(json!({
        "gpu_warm_s": 30,
        "values": { "mic.fixture.denoise": false },
    })));
    assert!(
        w.run_until(&mut p, SEC, |s| s.gate() == GpuGate::NoRecorder),
        "{:?}",
        w.gate()
    );
    p.run(Duration::from_millis(500));
    assert_eq!(w.gate(), GpuGate::NoRecorder);
    assert_eq!(
        w.fixture_loads(),
        1,
        "a GPU chain loaded: {:?}",
        w.lines.lock().unwrap()
    );
}

/// A change of live parameters within the grace keeps it: the same chain,
/// no reload.
#[test]
fn a_live_change_within_the_grace_keeps_it() {
    let w = InProcess::start(warm_snapshot(json!({ "gpu_warm_s": 30 })), (Some(0), false));
    let mut p = Plugin::new(&w.socket);
    w.record(&mut p);
    w.set_consumers(Some(0), false);
    p.run(Duration::from_millis(300));
    w.shared.apply_snapshot(warm_snapshot(json!({
        "gpu_warm_s": 30,
        "values": { "mic.fixture.gain": 200 },
    })));
    p.run(Duration::from_millis(500));
    assert_eq!(w.gate(), GpuGate::Allowed, "held");
    assert_eq!(w.denoise_provider().as_deref(), Some(FIXTURE_ID));
    assert_eq!(
        w.fixture_loads(),
        1,
        "reloaded: {:?}",
        w.lines.lock().unwrap()
    );
}

/// A change of the policy within the grace ends it, also to one that
/// still allows the GPU (`with-camera` with the camera on).
#[test]
fn a_policy_change_within_the_grace_ends_it() {
    let w = InProcess::start(warm_snapshot(json!({ "gpu_warm_s": 30 })), (Some(0), false));
    let mut p = Plugin::new(&w.socket);
    w.record(&mut p);
    w.set_consumers(Some(0), false);
    p.run(Duration::from_millis(300));
    assert_eq!(w.gate(), GpuGate::Allowed, "held");
    w.shared.apply_snapshot(warm_snapshot(json!({
        "gpu_warm_s": 30,
        "gpu_policy": "with-camera",
        "camera_active": true,
    })));
    assert!(
        w.run_until(&mut p, SEC, |s| s.gate() == GpuGate::NoRecorder),
        "{:?}",
        w.gate()
    );
    assert!(w.run_until(&mut p, 2 * SEC, |s| s.denoise_provider().is_none()));
}

/// The plugin disconnects and connects again within the grace, PipeWire's
/// graph not known for a while (pending): the warm chain stays and serves
/// the next recorder without a reload.
#[test]
fn a_reconnect_within_the_grace_keeps_the_warm_chain() {
    let w = InProcess::start(warm_snapshot(json!({ "gpu_warm_s": 30 })), (Some(0), false));
    let mut p = Plugin::new(&w.socket);
    w.record(&mut p);
    w.set_consumers(Some(0), false);
    p.run(Duration::from_millis(300));
    p.disconnect();
    w.set_consumers(None, true);
    p.run(Duration::from_millis(500));
    assert_eq!(w.gate(), GpuGate::Allowed, "held while pending");
    p.connect(5 * SEC).unwrap();
    p.run(Duration::from_millis(300));
    assert_eq!(w.gate(), GpuGate::Allowed);
    w.set_consumers(Some(1), false);
    let n = p.out.len();
    assert!(p.run_until(5 * SEC, |o| processed(&o[n..]).len() >= 20));
    assert_eq!(w.gpu_loads(), 1, "reloaded: {:?}", w.lines.lock().unwrap());
    assert_eq!(w.denoise_provider().as_deref(), Some(FIXTURE_ID));
}

/// The idle exit waits for the end of the grace (the later of the two).
#[test]
fn the_idle_exit_waits_for_the_grace() {
    let mut w = InProcess::start(
        warm_snapshot(json!({ "gpu_warm_s": 3, "idle_exit_s": 1 })),
        (Some(0), false),
    );
    let mut p = Plugin::new(&w.socket);
    w.record(&mut p);
    w.set_consumers(Some(0), false);
    let left = w.started.elapsed();
    p.disconnect();
    let (exit, at) = w.wait_exit(10 * SEC).expect("the worker did not exit");
    assert_eq!(exit, Exit::Idle);
    let after = at - left;
    assert!(
        after >= Duration::from_millis(2900) && after < Duration::from_secs(6),
        "exited {after:?} after the recorder left"
    );
}

/// A stand-in for the video daemon: every `StatusChanged` is answered at
/// once with calls (it asks for the modules and the settings), which are
/// activity for the idle exit. Records when each change came.
struct Daemon {
    changes: Arc<Mutex<Vec<Instant>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Daemon {
    fn start(shared: &Arc<Shared>) -> Daemon {
        let changes: Arc<Mutex<Vec<Instant>>> = Arc::default();
        let stop: Arc<std::sync::atomic::AtomicBool> = Arc::default();
        let thread = {
            let (shared, changes, stop) = (shared.clone(), changes.clone(), stop.clone());
            let mut rx = shared.status.subscribe();
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) && !shared.shutdown.load(Ordering::SeqCst) {
                    if rx.has_changed().unwrap_or(false) {
                        rx.borrow_and_update();
                        changes.lock().unwrap().push(Instant::now());
                        drop(shared.call());
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
            })
        };
        Daemon {
            changes,
            stop,
            thread: Some(thread),
        }
    }

    /// The status changes since `t`.
    fn changes_since(&self, t: Instant) -> usize {
        self.changes
            .lock()
            .unwrap()
            .iter()
            .filter(|c| **c >= t)
            .count()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The grace ends while no plugin is connected (the recorder stopped, then
/// the plugin left): the plan and the chain stay as they are, with no load
/// and no status change, so that the daemon's answer to one does not put
/// the idle exit off. The worker exits at the later of the last activity
/// plus `idle_exit_s` and the grace's end, also when the grace outlasts
/// the idle clock.
#[test]
fn a_grace_ending_with_no_plugin_changes_nothing_and_the_idle_exit_is_not_put_off() {
    // (gpu_warm_s, idle_exit_s, the plugin leaves after the recorder, ms)
    for (warm, idle, plays) in [(3u64, 2u64, 1500u64), (3, 1, 0)] {
        let mut w = InProcess::start(
            warm_snapshot(json!({ "gpu_warm_s": warm, "idle_exit_s": idle })),
            (Some(0), false),
        );
        let daemon = Daemon::start(&w.shared);
        let mut p = Plugin::new(&w.socket);
        w.record(&mut p);
        w.set_consumers(Some(0), false);
        let left = Instant::now();
        p.run(Duration::from_millis(plays));
        p.disconnect();
        let last_input = Instant::now();
        let grace_end = left + Duration::from_secs(warm);
        let want = (last_input + Duration::from_secs(idle)).max(grace_end);
        let (exit, _) = w.wait_exit(15 * SEC).expect("the worker did not exit");
        let at = Instant::now();
        assert_eq!(exit, Exit::Idle);
        let late = at.saturating_duration_since(want);
        assert!(
            at + Duration::from_millis(100) >= want && late < Duration::from_millis(700),
            "warm {warm} s, idle {idle} s: exited {:?} after the recorder left, wanted {:?}",
            at - left,
            want - left
        );
        assert_eq!(
            daemon.changes_since(grace_end - Duration::from_millis(100)),
            0,
            "a status change at the grace's end"
        );
        assert_eq!(w.gate(), GpuGate::Allowed, "replanned at the grace's end");
        assert_eq!(w.denoise_provider().as_deref(), Some(FIXTURE_ID));
        assert_eq!(
            w.fixture_loads(),
            1,
            "a chain loaded: {:?}",
            w.lines.lock().unwrap()
        );
    }
}

/// After a grace that ended with no plugin connected, the next connection
/// decides the gate afresh: a level meter gets the CPU plan; a recorder
/// gets the GPU chain still loaded, with no reload.
#[test]
fn a_connection_after_a_grace_ended_with_no_plugin_decides_afresh() {
    for recorder in [false, true] {
        let w = InProcess::start(warm_snapshot(json!({ "gpu_warm_s": 1 })), (Some(0), false));
        let mut p = Plugin::new(&w.socket);
        w.record(&mut p);
        w.set_consumers(Some(0), false);
        p.disconnect();
        // Well past the grace.
        p.run(Duration::from_millis(1800));
        assert_eq!(w.gate(), GpuGate::Allowed, "held after the grace");
        assert_eq!(w.denoise_provider().as_deref(), Some(FIXTURE_ID));

        w.set_consumers(Some(u32::from(recorder)), false);
        p.connect(5 * SEC).unwrap();
        if recorder {
            let n = p.out.len();
            assert!(p.run_until(5 * SEC, |o| processed(&o[n..]).len() >= 20));
            assert_eq!(w.gate(), GpuGate::Allowed);
            assert_eq!(w.denoise_provider().as_deref(), Some(FIXTURE_ID));
            assert_eq!(w.gpu_loads(), 1, "reloaded: {:?}", w.lines.lock().unwrap());
        } else {
            assert!(
                w.run_until(&mut p, SEC, |s| s.gate() == GpuGate::NoRecorder),
                "a level meter kept the GPU plan: {:?}",
                w.gate()
            );
            assert!(w.run_until(&mut p, 2 * SEC, |s| s.denoise_provider().is_none()));
        }
    }
}

// ---- the hardware gain: the engine in process, a fake mixer ----

/// The ALSA side [`GainPipeWire`] reports, and its serial.
type FakeSources = Arc<Mutex<(u64, Option<AlsaSources>)>>;

/// A PipeWire whose hardware sources the test sets.
struct GainPipeWire(FakeSources);

impl PipeWireControl for GainPipeWire {
    fn devices(&self) -> Result<Vec<SourceDevice>, PipeWireError> {
        Err(PipeWireError::Unavailable("fake".into()))
    }

    fn make_default(&self, _target: &str) -> Result<PreviousDefault, PipeWireError> {
        Err(PipeWireError::Unavailable("fake".into()))
    }

    fn restore_default(
        &self,
        _previous: &str,
        _in_force: Option<&str>,
    ) -> Result<(), PipeWireError> {
        Err(PipeWireError::Unavailable("fake".into()))
    }

    fn pin(&self, _target: &str, _in_force: Option<&str>) -> Result<(), PipeWireError> {
        Err(PipeWireError::Unavailable("fake".into()))
    }

    fn consumers(&self) -> Option<Consumers> {
        Some(Consumers::default())
    }

    fn alsa_sources(&self) -> Option<AlsaSources> {
        self.0.lock().unwrap().1.clone()
    }

    fn alsa_sources_serial(&self) -> u64 {
        self.0.lock().unwrap().0
    }
}

/// Every call the engine's gain thread made of [`GainMixer`].
#[derive(Debug, Default)]
struct GainCalls {
    reads: usize,
    sets: Vec<f64>,
    db: f64,
}

/// One capture control, `Headset`, +18..+54 dB in 1 dB steps.
struct GainMixer(Arc<Mutex<GainCalls>>);

impl Mixer for GainMixer {
    fn capture_controls(&mut self, _card: &str) -> Result<Vec<String>, String> {
        Ok(vec!["Headset".into()])
    }

    fn read(&mut self, _card: &str, control: &str) -> Result<Reading, String> {
        let mut c = self.0.lock().unwrap();
        c.reads += 1;
        Ok(Reading {
            control: control.into(),
            db: c.db,
            min_db: 18.0,
            max_db: 54.0,
            step_db: 1.0,
        })
    }

    fn set_db(&mut self, _card: &str, _control: &str, db: f64) -> Result<(), String> {
        let mut c = self.0.lock().unwrap();
        c.sets.push(db);
        c.db = db;
        Ok(())
    }
}

/// The engine hands the gain thread the device and the value only when
/// they (or the serial) change, and the status changes only with the
/// reading: pending before PipeWire's sources are known.
#[test]
fn the_engine_hands_the_gain_over_on_change_only() {
    let env = Env::new();
    let snapshot = |serial: u64, warm: u64| {
        Snapshot::parse(
            &json!({
                "version": 1,
                "device": "alsa_input.usb-brio",
                "idle_exit_s": 0,
                "gpu_warm_s": warm,
                "hardware_gain_db": 35.0,
                "hardware_gain_serial": serial,
            })
            .to_string(),
        )
        .unwrap()
    };
    let host = Host::load(
        obc_core::config::Config::default(),
        &[],
        None,
        &snapshot(0, 0),
        Arc::new(|_, _, _| {}),
    );
    let sources: FakeSources = Arc::new(Mutex::new((0, None)));
    let calls: Arc<Mutex<GainCalls>> = Arc::new(Mutex::new(GainCalls {
        db: 40.0,
        ..GainCalls::default()
    }));
    let shared = Arc::new(
        Shared::new(
            host,
            snapshot(0, 0),
            Box::new(GainPipeWire(sources.clone())),
        )
        .with_mixer(Box::new(GainMixer(calls.clone()))),
    );
    let mut status = shared.status.subscribe();
    let socket = env.sockets.path().join("gain.sock");
    let engine = Engine::new(shared.clone(), listener::bind(&socket).unwrap());
    let engine = std::thread::spawn(move || engine.run());
    let stop = |engine: std::thread::JoinHandle<Exit>| {
        shared.shutdown.store(true, Ordering::SeqCst);
        engine.join().unwrap()
    };
    let wait = |what: &str, cond: &dyn Fn() -> bool| {
        let end = Instant::now() + 5 * SEC;
        while !cond() {
            assert!(Instant::now() < end, "never {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    };

    // PipeWire not synced: nothing handed over, the gain pending.
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(shared.status.borrow().hardware_gain, None);
    assert_eq!(calls.lock().unwrap().reads, 0);

    // The device appears: set once, and reported.
    *sources.lock().unwrap() = (
        1,
        Some(AlsaSources {
            capture: None,
            captured: None,
            sources: vec![AlsaSource {
                id: 33,
                name: "alsa_input.usb-brio".into(),
                card: Some("0".into()),
                usb: true,
            }],
        }),
    );
    wait("applied", &|| {
        shared
            .status
            .borrow()
            .hardware_gain
            .clone()
            .flatten()
            .is_some_and(|g| g.applied && g.db == 35.0)
    });
    assert_eq!(calls.lock().unwrap().sets, [35.0]);

    // Many turns of the loop, the sources' serial moving with the same
    // sources: nothing handed over (no read), no status change.
    let reads = calls.lock().unwrap().reads;
    status.mark_unchanged();
    for n in 2..12 {
        sources.lock().unwrap().0 = n;
        std::thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(calls.lock().unwrap().reads, reads, "handed over again");
    assert!(!status.has_changed().unwrap(), "{:?}", *status.borrow());

    // Another snapshot, the same gain and serial: nothing set.
    shared.apply_snapshot(snapshot(0, 5));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(calls.lock().unwrap().sets, [35.0]);

    // The same value set again (another serial): set again.
    shared.apply_snapshot(snapshot(1, 5));
    wait("set again", &|| calls.lock().unwrap().sets.len() == 2);
    assert_eq!(stop(engine), Exit::Shutdown);
}
