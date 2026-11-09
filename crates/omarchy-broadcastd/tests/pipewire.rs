//! The daemon's PipeWire camera node end to end (cargo feature `pipewire`):
//! the `omarchy-broadcastd` binary on a private D-Bus daemon, with `[pipewire]
//! enabled = true`, the loopback named by `OBC_TEST_SINK`, the camera of
//! `OBC_TEST_CAMERA`, the user's own PipeWire daemon, and an in-process
//! `pipewiresrc` consumer of the node (gst-plugin-pipewire).
//!
//! Run with
//! `OBC_TEST_SINK=/dev/video11 OBC_TEST_CAMERA=046d:0944
//! cargo test -p omarchy-broadcastd --features pipewire --test pipewire -- --ignored --nocapture`.
//!
//! The consumer targets the node by name (`omarchy-broadcast`) with
//! `node.dont-fallback` and `node.dont-reconnect`, so it can never fall
//! back to a real camera; a consumer whose link does not show up in
//! `PipeWireConsumers` within 2 s is stopped at once and the test fails.
//! Before the daemon starts, the test checks that no other PipeWire node
//! carries that name (an installed daemon with the node enabled), with
//! `pw-dump`, read only. It changes no PipeWire or WirePlumber setting.
//!
//! The daemon runs with `OBC_TEST_STAMP_FORWARDED=1`, as in the `hardware`
//! test: the frames that came through the camera session carry the
//! `obc_pipeline::stamp` mark, so the consumer tells them from the writer's
//! black placeholders whatever the picture.

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{DaemonEnv, Guard, PrivateBus};
use futures_util::StreamExt;
use gst::prelude::*;
use obc_core::CameraSelector;
use obc_ipc::{BUS_NAME, OBJECT_PATH, OmarchyBroadcastProxy, State};
use omarchy_broadcastd::consumers::{NodeId, count_holders};
use zbus::fdo;
use zbus::proxy::CacheProperties;

/// The daemon's node name (`PwOutputConfig::DEFAULT_NODE_NAME`).
const NODE_NAME: &str = "omarchy-broadcast";
/// `[lifecycle] idle_grace_s` of the test config.
const GRACE: Duration = Duration::from_secs(1);

fn required(var: &str, what: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| {
        panic!(
            "this test needs {var}: {what}. It also needs the user PipeWire daemon (with \
             WirePlumber), gst-plugin-pipewire, an NVIDIA GPU and /usr/bin/dbus-daemon"
        )
    })
}

/// The node `OBC_TEST_CAMERA` resolves to, and the selector.
fn resolve_camera() -> (String, PathBuf) {
    let selector = required(
        "OBC_TEST_CAMERA",
        "a colour camera selector (vid:pid, name or /dev/videoN)",
    );
    let sel: CameraSelector = selector.parse().expect("a valid camera selector");
    let devices = obc_pipeline::scan_sysfs();
    let picked =
        obc_pipeline::select_camera(&devices, Some(&sel), &obc_core::ModeRequest::default())
            .unwrap_or_else(|e| panic!("OBC_TEST_CAMERA={selector}: {e}"));
    (selector, picked.device.path.clone())
}

