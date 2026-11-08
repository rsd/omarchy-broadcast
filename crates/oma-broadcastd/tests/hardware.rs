//! The daemon end to end on real devices: the `oma-broadcastd` binary on a
//! private D-Bus daemon, the loopback named by `OBC_TEST_SINK`, the two
//! cameras of `OBC_TEST_CAMERA` and `OBC_TEST_CAMERA_2`, and a consumer in
//! another process (`obc-consumer` of `obc-pipeline`).
//!
//! Run with
//! `OBC_TEST_SINK=/dev/video11 OBC_TEST_CAMERA=046d:0944 OBC_TEST_CAMERA_2=5986:1193
//! cargo test -p oma-broadcastd --test hardware -- --ignored --test-threads=1 --nocapture`.
//!
//! The consumer binary is built by the test (`cargo build -p obc-pipeline
//! --bin obc-consumer`, a no-op when it is current) and taken from the same
//! target directory as this test; `OBC_CONSUMER=<path>` overrides it.
//!
//! The daemon runs with `OBC_TEST_STAMP_FORWARDED=1`: every frame its
//! output forwards to the loopback writer carries the `obc_pipeline::stamp`
//! mark, so the consumer tells frames that came through the camera session
//! from the writer's own black placeholders whatever the picture (the
//! cameras may well deliver black frames, e.g. with a privacy shutter).
//! One camera control is changed, in the last part only: the streaming
//! camera's `exposure_dynamic_framerate` is set to 1 so the daemon (with
//! `camera.fixed_framerate = true`) has something to pin and restore on
//! SIGTERM; the test puts the original value back, also when it fails.
//! A camera without that control writable skips that check, saying so.

mod common;

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{DaemonEnv, Guard, PrivateBus};
use futures_util::StreamExt;
use obc_core::CameraSelector;
use obc_ipc::{BUS_NAME, IpcError, OmaBroadcastProxy, State, Stats};
use oma_broadcastd::consumers::{NodeId, count_holders};
use zbus::proxy::CacheProperties;

const IGNORE: &str = "needs OBC_TEST_SINK (a free v4l2loopback device with exclusive_caps=1), \
     OBC_TEST_CAMERA and OBC_TEST_CAMERA_2 (two colour cameras), an NVIDIA GPU with nvidia-smi, \
     /usr/bin/dbus-daemon and cargo (to build obc-consumer)";

fn required(var: &str, what: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| panic!("this test needs {var}: {what}. {IGNORE}"))
}

/// A test camera: the node it resolves to and its USB id.
struct TestCamera {
    selector: String,
    node: PathBuf,
    usb: String,
}

fn resolve_camera(var: &str) -> TestCamera {
    let selector = required(
        var,
        "a colour camera selector (vid:pid, name or /dev/videoN)",
    );
    let sel: CameraSelector = selector.parse().expect("a valid camera selector");
    let devices = obc_pipeline::scan_sysfs();
    let picked =
        obc_pipeline::select_camera(&devices, Some(&sel), &obc_core::ModeRequest::default())
            .unwrap_or_else(|e| panic!("{var}={selector}: {e}"));
    TestCamera {
        selector,
        node: picked.device.path.clone(),
        usb: picked.device.usb.map(|u| u.to_string()).unwrap_or_default(),
    }
}

/// `obc-consumer`, built if needed.
fn consumer_binary() -> PathBuf {
    if let Some(p) = std::env::var_os("OBC_CONSUMER") {
        return PathBuf::from(p);
    }
    let exe = std::env::current_exe().unwrap();
    let profile_dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("test binary under target/<profile>/deps")
        .to_path_buf();
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
    let mut build = Command::new(env!("CARGO"));
    build
        .args(["build", "-q", "-p", "obc-pipeline", "--bin", "obc-consumer"])
        .arg("--manifest-path")
        .arg(&manifest);
    if profile_dir.file_name().is_some_and(|n| n == "release") {
        build.arg("--release");
    }
    let st = build.status().expect("run cargo to build obc-consumer");
    assert!(st.success(), "building obc-consumer failed: {st}");
    let bin = profile_dir.join("obc-consumer");
    assert!(bin.exists(), "{} missing after the build", bin.display());
    bin
}

