//! The audio worker end to end: its binary, real module libraries and a
//! fake mic plugin driving the shared-memory rings (see `common`).

mod common;

use std::time::Duration;

use common::{Env, FIXTURE_ID, Out, Plugin, PrivateBus};
use obc_audio::proto::RefuseReason;
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
        "values": { "mic.hello.enabled": true, "mic.hello.gain": -6 },
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
    live["values"]["mic.hello.gain"] = json!(0);
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
    tone["values"]["mic.hello.tone"] = json!(true);
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
        extra["values"] = json!({ "mic.fixture.gain": 100, "mic.hello.enabled": true });
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
        "values": { "mic.hello.enabled": false, "mic.hello.gain": -6 },
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
    more["values"]["mic.hello.enabled"] = json!(true);
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
        extra["values"] = json!({ "mic.fixture.gain": 100, "mic.hello.enabled": true });
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
