//! The worker under test: its binary spawned as systemd would (the test
//! holds the listening socket and hands it over as descriptor 3, with
//! `LISTEN_FDS`/`LISTEN_PID`), with modules built from
//! `examples/module-hello-audio` and the host's fixture module (`audio`
//! feature), and a fake mic plugin (the `obc-audio` client side) that
//! sends a synthetic speech-like signal at simulated quanta.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use obc_audio::BLOCK_SAMPLES;
use obc_audio::client::{ClientRings, ConnectError, Control};
use obc_audio::server::Listener;
use obc_audio::shm::{SLOT_FLAG_DISCONT, SLOT_FLAG_NEW_STREAM, SLOT_FLAG_RAW};

pub const FIXTURE_ID: &str = "org.example.fixture-audio";
pub const HELLO_ID: &str = "org.example.hello-audio";

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Builds the cdylib of the package at `dir` with `features` (`--locked`:
/// its committed `Cargo.lock` as it is) and copies it to `name.so`, once
/// per test binary.
fn build(dir: &Path, lib: &str, features: &str, name: &str) -> PathBuf {
    static BUILT: Mutex<BTreeMap<String, PathBuf>> = Mutex::new(BTreeMap::new());
    let mut built = BUILT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = built.get(name) {
        return p.clone();
    }
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let target = tmp.join("module-builds");
    let mut cmd = Command::new(env!("CARGO"));
    cmd.args(["build", "--quiet", "--locked", "--manifest-path"])
        .arg(dir.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&target);
    if !features.is_empty() {
        cmd.args(["--features", features]);
    }
    let status = cmd.status().expect("run cargo");
    assert!(
        status.success(),
        "building {} failed (with --locked: if its Cargo.lock is out of date, run \
         `cargo update --workspace --manifest-path {}`)",
        dir.display(),
        dir.join("Cargo.toml").display()
    );
    let libs = tmp.join("module-libs");
    std::fs::create_dir_all(&libs).unwrap();
    let out = libs.join(format!("{name}.so"));
    std::fs::copy(target.join("debug").join(format!("lib{lib}.so")), &out).unwrap();
    built.insert(name.to_string(), out.clone());
    out
}

/// The fixture module of obc-module-host, built as an audio module.
pub fn fixture_lib() -> PathBuf {
    build(
        &repo().join("crates/obc-module-host/tests/fixtures/module-fixture"),
        "obc_module_fixture",
        "audio",
        "fixture-audio",
    )
}

/// The example audio module.
pub fn hello_lib() -> PathBuf {
    build(
        &repo().join("examples/module-hello-audio"),
        "obc_module_hello_audio",
        "",
        "hello-audio",
    )
}

/// The fixture's manifest; `enhance_extra` is added to its `mic-enhance`
/// stage (a `conflicts` line, say).
pub fn fixture_manifest(library: &Path, enhance_extra: &str) -> String {
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
frame_samples = [480, 960]

[[stage]]
id = "mic-dereverb"
slot = "clean"
rank = 200
device = "cpu"
position = 50
frames = ["host"]
rates = [48000]
frame_samples = [480, 960]

[[stage]]
id = "mic-enhance"
slot = "shape"
rank = 200
device = "cpu"
frames = ["host"]
rates = [48000, 16000]
frame_samples = [480]
{enhance_extra}
"#,
        library.display()
    )
}