/// One line of `obc-consumer`.
#[derive(Debug, Clone, PartialEq)]
enum Line {
    Caps(String),
    /// Whether the frame carries a stamp.
    Frame(bool),
    Problem(String),
}

/// A loopback consumer in its own process; killed when dropped.
struct Consumer {
    guard: Guard,
    lines: Arc<Mutex<Vec<(Instant, Line)>>>,
}

impl Consumer {
    fn spawn(bin: &Path, sink: &Path) -> Consumer {
        let mut child = Command::new(bin)
            .arg(sink)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn obc-consumer");
        let out = child.stdout.take().unwrap();
        let lines: Arc<Mutex<Vec<(Instant, Line)>>> = Arc::default();
        let l = Arc::clone(&lines);
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                let (word, rest) = line.split_once(' ').unwrap_or((&line, ""));
                let parsed = match word {
                    "caps" => Line::Caps(rest.to_string()),
                    "frame" => Line::Frame(rest.rsplit(' ').next().is_some_and(|s| s != "-")),
                    _ => Line::Problem(line.clone()),
                };
                l.lock().unwrap().push((Instant::now(), parsed));
            }
        });
        Consumer {
            guard: Guard::new(child, "obc-consumer", Duration::from_secs(2)),
            lines,
        }
    }

    fn lines(&self) -> Vec<(Instant, Line)> {
        self.lines.lock().unwrap().clone()
    }

    /// (stamped, unstamped) frames so far.
    fn counts(&self) -> (usize, usize) {
        let l = self.lines();
        let stamped = l.iter().filter(|(_, x)| *x == Line::Frame(true)).count();
        let plain = l.iter().filter(|(_, x)| *x == Line::Frame(false)).count();
        (stamped, plain)
    }

    fn caps(&self) -> Vec<String> {
        self.lines()
            .into_iter()
            .filter_map(|(_, l)| match l {
                Line::Caps(c) => Some(c),
                _ => None,
            })
            .collect()
    }

    fn problems(&self) -> Vec<String> {
        self.lines()
            .into_iter()
            .filter_map(|(_, l)| match l {
                Line::Problem(p) => Some(p),
                _ => None,
            })
            .collect()
    }

    /// Runs of unstamped (`false`) and stamped (`true`) frames.
    fn runs(&self) -> Vec<(bool, usize)> {
        let mut out: Vec<(bool, usize)> = Vec::new();
        for (_, l) in self.lines() {
            let Line::Frame(s) = l else { continue };
            match out.last_mut() {
                Some((last, n)) if *last == s => *n += 1,
                _ => out.push((s, 1)),
            }
        }
        out
    }

    /// Waits until `more` more frames of the kind (`stamped`) arrived.
    fn wait_more(&self, stamped: bool, more: usize, limit: Duration) -> bool {
        let pick = |c: (usize, usize)| if stamped { c.0 } else { c.1 };
        let start = pick(self.counts());
        wait(limit, || pick(self.counts()) >= start + more)
    }

    fn close(mut self) {
        // Closing stdin ends it cleanly (its device fd closes on exit).
        drop(self.guard.child.as_mut().and_then(|c| c.stdin.take()));
        if self.guard.wait_exit(Duration::from_secs(3)).is_none() {
            self.guard.terminate();
        }
    }
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

/// The device nodes process `pid` holds open.
fn open_nodes(pid: u32) -> Vec<PathBuf> {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|fd| std::fs::read_link(fd.path()).ok())
        .filter(|t| t.starts_with("/dev/"))
        .collect()
}

fn holds(pid: u32, node: &Path) -> bool {
    open_nodes(pid).iter().any(|n| n == node)
}

