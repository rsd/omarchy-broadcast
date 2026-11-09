//! The live preview end to end on a real loopback: the `omarchy-broadcastd`
//! binary on a private D-Bus daemon, the loopback named by `OBC_TEST_SINK`,
//! and `omarchy-broadcast preview --json` holding the lease in its own
//! process (the CLI is built by the test, `cargo build -p omarchy-broadcast`,
//! into the same target directory; `OBC_CLI=<path>` overrides it).
//!
//! Run with
//! `OBC_TEST_SINK=/dev/video11 [OBC_TEST_CAMERA=046d:0944]
//! cargo test -p omarchy-broadcastd --test preview -- --ignored --nocapture`.
//!
//! Without a camera the daemon is switched off first (`SetEnabled(false)`,
//! in the test's own state file), so the lease opens no camera and the
//! preview carries the writer's black placeholders. With `OBC_TEST_CAMERA`
//! the same runs enabled: the lease starts the camera, and the preview
//! carries its frames. Either way, at 15 and at 30 frames per second, the
//! frames arrive at the rate asked (within 20 %, over at least 5 s), the
//! frame file is a 480x270 JPEG of mode 0600, it is gone once the lease is
//! released (SIGTERM of the CLI), and a `kill -9` of the CLI ends its lease
//! (`PreviewConsumers` back to 0) and removes the file.
//!
//! The daemon's CPU use with no lease, a 15 and a 30 fps lease, and the
//! encoder's time per frame (the daemon's debug log) are printed, not
//! checked.

mod common;

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{DaemonEnv, Guard, PrivateBus};
use obc_core::CameraSelector;
use obc_ipc::{OmarchyBroadcastProxy, State};
use omarchy_broadcastd::consumers::{NodeId, count_holders};
use zbus::proxy::CacheProperties;

/// How long the frames are counted at each rate.
const SPAN: Duration = Duration::from_secs(6);
/// How far the measured rate may be from the one asked.
const TOLERANCE: f64 = 0.2;

/// `omarchy-broadcast`, built if needed.
fn cli_binary() -> PathBuf {
    if let Some(p) = std::env::var_os("OBC_CLI") {
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
        .args([
            "build",
            "-q",
            "-p",
            "omarchy-broadcast",
            "--bin",
            "omarchy-broadcast",
        ])
        .arg("--manifest-path")
        .arg(&manifest);
    if profile_dir.file_name().is_some_and(|n| n == "release") {
        build.arg("--release");
    }
    let st = build
        .status()
        .expect("run cargo to build omarchy-broadcast");
    assert!(st.success(), "building omarchy-broadcast failed: {st}");
    let bin = profile_dir.join("omarchy-broadcast");
    assert!(bin.exists(), "{} missing after the build", bin.display());
    bin
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

/// A blocking client of the daemon on the private bus.
struct Client {
    rt: tokio::runtime::Runtime,
    proxy: OmarchyBroadcastProxy<'static>,
}

impl Client {
    fn connect(env: &DaemonEnv) -> Client {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let proxy = rt.block_on(async {
            let conn = env.connect().await;
            OmarchyBroadcastProxy::builder(&conn)
                .cache_properties(CacheProperties::No)
                .build()
                .await
                .unwrap()
        });
        Client { rt, proxy }
    }

    fn state(&self) -> Option<State> {
        self.rt.block_on(self.proxy.state()).ok()
    }

    fn previews(&self) -> u32 {
        self.rt
            .block_on(self.proxy.preview_consumers())
            .unwrap_or(u32::MAX)
    }

    fn set_enabled(&self, on: bool) {
        self.rt.block_on(self.proxy.set_enabled(on)).unwrap();
    }
}

/// One line of `preview --json`, as far as this test reads it (the JSON
/// contract itself is the CLI's tests' business).
#[derive(Debug, Clone)]
struct Line {
    kind: String,
    active: Option<bool>,
    seq: Option<u64>,
    path: Option<String>,
}

/// The string value of `"key":"..."` in `line` (no escapes in these).
fn string_field(line: &str, key: &str) -> Option<String> {
    let start = line.find(&format!("\"{key}\":\""))? + key.len() + 4;
    let end = start + line[start..].find('"')?;
    Some(line[start..end].to_string())
}

/// The literal after `"key":` in `line`, up to the next `,` or `}`.
fn raw_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let start = line.find(&format!("\"{key}\":"))? + key.len() + 3;
    let end = start + line[start..].find([',', '}'])?;
    Some(&line[start..end])
}

impl Line {
    fn parse(line: &str) -> Line {
        assert!(line.starts_with("{\"v\":1,\"type\":"), "{line}");
        Line {
            kind: string_field(line, "type").unwrap_or_default(),
            active: raw_field(line, "active").map(|v| v == "true"),
            seq: raw_field(line, "seq").and_then(|v| v.parse().ok()),
            path: string_field(line, "path"),
        }
    }
}

/// `omarchy-broadcast preview --json --fps <fps>` with its lines, each with
/// the time it arrived.
struct Preview {
    guard: Guard,
    lines: Arc<Mutex<Vec<(Instant, Line)>>>,
}

impl Preview {
    fn spawn(cli: &Path, env: &DaemonEnv, fps: u32) -> Preview {
        let mut child = Command::new(cli)
            .args(["preview", "--json", "--fps", &fps.to_string()])
            .env("DBUS_SESSION_BUS_ADDRESS", &env.bus_address)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn omarchy-broadcast preview");
        let out = child.stdout.take().unwrap();
        let lines: Arc<Mutex<Vec<(Instant, Line)>>> = Arc::default();
        let l = Arc::clone(&lines);
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                l.lock().unwrap().push((Instant::now(), Line::parse(&line)));
            }
        });
        Preview {
            guard: Guard::new(child, "omarchy-broadcast preview", Duration::from_secs(5)),
            lines,
        }
    }

    /// The path of the `preview` line announcing the lease.
    fn wait_active(&self, limit: Duration) -> String {
        assert!(
            wait(limit, || self
                .lines
                .lock()
                .unwrap()
                .iter()
                .any(|(_, v)| v.kind == "preview" && v.active == Some(true))),
            "no lease within {limit:?}: {:?}",
            self.lines.lock().unwrap()
        );
        let lines = self.lines.lock().unwrap();
        let (_, v) = lines.iter().find(|(_, v)| v.kind == "preview").unwrap();
        v.path.clone().expect("the lease's path")
    }

    /// The `frame` lines after `since`: (arrival, seq).
    fn frames_since(&self, since: Instant) -> Vec<(Instant, u64)> {
        self.lines
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, v)| *t >= since && v.kind == "frame")
            .map(|(t, v)| (*t, v.seq.expect("a frame's seq")))
            .collect()
    }
}