fn wait(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if done() {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// How many PipeWire objects carry `node.name` = [`NODE_NAME`], per
/// `pw-dump` (read only).
fn pw_nodes_named() -> usize {
    let out = std::process::Command::new("pw-dump")
        .output()
        .unwrap_or_else(|e| panic!("this test needs pw-dump (package pipewire): {e}"));
    assert!(out.status.success(), "pw-dump failed: {out:?}");
    String::from_utf8_lossy(&out.stdout)
        .matches(&format!("\"node.name\": \"{NODE_NAME}\""))
        .count()
}

/// The device nodes process `pid` holds open.
fn holds(pid: u32, node: &Path) -> bool {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|fd| std::fs::read_link(fd.path()).ok())
        .any(|t| t == node)
}

/// A synchronous client of the daemon on the private bus, recording every
/// `State` the daemon announces (`PropertiesChanged`, in order).
struct Client {
    rt: tokio::runtime::Runtime,
    conn: zbus::Connection,
    proxy: OmarchyBroadcastProxy<'static>,
    states: Arc<Mutex<Vec<(Instant, String)>>>,
}

impl Client {
    fn connect(env: &DaemonEnv) -> Client {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let conn = rt.block_on(env.connect());
        let proxy = rt.block_on(async {
            OmarchyBroadcastProxy::builder(&conn)
                .cache_properties(CacheProperties::No)
                .build()
                .await
                .unwrap()
        });
        let props = rt.block_on(async {
            fdo::PropertiesProxy::builder(&conn)
                .destination(BUS_NAME)
                .path(OBJECT_PATH)
                .build()
                .await
                .unwrap()
        });
        let mut stream = rt.block_on(props.receive_properties_changed()).unwrap();
        let states: Arc<Mutex<Vec<(Instant, String)>>> = Arc::default();
        let s = Arc::clone(&states);
        rt.spawn(async move {
            // Keep the proxy (and its match rule) alive with the stream.
            let _props = props;
            while let Some(signal) = stream.next().await {
                let Ok(args) = signal.args() else { continue };
                if let Some(v) = args.changed_properties().get("State")
                    && let Ok(state) = String::try_from(v)
                {
                    s.lock().unwrap().push((Instant::now(), state));
                }
            }
        });
        Client {
            rt,
            conn,
            proxy,
            states,
        }
    }

    fn has_name(&self) -> bool {
        let dbus = self.rt.block_on(fdo::DBusProxy::new(&self.conn)).unwrap();
        self.rt
            .block_on(dbus.name_has_owner(BUS_NAME.try_into().unwrap()))
            .unwrap_or(false)
    }

    fn state(&self) -> State {
        self.rt.block_on(self.proxy.state()).unwrap()
    }
    fn error(&self) -> String {
        self.rt.block_on(self.proxy.error()).unwrap()
    }
    fn consumers(&self) -> u32 {
        self.rt.block_on(self.proxy.consumers()).unwrap()
    }
    fn loopback_consumers(&self) -> u32 {
        self.rt.block_on(self.proxy.loopback_consumers()).unwrap()
    }
    fn pipewire_consumers(&self) -> u32 {
        self.rt.block_on(self.proxy.pipewire_consumers()).unwrap()
    }
    fn pipewire_output(&self) -> String {
        self.rt.block_on(self.proxy.pipewire_output()).unwrap()
    }
    fn output_mode(&self) -> String {
        self.rt.block_on(self.proxy.output_mode()).unwrap()
    }
    fn cameras(&self) -> usize {
        self.rt.block_on(self.proxy.cameras()).unwrap().len()
    }

    /// The `State` values announced since `since`, in order.
    fn states_since(&self, since: Instant) -> Vec<String> {
        self.states
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| *t >= since)
            .map(|(_, s)| s.clone())
            .collect()
    }
}

/// An in-process `pipewiresrc` consumer of the daemon's node, NV12 only;
/// set to NULL (its link goes) when dropped.
struct Consumer {
    pipeline: gst::Pipeline,
    sink: gst_app::AppSink,
}

impl Drop for Consumer {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl Consumer {
    fn start() -> Consumer {
        let src = gst::ElementFactory::make("pipewiresrc")
            .property("target-object", NODE_NAME)
            .property(
                "stream-properties",
                gst::Structure::builder("props")
                    .field("node.dont-fallback", "true")
                    .field("node.dont-reconnect", "true")
                    .build(),
            )
            .build()
            .expect("pipewiresrc (gst-plugin-pipewire) is needed");
        let filter = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                "video/x-raw,format=NV12".parse::<gst::Caps>().unwrap(),
            )
            .build()
            .unwrap();
        let sink = gst_app::AppSink::builder()
            .sync(false)
            .max_buffers(2)
            .leaky_type(gst_app::AppLeakyType::Downstream)
            .build();
        let pipeline = gst::Pipeline::new();
        pipeline
            .add_many([&src, &filter, sink.upcast_ref()])
            .unwrap();
        gst::Element::link_many([&src, &filter, sink.upcast_ref()]).unwrap();
        let c = Consumer { pipeline, sink };
        c.pipeline
            .set_state(gst::State::Playing)
            .expect("the pipewiresrc consumer does not start");
        c
    }