/// The example module's manifest.
pub fn hello_manifest(library: &Path) -> String {
    let text = std::fs::read_to_string(repo().join("examples/module-hello-audio/hello-audio.toml"))
        .unwrap();
    text.lines()
        .map(|l| {
            if l.starts_with("library") {
                format!("library = \"{}\"", library.display())
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A video module's manifest (the worker lists it, unavailable).
pub fn video_manifest() -> String {
    r#"abi = 1
id = "org.example.video"
name = "Video"
version = "1"
licence = "MIT"
kind = "video-stage"
library = "/nonexistent/video.so"

[[stage]]
id = "alpha"
slot = "pre-matte"
rank = 100
device = "cpu"
frames = ["host"]
formats = ["nv12"]
"#
    .into()
}

/// A short directory for sockets (a unix socket path has at most 108
/// bytes): under `$XDG_RUNTIME_DIR` when short, else `/tmp`.
pub fn socket_dir() -> tempfile::TempDir {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|d| d.is_absolute() && d.as_os_str().len() <= 64);
    runtime
        .into_iter()
        .chain([PathBuf::from("/tmp")])
        .find_map(|base| {
            tempfile::Builder::new()
                .prefix("obc-audiod-")
                .tempdir_in(base)
                .ok()
        })
        .expect("no writable directory for the test sockets")
}

/// A private session bus, stopped when dropped.
pub struct PrivateBus {
    pub address: String,
    child: Child,
    _dir: tempfile::TempDir,
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub const DBUS_DAEMON: &str = "/usr/bin/dbus-daemon";

impl PrivateBus {
    pub fn start() -> PrivateBus {
        assert!(
            Path::new(DBUS_DAEMON).exists(),
            "this test needs {DBUS_DAEMON} (package `dbus`) for a private session bus"
        );
        let dir = socket_dir();
        let config = dir.path().join("bus.conf");
        std::fs::write(
            &config,
            format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:dir={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
                dir.path().display()
            ),
        )
        .unwrap();
        let mut child = Command::new(DBUS_DAEMON)
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--print-address=1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let address = line.trim().to_string();
        assert!(address.starts_with("unix:"), "{line:?}");
        PrivateBus {
            address,
            child,
            _dir: dir,
        }
    }

    /// Calls `method` of the worker with `arg` (or none); the string it
    /// returns, or the error's name and message.
    pub fn call(&self, method: &str, arg: Option<&str>) -> Result<String, String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let conn = zbus::connection::Builder::address(self.address.as_str())
                .build()
                .await
                .map_err(|e| e.to_string())?;
            let dest = omarchy_broadcast_audiod::dbus::BUS_NAME;
            let path = omarchy_broadcast_audiod::dbus::OBJECT_PATH;
            let iface = omarchy_broadcast_audiod::dbus::INTERFACE;
            let reply = match arg {
                Some(a) => {
                    conn.call_method(Some(dest), path, Some(iface), method, &(a,))
                        .await
                }
                None => {
                    conn.call_method(Some(dest), path, Some(iface), method, &())
                        .await
                }
            };
            let reply = reply.map_err(|e| e.to_string())?;
            let body = reply.body();
            if body.signature().to_string().is_empty() {
                return Ok(String::new());
            }
            body.deserialize::<String>().map_err(|e| e.to_string())
        })
    }

    /// Calls `method` of the worker with a string, for a boolean.
    pub fn call_bool(&self, method: &str, arg: &str) -> Result<bool, String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let conn = zbus::connection::Builder::address(self.address.as_str())
                .build()
                .await
                .map_err(|e| e.to_string())?;
            let reply = conn
                .call_method(
                    Some(omarchy_broadcast_audiod::dbus::BUS_NAME),
                    omarchy_broadcast_audiod::dbus::OBJECT_PATH,
                    Some(omarchy_broadcast_audiod::dbus::INTERFACE),
                    method,
                    &(arg,),
                )
                .await
                .map_err(|e| e.to_string())?;
            reply
                .body()
                .deserialize::<bool>()
                .map_err(|e| e.to_string())
        })
    }

    /// Waits until the worker answers `GetStatus`.
    pub fn wait_for_worker(&self, timeout: Duration) {
        let end = Instant::now() + timeout;
        loop {
            match self.call("GetStatus", None) {
                Ok(_) => return,
                Err(e) if Instant::now() > end => panic!("the worker never answered: {e}"),
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }

    /// The status, parsed.
    pub fn status(&self) -> serde_json::Value {
        serde_json::from_str(&self.call("GetStatus", None).unwrap()).unwrap()
    }
}

/// The environment of a worker: config, state and data homes, a module
/// directory, the listening socket (held by the test, as systemd holds it)
/// and the worker's log.
pub struct Env {
    pub dir: tempfile::TempDir,
    pub sockets: tempfile::TempDir,
    pub socket_path: PathBuf,
    pub listener: Listener,
    pub modules: PathBuf,
    pub state: PathBuf,
    /// The bus address the worker gets (a dead one without a bus).
    pub bus: Option<String>,
    pub env: Vec<(String, String)>,
    logs: usize,
}

impl Env {
    pub fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let sockets = socket_dir();
        let socket_path = sockets.path().join("audio.sock");
        let listener = Listener::bind(&socket_path).unwrap();
        let modules = dir.path().join("modules.d");
        std::fs::create_dir_all(&modules).unwrap();
        let state = dir.path().join("state/omarchy-broadcast");
        std::fs::create_dir_all(&state).unwrap();
        Env {
            dir,
            sockets,
            socket_path,
            listener,
            modules,
            state,
            bus: None,
            env: Vec::new(),
            logs: 0,
        }
    }

    /// Installs a manifest.
    pub fn manifest(&self, name: &str, text: &str) {
        std::fs::write(self.modules.join(format!("{name}.toml")), text).unwrap();
    }

    /// Installs the fixture module (its own copy of the library).
    pub fn fixture(&self, enhance_extra: &str) {
        let lib = self.dir.path().join("fixture.so");
        std::fs::copy(fixture_lib(), &lib).unwrap();
        self.manifest("fixture", &fixture_manifest(&lib, enhance_extra));
    }

    /// Installs the example module.
    pub fn hello(&self) {
        let lib = self.dir.path().join("hello.so");
        std::fs::copy(hello_lib(), &lib).unwrap();
        self.manifest("hello", &hello_manifest(&lib));
    }

    pub fn snapshot_path(&self) -> PathBuf {
        self.state.join("audio-settings.json")
    }

    /// Writes the snapshot file (as the daemon would).
    pub fn snapshot(&self, json: serde_json::Value) {
        std::fs::write(self.snapshot_path(), json.to_string()).unwrap();
    }

    pub fn ledger_path(&self) -> PathBuf {
        self.state.join("audio-modules-crash.json")
    }

    fn command(&mut self) -> (Command, PathBuf) {
        self.logs += 1;
        let log = self.dir.path().join(format!("worker-{}.log", self.logs));
        let mut cmd = Command::new("/bin/sh");
        let home = self.dir.path().join("home");
        let bus = self.bus.clone().unwrap_or_else(|| {
            format!("unix:path={}", self.sockets.path().join("no-bus").display())
        });
        cmd.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .env("XDG_STATE_HOME", self.dir.path().join("state"))
            .env("XDG_DATA_HOME", self.dir.path().join("data"))
            .env("DBUS_SESSION_BUS_ADDRESS", bus)
            .env(obc_audio::SOCKET_ENV, &self.socket_path)
            // Never the user's PipeWire: a runtime directory with none.
            .env(
                "PIPEWIRE_RUNTIME_DIR",
                self.sockets.path().join("no-pipewire"),
            )
            .envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap());
        (cmd, log)
    }

    /// Starts a worker as systemd's socket activation would: the listening
    /// socket as descriptor 3, `LISTEN_FDS=1`, `LISTEN_PID` its pid.
    pub fn spawn(&mut self) -> Worker {
        let bin = env!("CARGO_BIN_EXE_omarchy-broadcast-audiod");
        let (mut cmd, log) = self.command();
        // Only the test's own module directory, never the system's.
        cmd.arg("-c")
            .arg("export LISTEN_FDS=1 LISTEN_PID=$$; exec \"$0\" \"$@\"")
            .arg(bin)
            .arg("--module-dir")
            .arg(&self.modules);
        let fd = self.listener.as_fd().as_raw_fd();
        // SAFETY: dup2, fcntl and setrlimit are async-signal-safe.
        unsafe {
            cmd.pre_exec(move || {
                // dup2 onto itself keeps close-on-exec: clear it then.
                if fd == 3 {
                    if libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                } else if libc::dup2(fd, 3) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let none = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                libc::setrlimit(libc::RLIMIT_CORE, &none);
                Ok(())
            });
        }
        Worker {
            child: cmd.spawn().unwrap(),
            log,
        }
    }

    /// Starts a worker directly: it binds `socket` itself.
    pub fn spawn_direct(&mut self, socket: &Path) -> Worker {
        let bin = env!("CARGO_BIN_EXE_omarchy-broadcast-audiod");
        let (mut cmd, log) = self.command();
        cmd.env(obc_audio::SOCKET_ENV, socket)
            .arg("-c")
            .arg("exec \"$0\" \"$@\"")
            .arg(bin)
            .arg("--module-dir")
            .arg(&self.modules);
        Worker {
            child: cmd.spawn().unwrap(),
            log,
        }
    }
}