/// The width and height in a JPEG's SOF segment.
fn jpeg_size(data: &[u8]) -> Option<(u32, u32)> {
    if data.get(..2)? != [0xFF, 0xD8] {
        return None;
    }
    let mut i = 2;
    while i + 4 <= data.len() {
        if data[i] != 0xFF {
            return None;
        }
        let marker = data[i + 1];
        let len = usize::from(u16::from_be_bytes([data[i + 2], data[i + 3]]));
        if matches!(marker, 0xC0..=0xC3) {
            let h = u16::from_be_bytes([*data.get(i + 5)?, *data.get(i + 6)?]);
            let w = u16::from_be_bytes([*data.get(i + 7)?, *data.get(i + 8)?]);
            return Some((u32::from(w), u32::from(h)));
        }
        i += 2 + len;
    }
    None
}

/// CPU time process `pid` has used (user + system), in clock ticks.
fn cpu_ticks(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let rest = &stat[stat.rfind(')').unwrap() + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
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

/// The daemon with its standard error captured (and echoed).
fn spawn_daemon(env: &DaemonEnv) -> (Guard, Arc<Mutex<Vec<String>>>) {
    let mut child = env
        .command()
        .arg("-v")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn omarchy-broadcastd");
    let err = child.stderr.take().unwrap();
    let log: Arc<Mutex<Vec<String>>> = Arc::default();
    let l = Arc::clone(&log);
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(err).lines() {
            let Ok(line) = line else { break };
            eprintln!("{line}");
            l.lock().unwrap().push(line);
        }
    });
    (
        Guard::new(child, "omarchy-broadcastd", Duration::from_secs(10)),
        log,
    )
}

/// The encoder's per-frame time from the daemon's last `live preview
/// stopped` debug line.
fn last_frame_ms(log: &Arc<Mutex<Vec<String>>>) -> String {
    log.lock()
        .unwrap()
        .iter()
        .rev()
        .find(|l| l.contains("live preview stopped"))
        .and_then(|l| l.split(", ").last())
        .unwrap_or("n/a")
        .to_string()
}