    /// The first ERROR on the consumer's bus, if any so far.
    fn error(&self) -> Option<String> {
        let bus = self.pipeline.bus().unwrap();
        while let Some(msg) = bus.pop() {
            if let gst::MessageView::Error(e) = msg.view() {
                return Some(format!("{} ({:?})", e.error(), e.debug()));
            }
        }
        None
    }

    fn pull(&self, timeout: Duration) -> Option<gst::Sample> {
        self.sink
            .try_pull_sample(gst::ClockTime::from_nseconds(timeout.as_nanos() as u64))
    }
}

/// Whether the sample's frame carries the `obc_pipeline::stamp` mark (came
/// through the camera session).
fn stamped(sample: &gst::Sample) -> bool {
    let map = sample.buffer().unwrap().map_readable().unwrap();
    obc_pipeline::stamp::read_stamp(&map).is_some()
}

/// A PipeWire consumer drives the camera like a loopback one (see the
/// module docs).
#[test]
#[ignore = "needs the user PipeWire daemon (with WirePlumber), gst-plugin-pipewire (pipewiresrc), pw-dump, OBC_TEST_SINK (a free v4l2loopback device with exclusive_caps=1), OBC_TEST_CAMERA (a colour camera), an NVIDIA GPU and /usr/bin/dbus-daemon"]
fn a_pipewire_consumer_starts_and_stops_the_camera() {
    let sink = PathBuf::from(required(
        "OBC_TEST_SINK",
        "a free v4l2loopback device created with exclusive_caps=1 (never found by label)",
    ));
    let sink = std::fs::canonicalize(&sink).unwrap_or(sink);
    let (camera, cam_node) = resolve_camera();
    gst::init().unwrap();
    assert!(
        gst::ElementFactory::find("pipewiresrc").is_some(),
        "this test needs pipewiresrc (gst-plugin-pipewire)"
    );
    // Never disturb another process holding the devices.
    for node in [&sink, &cam_node] {
        let n = count_holders(Path::new("/proc"), &NodeId::of(node), std::process::id());
        assert_eq!(
            n,
            0,
            "{} is held by another process (see `fuser -v {}`): not touching it",
            node.display(),
            node.display()
        );
    }
    // The consumer must find the daemon's node and no other.
    assert_eq!(
        pw_nodes_named(),
        0,
        "a PipeWire node named {NODE_NAME:?} already exists (an installed daemon with \
         [pipewire] enabled?): not starting a second one"
    );

    let dir = tempfile::tempdir().unwrap();
    let bus = PrivateBus::start(dir.path());
    let env = DaemonEnv::new(dir.path(), &bus);
    std::fs::write(
        env.config_file(),
        format!(
            "version = 1\n[camera]\npreference = [{camera:?}]\n\
             [loopback]\ndevice = {:?}\n\
             [lifecycle]\nidle_grace_s = {}\nstart_debounce_ms = 300\n\
             [pipewire]\nenabled = true\n",
            sink.display().to_string(),
            GRACE.as_secs_f64()
        ),
    )
    .unwrap();

    // ---- idle: the node is ready, nobody linked ----
    let child = env
        .command()
        .arg("-v")
        .env("OBC_TEST_STAMP_FORWARDED", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn omarchy-broadcastd");
    let mut daemon = Guard::new(child, "omarchy-broadcastd", Duration::from_secs(10));
    let pid = daemon.pid();
    let client = Client::connect(&env);
    assert!(
        wait(Duration::from_secs(15), || client.has_name()),
        "the daemon did not take {BUS_NAME}"
    );
    assert!(
        wait(Duration::from_secs(15), || client.cameras() > 0),
        "the daemon listed no camera"
    );
    assert_eq!(client.state(), State::Idle, "error: {}", client.error());
    assert_eq!(client.output_mode(), "NV12 1920x1080@30");
    assert!(
        wait(Duration::from_secs(10), || client.pipewire_output()
            == "ready"),
        "PipeWireOutput is {:?}, not ready",
        client.pipewire_output()
    );
    assert_eq!(client.pipewire_consumers(), 0);
    assert_eq!(client.consumers(), 0);
    assert!(
        pw_nodes_named() >= 1,
        "PipeWireOutput is ready but pw-dump lists no node named {NODE_NAME:?}"
    );
    assert!(!holds(pid, &cam_node), "the camera is open while idle");

    // ---- a PipeWire consumer: starting, then active ----
    let linked = Instant::now();
    let consumer = Consumer::start();
    if !wait(Duration::from_secs(2), || client.pipewire_consumers() == 1) {
        let err = consumer.error();
        drop(consumer);
        panic!(
            "the consumer did not link to the daemon's node within 2 s (PipeWireConsumers {}, \
             error {err:?})",
            client.pipewire_consumers()
        );
    }
    eprintln!(
        "PipeWireConsumers 1 {:?} after the consumer started",
        linked.elapsed()
    );
    assert!(
        wait(Duration::from_secs(20), || client.state() == State::Active),
        "not active within 20 s: {} ({})",
        client.state(),
        client.error()
    );
    eprintln!("ACTIVE {:?} after the consumer started", linked.elapsed());
    // The signal may still be on its way to the recorder.
    wait(Duration::from_secs(2), || {
        client.states_since(linked).iter().any(|s| s == "active")
    });
    let states = client.states_since(linked);
    assert_eq!(
        states.iter().map(String::as_str).collect::<Vec<_>>(),
        ["starting", "active"],
        "State announced since the consumer started"
    );
    assert_eq!(client.consumers(), 1);
    assert_eq!(client.loopback_consumers(), 0);
    assert_eq!(client.pipewire_output(), "streaming");
    assert!(holds(pid, &cam_node), "streams from {}", cam_node.display());

    // Frames through the camera session arrive, NV12 at the contract size.
    let mut first = None;
    let mut camera_frames = 0;
    let deadline = Instant::now() + Duration::from_secs(10);
    while camera_frames < 30 && Instant::now() < deadline {
        let Some(s) = consumer.pull(Duration::from_millis(500)) else {
            continue;
        };
        if first.is_none() {
            let info = gst_video::VideoInfo::from_caps(s.caps().unwrap()).unwrap();
            assert_eq!(
                (info.format(), info.width(), info.height()),
                (gst_video::VideoFormat::Nv12, 1920, 1080)
            );
            first = Some(s.caps().unwrap().to_string());
        }
        if stamped(&s) {
            camera_frames += 1;
        }
    }
    assert!(
        camera_frames >= 30,
        "{camera_frames} camera frames at the PipeWire consumer within 10 s (error {:?})",
        consumer.error()
    );
    assert_eq!(consumer.error(), None);
    eprintln!("{camera_frames} camera frames at the consumer, caps {first:?}");

    // ---- the consumer leaves: idle after the grace ----
    drop(consumer);
    let left = Instant::now();
    assert!(
        wait(Duration::from_secs(3), || client.pipewire_consumers() == 0),
        "PipeWireConsumers still {} after the consumer stopped",
        client.pipewire_consumers()
    );
    let unlinked = left.elapsed();
    assert!(
        wait(Duration::from_secs(15), || client.state() == State::Idle),
        "not idle after the consumer left: {} ({})",
        client.state(),
        client.error()
    );
    let to_idle = left.elapsed();
    eprintln!(
        "link gone {unlinked:?}, IDLE {to_idle:?} after the consumer stopped (grace {GRACE:?})"
    );
    assert!(to_idle >= GRACE - Duration::from_millis(50), "{to_idle:?}");
    assert_eq!(client.pipewire_consumers(), 0);
    assert_eq!(client.consumers(), 0);
    assert!(
        wait(Duration::from_secs(3), || client.pipewire_output()
            == "ready"),
        "PipeWireOutput {:?} after the consumer left",
        client.pipewire_output()
    );
    assert!(
        wait(Duration::from_secs(3), || !holds(pid, &cam_node)),
        "the camera is still open while idle"
    );

    // ---- SIGTERM: the node goes with the daemon ----
    assert_eq!(daemon.terminate(), Some(0), "the daemon exits 0 on SIGTERM");
    assert!(
        wait(Duration::from_secs(3), || pw_nodes_named() == 0),
        "the node outlived the daemon"
    );
}