/// A worker process, killed when dropped.
pub struct Worker {
    pub child: Child,
    pub log: PathBuf,
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Worker {
    pub fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Waits until the log has a line containing `needle`.
    pub fn wait_log(&self, needle: &str, timeout: Duration) {
        let end = Instant::now() + timeout;
        while !self.log_text().contains(needle) {
            assert!(
                Instant::now() < end,
                "no log line with {needle:?}:\n{}",
                self.log_text()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits for the process to end; its exit status.
    pub fn wait_exit(&mut self, timeout: Duration) -> std::process::ExitStatus {
        let end = Instant::now() + timeout;
        loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                return s;
            }
            assert!(
                Instant::now() < end,
                "the worker did not exit:\n{}",
                self.log_text()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// SIGKILL.
    pub fn kill9(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

/// The synthetic speech-like input: voiced harmonics of a gliding pitch
/// under a syllable envelope (about 4 per second), plus a little
/// deterministic noise. A function of the absolute sample index.
pub fn signal(n: u64) -> f32 {
    use std::f64::consts::TAU;
    let t = n as f64 / 48_000.0;
    let env = (0.5 - 0.5 * (TAU * 4.0 * t).cos()).powi(2);
    let f0 = 150.0 + 20.0 * (TAU * 0.7 * t).sin();
    let voiced = 0.35 * (TAU * f0 * t).sin()
        + 0.2 * (TAU * 2.0 * f0 * t).sin()
        + 0.1 * (TAU * 3.0 * f0 * t).sin();
    let mut x = n.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
    x ^= x >> 33;
    let noise = (x % 2001) as f64 / 1000.0 - 1.0;
    (env * voiced + 0.02 * noise) as f32
}

/// The input block `seq`.
pub fn input_block(seq: u64) -> [f32; BLOCK_SAMPLES] {
    let mut b = [0.0; BLOCK_SAMPLES];
    for (i, s) in b.iter_mut().enumerate() {
        *s = signal(seq * BLOCK_SAMPLES as u64 + i as u64);
    }
    b
}

/// One output block the plugin received.
#[derive(Debug, Clone)]
pub struct Out {
    pub seq: u64,
    pub flags: u32,
    pub samples: Vec<f32>,
    /// The connection it came on (1, 2, ...).
    pub conn: u32,
}

impl Out {
    pub fn raw(&self) -> bool {
        self.flags & SLOT_FLAG_RAW != 0
    }

    /// Whether the samples are the input's times `gain`.
    pub fn is_input_times(&self, gain: f32) -> bool {
        let want = input_block(self.seq);
        self.samples
            .iter()
            .zip(want)
            .all(|(a, b)| (a - b * gain).abs() <= 1e-4)
    }
}

/// The simulated quantum, in samples (not a multiple of 480).
pub const QUANTUM: u64 = 1024;

/// A fake mic plugin. While it has no connection, it plays raw by its own
/// path (counted in `raw_own`), as the real plugin does.
pub struct Plugin {
    pub path: PathBuf,
    conn: Option<(ClientRings, Control)>,
    pub conns: u32,
    next_seq: u64,
    pending_flags: u32,
    acc: u64,
    pub sent: u64,
    pub raw_own: u64,
    pub out: Vec<Out>,
}

impl Plugin {
    pub fn new(path: &Path) -> Plugin {
        Plugin {
            path: path.to_path_buf(),
            conn: None,
            conns: 0,
            next_seq: 0,
            pending_flags: 0,
            acc: 0,
            sent: 0,
            raw_own: 0,
            out: Vec::new(),
        }
    }

    /// Connects, retrying until `timeout`.
    pub fn connect(&mut self, timeout: Duration) -> Result<(), ConnectError> {
        let end = Instant::now() + timeout;
        loop {
            match obc_audio::client::connect(&self.path, 8, Duration::from_secs(3)) {
                Ok(c) => {
                    self.conn = Some(c);
                    self.conns += 1;
                    self.pending_flags = SLOT_FLAG_NEW_STREAM;
                    return Ok(());
                }
                Err(e) if e.is_refusal() || Instant::now() > end => return Err(e),
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }

    pub fn connected(&self) -> bool {
        self.conn.is_some()
    }

    pub fn rings(&self) -> &ClientRings {
        &self.conn.as_ref().expect("connected").0
    }

    pub fn accept(&self) -> obc_audio::proto::Accept {
        self.conn.as_ref().expect("connected").1.accept
    }

    /// Hangs up.
    pub fn disconnect(&mut self) {
        self.conn = None;
    }

    fn hung_up(&self) -> bool {
        let Some((rings, control)) = &self.conn else {
            return true;
        };
        let _ = rings;
        let mut fds = [obc_audio::sys::pollfd(control.socket.as_fd(), libc::POLLIN)];
        obc_audio::sys::poll(&mut fds, Some(Duration::ZERO)).unwrap_or(0) > 0
    }

    /// Runs the audio callback at real pace for `dur`: blocks of the input
    /// signal are sent (or, without a connection, played raw by the
    /// plugin's own path) and the worker's blocks taken. Stops early when
    /// the worker hangs up (and drops the connection).
    pub fn run(&mut self, dur: Duration) {
        let quantum = Duration::from_micros(QUANTUM * 1_000_000 / 48_000);
        let start = Instant::now();
        let mut next = start;
        while start.elapsed() < dur {
            self.acc += QUANTUM;
            while self.acc >= BLOCK_SAMPLES as u64 {
                self.acc -= BLOCK_SAMPLES as u64;
                let seq = self.next_seq;
                self.next_seq += 1;
                match &mut self.conn {
                    Some((rings, _)) => {
                        let flags = std::mem::take(&mut self.pending_flags);
                        match rings.push_input(seq, flags, &input_block(seq)) {
                            Ok(true) => self.sent += 1,
                            Ok(false) => {
                                self.pending_flags = flags | SLOT_FLAG_DISCONT;
                                self.raw_own += 1;
                            }
                            Err(_) => self.conn = None,
                        }
                    }
                    None => self.raw_own += 1,
                }
            }
            if let Some((rings, _)) = &self.conn {
                rings.notify();
            }
            self.take_outputs();
            if self.conn.is_some() && self.hung_up() {
                self.take_outputs();
                self.conn = None;
                return;
            }
            next += quantum;
            if let Some(wait) = next.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
        }
        self.take_outputs();
    }

    fn take_outputs(&mut self) {
        let conn = self.conns;
        let Some((rings, _)) = &mut self.conn else {
            return;
        };
        let mut b = [0.0f32; BLOCK_SAMPLES];
        while let Ok(Some(meta)) = rings.pop_output(&mut b) {
            self.out.push(Out {
                seq: meta.seq,
                flags: meta.flags,
                samples: b.to_vec(),
                conn,
            });
        }
    }

    /// Sends one block and waits (at most `timeout`) for its answer;
    /// whether it came.
    pub fn step(&mut self, timeout: Duration) -> bool {
        let seq = self.next_seq;
        self.next_seq += 1;
        let Some((rings, _)) = &mut self.conn else {
            self.raw_own += 1;
            return false;
        };
        let flags = std::mem::take(&mut self.pending_flags);
        if rings.push_input(seq, flags, &input_block(seq)) != Ok(true) {
            return false;
        }
        self.sent += 1;
        rings.notify();
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            self.take_outputs();
            if self.out.last().is_some_and(|o| o.seq == seq) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        false
    }

    /// Runs until `cond` holds of the outputs, at most `timeout`.
    pub fn run_until(&mut self, timeout: Duration, cond: impl Fn(&[Out]) -> bool) -> bool {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            self.run(Duration::from_millis(50));
            if cond(&self.out) {
                return true;
            }
            if self.conn.is_none() {
                return false;
            }
        }
        false
    }

    /// The outputs of connection `conn` are in order, one per block sent,
    /// with no sequence number twice.
    pub fn assert_in_order(&self) {
        let mut last: BTreeMap<u32, u64> = BTreeMap::new();
        for o in &self.out {
            if let Some(prev) = last.get(&o.conn) {
                assert!(o.seq > *prev, "seq {} after {prev}", o.seq);
            }
            last.insert(o.conn, o.seq);
            assert!(o.seq < self.next_seq);
        }
    }
}
