//! The worker's PipeWire side against a PRIVATE, headless PipeWire and
//! WirePlumber (`scripts/mic-pw-sandbox.sh`: temporary runtime, config,
//! state and data directories inside `dbus-run-session`, no device
//! monitors, two virtual microphones fed by `pw-cat`), with the real mic
//! host (`dist/pipewire/omarchy-broadcast-mic.conf`) loading the plugin
//! built from `crates/obc-mic-shim`.
//!
//! Ignored by default (it needs pipewire, wireplumber, the pw-* tools,
//! dbus-run-session and python3): `cargo test -p omarchy-broadcast-audiod
//! --test pipewire -- --ignored`. The test re-runs itself inside the
//! sandbox, and the inner run refuses to start unless its runtime
//! directory is the sandbox's temporary one: the user's PipeWire,
//! WirePlumber, default source and ~/.config are never touched.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const TEST: &str = "the_mic_in_a_private_pipewire";

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Builds the forwarding plugin (obc-mic-shim) once, in this test's own
/// target directory.
fn plugin() -> PathBuf {
    let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("mic-shim");
    let status = Command::new(env!("CARGO"))
        .args(["build", "--quiet", "-p", "obc-mic-shim", "--target-dir"])
        .arg(&target)
        .current_dir(repo())
        .status()
        .expect("run cargo");
    assert!(status.success(), "building obc-mic-shim failed");
    target.join("debug/libomarchy_broadcast_mic.so")
}