fn holds_nvidia(pid: u32) -> bool {
    open_nodes(pid)
        .iter()
        .any(|n| n.to_string_lossy().starts_with("/dev/nvidia"))
}

/// The pids nvidia-smi lists with a compute context.
fn cuda_pids() -> Vec<u32> {
    let out = Command::new("nvidia-smi")
        .args(["--query-compute-apps=pid", "--format=csv,noheader"])
        .output()
        .unwrap_or_else(|e| panic!("this test needs nvidia-smi ({e}). {IGNORE}"));
    assert!(out.status.success(), "nvidia-smi failed: {out:?}");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// A synchronous client of the daemon on the private bus.
struct Client {
    rt: tokio::runtime::Runtime,
    conn: zbus::Connection,
    proxy: OmaBroadcastProxy<'static>,
    stats: Arc<Mutex<Vec<(Instant, Stats)>>>,
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
            OmaBroadcastProxy::builder(&conn)
                .cache_properties(CacheProperties::No)
                .build()
                .await
                .unwrap()
        });
        let stats: Arc<Mutex<Vec<(Instant, Stats)>>> = Arc::default();
        let s = Arc::clone(&stats);
        let p = proxy.clone();
        rt.spawn(async move {
            let mut stream = p.receive_stats().await.unwrap();
            while let Some(sig) = stream.next().await {
                if let Ok(a) = sig.args() {
                    s.lock().unwrap().push((Instant::now(), a.to_stats()));
                }
            }
        });
        Client {
            rt,
            conn,
            proxy,
            stats,
        }
    }

    fn has_name(&self) -> bool {
        let dbus = self
            .rt
            .block_on(zbus::fdo::DBusProxy::new(&self.conn))
            .unwrap();
        self.rt
            .block_on(dbus.name_has_owner(BUS_NAME.try_into().unwrap()))
            .unwrap_or(false)
    }

    /// Waits until the daemon owns its name; how long that took.
    fn wait_for_name(&self, limit: Duration) -> Duration {
        let t = Instant::now();
        let ok = wait(limit, || self.has_name());
        assert!(ok, "the daemon did not take {BUS_NAME} within {limit:?}");
        t.elapsed()
    }

    fn wait_for_daemon(&self, limit: Duration) {
        self.wait_for_name(limit);
        // The controller has run once the cameras are listed.
        assert!(
            wait(limit, || !self.cameras().is_empty()),
            "the daemon listed no camera"
        );
    }

    fn state(&self) -> State {
        self.rt.block_on(self.proxy.state()).unwrap()
    }
    fn error(&self) -> String {
        self.rt.block_on(self.proxy.error()).unwrap()
    }
    fn enabled(&self) -> bool {
        self.rt.block_on(self.proxy.enabled()).unwrap()
    }
    fn consumers(&self) -> u32 {
        self.rt.block_on(self.proxy.consumers()).unwrap()
    }
    fn cameras(&self) -> Vec<obc_ipc::CameraInfo> {
        self.rt.block_on(self.proxy.cameras()).unwrap()
    }
    fn active(&self) -> String {
        self.rt.block_on(self.proxy.active_camera()).unwrap()
    }
    fn output_mode(&self) -> String {
        self.rt.block_on(self.proxy.output_mode()).unwrap()
    }
    fn set_enabled(&self, on: bool) -> Result<(), IpcError> {
        self.rt.block_on(self.proxy.set_enabled(on))
    }
    fn select(&self, s: &str) -> Result<(), IpcError> {
        self.rt.block_on(self.proxy.select_camera(s))
    }
    fn next(&self) -> Result<(), IpcError> {
        self.rt.block_on(self.proxy.next_camera())
    }
    fn reload(&self) -> Result<(), IpcError> {
        self.rt.block_on(self.proxy.reload_config())
    }

    fn wait_state(&self, want: State, limit: Duration) -> Option<Duration> {
        let t = Instant::now();
        wait(limit, || self.state() == want).then(|| t.elapsed())
    }
}

