//! The `oma-broadcast` binary against a mock daemon on a private D-Bus bus.
//!
//! Each test starts its own `dbus-daemon` (a session-type bus listening on a
//! socket under the target's temporary directory) and passes its address to
//! the CLI child alone, through `DBUS_SESSION_BUS_ADDRESS`; the user's session
//! bus is never used and the daemon's name is only ever claimed on the
//! private bus. The test fails, naming it, when `dbus-daemon` is missing.
//!
//! The private bus has one activation file for `br.com.dias.OmaBroadcast`
//! whose program only creates a marker file (and fails without claiming the
//! name), so a test can tell whether a command tried to start the daemon.
//!
//! The mock implements `br.com.dias.OmaBroadcast1` closely enough for the
//! CLI (adapted from `obc-ipc`'s mock server); it is not the daemon.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use obc_ipc::{BUS_NAME, CameraInfo, INTERFACE_REVISION, IpcError, OBJECT_PATH, State};
use serde_json::Value;
use zbus::object_server::{InterfaceRef, SignalEmitter};
use zbus::{Connection, connection, interface};

const CLI: &str = env!("CARGO_BIN_EXE_oma-broadcast");
const DBUS_DAEMON: &str = "dbus-daemon";
const SH: &str = "/bin/sh";
/// Generous bound for anything the CLI should do "at once".
const TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Child processes

/// A child process that is killed and reaped when dropped, so a failing test
/// leaves nothing running.
struct Guard(Option<Child>);