#[test]
#[ignore = "needs a private PipeWire and WirePlumber (scripts/mic-pw-sandbox.sh); run with --ignored"]
fn the_mic_in_a_private_pipewire() {
    if std::env::var_os("OBC_PW_SANDBOX").is_none() {
        let plugin = plugin();
        let me = std::env::current_exe().unwrap();
        let status = Command::new(repo().join("scripts/mic-pw-sandbox.sh"))
            .arg("--plugin")
            .arg(&plugin)
            .arg("--")
            .arg(me)
            .args([
                "--exact",
                TEST,
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .status()
            .expect("run scripts/mic-pw-sandbox.sh");
        assert!(status.success(), "the sandboxed run failed: {status:?}");
        return;
    }
    inner();
}

/// Refuses to go on unless every directory PipeWire, WirePlumber and the
/// worker use is the sandbox's temporary one.
fn assert_isolated() -> PathBuf {
    let sandbox = PathBuf::from(std::env::var_os("OBC_PW_SANDBOX").unwrap());
    let tmp =
        std::env::var_os("OBC_PW_SANDBOX_TMP").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    assert!(
        sandbox.starts_with(&tmp) && sandbox != tmp,
        "{} is not a temporary directory",
        sandbox.display()
    );
    let name = sandbox.file_name().unwrap().to_string_lossy().to_string();
    assert!(name.starts_with("obc-pw."), "{name}");
    // Every per-user XDG directory, set or not, and PipeWire's.
    let mut vars: Vec<String> = [
        "XDG_RUNTIME_DIR",
        "PIPEWIRE_RUNTIME_DIR",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
    ]
    .map(String::from)
    .to_vec();
    vars.extend(
        std::env::vars_os()
            .filter_map(|(k, _)| k.into_string().ok())
            .filter(|k| k.starts_with("XDG_") && (k.ends_with("_HOME") || k.ends_with("_DIR"))),
    );
    for var in &vars {
        let v = PathBuf::from(std::env::var_os(var).unwrap_or_default());
        assert!(
            v.starts_with(&sandbox),
            "{var}={} is outside the sandbox {}",
            v.display(),
            sandbox.display()
        );
    }
    // The worker's socket and the session bus too.
    let socket = obc_audio::default_socket_path().expect("a socket path");
    assert!(
        socket.starts_with(&sandbox),
        "the worker's socket {} is outside the sandbox",
        socket.display()
    );
    assert!(std::env::var_os(obc_audio::SOCKET_ENV).is_none());
    let bus = std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap_or_default();
    let bus_path = bus
        .strip_prefix("unix:path=")
        .map(|rest| PathBuf::from(rest.split(',').next().unwrap_or_default()));
    assert!(
        bus_path.is_some_and(|p| p.starts_with(&sandbox)),
        "the session bus {bus:?} is outside the sandbox {}",
        sandbox.display()
    );
    let uid = unsafe { libc::getuid() };
    assert_ne!(
        PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap()),
        PathBuf::from(format!("/run/user/{uid}"))
    );
    assert!(std::env::var_os("PIPEWIRE_REMOTE").is_none());
    sandbox
}

/// A child process, killed when dropped.
struct Proc(Child, &'static str);

impl Proc {
    fn kill9(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }

    fn stop(&mut self) {
        // SIGTERM, then wait.
        unsafe {
            libc::kill(self.0.id() as i32, libc::SIGTERM);
        }
        let end = Instant::now() + Duration::from_secs(3);
        while Instant::now() < end {
            if let Ok(Some(_)) = self.0.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.kill9();
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        if let Ok(None) = self.0.try_wait() {
            eprintln!("stopping {}", self.1);
            self.kill9();
        }
    }
}

fn spawn(sandbox: &Path, name: &'static str, cmd: &mut Command) -> Proc {
    let log = std::fs::File::create(sandbox.join(format!("{name}.log"))).unwrap();
    let child = cmd
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap_or_else(|e| panic!("{name}: {e}"));
    Proc(child, name)
}

fn mic_host(sandbox: &Path) -> Proc {
    let conf = std::env::var("OBC_PW_SANDBOX_MIC_CONF").unwrap();
    spawn(
        sandbox,
        "mic-host",
        Command::new("pipewire").arg("-c").arg(conf),
    )
}

fn worker(sandbox: &Path, snapshot: &Path) -> Proc {
    spawn(
        sandbox,
        "worker",
        Command::new(env!("CARGO_BIN_EXE_omarchy-broadcast-audiod"))
            // Never the SDK environment the worker applies to itself.
            .env(obc_core::sdk_env::OPT_OUT, "1")
            .arg("-v")
            .arg("--snapshot")
            .arg(snapshot)
            .arg("--module-dir")
            .arg(sandbox.join("no-modules")),
    )
}

fn record(sandbox: &Path, name: &'static str, extra: &str) -> Proc {
    spawn(
        sandbox,
        name,
        Command::new("pw-record")
            .args(["--target", "omarchy-broadcast-mic", "-P"])
            .arg(format!("{{ node.name = {name} {extra} }}"))
            .arg("/dev/null"),
    )
}

fn pw_dump() -> Value {
    let out = Command::new("pw-dump").output().expect("pw-dump");
    serde_json::from_slice(&out.stdout).unwrap_or(Value::Array(vec![]))
}

fn node<'a>(dump: &'a Value, name: &str) -> Option<&'a Value> {
    dump.as_array()?
        .iter()
        .find(|o| o["type"] == "PipeWire:Interface:Node" && o["info"]["props"]["node.name"] == name)
}

fn id_of(dump: &Value, name: &str) -> Option<u64> {
    node(dump, name)?["id"].as_u64()
}

/// A JSON scalar as text (a number or a string alike).
fn text(v: &Value) -> String {
    v.as_str().map_or_else(|| v.to_string(), str::to_string)
}

fn serial_of(dump: &Value, name: &str) -> Option<String> {
    node(dump, name).map(|n| text(&n["info"]["props"]["object.serial"]))
}

/// The `default` metadata's entries: (subject, key, value).
fn metadata(dump: &Value) -> Vec<(u64, String, Value)> {
    let Some(m) = dump.as_array().and_then(|a| {
        a.iter().find(|o| {
            o["type"] == "PipeWire:Interface:Metadata" && o["props"]["metadata.name"] == "default"
        })
    }) else {
        return vec![];
    };
    m["metadata"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .map(|e| {
                    (
                        e["subject"].as_u64().unwrap_or(0),
                        e["key"].as_str().unwrap_or("").to_string(),
                        e["value"].clone(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn meta_get(dump: &Value, subject: u64, key: &str) -> Option<Value> {
    metadata(dump)
        .into_iter()
        .find(|(s, k, _)| *s == subject && k == key)
        .map(|(_, _, v)| v)
}

fn configured_source(dump: &Value) -> Option<String> {
    meta_get(dump, 0, "default.configured.audio.source")
        .and_then(|v| v["name"].as_str().map(str::to_string))
}

/// Whether a link goes from node `from` to node `to`.
fn linked(dump: &Value, from: &str, to: &str) -> bool {
    let (Some(f), Some(t)) = (id_of(dump, from), id_of(dump, to)) else {
        return false;
    };
    dump.as_array().unwrap().iter().any(|o| {
        o["type"] == "PipeWire:Interface:Link"
            && o["info"]["output-node-id"].as_u64() == Some(f)
            && o["info"]["input-node-id"].as_u64() == Some(t)
    })
}

/// The `rate` of the mic node's `ProcessLatency`.
fn process_latency(dump: &Value) -> Option<u64> {
    node(dump, "omarchy-broadcast-mic")?["info"]["params"]["ProcessLatency"]
        .as_array()?
        .first()?["rate"]
        .as_u64()
}

/// Waits for `cond` on fresh dumps; the last dump on failure.
fn wait_dump(what: &str, timeout: Duration, cond: impl Fn(&Value) -> bool) -> Value {
    let end = Instant::now() + timeout;
    loop {
        let d = pw_dump();
        if cond(&d) {
            return d;
        }
        if Instant::now() > end {
            panic!(
                "timed out waiting for {what}:\n{}",
                serde_json::to_string_pretty(&d).unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The worker over the (private) session bus.
struct Bus {
    rt: tokio::runtime::Runtime,
    conn: zbus::Connection,
}

impl Bus {
    fn new() -> Bus {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let conn = rt.block_on(zbus::Connection::session()).unwrap();
        Bus { rt, conn }
    }

    fn call<B>(&self, method: &str, arg: Option<&str>) -> Result<B, String>
    where
        B: for<'d> serde::Deserialize<'d> + zbus::zvariant::Type,
    {
        use omarchy_broadcast_audiod::dbus::{BUS_NAME, INTERFACE, OBJECT_PATH};
        self.rt.block_on(async {
            let reply = match arg {
                Some(a) => {
                    self.conn
                        .call_method(Some(BUS_NAME), OBJECT_PATH, Some(INTERFACE), method, &(a,))
                        .await
                }
                None => {
                    self.conn
                        .call_method(Some(BUS_NAME), OBJECT_PATH, Some(INTERFACE), method, &())
                        .await
                }
            }
            .map_err(|e| e.to_string())?;
            reply.body().deserialize::<B>().map_err(|e| e.to_string())
        })
    }

    fn json(&self, method: &str, arg: Option<&str>) -> Value {
        let s: String = self
            .call(method, arg)
            .unwrap_or_else(|e| panic!("{method}: {e}"));
        serde_json::from_str(&s).unwrap()
    }

    fn unit(&self, method: &str, arg: &str) {
        self.rt
            .block_on(async {
                use omarchy_broadcast_audiod::dbus::{BUS_NAME, INTERFACE, OBJECT_PATH};
                self.conn
                    .call_method(
                        Some(BUS_NAME),
                        OBJECT_PATH,
                        Some(INTERFACE),
                        method,
                        &(arg,),
                    )
                    .await
            })
            .unwrap_or_else(|e| panic!("{method}: {e}"));
    }

    fn wait_worker(&self) {
        let end = Instant::now() + Duration::from_secs(10);
        while self.call::<String>("GetStatus", None).is_err() {
            assert!(Instant::now() < end, "the worker never answered");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_status(&self, what: &str, cond: impl Fn(&Value) -> bool) -> Value {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let s = self.json("GetStatus", None);
            if cond(&s) {
                return s;
            }
            assert!(Instant::now() < end, "timed out waiting for {what}: {s}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn inner() {
    let sandbox = assert_isolated();
    let secs = Duration::from_secs;
    std::fs::create_dir_all(sandbox.join("omarchy-broadcast")).unwrap();
    std::fs::create_dir_all(sandbox.join("no-modules")).unwrap();
    // No modules; a pipeline depth of 3 so the delay differs from the
    // plugin's own 480.
    let snapshot = sandbox.join("audio-settings.json");
    std::fs::write(
        &snapshot,
        r#"{"version":1,"idle_exit_s":0,"pipeline_depth":3}"#,
    )
    .unwrap();

    let mut host = mic_host(&sandbox);
    wait_dump("the mic host", secs(10), |d| {
        id_of(d, "omarchy-broadcast-mic.capture").is_some()
            && id_of(d, "omarchy-broadcast-mic").is_some()
    });
    let mut w = worker(&sandbox, &snapshot);
    let bus = Bus::new();
    bus.wait_worker();

    // The devices: both virtual mics, never ours.
    let devices = bus.json("GetDevices", None);
    let names: Vec<&str> = devices["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"obc-test-mic-a") && names.contains(&"obc-test-mic-b"),
        "{devices}"
    );
    assert!(
        !names.iter().any(|n| n.starts_with("omarchy-broadcast-mic")),
        "{devices}"
    );

    // Use as default: ours, pinned to the previous default (mic A).
    let d = pw_dump();
    assert_eq!(configured_source(&d).as_deref(), Some("obc-test-mic-a"));
    let r = bus.json("MakeDefault", Some(""));
    assert_eq!(r["previous"], "obc-test-mic-a", "{r}");
    let d = wait_dump("ours the default, pinned to mic A", secs(5), |d| {
        let capture = id_of(d, "omarchy-broadcast-mic.capture").unwrap();
        configured_source(d).as_deref() == Some("omarchy-broadcast-mic")
            && meta_get(d, capture, "target.object").map(|v| text(&v))
                == serial_of(d, "obc-test-mic-a")
            && linked(d, "obc-test-mic-a", "omarchy-broadcast-mic.capture")
    });
    assert!(!linked(
        &d,
        "omarchy-broadcast-mic",
        "omarchy-broadcast-mic.capture"
    ));

    // Again, to mic B: the previous default is still mic A (never ours).
    let r = bus.json("MakeDefault", Some("obc-test-mic-b"));
    assert_eq!(r["previous"], "obc-test-mic-a", "{r}");
    wait_dump("pinned to mic B", secs(5), |d| {
        linked(d, "obc-test-mic-b", "omarchy-broadcast-mic.capture")
            && !linked(d, "obc-test-mic-a", "omarchy-broadcast-mic.capture")
    });
    let e = bus
        .call::<String>("MakeDefault", Some("omarchy-broadcast-mic"))
        .unwrap_err();
    assert!(e.contains("InvalidArgs"), "{e}");

    // The mic host restarts: the new capture node is pinned to mic B again
    // (WirePlumber's restore-stream), with no help from the worker.
    host.stop();
    wait_dump("the mic host gone", secs(5), |d| {
        id_of(d, "omarchy-broadcast-mic.capture").is_none()
    });
    w.kill9();
    let mut host = mic_host(&sandbox);
    wait_dump("the pin after a mic host restart", secs(10), |d| {
        let Some(capture) = id_of(d, "omarchy-broadcast-mic.capture") else {
            return false;
        };
        meta_get(d, capture, "target.object").map(|v| text(&v)) == serial_of(d, "obc-test-mic-b")
            && linked(d, "obc-test-mic-b", "omarchy-broadcast-mic.capture")
            && configured_source(d).as_deref() == Some("omarchy-broadcast-mic")
    });
    // And the worker restarts: nothing changes.
    let mut w = worker(&sandbox, &snapshot);
    bus.wait_worker();
    std::thread::sleep(secs(1));
    let d = pw_dump();
    assert!(linked(
        &d,
        "obc-test-mic-b",
        "omarchy-broadcast-mic.capture"
    ));
    assert_eq!(
        configured_source(&d).as_deref(),
        Some("omarchy-broadcast-mic")
    );
    // Idempotent across a worker restart: the previous default is the pin.
    let r = bus.json("MakeDefault", Some("obc-test-mic-b"));
    assert_eq!(r["previous"], "obc-test-mic-b", "{r}");

    // Consumers: a level meter alone is not one; a recorder is.
    let s = bus.wait_status("no consumer", |s| s["consumers_known"] == true);
    assert_eq!(s["consumers"], 0, "{s}");
    let _meter = record(&sandbox, "obc-meter", "stream.monitor = true");
    wait_dump("the meter linked", secs(5), |d| {
        linked(d, "omarchy-broadcast-mic", "obc-meter")
    });
    std::thread::sleep(Duration::from_millis(600));
    let s = bus.json("GetStatus", None);
    assert_eq!(s["consumers"], 0, "a level meter is not a consumer: {s}");
    let mut rec = record(&sandbox, "obc-rec", "");
    wait_dump("the recorder linked", secs(5), |d| {
        linked(d, "omarchy-broadcast-mic", "obc-rec")
    });
    let s = bus.wait_status("one consumer", |s| s["consumers"] == 1);
    assert_eq!(s["consumers_known"], true);

    // ProcessLatency: the transport delay the plugin publishes at depth 3
    // (no effect), for the graph's quantum (a power of two: PipeWire
    // rounds the mic host's 480 down), never the 3 x 480 = 1440 of a
    // 480-sample quantum.
    bus.wait_status("the plugin connected", |s| s["connected"] == true);
    let delays: Vec<u64> = (5..=11)
        .map(|k| u64::from(obc_audio::transport_delay(1 << k, 3)))
        .collect();
    assert!(!delays.contains(&1440));
    let d = wait_dump("ProcessLatency of the transport delay", secs(10), |d| {
        process_latency(d).is_some_and(|l| delays.contains(&l))
    });
    let want = process_latency(&d).unwrap();
    let s = bus.wait_status("the status latency", |s| {
        s["latency_ms"]
            .as_f64()
            .is_some_and(|ms| (ms - want as f64 / 48.0).abs() < 0.01)
    });
    assert_eq!(s["latency_samples"], 0, "{s}");

    rec.stop();
    bus.wait_status("the recorder gone", |s| s["consumers"] == 0);

    // Restore: mic A the default again, the capture side unpinned.
    bus.unit("RestoreDefault", "obc-test-mic-a");
    wait_dump("mic A the default, unpinned", secs(5), |d| {
        let capture = id_of(d, "omarchy-broadcast-mic.capture").unwrap();
        configured_source(d).as_deref() == Some("obc-test-mic-a")
            && meta_get(d, capture, "target.object").is_none()
            && linked(d, "obc-test-mic-a", "omarchy-broadcast-mic.capture")
    });
    // Restoring again changes nothing.
    bus.unit("RestoreDefault", "obc-test-mic-b");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        configured_source(&pw_dump()).as_deref(),
        Some("obc-test-mic-a")
    );

    // The unpin is remembered too: after a mic host restart the capture
    // side follows the default (mic A), not mic B.
    host.stop();
    wait_dump("the mic host gone", secs(5), |d| {
        id_of(d, "omarchy-broadcast-mic.capture").is_none()
    });
    let _host = mic_host(&sandbox);
    wait_dump("the capture side following the default", secs(10), |d| {
        let Some(capture) = id_of(d, "omarchy-broadcast-mic.capture") else {
            return false;
        };
        meta_get(d, capture, "target.object").is_none()
            && linked(d, "obc-test-mic-a", "omarchy-broadcast-mic.capture")
    });
    w.stop();
}