fn spawn_daemon(env: &DaemonEnv) -> Guard {
    let child = env
        .command()
        .arg("-v")
        .env("OBC_TEST_STAMP_FORWARDED", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn oma-broadcastd");
    Guard::new(child, "oma-broadcastd", Duration::from_secs(10))
}

fn config(sink: &Path, first: &str, second: &str, extra: &str) -> String {
    config_with(sink, first, second, "", extra)
}

/// [`config`] with `camera` lines added to the `[camera]` table.
fn config_with(sink: &Path, first: &str, second: &str, camera: &str, extra: &str) -> String {
    format!(
        "version = 1\n[camera]\npreference = [{first:?}, {second:?}]\n{camera}\
         [loopback]\ndevice = {:?}\n\
         [lifecycle]\nidle_grace_s = 1.0\nstart_debounce_ms = 300\n{extra}",
        sink.display().to_string()
    )
}

/// CPU time process `pid` has used (user + system), in clock ticks.
fn cpu_ticks(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    // The fields after the command name (which may hold spaces).
    let rest = &stat[stat.rfind(')').unwrap() + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    // utime and stime are fields 14 and 15 of the line, 12 and 13 here.
    f[11].parse::<u64>().unwrap() + f[12].parse::<u64>().unwrap()
}

/// The CPU process `pid` uses over `span`, in percent of one core.
fn cpu_percent(pid: u32, span: Duration) -> f64 {
    // SAFETY: sysconf has no preconditions.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    let a = cpu_ticks(pid);
    let t = Instant::now();
    std::thread::sleep(span);
    let b = cpu_ticks(pid);
    (b - a) as f64 / hz / t.elapsed().as_secs_f64() * 100.0
}

/// Puts a camera control back when dropped (also when the test fails).
struct RestoreControl {
    path: PathBuf,
    id: u32,
    value: i32,
}

impl Drop for RestoreControl {
    fn drop(&mut self) {
        if let Err(e) = obc_pipeline::controls::set_ctrl(&self.path, self.id, self.value) {
            eprintln!(
                "could not restore control {:#x} of {} to {}: {e}",
                self.id,
                self.path.display(),
                self.value
            );
        }
    }
}

/// The whole lifecycle on real devices (see the module docs).
#[test]
#[ignore = "needs OBC_TEST_SINK (a free v4l2loopback device with exclusive_caps=1), OBC_TEST_CAMERA and OBC_TEST_CAMERA_2 (two colour cameras), an NVIDIA GPU with nvidia-smi, /usr/bin/dbus-daemon and cargo"]
fn the_daemon_runs_the_lifecycle_on_real_devices() {
    let sink = PathBuf::from(required(
        "OBC_TEST_SINK",
        "a free v4l2loopback device created with exclusive_caps=1 (never found by label)",
    ));
    let sink = std::fs::canonicalize(&sink).unwrap_or(sink);
    let cam1 = resolve_camera("OBC_TEST_CAMERA");
    let cam2 = resolve_camera("OBC_TEST_CAMERA_2");
    assert_ne!(cam1.node, cam2.node, "two different cameras are needed");
    let _ = cuda_pids(); // nvidia-smi present
    let consumer_bin = consumer_binary();
    // Never disturb another process holding the devices.
    for node in [&sink, &cam1.node, &cam2.node] {
        let n = count_holders(Path::new("/proc"), &NodeId::of(node), std::process::id());
        assert_eq!(
            n,
            0,
            "{} is held by another process (see `fuser -v {}`): not touching it",
            node.display(),
            node.display()
        );
    }

    let dir = tempfile::tempdir().unwrap();
    let bus = PrivateBus::start(dir.path());
    let env = DaemonEnv::new(dir.path(), &bus);
    std::fs::write(
        env.config_file(),
        config(&sink, &cam1.selector, &cam2.selector, ""),
    )
    .unwrap();

    // ---- idle ----
    let mut daemon = spawn_daemon(&env);
    let pid = daemon.pid();
    let client = Client::connect(&env);
    let named = client.wait_for_name(Duration::from_secs(15));
    // The name comes after the first writer start: the moment it is owned,
    // the loopback is already a capture device (WirePlumber, ordered after
    // the unit, probes it as one).
    assert!(
        obc_pipeline::discovery::query_cap(&sink)
            .is_some_and(|q| obc_pipeline::DeviceInfo::capture_from_caps(q.caps)),
        "{} is not a capture device when the daemon takes its name",
        sink.display()
    );
    eprintln!("name owned {named:?} after the spawn; the loopback was already a capture device");
    client.wait_for_daemon(Duration::from_secs(15));
    assert_eq!(client.state(), State::Idle, "error: {}", client.error());
    assert_eq!(client.error(), "");
    assert_eq!(client.output_mode(), "NV12 1920x1080@30");
    let cams = client.cameras();
    let id_of = |usb: &str| {
        cams.iter()
            .find(|c| c.usb == usb)
            .unwrap_or_else(|| panic!("camera {usb} not listed: {cams:?}"))
            .id
            .clone()
    };
    let (id1, id2) = (id_of(&cam1.usb), id_of(&cam2.usb));
    assert_eq!(cams[0].id, id1, "preference order: {cams:?}");
    assert_eq!(cams[1].id, id2, "preference order: {cams:?}");
    assert_eq!(client.active(), id1);
    eprintln!("cameras: {cams:?}");
    // The loopback is a capture device (the writer streams) ...
    assert!(
        wait(Duration::from_secs(5), || {
            obc_pipeline::discovery::query_cap(&sink)
                .is_some_and(|q| obc_pipeline::DeviceInfo::capture_from_caps(q.caps))
        }),
        "{} is not a capture device while the daemon idles",
        sink.display()
    );
    // ... while the daemon has no camera, no GPU.
    assert!(holds(pid, &sink), "the writer holds the loopback");
    assert!(
        !holds(pid, &cam1.node) && !holds(pid, &cam2.node),
        "{:?}",
        open_nodes(pid)
    );
    assert!(
        !holds_nvidia(pid),
        "a GPU device open before the first activation: {:?}",
        open_nodes(pid)
    );
    assert!(!cuda_pids().contains(&pid), "a CUDA context while idle");
    assert!(wait(Duration::from_secs(2), || client.consumers() == 0));
    let idle_cpu = cpu_percent(pid, Duration::from_secs(5));
    eprintln!("CPU idle (no consumer) over 5 s: {idle_cpu:.2} % of one core");

    // ---- a consumer: starting, then active ----
    let consumer = Consumer::spawn(&consumer_bin, &sink);
    let opened = Instant::now();
    assert!(
        wait(Duration::from_secs(5), || client.consumers() == 1),
        "Consumers never 1"
    );
    let n = client
        .wait_state(State::Active, Duration::from_secs(20))
        .map(|_| opened.elapsed())
        .unwrap_or_else(|| {
            panic!(
                "not active within 20 s: {} ({})",
                client.state(),
                client.error()
            )
        });
    assert!(
        consumer.wait_more(true, 30, Duration::from_secs(10)),
        "no forwarded frames at the consumer: {:?}",
        consumer.runs()
    );
    let first_stamped = consumer
        .lines()
        .iter()
        .find(|(_, l)| *l == Line::Frame(true))
        .map(|(t, _)| t.saturating_duration_since(opened));
    eprintln!(
        "ACTIVE (first camera frame at the output) {n:?} after the open; first frame through the \
         session's output at the consumer {first_stamped:?} after the open"
    );
    assert!(
        holds(pid, &cam1.node),
        "streams from {}",
        cam1.node.display()
    );
    assert!(
        wait(Duration::from_secs(5), || cuda_pids().contains(&pid)),
        "no CUDA context while active"
    );
    // Stats about once per second.
    assert!(
        wait(Duration::from_secs(5), || client
            .stats
            .lock()
            .unwrap()
            .len()
            >= 3),
        "fewer than 3 Stats signals while active"
    );
    for (_, s) in client.stats.lock().unwrap().iter() {
        eprintln!("Stats {s:?}");
    }
    {
        let st = client.stats.lock().unwrap();
        let (_, last) = st.last().unwrap();
        assert!((25.0..=35.0).contains(&last.fps), "{last:?}");
        assert!(
            last.latency_p50_ms > 0.0 && last.latency_p50_ms < 1000.0,
            "{last:?}"
        );
        assert!(last.cpu_percent >= 0.0, "{last:?}");
        assert!(last.gpu_percent == -1.0 || (0.0..=100.0).contains(&last.gpu_percent));
    }

    // ---- camera switches while active ----
    client.next().unwrap();
    assert_eq!(client.active(), id2);
    assert!(
        wait(Duration::from_secs(5), || holds(pid, &cam2.node)
            && !holds(pid, &cam1.node)),
        "after NextCamera: {:?}",
        open_nodes(pid)
    );
    assert_eq!(client.state(), State::Active);
    assert!(
        consumer.wait_more(true, 30, Duration::from_secs(10)),
        "{:?}",
        consumer.runs()
    );
    client.select(&cam1.selector).unwrap();
    assert_eq!(client.active(), id1);
    assert!(
        wait(Duration::from_secs(5), || holds(pid, &cam1.node)
            && !holds(pid, &cam2.node)),
        "after SelectCamera: {:?}",
        open_nodes(pid)
    );
    assert!(
        consumer.wait_more(true, 30, Duration::from_secs(10)),
        "{:?}",
        consumer.runs()
    );
    match client.select("no such camera at all") {
        Err(IpcError::NoSuchCamera(_)) => {}
        other => panic!("{other:?}"),
    }
    // End on the camera that is not first in the preference, so the
    // restart below shows the selection persisting.
    client.select(&cam2.selector).unwrap();
    assert_eq!(client.active(), id2);
    assert!(
        wait(Duration::from_secs(5), || holds(pid, &cam2.node)
            && !holds(pid, &cam1.node)),
        "after SelectCamera(2): {:?}",
        open_nodes(pid)
    );
    assert!(
        consumer.wait_more(true, 30, Duration::from_secs(10)),
        "{:?}",
        consumer.runs()
    );

    // ---- off while active: camera and CUDA released, black keeps flowing ----
    client.set_enabled(false).unwrap();
    assert_eq!(client.state(), State::Off);
    assert!(
        wait(Duration::from_secs(3), || !holds(pid, &cam1.node)
            && !holds(pid, &cam2.node)),
        "a camera still open while off: {:?}",
        open_nodes(pid)
    );
    let released = Instant::now();
    assert!(
        wait(Duration::from_secs(10), || !cuda_pids().contains(&pid)),
        "the CUDA context outlived the session"
    );
    eprintln!(
        "off: CUDA context gone {:?} after the camera closed",
        released.elapsed()
    );
    assert!(
        consumer.wait_more(false, 30, Duration::from_secs(5)),
        "the consumer stopped receiving frames while off: {:?}",
        consumer.runs()
    );
    let runs = consumer.runs();
    eprintln!("consumer 1 runs (stamped = via the camera session): {runs:?}");
    assert_eq!(
        runs.iter().map(|r| r.0).collect::<Vec<_>>(),
        [false, true, false],
        "placeholders, camera session, placeholders: {runs:?}"
    );
    assert_eq!(
        consumer.caps().len(),
        1,
        "one caps negotiation: {:?}",
        consumer.caps()
    );
    assert!(consumer.problems().is_empty(), "{:?}", consumer.problems());
    consumer.close();
    assert!(wait(Duration::from_secs(3), || client.consumers() == 0));

    // ---- restart: Enabled persisted ----
    assert_eq!(daemon.terminate(), Some(0), "the daemon exits 0 on SIGTERM");
    drop(daemon);
    let mut daemon = spawn_daemon(&env);
    let pid = daemon.pid();
    client.wait_for_daemon(Duration::from_secs(15));
    assert!(!client.enabled(), "Enabled=false survives a restart");
    assert_eq!(client.state(), State::Off);
    assert_ne!(cams[0].id, id2, "id2 is not first in the preference");
    assert_eq!(client.active(), id2, "the selection survives a restart");
    let consumer = Consumer::spawn(&consumer_bin, &sink);
    assert!(wait(Duration::from_secs(5), || client.consumers() == 1));
    // Off with a consumer past the debounce: the daemon must stay near its
    // idle CPU (a zero wakeup would spin a core).
    std::thread::sleep(Duration::from_millis(500));
    let off_cpu = cpu_percent(pid, Duration::from_secs(5));
    eprintln!(
        "CPU off with a consumer over 5 s: {off_cpu:.2} % of one core (idle: {idle_cpu:.2} %)"
    );
    assert!(
        off_cpu < 5.0,
        "off with a consumer: {off_cpu:.2} % of one core"
    );
    assert_eq!(client.state(), State::Off);
    assert!(!holds(pid, &cam1.node) && !holds(pid, &cam2.node));
    client.set_enabled(true).unwrap();
    assert!(
        client
            .wait_state(State::Active, Duration::from_secs(20))
            .is_some(),
        "not active after SetEnabled(true): {}",
        client.error()
    );
    assert!(holds(pid, &cam2.node), "streams from the selected camera");
    assert!(
        consumer.wait_more(true, 30, Duration::from_secs(10)),
        "{:?}",
        consumer.runs()
    );

    // ---- the consumer leaves: idle after the grace ----
    let closed = Instant::now();
    consumer.close();
    // close() returns once the consumer process has exited (its device
    // descriptor closed).
    let exited = Instant::now();
    let to_idle = client
        .wait_state(State::Idle, Duration::from_secs(15))
        .map(|_| exited.elapsed())
        .unwrap_or_else(|| panic!("not idle after the consumer left: {}", client.state()));
    let grace = Duration::from_secs(1);
    eprintln!(
        "IDLE {to_idle:?} after the consumer exited (grace {grace:?}, so {:?} beyond it; the \
         consumer took {:?} to exit after its stdin closed)",
        to_idle.saturating_sub(grace),
        exited.saturating_duration_since(closed)
    );
    assert!(to_idle >= grace - Duration::from_millis(50), "{to_idle:?}");
    assert!(
        !holds(pid, &cam1.node) && !holds(pid, &cam2.node),
        "{:?}",
        open_nodes(pid)
    );
    let t = Instant::now();
    assert!(
        wait(Duration::from_secs(10), || !cuda_pids().contains(&pid)),
        "CUDA context still listed while idle"
    );
    eprintln!("idle: CUDA context gone within {:?}", t.elapsed());

    // ---- config reloads ----
    std::fs::write(
        env.config_file(),
        config(&sink, &cam2.selector, &cam1.selector, ""),
    )
    .unwrap();
    daemon.signal(libc::SIGHUP);
    assert!(
        wait(Duration::from_secs(5), || client
            .cameras()
            .first()
            .is_some_and(|c| c.id == id2)),
        "SIGHUP did not apply the new preference: {:?}",
        client.cameras()
    );
    std::fs::write(
        env.config_file(),
        config(&sink, &cam1.selector, &cam2.selector, ""),
    )
    .unwrap();
    client.reload().unwrap();
    assert_eq!(client.cameras()[0].id, id1, "ReloadConfig applies at once");
    std::fs::write(
        env.config_file(),
        config(&sink, &cam2.selector, &cam1.selector, "[camera]\n"),
    )
    .unwrap();
    match client.reload() {
        Err(IpcError::InvalidConfig(m)) => eprintln!("rejected as expected: {m}"),
        other => panic!("expected InvalidConfig, got {other:?}"),
    }
    assert!(!client.error().is_empty());
    assert_eq!(client.cameras()[0].id, id1, "the old config stays in force");
    assert_eq!(client.state(), State::Idle);
    std::fs::write(
        env.config_file(),
        config(&sink, &cam1.selector, &cam2.selector, ""),
    )
    .unwrap();
    client.reload().unwrap();
    assert_eq!(client.error(), "");

    // ---- SIGTERM while active: camera closed, its control restored ----
    use obc_pipeline::controls::{CID_EXPOSURE_AUTO_PRIORITY, get_ctrl, probe_ctrl, set_ctrl};
    std::fs::write(
        env.config_file(),
        config_with(
            &sink,
            &cam1.selector,
            &cam2.selector,
            "fixed_framerate = true\n",
            "",
        ),
    )
    .unwrap();
    client.reload().unwrap();
    assert_eq!(client.active(), id2, "the selection still holds");
    let streaming = cam2.node.clone();
    // Start from "on", so the daemon has something to pin and restore.
    let restore = match probe_ctrl(&streaming, CID_EXPOSURE_AUTO_PRIORITY) {
        Ok((info, original)) if !info.read_only() => {
            set_ctrl(&streaming, CID_EXPOSURE_AUTO_PRIORITY, 1).unwrap();
            Some(RestoreControl {
                path: streaming.clone(),
                id: CID_EXPOSURE_AUTO_PRIORITY,
                value: original,
            })
        }
        Ok(_) => {
            eprintln!(
                "SKIP the control check: exposure_dynamic_framerate is read-only on {}",
                streaming.display()
            );
            None
        }
        Err(skip) => {
            eprintln!(
                "SKIP the control check: exposure_dynamic_framerate {skip} on {}",
                streaming.display()
            );
            None
        }
    };
    let consumer = Consumer::spawn(&consumer_bin, &sink);
    assert!(
        client
            .wait_state(State::Active, Duration::from_secs(20))
            .is_some(),
        "not active: {} ({})",
        client.state(),
        client.error()
    );
    assert!(holds(pid, &streaming), "{:?}", open_nodes(pid));
    if restore.is_some() {
        assert_eq!(
            get_ctrl(&streaming, CID_EXPOSURE_AUTO_PRIORITY).unwrap(),
            0,
            "pinned while active"
        );
    }
    let t = Instant::now();
    daemon.signal(libc::SIGTERM);
    let code = daemon.wait_exit(Duration::from_secs(10));
    let took = t.elapsed();
    eprintln!("SIGTERM while active: exit {code:?} after {took:?}");
    assert_eq!(code, Some(0), "the daemon exits 0 on SIGTERM while active");
    assert_eq!(
        count_holders(
            Path::new("/proc"),
            &NodeId::of(&streaming),
            std::process::id()
        ),
        0,
        "the camera is released"
    );
    if restore.is_some() {
        assert_eq!(
            get_ctrl(&streaming, CID_EXPOSURE_AUTO_PRIORITY).unwrap(),
            1,
            "exposure_dynamic_framerate restored at shutdown"
        );
    }
    assert!(!cuda_pids().contains(&pid));
    consumer.close();
    drop(daemon);
    assert_eq!(
        count_holders(Path::new("/proc"), &NodeId::of(&sink), std::process::id()),
        0,
        "the loopback is released"
    );

    // ---- the bus goes away: the daemon exits with a failure ----
    let mut daemon = spawn_daemon(&env);
    client.wait_for_daemon(Duration::from_secs(15));
    drop(client);
    let t = Instant::now();
    drop(bus);
    let code = daemon.wait_exit(Duration::from_secs(10));
    eprintln!("bus gone: daemon exit {code:?} after {:?}", t.elapsed());
    assert_eq!(
        code,
        Some(1),
        "a lost bus is a failure (systemd restarts it)"
    );
    assert_eq!(
        count_holders(Path::new("/proc"), &NodeId::of(&sink), std::process::id()),
        0,
        "the loopback is released"
    );
}