#[test]
#[ignore = "needs OBC_TEST_SINK (a free v4l2loopback device with exclusive_caps=1), /usr/bin/dbus-daemon, gst-plugins-good (jpegenc) and cargo (to build omarchy-broadcast); OBC_TEST_CAMERA (optional) runs it with the camera active"]
fn the_preview_streams_at_the_rate_asked_and_ends_with_its_holder() {
    let sink = PathBuf::from(std::env::var("OBC_TEST_SINK").unwrap_or_else(|_| {
        panic!("this test needs OBC_TEST_SINK: a free v4l2loopback device (exclusive_caps=1)")
    }));
    let sink = std::fs::canonicalize(&sink).unwrap_or(sink);
    let camera = std::env::var("OBC_TEST_CAMERA").ok().map(|selector| {
        let sel: CameraSelector = selector.parse().expect("a valid camera selector");
        let devices = obc_pipeline::scan_sysfs();
        let picked =
            obc_pipeline::select_camera(&devices, Some(&sel), &obc_core::ModeRequest::default())
                .unwrap_or_else(|e| panic!("OBC_TEST_CAMERA={selector}: {e}"));
        (selector, picked.device.path.clone())
    });
    for node in std::iter::once(&sink).chain(camera.as_ref().map(|(_, n)| n)) {
        let n = count_holders(Path::new("/proc"), &NodeId::of(node), std::process::id());
        assert_eq!(
            n,
            0,
            "{} is held by another process (see `fuser -v {}`): not touching it",
            node.display(),
            node.display()
        );
    }
    let cli = cli_binary();
    let dir = tempfile::tempdir().unwrap();
    let bus = PrivateBus::start(dir.path());
    let env = DaemonEnv::new(dir.path(), &bus);
    let preference = camera
        .as_ref()
        .map(|(s, _)| format!("[camera]\npreference = [{s:?}]\n"))
        .unwrap_or_default();
    std::fs::write(
        env.config_file(),
        format!(
            "version = 1\n{preference}[loopback]\ndevice = {:?}\n\
             [lifecycle]\nidle_grace_s = 1.0\nstart_debounce_ms = 300\n",
            sink.display().to_string()
        ),
    )
    .unwrap();
    let (mut daemon, log) = spawn_daemon(&env);
    let pid = daemon.pid();
    let client = Client::connect(&env);
    assert!(
        wait(Duration::from_secs(15), || client
            .state()
            .is_some_and(|s| s != State::Error)),
        "the daemon did not come up: {:?}",
        client.state()
    );
    let active = camera.is_some();
    if !active {
        // No camera to open: off, so the lease carries the placeholders.
        client.set_enabled(false);
    }
    let what = if active {
        "active (camera)"
    } else {
        "off (placeholders)"
    };
    let frame_file = env.runtime_dir.join("omarchy-broadcast/preview.jpg");
    std::thread::sleep(Duration::from_secs(2));
    let idle_cpu = cpu_percent(pid, Duration::from_secs(10));
    eprintln!("[measure] {what}, no lease: daemon CPU {idle_cpu:.1} % of one core");

    for fps in [15u32, 30] {
        let mut preview = Preview::spawn(&cli, &env, fps);
        let path = preview.wait_active(Duration::from_secs(10));
        assert_eq!(PathBuf::from(&path), frame_file);
        assert_eq!(client.previews(), 1);
        if active {
            assert!(
                wait(Duration::from_secs(15), || client.state()
                    == Some(State::Active)),
                "the lease did not start the camera: {:?}",
                client.state()
            );
        } else {
            assert_eq!(client.state(), Some(State::Off));
        }
        // Settled, then counted.
        std::thread::sleep(Duration::from_secs(1));
        let since = Instant::now();
        let cpu = cpu_percent(pid, Duration::from_secs(10).max(SPAN));
        let frames = preview.frames_since(since);
        assert!(frames.len() > 2, "{fps} fps: {} frames", frames.len());
        let (first, last) = (frames[0], frames[frames.len() - 1]);
        let secs = (last.0 - first.0).as_secs_f64();
        assert!(secs >= 5.0, "{fps} fps: frames over {secs:.1} s only");
        let rate = (frames.len() - 1) as f64 / secs;
        eprintln!(
            "[measure] {what}, {fps} fps lease: {rate:.2} frames/s, daemon CPU {cpu:.1} % of one \
             core"
        );
        assert!(
            (rate - f64::from(fps)).abs() <= f64::from(fps) * TOLERANCE,
            "{fps} fps asked, {rate:.2} measured"
        );
        assert!(
            frames.windows(2).all(|w| w[1].1 > w[0].1),
            "seq increases strictly"
        );
        let data = std::fs::read(&frame_file).unwrap();
        assert_eq!(jpeg_size(&data), Some((480, 270)), "a 480x270 JPEG");
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&frame_file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            let dmode = std::fs::metadata(frame_file.parent().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(dmode & 0o7777, 0o700);
        }
        // Released on SIGTERM: the lease and the file go.
        assert_eq!(preview.guard.terminate(), Some(0), "preview exit status");
        assert!(
            wait(Duration::from_secs(5), || client.previews() == 0),
            "the lease outlived the CLI"
        );
        assert!(
            wait(Duration::from_secs(5), || !frame_file.exists()),
            "the frame file outlived the lease"
        );
        eprintln!(
            "[measure] {what}, {fps} fps lease: encoder {}",
            last_frame_ms(&log)
        );
    }

    // kill -9: the bus tells the daemon, which ends the lease.
    let mut preview = Preview::spawn(&cli, &env, 15);
    preview.wait_active(Duration::from_secs(10));
    assert!(
        wait(Duration::from_secs(5), || frame_file.exists()),
        "no frame file"
    );
    preview.guard.signal(libc::SIGKILL);
    assert!(preview.guard.wait_exit(Duration::from_secs(5)).is_some());
    assert!(
        wait(Duration::from_secs(5), || client.previews() == 0),
        "the killed CLI's lease was not ended"
    );
    assert!(
        wait(Duration::from_secs(5), || !frame_file.exists()),
        "the frame file outlived the killed CLI's lease"
    );
    if active {
        assert!(
            wait(Duration::from_secs(10), || client.state()
                == Some(State::Idle)),
            "idle after the grace: {:?}",
            client.state()
        );
    }
    assert_eq!(daemon.terminate(), Some(0), "daemon exit status");
    assert!(!frame_file.exists());
}