impl Guard {
    fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("child already reaped")
    }

    fn pid(&mut self) -> i32 {
        i32::try_from(self.child().id()).unwrap()
    }

    /// Waits up to `limit` for the child to exit.
    fn wait_for(&mut self, limit: Duration, what: &str) -> ExitStatus {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = self.child().try_wait().unwrap() {
                self.0 = None;
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "{what}: still running after {limit:?}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn signal(&mut self, sig: libc::c_int) {
        let pid = self.pid();
        // SAFETY: plain kill(2) on our own child's pid.
        let rc = unsafe { libc::kill(pid, sig) };
        assert_eq!(rc, 0, "kill({pid}, {sig})");
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn collect(mut r: impl Read + Send + 'static) -> JoinHandle<String> {
    thread::spawn(move || {
        let mut s = String::new();
        let _ = r.read_to_string(&mut s);
        s
    })
}

// ---------------------------------------------------------------------------
// The private bus

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct Bus {
    address: String,
    dir: PathBuf,
    daemon: Guard,
}

impl Bus {
    fn start() -> Bus {
        assert!(
            Path::new(SH).exists(),
            "{SH} is required by the activation probe of these tests"
        );
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "cli-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let services = dir.join("services");
        std::fs::create_dir_all(&services).unwrap();
        std::fs::write(
            services.join(format!("{BUS_NAME}.service")),
            format!(
                // Fails at once, so the activation fails at once.
                "[D-BUS Service]\nName={BUS_NAME}\nExec={SH} -c 'echo > \"{}\"; exit 1'\n",
                dir.join("activated").display()
            ),
        )
        .unwrap();
        let config = dir.join("bus.conf");
        std::fs::write(
            &config,
            format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={}</listen>
  <auth>EXTERNAL</auth>
  <servicedir>{}</servicedir>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
                dir.join("bus").display(),
                services.display()
            ),
        )
        .unwrap();

        let child = Command::new(DBUS_DAEMON)
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--print-address=1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| {
                panic!(
                    "cannot run {DBUS_DAEMON} ({e}): these tests need the D-Bus reference \
                     daemon (package `dbus`) on PATH"
                )
            });
        let mut daemon = Guard(Some(child));
        let stdout = daemon.child().stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = tx.send(line);
        });
        let address = rx
            .recv_timeout(TIMEOUT)
            .unwrap_or_else(|_| panic!("{DBUS_DAEMON} printed no address"))
            .trim()
            .to_string();
        assert!(
            address.starts_with("unix:"),
            "{DBUS_DAEMON} printed {address:?}"
        );
        Bus {
            address,
            dir,
            daemon,
        }
    }

    fn activation_attempted(&self) -> bool {
        self.dir.join("activated").exists()
    }

    fn cli(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(CLI);
        cmd.args(args)
            .env("DBUS_SESSION_BUS_ADDRESS", &self.address)
            .env_remove("DBUS_STARTER_ADDRESS")
            .env_remove("DBUS_STARTER_BUS_TYPE")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    /// Runs the CLI to completion.
    fn run(&self, args: &[&str]) -> Output {
        run_cmd(self.cli(args), args)
    }

    /// Starts `watch`; its stdout lines arrive on [`Watch::next`]. With
    /// `keep`, the reader closes the pipe after that many lines.
    fn watch(&self, args: &[&str], keep: Option<usize>) -> Watch {
        let mut guard = Guard(Some(self.cli(args).spawn().unwrap()));
        let stdout = guard.child().stdout.take().unwrap();
        let stderr = collect(guard.child().stderr.take().unwrap());
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut count = 0;
            loop {
                if keep == Some(count) {
                    return; // drops the read end of the pipe
                }
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        count += 1;
                        if tx.send(line).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        Watch {
            child: guard,
            lines: rx,
            stderr: Some(stderr),
        }
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        // Stop the bus before removing its directory.
        drop(self.daemon.0.take().map(|c| Guard(Some(c))));
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Output {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

fn run_cmd(mut cmd: Command, args: &[&str]) -> Output {
    let mut guard = Guard(Some(cmd.spawn().unwrap()));
    let stdout = collect(guard.child().stdout.take().unwrap());
    let stderr = collect(guard.child().stderr.take().unwrap());
    let status = guard.wait_for(Duration::from_secs(40), &format!("oma-broadcast {args:?}"));
    Output {
        status,
        stdout: stdout.join().unwrap(),
        stderr: stderr.join().unwrap(),
    }
}

struct Watch {
    child: Guard,
    lines: Receiver<String>,
    stderr: Option<JoinHandle<String>>,
}

impl Watch {
    fn next(&self, what: &str) -> String {
        match self.lines.recv_timeout(TIMEOUT) {
            Ok(line) => line,
            Err(e) => panic!("watch: no line for {what} ({e:?})"),
        }
    }

    /// The next line, parsed and checked against the JSON contract.
    fn next_json(&self, what: &str) -> Value {
        json_line(&self.next(what))
    }

    /// Asserts that no line arrives within `d`.
    fn quiet_for(&self, d: Duration, what: &str) {
        match self.lines.recv_timeout(d) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(line) => panic!("{what}: unexpected line {line:?}"),
            Err(RecvTimeoutError::Disconnected) => panic!("{what}: watch output ended"),
        }
    }

    /// Waits for the child to exit and returns its status and stderr.
    fn finish(mut self, what: &str) -> (ExitStatus, String) {
        let status = self.child.wait_for(Duration::from_secs(5), what);
        let stderr = self.stderr.take().unwrap().join().unwrap();
        (status, stderr)
    }
}

/// One line of `--json` output: one object, `v` = 1, a `type`.
fn json_line(line: &str) -> Value {
    assert!(line.ends_with('\n'), "unterminated line {line:?}");
    let body = line.trim_end_matches('\n');
    assert!(!body.contains('\n'));
    assert!(body.len() <= 4096, "{} bytes", body.len());
    let v: Value = serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body}"));
    assert!(v.is_object(), "{body}");
    assert_eq!(v["v"], 1, "{body}");
    assert!(v["type"].is_string(), "{body}");
    v
}

// ---------------------------------------------------------------------------
// The mock daemon

struct Mock {
    enabled: bool,
    state: State,
    error: String,
    cameras: Vec<CameraInfo>,
    active: usize,
    consumers: u32,
    effect: String,
    blur_strength: f64,
    background_image: String,
    /// The effect calls received, in order, as `Method(argument)`.
    calls: Vec<String>,
}

impl Mock {
    fn new() -> Self {
        Mock {
            enabled: true,
            state: State::Idle,
            error: String::new(),
            cameras: vec![
                CameraInfo {
                    id: "usb-1234_abcd".into(),
                    name: "First Camera".into(),
                    usb: "1234:abcd".into(),
                },
                CameraInfo {
                    id: "platform-video2".into(),
                    name: "Second Camera".into(),
                    usb: String::new(),
                },
            ],
            active: 0,
            consumers: 0,
            effect: "none".into(),
            blur_strength: 0.5,
            background_image: String::new(),
            calls: Vec::new(),
        }
    }
}

#[interface(name = "br.com.dias.OmaBroadcast1")]
impl Mock {
    async fn set_enabled(
        &mut self,
        enabled: bool,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        self.enabled = enabled;
        self.state = if enabled { State::Idle } else { State::Off };
        self.enabled_changed(&emitter).await?;
        self.state_changed(&emitter).await?;
        Ok(())
    }

    async fn select_camera(
        &mut self,
        selector: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        let want = selector.to_lowercase();
        let found = self.cameras.iter().position(|c| {
            c.id == selector || c.usb == selector || c.name.to_lowercase().contains(&want)
        });
        match found {
            Some(i) => {
                self.active = i;
                self.active_camera_changed(&emitter).await?;
                Ok(())
            }
            None => Err(IpcError::NoSuchCamera(format!(
                "no camera matches {selector:?}"
            ))),
        }
    }

    async fn next_camera(
        &mut self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        if self.cameras.is_empty() {
            return Err(IpcError::Unavailable("no camera present".into()));
        }
        self.active = (self.active + 1) % self.cameras.len();
        self.active_camera_changed(&emitter).await?;
        Ok(())
    }

    async fn reload_config(&mut self) -> Result<(), IpcError> {
        Ok(())
    }

    async fn set_effect(
        &mut self,
        effect: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        self.calls.push(format!("SetEffect({effect})"));
        match effect {
            "replace" if self.background_image.is_empty() => Err(IpcError::InvalidArgument(
                "replace needs a background image: set one with SetBackgroundImage".into(),
            )),
            "none" | "blur" | "replace" => {
                self.effect = effect.into();
                self.effect_changed(&emitter).await?;
                Ok(())
            }
            other => Err(IpcError::InvalidArgument(format!(
                "unknown effect {other:?}"
            ))),
        }
    }

    async fn set_blur_strength(
        &mut self,
        strength: f64,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        self.calls.push(format!("SetBlurStrength({strength})"));
        if !(0.0..=1.0).contains(&strength) {
            return Err(IpcError::InvalidArgument(format!(
                "blur strength {strength} is outside 0 to 1"
            )));
        }
        self.blur_strength = strength;
        self.blur_strength_changed(&emitter).await?;
        Ok(())
    }

    /// Accepts an absolute path to a regular file whose name does not end in
    /// `.txt` (standing in for the daemon's image check).
    async fn set_background_image(
        &mut self,
        path: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        self.calls.push(format!("SetBackgroundImage({path})"));
        if !Path::new(path).is_absolute() || !Path::new(path).is_file() {
            return Err(IpcError::InvalidArgument(format!(
                "{path:?} is not an absolute path to a regular file"
            )));
        }
        if path.ends_with(".txt") {
            return Err(IpcError::InvalidArgument(format!("{path}: not an image")));
        }
        self.background_image = path.into();
        self.background_image_changed(&emitter).await?;
        Ok(())
    }

    #[zbus(signal)]
    async fn stats(
        emitter: &SignalEmitter<'_>,
        fps: f64,
        latency_p50_ms: f64,
        cpu_percent: f64,
        gpu_percent: f64,
    ) -> zbus::Result<()>;

    #[zbus(property)]
    fn state(&self) -> State {
        self.state
    }

    #[zbus(property)]
    fn error(&self) -> String {
        self.error.clone()
    }

    #[zbus(property)]
    fn enabled(&self) -> bool {
        self.enabled
    }

    #[zbus(property)]
    fn cameras(&self) -> Vec<CameraInfo> {
        self.cameras.clone()
    }

    #[zbus(property)]
    fn active_camera(&self) -> String {
        self.cameras
            .get(self.active)
            .map(|c| c.id.clone())
            .unwrap_or_default()
    }

    #[zbus(property)]
    fn output_mode(&self) -> String {
        "NV12 1920x1080@30".into()
    }

    #[zbus(property)]
    fn effect(&self) -> String {
        self.effect.clone()
    }

    #[zbus(property)]
    fn blur_strength(&self) -> f64 {
        self.blur_strength
    }

    #[zbus(property)]
    fn background_image(&self) -> String {
        self.background_image.clone()
    }

    #[zbus(property)]
    fn consumers(&self) -> u32 {
        self.consumers
    }

    #[zbus(property)]
    fn interface_revision(&self) -> u32 {
        INTERFACE_REVISION
    }
}

/// [`Mock`] served on the private bus under the daemon's name, on its own
/// runtime; the test thread drives it synchronously.
struct MockDaemon {
    rt: tokio::runtime::Runtime,
    conn: Connection,
}

impl MockDaemon {
    fn start(bus: &Bus) -> MockDaemon {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let conn = rt
            .block_on(
                connection::Builder::address(bus.address.as_str())
                    .serve_at(OBJECT_PATH, Mock::new())
                    .name(BUS_NAME)
                    .build(),
            )
            .unwrap();
        MockDaemon { rt, conn }
    }

    fn iface(&self) -> InterfaceRef<Mock> {
        self.rt
            .block_on(self.conn.object_server().interface(OBJECT_PATH))
            .unwrap()
    }

    fn active(&self) -> (bool, State, usize) {
        let iface = self.iface();
        self.rt.block_on(async {
            let m = iface.get().await;
            (m.enabled, m.state, m.active)
        })
    }

    /// The effect calls received since the last call, in order.
    fn take_calls(&self) -> Vec<String> {
        let iface = self.iface();
        self.rt
            .block_on(async { std::mem::take(&mut iface.get_mut().await.calls) })
    }

    fn set_consumers(&self, n: u32) {
        let iface = self.iface();
        self.rt.block_on(async {
            iface.get_mut().await.consumers = n;
            let m = iface.get().await;
            m.consumers_changed(iface.signal_emitter()).await.unwrap();
        });
    }

    fn set_state(&self, state: State, error: &str) {
        let iface = self.iface();
        self.rt.block_on(async {
            {
                let mut m = iface.get_mut().await;
                m.state = state;
                m.error = error.into();
            }
            let m = iface.get().await;
            m.state_changed(iface.signal_emitter()).await.unwrap();
            m.error_changed(iface.signal_emitter()).await.unwrap();
        });
    }

    fn add_camera(&self, id: &str, name: &str) {
        let iface = self.iface();
        self.rt.block_on(async {
            iface.get_mut().await.cameras.push(CameraInfo {
                id: id.into(),
                name: name.into(),
                usb: String::new(),
            });
            let m = iface.get().await;
            m.cameras_changed(iface.signal_emitter()).await.unwrap();
        });
    }

    fn emit_stats(&self, fps: f64, latency: f64, cpu: f64, gpu: f64) {
        let iface = self.iface();
        self.rt.block_on(async {
            Mock::stats(iface.signal_emitter(), fps, latency, cpu, gpu)
                .await
                .unwrap();
        });
    }

    fn release_name(&self) {
        assert!(self.rt.block_on(self.conn.release_name(BUS_NAME)).unwrap());
    }

    fn request_name(&self) {
        self.rt.block_on(self.conn.request_name(BUS_NAME)).unwrap();
    }
}

/// Holds the daemon's name and serves its interface, but no property read
/// ever returns: a daemon whose main loop is stuck.
struct Stuck;

#[interface(name = "br.com.dias.OmaBroadcast1")]
impl Stuck {
    #[zbus(property)]
    async fn state(&self) -> State {
        std::future::pending().await
    }
}

/// [`Stuck`] served on the private bus under the daemon's name.
struct StuckDaemon {
    rt: tokio::runtime::Runtime,
    conn: Connection,
}

impl StuckDaemon {
    fn start(bus: &Bus) -> StuckDaemon {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let conn = rt
            .block_on(
                connection::Builder::address(bus.address.as_str())
                    .serve_at(OBJECT_PATH, Stuck)
                    .name(BUS_NAME)
                    .build(),
            )
            .unwrap();
        StuckDaemon { rt, conn }
    }

    /// Gives up the name, then disconnects (which fails its pending calls).
    fn stop(self) {
        assert!(self.rt.block_on(self.conn.release_name(BUS_NAME)).unwrap());
    }
}

/// A daemon of interface revision 1: the properties before the effect
/// controls, and no effect methods (a call to one fails with
/// `UnknownMethod`).
struct Rev1;

#[interface(name = "br.com.dias.OmaBroadcast1")]
impl Rev1 {
    async fn set_enabled(&mut self, _enabled: bool) -> Result<(), IpcError> {
        Ok(())
    }

    #[zbus(property)]
    fn state(&self) -> State {
        State::Idle
    }

    #[zbus(property)]
    fn error(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn enabled(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn cameras(&self) -> Vec<CameraInfo> {
        Vec::new()
    }

    #[zbus(property)]
    fn active_camera(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn output_mode(&self) -> String {
        "NV12 1920x1080@30".into()
    }

    #[zbus(property)]
    fn effect(&self) -> String {
        "none".into()
    }

    #[zbus(property)]
    fn consumers(&self) -> u32 {
        0
    }

    #[zbus(property)]
    fn interface_revision(&self) -> u32 {
        1
    }
}

/// [`Rev1`] served on the private bus under the daemon's name.
struct Rev1Daemon {
    _rt: tokio::runtime::Runtime,
    _conn: Connection,
}

impl Rev1Daemon {
    fn start(bus: &Bus) -> Rev1Daemon {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let conn = rt
            .block_on(
                connection::Builder::address(bus.address.as_str())
                    .serve_at(OBJECT_PATH, Rev1)
                    .name(BUS_NAME)
                    .build(),
            )
            .unwrap();
        Rev1Daemon {
            _rt: rt,
            _conn: conn,
        }
    }
}

// ---------------------------------------------------------------------------
// One-shot commands

#[test]
fn status_human_and_json() {
    let bus = Bus::start();
    let _mock = MockDaemon::start(&bus);

    let out = bus.run(&["status"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        "State:      idle\n\
         Enabled:    yes\n\
         Camera:     First Camera (1234:abcd)\n\
         Consumers:  0\n\
         Output:     NV12 1920x1080@30\n\
         Effect:     none\n"
    );
    assert_eq!(out.stderr, "");

    let out = bus.run(&["status", "--json"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(out.stdout.lines().count(), 1, "{}", out.stdout);
    let v = json_line(&out.stdout);
    assert_eq!(v["type"], "snapshot");
    assert_eq!(v["state"], "idle");
    assert_eq!(v["enabled"], true);
    assert_eq!(v["error"], "");
    assert_eq!(v["active_camera"], "usb-1234_abcd");
    assert_eq!(v["active_camera_name"], "First Camera");
    assert_eq!(v["consumers"], 0);
    assert_eq!(v["output_mode"], "NV12 1920x1080@30");
    assert_eq!(v["effect"], "none");
    assert_eq!(v["blur_strength"], 0.5);
    assert_eq!(v["background_image"], "");
    assert_eq!(v["interface_revision"], INTERFACE_REVISION);
    assert_eq!(v["cameras"].as_array().unwrap().len(), 2);
    assert_eq!(v["cameras_truncated"], false);
    assert!(!bus.activation_attempted());
}

#[test]
fn cameras_human_and_json() {
    let bus = Bus::start();
    let _mock = MockDaemon::start(&bus);

    let out = bus.run(&["cameras"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        "* First Camera   1234:abcd  usb-1234_abcd\n  \
         Second Camera  -          platform-video2\n"
    );

    let out = bus.run(&["cameras", "--json"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(out.stdout.lines().count(), 1, "{}", out.stdout);
    let v = json_line(&out.stdout);
    assert_eq!(v["type"], "cameras");
    assert_eq!(v["active_camera"], "usb-1234_abcd");
    assert_eq!(
        v["cameras"],
        serde_json::json!([
            {"id": "usb-1234_abcd", "name": "First Camera", "usb": "1234:abcd", "active": true},
            {"id": "platform-video2", "name": "Second Camera", "usb": "", "active": false},
        ])
    );
    assert_eq!(v["cameras_truncated"], false);
}

#[test]
fn on_off_and_camera_commands() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);

    let out = bus.run(&["off"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(out.stdout, "Disabled (state: off)\n");
    assert!(!mock.active().0);

    let out = bus.run(&["on"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(out.stdout, "Enabled (state: idle)\n");
    assert_eq!(mock.active().1, State::Idle);

    let out = bus.run(&["camera", "next"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(out.stdout, "Camera: Second Camera\n");
    assert_eq!(mock.active().2, 1);

    let out = bus.run(&["camera", "1234:abcd"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(out.stdout, "Camera: First Camera (1234:abcd)\n");

    let out = bus.run(&["camera", "second"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(out.stdout, "Camera: Second Camera\n");

    let out = bus.run(&["camera", "usb-1234_abcd"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(mock.active().2, 0);

    let out = bus.run(&["camera", "nope"]);
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr);
    assert_eq!(out.stdout, "");
    assert_eq!(out.stderr, "oma-broadcast: no camera matches \"nope\"\n");
    assert_eq!(mock.active().2, 0);

    let out = bus.run(&["camera"]);
    assert_eq!(out.status.code(), Some(2), "{}", out.stderr);
    assert!(!bus.activation_attempted());
}

/// A file `name` under the bus's directory, and its canonical path.
fn image_file(bus: &Bus, name: &str) -> PathBuf {
    let path = bus.dir.join("pictures").join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"not really a picture").unwrap();
    std::fs::canonicalize(&path).unwrap()
}

#[test]
fn effect_shows_and_selects_the_effect() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);

    // Reading never starts the daemon and calls nothing.
    let out = bus.run(&["effect"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        "Effect:     none\nStrength:   0.50\nImage:      (none)\n"
    );
    let out = bus.run(&["effect", "--json"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(out.stdout.lines().count(), 1, "{}", out.stdout);
    let v = json_line(&out.stdout);
    assert_eq!(v["type"], "snapshot");
    assert_eq!(v["effect"], "none");
    assert_eq!(v["blur_strength"], 0.5);
    assert_eq!(v["background_image"], "");
    assert!(mock.take_calls().is_empty());

    // The strength is set before the effect.
    let out = bus.run(&["effect", "blur", "--strength", "0.8"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        "Effect:     blur\nStrength:   0.80\nImage:      (none)\n"
    );
    assert_eq!(
        mock.take_calls(),
        vec!["SetBlurStrength(0.8)", "SetEffect(blur)"]
    );
    let out = bus.run(&["effect", "blur"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(mock.take_calls(), vec!["SetEffect(blur)"]);
    let out = bus.run(&["status"]);
    assert!(
        out.stdout.ends_with("Effect:     blur\nStrength:   0.80\n"),
        "{}",
        out.stdout
    );

    // A relative image path is resolved against the CLI's directory and
    // sent absolute, before the effect.
    let image = image_file(&bus, "office.png");
    let args = ["effect", "replace", "--image", "../pictures/office.png"];
    let elsewhere = bus.dir.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let mut cmd = bus.cli(&args);
    cmd.current_dir(&elsewhere);
    let out = run_cmd(cmd, &args);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        format!(
            "Effect:     replace\nStrength:   0.80\nImage:      {}\n",
            image.display()
        )
    );
    assert_eq!(
        mock.take_calls(),
        vec![
            format!("SetBackgroundImage({})", image.display()),
            "SetEffect(replace)".into()
        ]
    );
    let out = bus.run(&["status", "--json"]);
    let v = json_line(&out.stdout);
    assert_eq!(v["effect"], "replace");
    assert_eq!(v["blur_strength"], 0.8);
    assert_eq!(v["background_image"], image.to_str().unwrap());
    let out = bus.run(&["status"]);
    assert!(
        out.stdout.ends_with(&format!(
            "Effect:     replace\nImage:      {}\n",
            image.display()
        )),
        "{}",
        out.stdout
    );

    let out = bus.run(&["effect", "none"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert!(
        out.stdout.starts_with("Effect:     none\n"),
        "{}",
        out.stdout
    );
    assert_eq!(mock.take_calls(), vec!["SetEffect(none)"]);
    assert_eq!(out.stderr, "");
}

#[test]
fn effect_usage_errors_exit_2_before_any_call() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    let image = image_file(&bus, "bg.png");
    let image = image.to_str().unwrap();
    let dir = bus.dir.to_str().unwrap();
    let missing = bus.dir.join("missing.png");
    let missing = missing.to_str().unwrap();
    for args in [
        &["effect", "blur", "--strength", "1.5"][..],
        &["effect", "blur", "--strength", "-0.1"],
        &["effect", "blur", "--strength", "NaN"],
        &["effect", "blur", "--strength", "inf"],
        &["effect", "blur", "--strength", "lots"],
        &["effect", "none", "--strength", "0.5"],
        &["effect", "replace", "--strength", "0.5", "--image", image],
        &["effect", "blur", "--image", image],
        &["effect", "none", "--image", image],
        &["effect", "replace", "--image", missing],
        &["effect", "replace", "--image", dir],
        &["effect", "replace", "--image", ""],
        &["effect", "--strength", "0.5"],
        &["effect", "--image", image],
        &["effect", "blur", "--json"],
        &["effect", "sparkle"],
    ] {
        let out = bus.run(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", out.stderr);
        assert_eq!(out.stdout, "", "{args:?}");
        assert!(
            out.stderr.starts_with("error: "),
            "{args:?}: {}",
            out.stderr
        );
    }
    let out = bus.run(&["effect", "none", "--strength", "0.5"]);
    assert!(
        out.stderr.contains("--strength applies to `blur`"),
        "{}",
        out.stderr
    );
    let out = bus.run(&["effect", "replace", "--image", missing]);
    assert!(out.stderr.contains(missing), "{}", out.stderr);
    assert!(mock.take_calls().is_empty());
    assert!(!bus.activation_attempted());
}

#[test]
fn effect_rejected_by_the_daemon_exits_1_with_its_message() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);

    // `replace` with no image set.
    let out = bus.run(&["effect", "replace"]);
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr);
    assert_eq!(out.stdout, "");
    assert_eq!(
        out.stderr,
        "oma-broadcast: replace needs a background image: set one with SetBackgroundImage\n"
    );
    assert_eq!(mock.take_calls(), vec!["SetEffect(replace)"]);

    // A rejected image stops the request: the effect is not selected.
    let notes = image_file(&bus, "notes.txt");
    let out = bus.run(&["effect", "replace", "--image", notes.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr);
    assert_eq!(
        out.stderr,
        format!("oma-broadcast: {}: not an image\n", notes.display())
    );
    assert_eq!(
        mock.take_calls(),
        vec![format!("SetBackgroundImage({})", notes.display())]
    );
    let out = bus.run(&["effect", "--json"]);
    assert_eq!(json_line(&out.stdout)["effect"], "none");
}

#[test]
fn effect_against_a_revision_1_daemon_says_it_is_unsupported() {
    let bus = Bus::start();
    let _daemon = Rev1Daemon::start(&bus);
    let expected = "oma-broadcast: the daemon does not support effects (its interface \
                    revision is 1, effects need 2); update oma-broadcastd\n";
    for args in [
        &["effect"][..],
        &["effect", "blur"],
        &["effect", "blur", "--strength", "0.3"],
        &["effect", "none"],
    ] {
        let out = bus.run(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", out.stderr);
        assert_eq!(out.stdout, "", "{args:?}");
        assert_eq!(out.stderr, expected, "{args:?}");
    }
    // `--json` prints the snapshot, whose revision tells a client the same.
    let out = bus.run(&["effect", "--json"]);
    assert!(out.status.success(), "{}", out.stderr);
    let v = json_line(&out.stdout);
    assert_eq!(v["interface_revision"], 1);
    assert_eq!(v["effect"], "none");
    // The other commands still work with it.
    let out = bus.run(&["status"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert!(out.stdout.contains("Effect:     none\n"), "{}", out.stdout);
    let out = bus.run(&["on"]);
    assert!(out.status.success(), "{}", out.stderr);
}

#[test]
fn daemon_not_running_has_its_own_exit_code_and_reads_never_activate() {
    let bus = Bus::start();

    let out = bus.run(&["status"]);
    assert_eq!(out.status.code(), Some(3), "{}", out.stderr);
    assert_eq!(out.stdout, "");
    assert_eq!(
        out.stderr,
        "oma-broadcast: oma-broadcastd is not running (no owner for br.com.dias.OmaBroadcast \
         on the session bus); start it with: systemctl --user start oma-broadcastd\n"
    );

    for args in [&["status", "--json"][..], &["cameras", "--json"][..]] {
        let out = bus.run(args);
        assert_eq!(out.status.code(), Some(3), "{args:?}: {}", out.stderr);
        assert_eq!(out.stdout.lines().count(), 1, "{}", out.stdout);
        let v = json_line(&out.stdout);
        assert_eq!(v["type"], "error");
        assert_eq!(v["code"], "daemon-not-running");
        assert!(v["error"].as_str().unwrap().contains("not running"), "{v}");
        assert_eq!(out.stderr, "");
    }
    let out = bus.run(&["cameras"]);
    assert_eq!(out.status.code(), Some(3), "{}", out.stderr);
    for args in [&["effect"][..], &["effect", "--json"]] {
        let out = bus.run(args);
        assert_eq!(out.status.code(), Some(3), "{args:?}: {}", out.stderr);
    }
    assert!(
        !bus.activation_attempted(),
        "status/cameras/effect must not start the daemon"
    );

    // A control command may start the daemon: here activation runs the
    // probe, which exits without claiming the name.
    let out = bus.run(&["on"]);
    assert_eq!(out.status.code(), Some(3), "{}", out.stderr);
    assert!(
        out.stderr.contains("D-Bus could not start it"),
        "{}",
        out.stderr
    );
    assert!(out.stderr.contains("systemctl --user start oma-broadcastd"));
    assert!(bus.activation_attempted(), "`on` should allow activation");
}

#[test]
fn no_session_bus_is_daemon_not_running() {
    let bus = Bus::start();
    let missing = bus.dir.join("no-such-bus");
    let mut cmd = bus.cli(&["status", "--json"]);
    cmd.env(
        "DBUS_SESSION_BUS_ADDRESS",
        format!("unix:path={}", missing.display()),
    );
    let out = run_cmd(cmd, &["status", "--json"]);
    assert_eq!(out.status.code(), Some(3), "{}", out.stderr);
    let v = json_line(&out.stdout);
    assert_eq!(v["type"], "error");
    assert_eq!(v["code"], "daemon-not-running");
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains("no D-Bus session bus"),
        "{v}"
    );

    let mut cmd = bus.cli(&["off"]);
    cmd.env(
        "DBUS_SESSION_BUS_ADDRESS",
        format!("unix:path={}", missing.display()),
    );
    let out = run_cmd(cmd, &["off"]);
    assert_eq!(out.status.code(), Some(3), "{}", out.stderr);
    assert!(
        out.stderr.contains("no D-Bus session bus"),
        "{}",
        out.stderr
    );
}

// ---------------------------------------------------------------------------
// watch

#[test]
fn watch_json_follows_changes_stats_and_outages() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    let watch = bus.watch(&["watch", "--json"], None);

    let v = watch.next_json("the first snapshot");
    assert_eq!(v["type"], "snapshot", "{v}");
    assert_eq!(v["state"], "idle");
    assert_eq!(v["consumers"], 0);
    assert_eq!(v["cameras"].as_array().unwrap().len(), 2);

    mock.set_consumers(1);
    let v = watch.next_json("a consumers change");
    assert_eq!(v["type"], "state", "{v}");
    assert_eq!(v["changed"], serde_json::json!(["consumers"]));
    assert_eq!(v["consumers"], 1);
    assert_eq!(v["state"], "idle");
    assert_eq!(v["active_camera_name"], "First Camera");

    mock.set_state(State::Active, "");
    let v = watch.next_json("a state change");
    assert_eq!(v["type"], "state", "{v}");
    assert_eq!(v["changed"], serde_json::json!(["state"]));
    assert_eq!(v["state"], "active");
    assert_eq!(v["consumers"], 1);

    mock.emit_stats(30.0, 12.5, 3.0, -1.0);
    let v = watch.next_json("a Stats signal");
    assert_eq!(v["type"], "stats", "{v}");
    assert_eq!(v["fps"], 30.0);
    assert_eq!(v["latency_p50_ms"], 12.5);
    assert_eq!(v["cpu_percent"], 3.0);
    assert_eq!(v["gpu_percent"], -1.0);

    // `effect`: the option, then the effect, each with its own line.
    let image = image_file(&bus, "bg.png");
    let out = bus.run(&["effect", "replace", "--image", image.to_str().unwrap()]);
    assert!(out.status.success(), "{}", out.stderr);
    let v = watch.next_json("a background image change");
    assert_eq!(v["type"], "state", "{v}");
    assert_eq!(v["changed"], serde_json::json!(["background_image"]));
    assert_eq!(v["background_image"], image.to_str().unwrap());
    assert_eq!(v["effect"], "none");
    let v = watch.next_json("an effect change");
    assert_eq!(v["changed"], serde_json::json!(["effect"]));
    assert_eq!(v["effect"], "replace");
    let out = bus.run(&["effect", "blur", "--strength", "0.25"]);
    assert!(out.status.success(), "{}", out.stderr);
    let v = watch.next_json("a blur strength change");
    assert_eq!(v["changed"], serde_json::json!(["blur_strength"]));
    assert_eq!(v["blur_strength"], 0.25);
    let v = watch.next_json("an effect change");
    assert_eq!(v["changed"], serde_json::json!(["effect"]));
    assert_eq!(v["effect"], "blur");

    mock.add_camera("platform-video9", "Third Camera");
    let v = watch.next_json("a camera list change");
    assert_eq!(v["type"], "cameras", "{v}");
    assert_eq!(v["cameras"].as_array().unwrap().len(), 3);
    assert_eq!(v["cameras"][2]["name"], "Third Camera");
    assert_eq!(v["active_camera"], "usb-1234_abcd");

    // Outage: one error line, none repeated by the 2 s re-check.
    mock.release_name();
    let v = watch.next_json("the outage");
    assert_eq!(v["type"], "error", "{v}");
    assert_eq!(v["code"], "daemon-not-running");
    watch.quiet_for(Duration::from_millis(4500), "during the outage");

    // Back: a fresh snapshot.
    mock.request_name();
    let v = watch.next_json("the snapshot after the outage");
    assert_eq!(v["type"], "snapshot", "{v}");
    assert_eq!(v["state"], "active");
    assert_eq!(v["cameras"].as_array().unwrap().len(), 3);

    mock.set_consumers(0);
    let v = watch.next_json("a change after the outage");
    assert_eq!(v["type"], "state", "{v}");
    assert_eq!(v["consumers"], 0);

    let mut watch = watch;
    watch.child.signal(libc::SIGTERM);
    let (status, stderr) = watch.finish("watch after SIGTERM");
    assert_eq!(status.code(), Some(0), "{status:?}");
    assert_eq!(stderr, "");
    assert!(
        !bus.activation_attempted(),
        "watch must not start the daemon"
    );
}

#[test]
fn watch_json_waits_for_the_daemon_to_appear() {
    let bus = Bus::start();
    let mut watch = bus.watch(&["watch", "--json"], None);

    let v = watch.next_json("the not-running line");
    assert_eq!(v["type"], "error", "{v}");
    assert_eq!(v["code"], "daemon-not-running");
    assert!(v["error"].as_str().unwrap().contains("not running"), "{v}");

    let _mock = MockDaemon::start(&bus);
    let v = watch.next_json("the snapshot once the daemon appears");
    assert_eq!(v["type"], "snapshot", "{v}");
    assert_eq!(v["state"], "idle");

    watch.child.signal(libc::SIGINT);
    let (status, stderr) = watch.finish("watch after SIGINT");
    assert_eq!(status.code(), Some(0), "{status:?}");
    assert_eq!(stderr, "");
    assert!(
        !bus.activation_attempted(),
        "watch must not start the daemon"
    );
}

#[test]
fn watch_json_reports_a_daemon_that_does_not_answer() {
    let bus = Bus::start();
    let stuck = StuckDaemon::start(&bus);
    let watch = bus.watch(&["watch", "--json"], None);

    // The read times out after 5 s: one dbus-error line for the outage.
    let v = watch.next_json("the error line for a silent daemon");
    assert_eq!(v["type"], "error", "{v}");
    assert_eq!(v["code"], "dbus-error", "{v}");
    assert!(v["error"].as_str().unwrap().contains("timed out"), "{v}");
    // The retry (2 s later) times out again without a second line.
    watch.quiet_for(Duration::from_secs(8), "while the daemon stays silent");

    // A daemon that answers takes over: a fresh snapshot.
    stuck.stop();
    let _mock = MockDaemon::start(&bus);
    let v = watch.next_json("the snapshot once the daemon answers");
    assert_eq!(v["type"], "snapshot", "{v}");
    assert_eq!(v["state"], "idle");

    let mut watch = watch;
    watch.child.signal(libc::SIGTERM);
    let (status, stderr) = watch.finish("watch after SIGTERM");
    assert_eq!(status.code(), Some(0), "{status:?}");
    assert_eq!(stderr, "");
    assert!(
        !bus.activation_attempted(),
        "watch must not start the daemon"
    );
}

#[test]
fn watch_exits_quietly_when_its_reader_goes_away() {
    let bus = Bus::start();
    let _mock = MockDaemon::start(&bus);
    // Like `oma-broadcast watch --json | head -1`: the reader takes one line
    // and closes the pipe; nothing else is printed in the meantime.
    let watch = bus.watch(&["watch", "--json"], Some(1));
    let v = watch.next_json("the first snapshot");
    assert_eq!(v["type"], "snapshot");
    let (status, stderr) = watch.finish("watch with a closed stdout");
    assert_eq!(status.code(), Some(0), "{status:?}");
    assert_eq!(stderr, "");
}

#[test]
fn watch_human_prints_changes_and_stats() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    let mut watch = bus.watch(&["watch"], None);

    assert_eq!(
        watch.next("the summary"),
        "oma-broadcastd: state idle | enabled yes | camera First Camera (1234:abcd) \
         | consumers 0 | output NV12 1920x1080@30 | effect none\n"
    );
    mock.set_consumers(2);
    assert_eq!(watch.next("a consumers change"), "consumers: 2\n");
    mock.set_state(State::Error, "camera lost");
    assert_eq!(watch.next("a state change"), "state: error\n");
    assert_eq!(watch.next("an error change"), "error: camera lost\n");
    mock.emit_stats(29.97, -1.0, 4.0, 15.0);
    assert_eq!(
        watch.next("a Stats signal"),
        "stats: 30.0 fps | latency n/a | cpu 4.0% | gpu 15.0%\n"
    );
    let out = bus.run(&["effect", "blur", "--strength", "0.75"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(watch.next("a strength change"), "blur strength: 0.75\n");
    assert_eq!(watch.next("an effect change"), "effect: blur\n");
    mock.release_name();
    assert!(
        watch
            .next("the outage")
            .starts_with("oma-broadcastd is not running"),
    );

    watch.child.signal(libc::SIGTERM);
    let (status, stderr) = watch.finish("watch after SIGTERM");
    assert_eq!(status.code(), Some(0), "{status:?}");
    assert_eq!(stderr, "");
}
