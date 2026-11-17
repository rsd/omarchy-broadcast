//! The `omarchy-broadcast` binary against a mock daemon on a private D-Bus bus.
//!
//! Each test starts its own `dbus-daemon` (a session-type bus listening on a
//! socket under the target's temporary directory) and passes its address to
//! the CLI child alone, through `DBUS_SESSION_BUS_ADDRESS`; the user's session
//! bus is never used and the daemon's name is only ever claimed on the
//! private bus. The test fails, naming it, when `dbus-daemon` is missing.
//!
//! The private bus has one activation file for `br.com.dias.OmarchyBroadcast`
//! whose program only creates a marker file (and fails without claiming the
//! name), so a test can tell whether a command tried to start the daemon.
//!
//! The mock implements `br.com.dias.OmarchyBroadcast1` closely enough for the
//! CLI (adapted from `obc-ipc`'s mock server); it is not the daemon.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use obc_ipc::settings::{VariantDict, WireChange, camera_option};
use obc_ipc::{
    BUS_NAME, CameraControl, CameraControls, CameraInfo, Condition, ConditionOp,
    INTERFACE_REVISION, IpcError, MicStatus, OBJECT_PATH, ResetScope, Setting, SettingChange,
    SettingKind, SettingValue, SettingsGroup, SettingsTree, State,
};
use serde_json::Value;
use zbus::object_server::{InterfaceRef, SignalEmitter};
use zbus::{Connection, OwnedVariant, connection, interface};

const CLI: &str = env!("CARGO_BIN_EXE_omarchy-broadcast");
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

/// A directory with a short path for a bus socket, removed when dropped. A
/// unix socket path is limited to 108 bytes, which a long `TMPDIR` or
/// target directory exceeds: `$XDG_RUNTIME_DIR` when it is set, short and
/// writable, else `/tmp`.
struct SocketDir(PathBuf);

impl SocketDir {
    fn new() -> SocketDir {
        use std::os::unix::fs::DirBuilderExt;
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|d| d.is_absolute() && d.as_os_str().len() <= 64);
        for base in runtime.into_iter().chain([PathBuf::from("/tmp")]) {
            loop {
                let dir = base.join(format!(
                    "obc-{}-{}",
                    std::process::id(),
                    NEXT_DIR.fetch_add(1, Ordering::SeqCst)
                ));
                match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
                    Ok(()) => return SocketDir(dir),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(_) => break,
                }
            }
        }
        panic!("no writable directory for the test bus socket ($XDG_RUNTIME_DIR, /tmp)");
    }
}

impl Drop for SocketDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Bus {
    address: String,
    dir: PathBuf,
    daemon: Guard,
    /// Where the bus socket is (after `daemon`: removed once it stopped).
    socket_dir: SocketDir,
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
        let socket_dir = SocketDir::new();
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
                socket_dir.0.join("bus").display(),
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
            socket_dir,
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
    let status = guard.wait_for(
        Duration::from_secs(40),
        &format!("omarchy-broadcast {args:?}"),
    );
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

    /// The full set of camera settings that follows a snapshot or a
    /// structural change: `count` `settings_group` lines, `index` 0 to
    /// `count - 1`, or one `settings_clear` line.
    fn settings_set(&self, what: &str) -> Vec<Value> {
        let first = self.next_json(what);
        if first["type"] == "settings_clear" {
            return vec![first];
        }
        assert_eq!(first["type"], "settings_group", "{what}: {first}");
        assert_eq!(first["index"], 0, "{what}: {first}");
        let count = first["count"].as_u64().unwrap();
        let mut lines = vec![first];
        for i in 1..count {
            let v = self.next_json(what);
            assert_eq!(v["type"], "settings_group", "{what}: {v}");
            assert_eq!(
                (&v["index"], &v["count"]),
                (&i.into(), &count.into()),
                "{v}"
            );
            lines.push(v);
        }
        lines
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
    /// `Warning` (revision 11).
    warning: String,
    cameras: Vec<CameraInfo>,
    active: usize,
    /// Holding the loopback.
    consumers: u32,
    /// Linked to the PipeWire output.
    pipewire_consumers: u32,
    pipewire_output: String,
    effect: String,
    blur_strength: f64,
    background_image: String,
    denoise: bool,
    /// The effect and preview calls received, in order, as
    /// `Method(argument)`.
    calls: Vec<String>,
    /// The modules (revision 9), their serial, the stage order and the
    /// profile.
    modules: Vec<obc_ipc::ModuleInfo>,
    modules_serial: u32,
    order: Vec<String>,
    profile: String,
    /// The preview leases, by the caller's unique name: their rate.
    leases: std::collections::BTreeMap<String, u32>,
    /// What `AcquirePreview` returns.
    preview_path: String,
    /// The preview calls and `GetCameraControls` calls received without
    /// `NO_AUTO_START` (which would have started the daemon had it not been
    /// running), as in `calls`.
    auto_start: Vec<String>,
    /// The image controls of the first camera (the others have none).
    controls: Vec<CameraControl>,
    /// The next `GetCameraControls` for a named camera first renames the
    /// first camera to this id (the active camera changing under the CLI),
    /// so the call itself finds no such camera.
    rename_on_get: Option<String>,
    /// `GetCameraControls` fails (a device that cannot be read).
    fail_gets: bool,
    /// The camera settings of the first camera (the others have none).
    settings: Vec<SettingsGroup>,
    /// The global settings (revision 8), in every tree after the camera's,
    /// with no camera too; none unless a test sets them.
    global: Vec<SettingsGroup>,
    settings_revision: u32,
    settings_serial: u32,
    /// The next valid `SetSetting` fails with this error instead (`"RateLimited"`
    /// or `"UnknownSetting"`).
    fail_next_set: Option<&'static str>,
    /// The config schema (revision 10), its serial and the files.
    config: Vec<obc_ipc::ConfigGroup>,
    config_serial: u32,
    config_files: Vec<obc_ipc::ConfigFileEntry>,
    /// The microphone (revision 13).
    mic: MicStatus,
    /// `SetMicHost` is refused with this (an ADR 0014 unit running).
    mic_refusal: Option<String>,
    /// The media gallery (revision 15): the items of each kind (the
    /// bundled ones first) and `MediaSerial`.
    media: std::collections::BTreeMap<String, Vec<obc_ipc::MediaEntry>>,
    media_serial: u32,
    /// The user's gallery root ([`USER_MEDIA`] unless a test sets one);
    /// an import writes an empty file there when it exists.
    media_user: String,
    /// The user's videos folder of a background (`GetMediaDirs`,
    /// [`USER_VIDEOS`] unless a test sets one).
    media_videos: String,
    /// `StageValues` (revision 16); none unless a test sets them.
    stage_values: Vec<obc_ipc::StageValue>,
    /// `CameraHealth` (revision 20): the first camera never measured,
    /// unless a test sets more.
    camera_health: obc_ipc::CameraHealth,
    /// `InterfaceRevision` ([`INTERFACE_REVISION`] unless a test sets an
    /// older one; the members stay served).
    revision: u32,
}

/// The mock's user gallery root.
const USER_MEDIA: &str = "/home/u/.local/share/omarchy-broadcast/media";
/// The mock's user videos folder of the backgrounds.
const USER_VIDEOS: &str = "/home/u/Videos/omarchy-broadcast";

/// The mock's gallery directories of `kind`: bundled and the user's under
/// `user`.
fn media_dirs(kind: &str, user: &str) -> (String, String) {
    let dir = if kind == "background" {
        "backgrounds"
    } else {
        kind
    };
    (
        format!("/usr/share/omarchy-broadcast/media/{dir}"),
        format!("{user}/{dir}"),
    )
}

/// An image item of the mock's gallery.
fn media_item(kind: &str, name: &str, label: &str, bundled: bool) -> obc_ipc::MediaEntry {
    media_item_in(USER_MEDIA, kind, name, label, bundled)
}

/// An image item of the mock's gallery, the user's under `user`.
fn media_item_in(
    user: &str,
    kind: &str,
    name: &str,
    label: &str,
    bundled: bool,
) -> obc_ipc::MediaEntry {
    let (b, u) = media_dirs(kind, user);
    obc_ipc::MediaEntry {
        path: format!("{}/{name}", if bundled { b } else { u }),
        label: label.into(),
        bundled,
        media_type: "image".into(),
        width: 3840,
        height: 2160,
        duration_ms: 0,
        poster: String::new(),
        cpu_ready: false,
        convert_command: String::new(),
    }
}

fn control(name: &str, min: i32, max: i32, step: i32, default: i32) -> CameraControl {
    CameraControl {
        name: name.into(),
        min,
        max,
        step,
        default,
        value: default,
        stored: false,
    }
}

/// The image controls the mock's first camera starts with.
fn mock_controls() -> Vec<CameraControl> {
    vec![
        control("brightness", -64, 64, 1, 0),
        control("saturation", 0, 100, 2, 50),
        control("sharpness", 0, 255, 1, 128),
    ]
}

const WB_AUTO: &str = "camera.white_balance.white_balance_automatic";
const WB_REASON: &str = "Turn off automatic white balance to change this";
const EXPOSURE_MODE: &str = "camera.exposure.auto_exposure";
const EXPOSURE_REASON: &str = "Set exposure to manual to change this";
const AF: &str = "camera.focus.focus_automatic_continuous";
const AF_REASON: &str = "Turn off continuous autofocus to change this";

/// The catalogue's reason for a dependent setting that is off.
fn reason_of(id: &str) -> &'static str {
    match id {
        "camera.white_balance.white_balance_temperature" => WB_REASON,
        "camera.exposure.exposure_time_absolute" => EXPOSURE_REASON,
        "camera.focus.focus_absolute" => AF_REASON,
        "effect.background.blur_strength" => BLUR_REASON,
        _ => "The camera does not allow changing this now",
    }
}

fn setting(id: &str, label: &str, kind: SettingKind, hint: Option<&str>) -> Setting {
    Setting {
        id: id.into(),
        label: label.into(),
        kind,
        scope: "camera".into(),
        stored: false,
        enabled: true,
        disabled_reason: String::new(),
        disabled_kind: String::new(),
        enabled_when: Vec::new(),
        hint: hint.map(String::from),
        description: None,
    }
}

fn int(
    id: &str,
    label: &str,
    (min, max, step): (i64, i64, i64),
    default: i64,
    value: i64,
) -> Setting {
    let kind = SettingKind::Int {
        min,
        max,
        step,
        default,
        value,
        unit: None,
        display: None,
    };
    setting(id, label, kind, Some("slider"))
}

fn switch(id: &str, label: &str, default: bool, value: bool) -> Setting {
    let kind = SettingKind::Bool { default, value };
    setting(id, label, kind, Some("switch"))
}

fn choice(id: &str, label: &str, options: &[(&str, &str)], default: &str, value: &str) -> Setting {
    let kind = SettingKind::Enum {
        options: options
            .iter()
            .map(|(v, l)| (v.to_string(), l.to_string()))
            .collect(),
        default: default.into(),
        value: value.into(),
    };
    setting(id, label, kind, Some("segmented"))
}

/// `s`, greyed while `master` does not compare to `value` by `op`.
fn depends(mut s: Setting, master: &str, op: ConditionOp, value: SettingValue) -> Setting {
    s.enabled_when = vec![Condition {
        id: master.into(),
        op,
        value,
    }];
    s.enabled = false;
    s.disabled_reason = reason_of(&s.id).into();
    s.disabled_kind = "parent_off".into();
    s
}

fn with_unit(mut s: Setting, u: &str) -> Setting {
    if let SettingKind::Int { unit, .. } = &mut s.kind {
        *unit = Some(u.into());
    }
    s
}

/// The settings of an MX Brio as the daemon publishes them (its values at
/// rest: automatic white balance, exposure and focus, so their manual
/// values are greyed), with an action added.
fn mock_settings() -> Vec<SettingsGroup> {
    let group = |id: &str, label: &str, settings| SettingsGroup {
        id: id.into(),
        label: label.into(),
        settings,
        surface: None,
    };
    vec![
        group(
            "image",
            "Image",
            vec![
                int(
                    "camera.image.brightness",
                    "Brightness",
                    (0, 255, 1),
                    128,
                    128,
                ),
                int("camera.image.contrast", "Contrast", (0, 255, 1), 128, 128),
                int(
                    "camera.image.saturation",
                    "Saturation",
                    (0, 255, 1),
                    128,
                    128,
                ),
                int("camera.image.sharpness", "Sharpness", (0, 255, 1), 128, 128),
                int(
                    "camera.image.backlight_compensation",
                    "Backlight compensation",
                    (0, 1, 1),
                    1,
                    1,
                ),
            ],
        ),
        group(
            "white_balance",
            "White balance",
            vec![
                switch(WB_AUTO, "Automatic white balance", true, true),
                depends(
                    with_unit(
                        int(
                            "camera.white_balance.white_balance_temperature",
                            "Temperature",
                            (2000, 7500, 1),
                            5000,
                            2975,
                        ),
                        "K",
                    ),
                    WB_AUTO,
                    ConditionOp::Eq,
                    SettingValue::Bool(false),
                ),
            ],
        ),
        group(
            "exposure",
            "Exposure",
            vec![
                choice(
                    EXPOSURE_MODE,
                    "Exposure mode",
                    &[("manual", "Manual"), ("aperture_priority", "Auto")],
                    "aperture_priority",
                    "aperture_priority",
                ),
                depends(
                    with_unit(
                        int(
                            "camera.exposure.exposure_time_absolute",
                            "Exposure time",
                            (3, 2047, 1),
                            156,
                            1250,
                        ),
                        "\u{d7}100 \u{b5}s",
                    ),
                    EXPOSURE_MODE,
                    ConditionOp::In,
                    SettingValue::List(vec![
                        SettingValue::Str("manual".into()),
                        SettingValue::Str("shutter_priority".into()),
                    ]),
                ),
                int("camera.exposure.gain", "Gain", (0, 255, 1), 0, 0),
                switch(
                    "camera.exposure.keep_framerate",
                    "Keep 30 fps in low light",
                    true,
                    false,
                ),
                choice(
                    "camera.exposure.power_line_frequency",
                    "Anti-flicker",
                    &[("disabled", "Off"), ("50hz", "50 Hz"), ("60hz", "60 Hz")],
                    "60hz",
                    "60hz",
                ),
            ],
        ),
        group(
            "focus",
            "Focus & zoom",
            vec![
                switch(AF, "Continuous autofocus", true, true),
                depends(
                    int("camera.focus.focus_absolute", "Focus", (0, 255, 5), 0, 40),
                    AF,
                    ConditionOp::Eq,
                    SettingValue::Bool(false),
                ),
                int(
                    "camera.focus.zoom_absolute",
                    "Zoom",
                    (100, 500, 1),
                    100,
                    100,
                ),
                int(
                    "camera.focus.pan_absolute",
                    "Pan",
                    (-36000, 36000, 3600),
                    0,
                    14400,
                ),
                int(
                    "camera.focus.tilt_absolute",
                    "Tilt",
                    (-36000, 36000, 3600),
                    0,
                    -14400,
                ),
                setting(
                    "camera.focus.pan_reset",
                    "Recentre pan",
                    SettingKind::Action,
                    None,
                ),
                choice(
                    "camera.focus.field_of_view",
                    "Field of view",
                    &[("90", "90\u{b0}"), ("78", "78\u{b0}"), ("65", "65\u{b0}")],
                    "90",
                    "65",
                ),
            ],
        ),
    ]
}

impl Mock {
    fn new() -> Self {
        Mock {
            enabled: true,
            state: State::Idle,
            error: String::new(),
            warning: String::new(),
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
            pipewire_consumers: 0,
            pipewire_output: "off".into(),
            effect: "none".into(),
            blur_strength: 0.5,
            background_image: String::new(),
            denoise: false,
            calls: Vec::new(),
            modules: Vec::new(),
            modules_serial: 1,
            order: Vec::new(),
            profile: "quality".into(),
            leases: std::collections::BTreeMap::new(),
            preview_path: PREVIEW_PATH.into(),
            auto_start: Vec::new(),
            controls: mock_controls(),
            rename_on_get: None,
            fail_gets: false,
            settings: mock_settings(),
            global: Vec::new(),
            settings_revision: 1,
            settings_serial: 1,
            fail_next_set: None,
            config: mock_config(),
            config_serial: 1,
            config_files: vec![
                obc_ipc::ConfigFileEntry {
                    path: "config.toml".into(),
                    status: "loaded".into(),
                    managed: false,
                    reason: String::new(),
                },
                obc_ipc::ConfigFileEntry {
                    path: "config.d/50-panel.toml".into(),
                    status: "absent".into(),
                    managed: true,
                    reason: String::new(),
                },
            ],
            mic: MicStatus::default(),
            mic_refusal: None,
            media: [
                (
                    "background".to_string(),
                    vec![media_item("background", "dusk.webp", "Dusk", true)],
                ),
                ("away".to_string(), Vec::new()),
            ]
            .into(),
            media_serial: 1,
            media_user: USER_MEDIA.into(),
            media_videos: USER_VIDEOS.into(),
            stage_values: Vec::new(),
            camera_health: obc_ipc::CameraHealth {
                camera: "usb-1234_abcd".into(),
                nominal_fps: 30.0,
                ..obc_ipc::CameraHealth::none()
            },
            revision: INTERFACE_REVISION,
        }
    }

    /// Announces every microphone property.
    async fn mic_changed(&self, emitter: &SignalEmitter<'_>) -> zbus::Result<()> {
        self.mic_consumers_changed(emitter).await?;
        self.mic_provider_changed(emitter).await?;
        self.mic_latency_ms_changed(emitter).await?;
        self.mic_error_changed(emitter).await?;
        self.mic_state_changed(emitter).await
    }

    /// The camera a settings method's `options` name (`""`: the active
    /// one): `None` for no camera at all.
    fn settings_camera(&self, options: &VariantDict) -> Result<Option<usize>, IpcError> {
        let camera = camera_option(options)?;
        if camera.is_empty() && self.cameras.is_empty() {
            return Ok(None);
        }
        self.controls_of(&camera).map(Some)
    }

    fn settings_tree(&self, at: Option<usize>) -> SettingsTree {
        SettingsTree {
            revision: self.settings_revision,
            serial: self.settings_serial,
            camera: at.map(|i| self.cameras[i].id.clone()).unwrap_or_default(),
            groups: if at == Some(0) {
                self.settings.clone()
            } else {
                Vec::new()
            }
            .into_iter()
            .chain(self.global.iter().cloned())
            .collect(),
        }
    }

    fn all_settings(&self) -> impl Iterator<Item = &Setting> {
        self.settings
            .iter()
            .chain(&self.global)
            .flat_map(|g| g.settings.iter())
    }

    fn setting_mut(&mut self, id: &str) -> Option<&mut Setting> {
        self.settings
            .iter_mut()
            .chain(self.global.iter_mut())
            .flat_map(|g| g.settings.iter_mut())
            .find(|s| s.id == id)
    }

    /// Recomputes `enabled` from the `enabled_when` conditions over the
    /// current values, as the daemon does; returns the ids that flipped.
    fn apply_dependencies(&mut self) -> Vec<String> {
        let values: std::collections::HashMap<String, SettingValue> = self
            .all_settings()
            .filter_map(|s| s.kind.value().map(|v| (s.id.clone(), v)))
            .collect();
        let mut flipped = Vec::new();
        for s in self
            .settings
            .iter_mut()
            .chain(self.global.iter_mut())
            .flat_map(|g| g.settings.iter_mut())
        {
            if s.enabled_when.is_empty() {
                continue;
            }
            let on = s.enabled_when.iter().all(|c| {
                let Some(v) = values.get(&c.id) else {
                    return true;
                };
                match (&c.op, &c.value) {
                    (ConditionOp::Eq, want) => v == want,
                    (ConditionOp::Ne, want) => v != want,
                    (ConditionOp::In, SettingValue::List(any)) => any.contains(v),
                    _ => true,
                }
            });
            if on != s.enabled {
                s.enabled = on;
                s.disabled_reason = if on {
                    String::new()
                } else {
                    reason_of(&s.id).into()
                };
                s.disabled_kind = if on { "" } else { "parent_off" }.into();
                flipped.push(s.id.clone());
            }
        }
        flipped
    }

    /// Bumps the revision and announces the changes of `ids` in one
    /// signal.
    async fn publish(&mut self, ids: &[String], emitter: &SignalEmitter<'_>) -> zbus::Result<()> {
        self.settings_revision += 1;
        let changes: Vec<WireChange> = ids
            .iter()
            .filter_map(|id| self.all_settings().find(|s| s.id == *id))
            .map(|s| {
                SettingChange {
                    id: s.id.clone(),
                    value: s.kind.value(),
                    stored: s.stored,
                    enabled: s.enabled,
                    disabled_reason: s.disabled_reason.clone(),
                    disabled_kind: s.disabled_kind.clone(),
                }
                .to_wire()
            })
            .collect();
        Mock::settings_changed(
            emitter,
            self.settings_revision,
            self.settings_serial,
            changes,
        )
        .await?;
        self.settings_revision_changed(emitter).await
    }

    /// A structural change (another camera): the serial and the revision
    /// move, and the signal carries no change.
    async fn restructure(&mut self, emitter: &SignalEmitter<'_>) -> zbus::Result<()> {
        self.settings_serial += 1;
        self.settings_revision += 1;
        Mock::settings_changed(
            emitter,
            self.settings_revision,
            self.settings_serial,
            Vec::new(),
        )
        .await?;
        self.settings_revision_changed(emitter).await?;
        self.settings_schema_serial_changed(emitter).await
    }

    /// The camera a control method names (`""`: the active one), as the
    /// daemon resolves it.
    fn controls_of(&self, camera: &str) -> Result<usize, IpcError> {
        if camera.is_empty() {
            if self.cameras.is_empty() {
                return Err(IpcError::Unavailable("no camera present".into()));
            }
            return Ok(self.active);
        }
        self.cameras
            .iter()
            .position(|c| c.id == camera)
            .ok_or_else(|| IpcError::NoSuchCamera(format!("no camera {camera:?} is present")))
    }

    /// The controls of camera `at`.
    fn controls_at(&self, at: usize) -> Vec<CameraControl> {
        if at == 0 {
            self.controls.clone()
        } else {
            Vec::new()
        }
    }
}

/// Where the mock says its preview frames are.
const PREVIEW_PATH: &str = "/run/user/1000/omarchy-broadcast/preview.jpg";

/// Whether the call `header` belongs to may start the daemon (it lacks
/// `NO_AUTO_START`).
fn may_auto_start(header: &zbus::message::Header<'_>) -> bool {
    !header
        .primary()
        .flags()
        .contains(zbus::message::Flags::NO_AUTO_START)
}

#[interface(name = "br.com.dias.OmarchyBroadcast1")]
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
                let other = i != self.active;
                self.active = i;
                self.active_camera_changed(&emitter).await?;
                if other {
                    self.restructure(&emitter).await?;
                }
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
        self.restructure(&emitter).await?;
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

    async fn set_denoise(
        &mut self,
        denoise: bool,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        self.calls.push(format!("SetDenoise({denoise})"));
        if self.denoise != denoise {
            self.denoise = denoise;
            self.denoise_changed(&emitter).await?;
        }
        Ok(())
    }

    async fn acquire_preview(
        &mut self,
        fps: u32,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<String, IpcError> {
        self.calls.push(format!("AcquirePreview({fps})"));
        if may_auto_start(&header) {
            self.auto_start.push(format!("AcquirePreview({fps})"));
        }
        if fps != 15 && fps != 30 {
            return Err(IpcError::InvalidArgument(format!("bad rate {fps}")));
        }
        let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();
        self.leases.insert(sender, fps);
        self.preview_consumers_changed(&emitter).await?;
        Ok(self.preview_path.clone())
    }

    async fn release_preview(
        &mut self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        self.calls.push("ReleasePreview".into());
        if may_auto_start(&header) {
            self.auto_start.push("ReleasePreview".into());
        }
        let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();
        if self.leases.remove(&sender).is_some() {
            self.preview_consumers_changed(&emitter).await?;
        }
        Ok(())
    }

    async fn get_camera_controls(
        &mut self,
        camera: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<Vec<CameraControl>, IpcError> {
        self.calls.push(format!("GetCameraControls({camera})"));
        if may_auto_start(&header) {
            self.auto_start.push(format!("GetCameraControls({camera})"));
        }
        if !camera.is_empty()
            && let Some(id) = self.rename_on_get.take()
        {
            self.cameras[0].id = id;
        }
        if self.fail_gets {
            return Err(IpcError::Failed("the device cannot be read".into()));
        }
        let at = self.controls_of(camera)?;
        Ok(self.controls_at(at))
    }

    /// Checks the name, the range and the step, as the daemon does.
    async fn set_camera_control(
        &mut self,
        camera: &str,
        name: &str,
        value: i32,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        self.calls
            .push(format!("SetCameraControl({camera},{name},{value})"));
        let at = self.controls_of(camera)?;
        let Some(c) = self.controls.iter_mut().find(|c| c.name == name && at == 0) else {
            return Err(IpcError::InvalidArgument(format!(
                "the camera has no control {name:?}"
            )));
        };
        if !(c.min..=c.max).contains(&value) {
            return Err(IpcError::InvalidArgument(format!(
                "{name} {value} is outside its range {}..={}",
                c.min, c.max
            )));
        }
        if (value - c.min) % c.step != 0 {
            return Err(IpcError::InvalidArgument(format!(
                "{name} {value} is not on its step of {} from {}",
                c.step, c.min
            )));
        }
        c.value = value;
        c.stored = true;
        self.camera_controls_changed(&emitter).await?;
        Ok(())
    }

    async fn reset_camera_controls(
        &mut self,
        camera: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        self.calls.push(format!("ResetCameraControls({camera})"));
        self.controls_of(camera)?;
        for c in &mut self.controls {
            c.value = c.default;
            c.stored = false;
        }
        self.camera_controls_changed(&emitter).await?;
        Ok(())
    }

    // The daemon's four named out args, so introspection matches it: the
    // tuple is spelled out (the macro does not see through the
    // `SettingsReply` alias, and would advertise one struct).
    #[zbus(out_args("revision", "serial", "camera", "groups"))]
    async fn get_settings(
        &mut self,
        options: VariantDict,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<(u32, u32, String, Vec<VariantDict>), IpcError> {
        let camera = camera_option(&options)?;
        self.calls.push(format!("GetSettings({camera})"));
        if may_auto_start(&header) {
            self.auto_start.push(format!("GetSettings({camera})"));
        }
        let at = self.settings_camera(&options)?;
        Ok(self.settings_tree(at).to_wire())
    }

    /// Checks as the daemon does, in its order: the id, whether the setting
    /// is enabled, the value's type, range, step or option.
    async fn set_setting(
        &mut self,
        id: String,
        value: OwnedVariant,
        options: VariantDict,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        let camera = camera_option(&options)?;
        let value = SettingValue::from_variant(&value);
        let shown = value.as_ref().map_or("?".into(), |v| format!("{v:?}"));
        self.calls
            .push(format!("SetSetting({camera},{id},{shown})"));
        // A global setting does not look at the camera.
        let global = self
            .global
            .iter()
            .any(|g| g.settings.iter().any(|s| s.id == id));
        let at = if global {
            Some(0)
        } else {
            self.settings_camera(&options)?
        };
        let unknown = || IpcError::UnknownSetting(format!("the camera has no setting {id:?}"));
        if at != Some(0) {
            return Err(unknown());
        }
        let Some(s) = self.setting_mut(&id) else {
            return Err(unknown());
        };
        if !s.enabled {
            return Err(IpcError::Disabled(s.disabled_reason.clone()));
        }
        let invalid = |why: String| Err(IpcError::InvalidArgument(format!("{id}: {why}")));
        match (&s.kind, &value) {
            (SettingKind::Int { min, max, step, .. }, Some(SettingValue::Int(v))) => {
                if !(*min..=*max).contains(v) {
                    return invalid(format!("{v} is outside {min}..={max}"));
                }
                if (v - min) % step != 0 {
                    return invalid(format!("{v} is not on its step of {step} from {min}"));
                }
            }
            (SettingKind::Bool { .. }, Some(SettingValue::Bool(_))) => {}
            (SettingKind::Enum { options, .. }, Some(SettingValue::Str(v))) => {
                if !options.iter().any(|(o, _)| o == v) {
                    return invalid(format!("{v:?} is not one of its options"));
                }
            }
            (SettingKind::Action, Some(SettingValue::Bool(true))) => {}
            _ => return invalid("a value of the wrong type".into()),
        }
        match self.fail_next_set.take() {
            Some("RateLimited") => {
                return Err(IpcError::RateLimited(
                    "too many changes to the camera: try again in a moment".into(),
                ));
            }
            Some(_) => return Err(unknown()),
            None => {}
        }
        let s = self.setting_mut(&id).unwrap();
        match (&mut s.kind, value) {
            (SettingKind::Int { value, .. }, Some(SettingValue::Int(v))) => *value = v,
            (SettingKind::Bool { value, .. }, Some(SettingValue::Bool(v))) => *value = v,
            (SettingKind::Enum { value, .. }, Some(SettingValue::Str(v))) => *value = v,
            _ => {}
        }
        if !matches!(s.kind, SettingKind::Action) {
            s.stored = true;
        }
        // A dependency flip travels in the same signal as its cause.
        let mut ids = vec![id.clone()];
        ids.extend(self.apply_dependencies());
        self.publish(&ids, &emitter).await?;
        Ok(())
    }

    async fn reset_settings(
        &mut self,
        scope: String,
        options: VariantDict,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        let camera = camera_option(&options)?;
        self.calls.push(format!("ResetSettings({scope},{camera})"));
        let Some(parsed) = ResetScope::parse(&scope) else {
            return Err(IpcError::InvalidArgument(format!("no scope {scope:?}")));
        };
        // A global group or setting does not look at the camera; the camera
        // scope never touches them.
        let global = match &parsed {
            ResetScope::Camera => false,
            ResetScope::Group(id) => self.global.iter().any(|g| g.id == *id),
            ResetScope::Setting(id) => self
                .global
                .iter()
                .any(|g| g.settings.iter().any(|s| s.id == *id)),
        };
        let at = if global {
            None
        } else {
            self.settings_camera(&options)?
        };
        let groups = if global {
            self.global.as_slice()
        } else if at == Some(0) {
            self.settings.as_slice()
        } else {
            &[]
        };
        match &parsed {
            ResetScope::Group(g) if !groups.iter().any(|x| x.id == *g) => {
                return Err(IpcError::InvalidArgument(format!("no group {g:?}")));
            }
            ResetScope::Setting(id)
                if !groups.iter().flat_map(|g| &g.settings).any(|s| s.id == *id) =>
            {
                return Err(IpcError::UnknownSetting(format!(
                    "the camera has no setting {id:?}"
                )));
            }
            _ => {}
        }
        let mut ids = Vec::new();
        if at == Some(0) || global {
            let groups = if global {
                &mut self.global
            } else {
                &mut self.settings
            };
            for g in groups {
                for s in &mut g.settings {
                    let hit = match &parsed {
                        ResetScope::Camera => true,
                        ResetScope::Group(id) => g.id == *id,
                        ResetScope::Setting(id) => s.id == *id,
                    };
                    if !hit {
                        continue;
                    }
                    match &mut s.kind {
                        SettingKind::Int { default, value, .. } => *value = *default,
                        SettingKind::Bool { default, value } => *value = *default,
                        SettingKind::Enum { default, value, .. } => *value = default.clone(),
                        SettingKind::Action | SettingKind::Unknown(_) => {}
                    }
                    s.stored = false;
                    ids.push(s.id.clone());
                }
            }
        }
        ids.extend(self.apply_dependencies());
        self.publish(&ids, &emitter).await?;
        Ok(())
    }

    #[zbus(signal)]
    async fn settings_changed(
        emitter: &SignalEmitter<'_>,
        revision: u32,
        serial: u32,
        changes: Vec<WireChange>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn stats(
        emitter: &SignalEmitter<'_>,
        fps: f64,
        latency_p50_ms: f64,
        cpu_percent: f64,
        gpu_percent: f64,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn preview_frame(emitter: &SignalEmitter<'_>, seq: u32) -> zbus::Result<()>;

    #[zbus(property)]
    fn state(&self) -> State {
        self.state
    }

    #[zbus(property)]
    fn error(&self) -> String {
        self.error.clone()
    }

    #[zbus(property)]
    fn warning(&self) -> String {
        self.warning.clone()
    }

    #[zbus(property)]
    fn stage_values(&self) -> Vec<obc_ipc::StageValue> {
        self.stage_values.clone()
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
    fn denoise(&self) -> bool {
        self.denoise
    }

    #[zbus(property)]
    fn consumers(&self) -> u32 {
        self.consumers + self.pipewire_consumers + self.preview_consumers()
    }

    #[zbus(property)]
    fn loopback_consumers(&self) -> u32 {
        self.consumers
    }

    #[zbus(property)]
    fn preview_consumers(&self) -> u32 {
        u32::try_from(self.leases.len()).unwrap()
    }

    #[zbus(property, name = "PipeWireConsumers")]
    fn pipewire_consumers(&self) -> u32 {
        self.pipewire_consumers
    }

    #[zbus(property, name = "PipeWireOutput")]
    fn pipewire_output(&self) -> String {
        self.pipewire_output.clone()
    }

    #[zbus(property)]
    fn camera_controls(&self) -> CameraControls {
        match self.cameras.get(self.active) {
            Some(c) => CameraControls {
                camera: c.id.clone(),
                controls: self.controls_at(self.active),
            },
            None => CameraControls::default(),
        }
    }

    #[zbus(property)]
    fn settings_revision(&self) -> u32 {
        self.settings_revision
    }

    #[zbus(property)]
    fn settings_schema_serial(&self) -> u32 {
        self.settings_serial
    }

    #[zbus(property)]
    fn modules_serial(&self) -> u32 {
        self.modules_serial
    }

    #[zbus(property)]
    fn backend_profile(&self) -> String {
        self.profile.clone()
    }

    async fn get_modules(&mut self) -> (u32, Vec<VariantDict>) {
        self.calls.push("GetModules()".into());
        obc_ipc::ModuleList {
            serial: self.modules_serial,
            modules: self.modules.clone(),
        }
        .to_wire()
    }

    async fn set_module_enabled(&mut self, id: String, enabled: bool) -> Result<(), IpcError> {
        self.calls
            .push(format!("SetModuleEnabled({id}, {enabled})"));
        let m = self
            .modules
            .iter_mut()
            .find(|m| m.id == id)
            .ok_or_else(|| IpcError::InvalidArgument(format!("No module {id} is installed")))?;
        m.enabled = enabled;
        m.status = if enabled { "available" } else { "disabled" }.into();
        m.reason = if enabled { "" } else { "Disabled" }.into();
        self.modules_serial += 1;
        Ok(())
    }

    async fn get_stage_order(&mut self) -> Vec<String> {
        self.calls.push("GetStageOrder()".into());
        self.order.clone()
    }

    async fn set_stage_order(&mut self, order: Vec<String>) -> Result<(), IpcError> {
        self.calls
            .push(format!("SetStageOrder({})", order.join(",")));
        if order.is_empty() {
            self.order = vec!["denoise".into(), "background".into(), "relight".into()];
            return Ok(());
        }
        if order.first().map(String::as_str) == Some("relight") {
            return Err(IpcError::InvalidArgument(
                "relight (post-matte) cannot come before background (matte)".into(),
            ));
        }
        self.order = order;
        Ok(())
    }

    async fn set_backend_profile(&mut self, profile: String) -> Result<(), IpcError> {
        self.calls.push(format!("SetBackendProfile({profile})"));
        self.profile = profile;
        Ok(())
    }

    async fn set_stage_pin(&mut self, stage: String, module: String) -> Result<(), IpcError> {
        self.calls.push(format!("SetStagePin({stage}, {module})"));
        for m in &mut self.modules {
            for s in &mut m.stages {
                if s.id == stage {
                    s.selected = module.is_empty() || m.id == module;
                }
            }
        }
        Ok(())
    }

    async fn get_stage_order_moves(&mut self) -> Vec<(String, String, bool, bool)> {
        self.calls.push("GetStageOrderMoves()".into());
        // The mock's one rule: relight never goes before background.
        let ok = |a: &str, b: &str| !(a == "background" && b == "relight");
        let n = self.order.len();
        (0..n)
            .map(|i| {
                let id = self.order[i].clone();
                let slot = match id.as_str() {
                    "background" => "matte",
                    "relight" => "post-matte",
                    _ => "pre-matte",
                };
                let up = i > 0 && ok(&self.order[i - 1], &id);
                let down = i + 1 < n && ok(&id, &self.order[i + 1]);
                (id, slot.to_string(), up, down)
            })
            .collect()
    }

    #[zbus(out_args("serial", "groups"))]
    async fn get_config_schema(&mut self) -> (u32, Vec<VariantDict>) {
        self.calls.push("GetConfigSchema()".into());
        obc_ipc::ConfigSchema {
            serial: self.config_serial,
            groups: self.config.clone(),
        }
        .to_wire()
    }

    async fn set_config(
        &mut self,
        key: String,
        value: OwnedVariant,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        let value = obc_ipc::ConfigValue::from_variant(&value);
        self.calls.push(format!("SetConfig({key}, {value:?})"));
        let row = self.config_row(&key)?;
        let value = value.ok_or_else(|| IpcError::InvalidArgument("bad type".into()))?;
        let fits = matches!(
            (&row.kind, &value),
            (obc_ipc::ConfigKind::Bool, obc_ipc::ConfigValue::Bool(_))
                | (
                    obc_ipc::ConfigKind::Int { .. },
                    obc_ipc::ConfigValue::Int(_)
                )
                | (
                    obc_ipc::ConfigKind::Float { .. },
                    obc_ipc::ConfigValue::Float(_)
                )
                | (
                    obc_ipc::ConfigKind::Enum { .. },
                    obc_ipc::ConfigValue::Str(_)
                )
                | (obc_ipc::ConfigKind::List, obc_ipc::ConfigValue::List(_))
                | (obc_ipc::ConfigKind::Path, obc_ipc::ConfigValue::Str(_))
        );
        if !fits {
            return Err(IpcError::InvalidArgument(format!("{key}: wrong type")));
        }
        if let (obc_ipc::ConfigKind::Enum { options }, obc_ipc::ConfigValue::Str(v)) =
            (&row.kind, &value)
            && !options.iter().any(|(o, _)| o == v)
        {
            return Err(IpcError::InvalidArgument(format!(
                "{key}: {v:?} is not one of its options"
            )));
        }
        if let (Some(id), obc_ipc::ConfigValue::Bool(b)) =
            (key.strip_prefix("modules.forced."), &value)
            && let Some(m) = self.modules.iter_mut().find(|m| m.id == id)
        {
            m.forced = *b;
        }
        let row = self.config_row(&key)?;
        row.value = value;
        row.source = "config.d/50-panel.toml".into();
        self.config_changed_now(&emitter).await;
        Ok(())
    }

    async fn reset_config(
        &mut self,
        key: String,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        self.calls.push(format!("ResetConfig({key})"));
        let row = self.config_row(&key)?;
        if let Some(d) = row.default.clone() {
            row.value = d;
        }
        row.source = String::new();
        self.config_changed_now(&emitter).await;
        Ok(())
    }

    #[zbus(out_args("serial", "dir", "files"))]
    async fn get_config_files(&mut self) -> (u32, String, Vec<(String, String, bool, String)>) {
        self.calls.push("GetConfigFiles()".into());
        obc_ipc::ConfigFiles {
            serial: self.config_serial,
            dir: "/home/u/.config/omarchy-broadcast".into(),
            files: self.config_files.clone(),
        }
        .to_wire()
    }

    #[zbus(signal)]
    async fn config_changed(emitter: &SignalEmitter<'_>, serial: u32) -> zbus::Result<()>;

    #[zbus(property)]
    fn config_serial(&self) -> u32 {
        self.config_serial
    }

    async fn set_mic_host(
        &mut self,
        on: bool,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        self.calls.push(format!("SetMicHost({on})"));
        if may_auto_start(&header) {
            self.auto_start.push(format!("SetMicHost({on})"));
        }
        if let Some(m) = &self.mic_refusal {
            return Err(IpcError::Unavailable(m.clone()));
        }
        self.mic = if on {
            MicStatus {
                state: "passthrough".into(),
                ..MicStatus::default()
            }
        } else {
            MicStatus::default()
        };
        self.mic_changed(&emitter).await?;
        Ok(())
    }

    #[zbus(property)]
    fn mic_state(&self) -> String {
        self.mic.state.clone()
    }

    #[zbus(property)]
    fn mic_consumers(&self) -> u32 {
        self.mic.consumers
    }

    #[zbus(property)]
    fn mic_provider(&self) -> String {
        self.mic.provider.clone()
    }

    #[zbus(property)]
    fn mic_latency_ms(&self) -> f64 {
        self.mic.latency_ms
    }

    #[zbus(property)]
    fn mic_error(&self) -> String {
        self.mic.error.clone()
    }

    #[zbus(out_args("serial", "bundled_dir", "user_dir", "items"))]
    async fn get_media(
        &mut self,
        kind: String,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<(u32, String, String, Vec<VariantDict>), IpcError> {
        self.calls.push(format!("GetMedia({kind})"));
        if may_auto_start(&header) {
            self.auto_start.push(format!("GetMedia({kind})"));
        }
        let items = self
            .media
            .get(&kind)
            .ok_or_else(|| IpcError::InvalidArgument(format!("unknown media kind {kind:?}")))?;
        let (bundled_dir, user_dir) = media_dirs(&kind, &self.media_user);
        Ok(obc_ipc::MediaReply {
            serial: self.media_serial,
            bundled_dir,
            user_dir,
            items: items.clone(),
        }
        .to_wire())
    }

    /// As the daemon: the user directory `GetMedia` gives and, for a
    /// background, the videos folder (`""` for away).
    async fn get_media_dirs(
        &mut self,
        kind: String,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<VariantDict, IpcError> {
        self.calls.push(format!("GetMediaDirs({kind})"));
        if may_auto_start(&header) {
            self.auto_start.push(format!("GetMediaDirs({kind})"));
        }
        if !self.media.contains_key(&kind) {
            return Err(IpcError::InvalidArgument(format!(
                "unknown media kind {kind:?}"
            )));
        }
        let (_, user_dir) = media_dirs(&kind, &self.media_user);
        let video_dir = if kind == "background" {
            self.media_videos.clone()
        } else {
            String::new()
        };
        Ok(obc_ipc::MediaDirs {
            user_dir,
            video_dir,
        }
        .to_dict())
    }

    /// Stores the file under its name in the user's directory; a `.txt`
    /// stands for a file that is not an image.
    async fn import_media(
        &mut self,
        kind: String,
        path: String,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<String, IpcError> {
        self.calls.push(format!("ImportMedia({kind}, {path})"));
        let Some(items) = self.media.get_mut(&kind) else {
            return Err(IpcError::InvalidArgument(format!(
                "unknown media kind {kind:?}"
            )));
        };
        if path.ends_with(".txt") {
            return Err(IpcError::InvalidArgument(format!(
                "cannot import {path}: not a PNG, JPEG or WebP image"
            )));
        }
        let name = Path::new(&path).file_name().unwrap().to_string_lossy();
        let item = media_item_in(&self.media_user, &kind, &name, "Imported", false);
        let stored = item.path.clone();
        if let Some(dir) = Path::new(&stored).parent().filter(|d| d.is_dir()) {
            std::fs::write(dir.join(&*name), b"").unwrap();
        }
        if !items.iter().any(|i| i.path == stored) {
            items.push(item);
            self.media_serial += 1;
            self.media_serial_changed(&emitter).await?;
        }
        Ok(stored)
    }

    /// As the daemon: a user item only; the background in force cleared
    /// first, refused while `replace` is selected.
    async fn remove_media(
        &mut self,
        path: String,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        self.calls.push(format!("RemoveMedia({path})"));
        let found = self.media.iter().find_map(|(kind, items)| {
            let at = items.iter().position(|i| i.path == path && !i.bundled)?;
            Some((kind.clone(), at))
        });
        let Some((kind, at)) = found else {
            return Err(IpcError::InvalidArgument(format!(
                "{path} is not an item of the user's media gallery"
            )));
        };
        if self.background_image == path && self.effect == "replace" {
            return Err(IpcError::InvalidArgument(
                "the replace effect is selected and needs its background image: select \
                 another effect before clearing it"
                    .into(),
            ));
        }
        self.media.get_mut(&kind).unwrap().remove(at);
        if self.background_image == path {
            self.background_image.clear();
            self.background_image_changed(&emitter).await?;
        }
        self.media_serial += 1;
        self.media_serial_changed(&emitter).await?;
        Ok(())
    }

    /// As the daemon, by name: a `.webm` already plays without the GPU,
    /// another video (`.mp4`, `.mkv`, `.mov`) gets a command converting it
    /// into the user's gallery, anything else is not a video.
    async fn get_convert_command(&mut self, path: String) -> Result<String, IpcError> {
        self.calls.push(format!("GetConvertCommand({path})"));
        let p = std::path::Path::new(&path);
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or_default();
        match ext {
            "webm" => Err(IpcError::InvalidArgument(format!(
                "the video file {path} already plays without an NVIDIA GPU: there is nothing \
                 to convert"
            ))),
            "mp4" | "mkv" | "mov" => {
                let stem = p.file_stem().unwrap().to_str().unwrap();
                Ok(format!(
                    "ffmpeg -nostdin -n -protocol_whitelist file -i '{path}' -f webm \
                     '{}/{stem}-cpu.webm'",
                    self.media_user
                ))
            }
            _ => Err(IpcError::InvalidArgument(format!(
                "the video file {path}: not an MP4, MOV, MKV or WebM file"
            ))),
        }
    }

    #[zbus(property)]
    fn media_serial(&self) -> u32 {
        self.media_serial
    }

    #[zbus(property)]
    fn interface_revision(&self) -> u32 {
        self.revision
    }

    #[zbus(property)]
    fn camera_health(&self) -> VariantDict {
        self.camera_health.to_dict()
    }

    /// As the daemon: `InvalidArgument` without a row; otherwise nothing
    /// differs from the recommendation any more, announced before the
    /// reply.
    async fn apply_recommended_settings(
        &mut self,
        camera: String,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        self.calls
            .push(format!("ApplyRecommendedSettings({camera})"));
        if !self.camera_health.has_recommendation {
            return Err(IpcError::InvalidArgument(
                "First Camera (usb-1234_abcd) has no recommended settings".into(),
            ));
        }
        self.camera_health.recommended.clear();
        self.camera_health_changed(&emitter).await?;
        Ok(())
    }
}

impl Mock {
    /// The row `key`, writable, or the daemon's refusal.
    fn config_row(&mut self, key: &str) -> Result<&mut obc_ipc::ConfigRow, IpcError> {
        let row = self
            .config
            .iter_mut()
            .flat_map(|g| g.rows.iter_mut())
            .find(|r| r.id == key)
            .ok_or_else(|| IpcError::UnknownSetting(format!("No configuration key {key}")))?;
        if !row.overridden_by.is_empty() {
            return Err(IpcError::Disabled(format!(
                "Overridden by {}",
                row.overridden_by
            )));
        }
        if !row.editable {
            return Err(IpcError::Disabled(format!(
                "{key} cannot be changed here: set it in config.toml"
            )));
        }
        Ok(row)
    }

    /// `ConfigSerial` moves: the property, then `ConfigChanged`.
    async fn config_changed_now(&mut self, emitter: &SignalEmitter<'_>) {
        self.config_serial += 1;
        self.config_serial_changed(emitter).await.unwrap();
        Mock::config_changed(emitter, self.config_serial)
            .await
            .unwrap();
    }
}

fn config_row(
    id: &str,
    label: &str,
    kind: obc_ipc::ConfigKind,
    value: obc_ipc::ConfigValue,
) -> obc_ipc::ConfigRow {
    obc_ipc::ConfigRow {
        id: id.into(),
        label: label.into(),
        description: format!("Help for {id}"),
        kind,
        default: Some(value.clone()),
        value,
        editable: true,
        source: String::new(),
        overridden_by: String::new(),
        apply: "reload".into(),
        enabled: true,
        disabled_reason: String::new(),
        notice: String::new(),
    }
}

/// The mock's config schema: a few rows of each kind.
fn mock_config() -> Vec<obc_ipc::ConfigGroup> {
    use obc_ipc::{ConfigKind as K, ConfigValue as V};
    let mut mode = config_row(
        "output.mode",
        "Output mode",
        K::String,
        V::Str("1920x1080@30".into()),
    );
    mode.editable = false;
    mode.apply = "restart".into();
    let mut grace = config_row(
        "lifecycle.idle_grace_s",
        "Idle grace",
        K::Int {
            min: 0,
            max: 3600,
            step: 1,
            unit: Some("s".into()),
        },
        V::Int(5),
    );
    grace.source = "config.toml".into();
    grace.apply = "live".into();
    let hold = config_row(
        "output.hold_timeout_s",
        "Hold timeout",
        K::Float {
            min: 0.0,
            max: 60.0,
            step: 0.5,
            unit: Some("s".into()),
        },
        V::Float(2.0),
    );
    let profile = config_row(
        "modules.profile",
        "Backend profile",
        K::Enum {
            options: vec![
                ("quality".into(), "Quality".into()),
                ("battery".into(), "Battery".into()),
            ],
        },
        V::Str("quality".into()),
    );
    let enabled = config_row(
        "modules.enabled.br.com.dias.maxine-vfx",
        "NVIDIA Maxine video effects",
        K::Bool,
        V::Bool(true),
    );
    let mut order = config_row("modules.order", "Stage order", K::List, V::List(Vec::new()));
    order.editable = false;
    let group = |id: &str, label: &str, rows| obc_ipc::ConfigGroup {
        id: id.into(),
        label: label.into(),
        rows,
    };
    vec![
        group("output", "Output", vec![mode, hold]),
        group("lifecycle", "Lifecycle", vec![grace]),
        group("modules", "Modules", vec![profile, enabled, order]),
    ]
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

    /// The mock's introspection data, read over the bus by another
    /// connection.
    fn introspect(&self, bus: &Bus) -> String {
        self.rt.block_on(async {
            let client = connection::Builder::address(bus.address.as_str())
                .build()
                .await
                .unwrap();
            zbus::fdo::IntrospectableProxy::builder(&client)
                .destination(BUS_NAME)
                .path(OBJECT_PATH)
                .build()
                .await
                .unwrap()
                .introspect()
                .await
                .unwrap()
        })
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

    /// `n` processes hold the loopback: the total and the loopback count
    /// change, each announced on its own, as the daemon does.
    fn set_consumers(&self, n: u32) {
        let iface = self.iface();
        self.rt.block_on(async {
            iface.get_mut().await.consumers = n;
            let m = iface.get().await;
            m.consumers_changed(iface.signal_emitter()).await.unwrap();
            m.loopback_consumers_changed(iface.signal_emitter())
                .await
                .unwrap();
        });
    }

    /// `StageValues` is `values`, announced.
    fn set_stage_values(&self, values: Vec<obc_ipc::StageValue>) {
        let iface = self.iface();
        self.rt.block_on(async {
            iface.get_mut().await.stage_values = values;
            let m = iface.get().await;
            m.stage_values_changed(iface.signal_emitter())
                .await
                .unwrap();
        });
    }

    /// `CameraHealth` is `h`, announced.
    fn set_camera_health(&self, h: obc_ipc::CameraHealth) {
        let iface = self.iface();
        self.rt.block_on(async {
            iface.get_mut().await.camera_health = h;
            let m = iface.get().await;
            m.camera_health_changed(iface.signal_emitter())
                .await
                .unwrap();
        });
    }

    /// `Warning` is `text`, announced.
    fn set_warning(&self, text: &str) {
        let iface = self.iface();
        self.rt.block_on(async {
            iface.get_mut().await.warning = text.into();
            let m = iface.get().await;
            m.warning_changed(iface.signal_emitter()).await.unwrap();
        });
    }

    /// The PipeWire output is in `output` with `n` links.
    fn set_pipewire(&self, n: u32, output: &str) {
        let iface = self.iface();
        self.rt.block_on(async {
            {
                let mut m = iface.get_mut().await;
                m.pipewire_consumers = n;
                m.pipewire_output = output.into();
            }
            let m = iface.get().await;
            m.consumers_changed(iface.signal_emitter()).await.unwrap();
            m.pipe_wire_consumers_changed(iface.signal_emitter())
                .await
                .unwrap();
            m.pipe_wire_output_changed(iface.signal_emitter())
                .await
                .unwrap();
        });
    }

    /// Changes the mock's fields directly (no signal).
    fn with(&self, f: impl FnOnce(&mut Mock)) {
        let iface = self.iface();
        self.rt.block_on(async {
            let mut m = iface.get_mut().await;
            f(&mut m);
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

    /// The preview leases held: `(holder, fps)`.
    fn leases(&self) -> Vec<(String, u32)> {
        let iface = self.iface();
        self.rt.block_on(async {
            iface
                .get()
                .await
                .leases
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect()
        })
    }

    /// The preview calls received without `NO_AUTO_START`.
    fn auto_start_calls(&self) -> Vec<String> {
        let iface = self.iface();
        self.rt
            .block_on(async { iface.get().await.auto_start.clone() })
    }

    /// `AcquirePreview` returns `path` from now on.
    fn set_preview_path(&self, path: &str) {
        let iface = self.iface();
        self.rt
            .block_on(async { iface.get_mut().await.preview_path = path.into() });
    }

    fn emit_frame(&self, seq: u32) {
        let iface = self.iface();
        self.rt.block_on(async {
            Mock::preview_frame(iface.signal_emitter(), seq)
                .await
                .unwrap();
        });
    }

    /// The microphone becomes `m`, announced.
    fn set_mic(&self, m: MicStatus) {
        let iface = self.iface();
        self.rt.block_on(async {
            iface.get_mut().await.mic = m;
            let m = iface.get().await;
            m.mic_changed(iface.signal_emitter()).await.unwrap();
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

    /// The first camera's controls become `controls`, announced.
    fn set_controls(&self, controls: Vec<CameraControl>) {
        let iface = self.iface();
        self.rt.block_on(async {
            iface.get_mut().await.controls = controls;
            let m = iface.get().await;
            m.camera_controls_changed(iface.signal_emitter())
                .await
                .unwrap();
        });
    }

    /// Announces `CameraControls` without changing it.
    fn announce_controls(&self) {
        let iface = self.iface();
        self.rt.block_on(async {
            let m = iface.get().await;
            m.camera_controls_changed(iface.signal_emitter())
                .await
                .unwrap();
        });
    }

    /// No camera is present any more (not announced).
    fn remove_cameras(&self) {
        let iface = self.iface();
        self.rt
            .block_on(async { iface.get_mut().await.cameras.clear() });
    }

    /// The next `GetCameraControls` for a named camera finds the first
    /// camera under the id `id` instead.
    fn rename_first_camera_on_next_get(&self, id: &str) {
        let iface = self.iface();
        self.rt
            .block_on(async { iface.get_mut().await.rename_on_get = Some(id.into()) });
    }

    /// `GetCameraControls` fails, or works again.
    fn fail_get_camera_controls(&self, fail: bool) {
        let iface = self.iface();
        self.rt
            .block_on(async { iface.get_mut().await.fail_gets = fail });
    }

    /// The first camera's id becomes `id` (not announced).
    fn set_first_camera_id(&self, id: &str) {
        let iface = self.iface();
        self.rt
            .block_on(async { iface.get_mut().await.cameras[0].id = id.into() });
    }

    /// The next valid `SetSetting` fails with `error` (`"RateLimited"`, or
    /// any other text for `UnknownSetting`).
    fn fail_next_set(&self, error: &'static str) {
        let iface = self.iface();
        self.rt
            .block_on(async { iface.get_mut().await.fail_next_set = Some(error) });
    }

    /// The settings structure changed (the serial moves), announced.
    fn restructure_settings(&self) {
        let iface = self.iface();
        self.rt.block_on(async {
            let mut m = iface.get_mut().await;
            m.restructure(iface.signal_emitter()).await.unwrap();
        });
    }

    /// The setting `id` (an int) becomes `value` and stored, announced as
    /// the daemon would; with `missed`, a revision is spent first without
    /// a signal (one the client missed).
    fn change_setting(&self, id: &str, v: i64, missed: bool) {
        let iface = self.iface();
        self.rt.block_on(async {
            let mut m = iface.get_mut().await;
            if missed {
                m.settings_revision += 1;
            }
            let s = m.setting_mut(id).unwrap();
            if let SettingKind::Int { value, .. } = &mut s.kind {
                *value = v;
            }
            s.stored = true;
            m.publish(&[id.to_string()], iface.signal_emitter())
                .await
                .unwrap();
        });
    }

    /// The global settings become `groups` (not announced).
    fn set_global(&self, groups: Vec<SettingsGroup>) {
        let iface = self.iface();
        self.rt
            .block_on(async { iface.get_mut().await.global = groups });
    }

    /// The first camera's settings become `groups` (not announced).
    fn set_settings(&self, groups: Vec<SettingsGroup>) {
        let iface = self.iface();
        self.rt
            .block_on(async { iface.get_mut().await.settings = groups });
    }

    /// `(SettingsRevision, SettingsSchemaSerial)`.
    fn settings_marks(&self) -> (u32, u32) {
        let iface = self.iface();
        self.rt.block_on(async {
            let m = iface.get().await;
            (m.settings_revision, m.settings_serial)
        })
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

#[interface(name = "br.com.dias.OmarchyBroadcast1")]
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

#[interface(name = "br.com.dias.OmarchyBroadcast1")]
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

/// A daemon of interface revision 3, the one before noise removal: every
/// member but `Denoise` and `SetDenoise` (a call to it fails with
/// `UnknownMethod`). The effect methods accept anything.
struct Rev3;

#[interface(name = "br.com.dias.OmarchyBroadcast1")]
impl Rev3 {
    async fn set_enabled(&mut self, _enabled: bool) -> Result<(), IpcError> {
        Ok(())
    }

    async fn select_camera(&mut self, _selector: &str) -> Result<(), IpcError> {
        Ok(())
    }

    async fn next_camera(&mut self) -> Result<(), IpcError> {
        Ok(())
    }

    async fn reload_config(&mut self) -> Result<(), IpcError> {
        Ok(())
    }

    async fn set_effect(&mut self, _effect: &str) -> Result<(), IpcError> {
        Ok(())
    }

    async fn set_blur_strength(&mut self, _strength: f64) -> Result<(), IpcError> {
        Ok(())
    }

    async fn set_background_image(&mut self, _path: &str) -> Result<(), IpcError> {
        Ok(())
    }

    #[zbus(signal)]
    async fn stats(
        emitter: &SignalEmitter<'_>,
        fps: f64,
        latency_ms: f64,
        cpu_percent: f64,
        gpu_percent: f64,
    ) -> zbus::Result<()>;

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
        "blur".into()
    }

    #[zbus(property)]
    fn blur_strength(&self) -> f64 {
        0.5
    }

    #[zbus(property)]
    fn background_image(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn consumers(&self) -> u32 {
        0
    }

    #[zbus(property)]
    fn loopback_consumers(&self) -> u32 {
        0
    }

    #[zbus(property, name = "PipeWireConsumers")]
    fn pipewire_consumers(&self) -> u32 {
        0
    }

    #[zbus(property, name = "PipeWireOutput")]
    fn pipewire_output(&self) -> String {
        "off".into()
    }

    #[zbus(property)]
    fn interface_revision(&self) -> u32 {
        3
    }
}

/// A daemon of interface revision 5, the one before the camera image
/// controls: no `CameraControls`, and a call to `GetCameraControls`,
/// `SetCameraControl` or `ResetCameraControls` fails with `UnknownMethod`.
/// (The members of revisions 2 to 5 that the CLI reads have their default
/// value when absent, so they are left out.)
struct Rev5;

#[interface(name = "br.com.dias.OmarchyBroadcast1")]
impl Rev5 {
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
        vec![CameraInfo {
            id: "usb-1234_abcd".into(),
            name: "First Camera".into(),
            usb: "1234:abcd".into(),
        }]
    }

    #[zbus(property)]
    fn active_camera(&self) -> String {
        "usb-1234_abcd".into()
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
        5
    }
}

/// A daemon of interface revision 15, as the CLI reads it: without
/// `StageValues` (revision 16). The members the CLI reads with a default
/// are left out, as for [`Rev6`].
struct Rev15;

#[interface(name = "br.com.dias.OmarchyBroadcast1")]
impl Rev15 {
    #[zbus(property)]
    fn state(&self) -> State {
        State::Active
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
        1
    }

    #[zbus(property)]
    fn interface_revision(&self) -> u32 {
        15
    }
}

/// A daemon of interface revision 6: the image controls but no camera
/// settings (`GetSettings`, `SetSetting`, `ResetSettings` and their
/// properties fail with `UnknownMethod` / `UnknownProperty`). As for
/// [`Rev5`], the members the CLI reads with a default are left out.
struct Rev6;

#[interface(name = "br.com.dias.OmarchyBroadcast1")]
impl Rev6 {
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
        vec![CameraInfo {
            id: "usb-1234_abcd".into(),
            name: "First Camera".into(),
            usb: "1234:abcd".into(),
        }]
    }

    #[zbus(property)]
    fn active_camera(&self) -> String {
        "usb-1234_abcd".into()
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
        6
    }
}

/// An older daemon ([`Rev1`], [`Rev3`], [`Rev5`], [`Rev6`]) served on the private bus under the
/// daemon's name.
struct OldDaemon {
    _rt: tokio::runtime::Runtime,
    _conn: Connection,
}

impl OldDaemon {
    fn start(bus: &Bus, iface: impl zbus::object_server::Interface) -> OldDaemon {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let conn = rt
            .block_on(
                connection::Builder::address(bus.address.as_str())
                    .serve_at(OBJECT_PATH, iface)
                    .name(BUS_NAME)
                    .build(),
            )
            .unwrap();
        OldDaemon {
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
         Effect:     none\n\
         Denoise:    off\n"
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
    assert_eq!(v["warning"], "");
    assert_eq!(v["active_camera"], "usb-1234_abcd");
    assert_eq!(v["active_camera_name"], "First Camera");
    assert_eq!(v["consumers"], 0);
    assert_eq!(v["loopback_consumers"], 0);
    assert_eq!(v["pipewire_consumers"], 0);
    assert_eq!(v["pipewire_output"], "off");
    assert_eq!(v["output_mode"], "NV12 1920x1080@30");
    assert_eq!(v["effect"], "none");
    assert_eq!(v["blur_strength"], 0.5);
    assert_eq!(v["background_image"], "");
    assert_eq!(v["denoise"], false);
    assert_eq!(v["interface_revision"], INTERFACE_REVISION);
    assert_eq!(v["cameras"].as_array().unwrap().len(), 2);
    assert_eq!(v["cameras_truncated"], false);
    assert!(!bus.activation_attempted());
}

#[test]
fn status_and_watch_show_the_warning() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    let text = "effect blur, noise removal unavailable: NVIDIA Maxine video effects: Disabled; \
                warm tint unavailable: Hello tint: Disabled";
    mock.set_warning(text);
    let out = bus.run(&["status"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert!(
        out.stdout.ends_with(&format!("Warning:    {text}\n")),
        "{}",
        out.stdout
    );
    let v = json_line(&bus.run(&["status", "--json"]).stdout);
    assert_eq!(
        (v["state"].as_str(), v["warning"].as_str()),
        (Some("idle"), Some(text))
    );
    assert_eq!(v["error"], "");
    let watch = bus.watch(&["watch", "--json"], None);
    let v = watch.next_json("the first snapshot");
    assert_eq!(v["warning"], text, "{v}");
    watch.settings_set("the settings after the snapshot");
    mock.set_warning("");
    let v = watch.next_json("warning");
    assert_eq!(v["type"], "state", "{v}");
    assert_eq!(v["changed"], serde_json::json!(["warning"]), "{v}");
    assert_eq!(v["warning"], "", "{v}");
}

fn zoom(value: f64) -> Vec<obc_ipc::StageValue> {
    vec![obc_ipc::StageValue {
        stage: "auto-frame".into(),
        name: "zoom".into(),
        value,
    }]
}

/// `stage_values` is always in the snapshot, `[]` when no stage reports;
/// `status` shows auto-frame's zoom, and `watch --json` prints a `state`
/// line when the values change.
#[test]
fn status_and_watch_show_the_stage_values() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    let v = json_line(&bus.run(&["status", "--json"]).stdout);
    assert_eq!(v["stage_values"], serde_json::json!([]), "{v}");
    let out = bus.run(&["status"]);
    assert!(!out.stdout.contains("zoom"), "{}", out.stdout);

    mock.set_stage_values(zoom(1.24));
    let out = bus.run(&["status"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert!(
        out.stdout
            .contains("Denoise:    off\nAuto-frame zoom: 1.24x\n"),
        "{}",
        out.stdout
    );
    let v = json_line(&bus.run(&["status", "--json"]).stdout);
    assert_eq!(
        v["stage_values"],
        serde_json::json!([{"stage": "auto-frame", "name": "zoom", "value": 1.24}]),
        "{v}"
    );

    let watch = bus.watch(&["watch", "--json"], None);
    let v = watch.next_json("the first snapshot");
    assert_eq!(v["type"], "snapshot", "{v}");
    assert_eq!(v["stage_values"][0]["value"], 1.24, "{v}");
    watch.settings_set("the settings after the snapshot");
    mock.set_stage_values(zoom(1.5));
    let v = watch.next_json("the new zoom");
    assert_eq!(v["type"], "state", "{v}");
    assert_eq!(v["changed"], serde_json::json!(["stage_values"]), "{v}");
    assert_eq!(v["stage_values"][0]["value"], 1.5, "{v}");
    mock.set_stage_values(Vec::new());
    let v = watch.next_json("no zoom");
    assert_eq!(v["changed"], serde_json::json!(["stage_values"]), "{v}");
    assert_eq!(v["stage_values"], serde_json::json!([]), "{v}");
}

/// A daemon before revision 16 has no `StageValues`: `[]`, and no line
/// for it in `status`.
#[test]
fn a_revision_15_daemon_has_no_stage_values() {
    let bus = Bus::start();
    let _old = OldDaemon::start(&bus, Rev15);
    let out = bus.run(&["status", "--json"]);
    assert!(out.status.success(), "{}", out.stderr);
    let v = json_line(&out.stdout);
    assert_eq!(v["interface_revision"], 15);
    assert_eq!(v["stage_values"], serde_json::json!([]), "{v}");
    let out = bus.run(&["status"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert!(!out.stdout.contains("zoom"), "{}", out.stdout);
}

#[test]
fn status_and_watch_show_the_pipewire_output() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    mock.set_consumers(1);
    mock.set_pipewire(1, "streaming");

    let out = bus.run(&["status"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        "State:      idle\n\
         Enabled:    yes\n\
         Camera:     First Camera (1234:abcd)\n\
         Consumers:  2 (loopback 1, PipeWire 1)\n\
         Output:     NV12 1920x1080@30\n\
         PipeWire:   streaming\n\
         Effect:     none\n\
         Denoise:    off\n"
    );
    let out = bus.run(&["status", "--json"]);
    assert!(out.status.success(), "{}", out.stderr);
    let v = json_line(&out.stdout);
    assert_eq!(v["consumers"], 2);
    assert_eq!(v["loopback_consumers"], 1);
    assert_eq!(v["pipewire_consumers"], 1);
    assert_eq!(v["pipewire_output"], "streaming");

    // `watch --json`: each property in its own state line.
    let watch = bus.watch(&["watch", "--json"], None);
    let v = watch.next_json("the first snapshot");
    assert_eq!(v["type"], "snapshot", "{v}");
    assert_eq!(v["pipewire_output"], "streaming");
    watch.settings_set("the settings after the snapshot");
    mock.set_pipewire(0, "ready");
    for (field, value) in [
        ("consumers", serde_json::json!(1)),
        ("pipewire_consumers", serde_json::json!(0)),
        ("pipewire_output", serde_json::json!("ready")),
    ] {
        let v = watch.next_json(field);
        assert_eq!(v["type"], "state", "{v}");
        assert_eq!(v["changed"], serde_json::json!([field]), "{v}");
        assert_eq!(v[field], value, "{v}");
    }
    let mut watch = watch;
    watch.child.signal(libc::SIGTERM);
    let (status, stderr) = watch.finish("watch --json");
    assert_eq!(status.code(), Some(0), "{status:?}");
    assert_eq!(stderr, "");

    // `watch`: the same, for a person.
    let mut watch = bus.watch(&["watch"], None);
    assert_eq!(
        watch.next("the summary"),
        "omarchy-broadcastd: state idle | enabled yes | camera First Camera (1234:abcd) \
         | consumers 1 (loopback 1, PipeWire 0) | output NV12 1920x1080@30 \
         | PipeWire ready | effect none | denoise off\n"
    );
    mock.set_pipewire(1, "streaming");
    assert_eq!(watch.next("the total"), "consumers: 2\n");
    assert_eq!(watch.next("the links"), "PipeWire consumers: 1\n");
    assert_eq!(watch.next("the state"), "PipeWire: streaming\n");
    watch.child.signal(libc::SIGTERM);
    let (status, stderr) = watch.finish("watch");
    assert_eq!(status.code(), Some(0), "{status:?}");
    assert_eq!(stderr, "");
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
    assert_eq!(
        out.stderr,
        "omarchy-broadcast: no camera matches \"nope\"\n"
    );
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
        "Effect:     none\nStrength:   0.50\nImage:      (none)\nDenoise:    off\n"
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
        "Effect:     blur\nStrength:   0.80\nImage:      (none)\nDenoise:    off\n"
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
        out.stdout
            .ends_with("Effect:     blur\nStrength:   0.80\nDenoise:    off\n"),
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
            "Effect:     replace\nStrength:   0.80\nImage:      {}\nDenoise:    off\n",
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
            "Effect:     replace\nImage:      {}\nDenoise:    off\n",
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
        "omarchy-broadcast: replace needs a background image: set one with SetBackgroundImage\n"
    );
    assert_eq!(mock.take_calls(), vec!["SetEffect(replace)"]);

    // A rejected image stops the request: the effect is not selected.
    let notes = image_file(&bus, "notes.txt");
    let out = bus.run(&["effect", "replace", "--image", notes.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr);
    assert_eq!(
        out.stderr,
        format!("omarchy-broadcast: {}: not an image\n", notes.display())
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
    let _daemon = OldDaemon::start(&bus, Rev1);
    let expected = "omarchy-broadcast: the daemon does not support effects (its interface \
                    revision is 1, effects need 2); update omarchy-broadcastd\n";
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
fn denoise_switches_noise_removal_and_status_shows_it() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    let out = bus.run(&["denoise", "on"]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    assert_eq!(out.stdout, "Denoise:    on\n");
    assert_eq!(out.stderr, "");
    assert_eq!(mock.take_calls(), vec!["SetDenoise(true)"]);
    let out = bus.run(&["status"]);
    assert!(
        out.stdout.ends_with("Effect:     none\nDenoise:    on\n"),
        "{}",
        out.stdout
    );
    let out = bus.run(&["status", "--json"]);
    assert_eq!(json_line(&out.stdout)["denoise"], true);
    let out = bus.run(&["effect", "--json"]);
    assert_eq!(json_line(&out.stdout)["denoise"], true);
    let out = bus.run(&["effect"]);
    assert!(out.stdout.ends_with("Denoise:    on\n"), "{}", out.stdout);
    let out = bus.run(&["denoise", "off"]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    assert_eq!(out.stdout, "Denoise:    off\n");
    assert_eq!(mock.take_calls(), vec!["SetDenoise(false)"]);
    assert_eq!(
        json_line(&bus.run(&["status", "--json"]).stdout)["denoise"],
        false
    );
    // Usage errors: before any call.
    for args in [
        &["denoise"][..],
        &["denoise", "yes"],
        &["denoise", "on", "off"],
        &["denoise", "--json"],
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
    assert!(mock.take_calls().is_empty());
    assert!(!bus.activation_attempted());
}

#[test]
fn denoise_against_an_older_daemon_says_it_is_unsupported() {
    let bus = Bus::start();
    let _daemon = OldDaemon::start(&bus, Rev1);
    for args in [&["denoise", "on"][..], &["denoise", "off"]] {
        let out = bus.run(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", out.stderr);
        assert_eq!(out.stdout, "", "{args:?}");
        assert_eq!(
            out.stderr,
            "omarchy-broadcast: the daemon does not support noise removal (its interface \
             revision is 1, noise removal needs 4); update omarchy-broadcastd\n",
            "{args:?}"
        );
    }
    // Its snapshot has no value: `null`, and no line for a person.
    let v = json_line(&bus.run(&["status", "--json"]).stdout);
    assert_eq!(v["denoise"], Value::Null);
    assert!(v.as_object().unwrap().contains_key("denoise"));
    let out = bus.run(&["status"]);
    assert!(!out.stdout.contains("Denoise"), "{}", out.stdout);
}

#[test]
fn denoise_against_a_revision_3_daemon_says_it_is_unsupported() {
    // The daemon before noise removal: effects and the PipeWire output,
    // no `Denoise` and no `SetDenoise`.
    let bus = Bus::start();
    let _daemon = OldDaemon::start(&bus, Rev3);
    for args in [&["denoise", "on"][..], &["denoise", "off"]] {
        let out = bus.run(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", out.stderr);
        assert_eq!(out.stdout, "", "{args:?}");
        assert_eq!(
            out.stderr,
            "omarchy-broadcast: the daemon does not support noise removal (its interface \
             revision is 3, noise removal needs 4); update omarchy-broadcastd\n",
            "{args:?}"
        );
    }
    // Its snapshot has no value: `null`, and no line for a person, in
    // `status` and `effect` alike; the rest is shown.
    let out = bus.run(&["status", "--json"]);
    assert!(out.status.success(), "{}", out.stderr);
    let v = json_line(&out.stdout);
    assert_eq!(v["interface_revision"], 3);
    assert_eq!(v["denoise"], Value::Null);
    assert!(v.as_object().unwrap().contains_key("denoise"));
    assert_eq!(v["effect"], "blur");
    assert_eq!(v["pipewire_output"], "off");
    let out = bus.run(&["status"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert!(
        out.stdout.ends_with("Effect:     blur\nStrength:   0.50\n"),
        "{}",
        out.stdout
    );
    let out = bus.run(&["effect"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        "Effect:     blur\nStrength:   0.50\nImage:      (none)\n"
    );
    let out = bus.run(&["effect", "--json"]);
    assert_eq!(json_line(&out.stdout)["denoise"], Value::Null);
    // The effect commands still work with it.
    let out = bus.run(&["effect", "none"]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
}

#[test]
fn daemon_not_running_has_its_own_exit_code_and_reads_never_activate() {
    let bus = Bus::start();

    let out = bus.run(&["status"]);
    assert_eq!(out.status.code(), Some(3), "{}", out.stderr);
    assert_eq!(out.stdout, "");
    assert_eq!(
        out.stderr,
        "omarchy-broadcast: omarchy-broadcastd is not running (no owner for br.com.dias.OmarchyBroadcast \
         on the session bus); start it with: systemctl --user start omarchy-broadcastd\n"
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
    assert!(
        out.stderr
            .contains("systemctl --user start omarchy-broadcastd")
    );
    assert!(bus.activation_attempted(), "`on` should allow activation");
}

#[test]
fn no_session_bus_is_daemon_not_running() {
    let bus = Bus::start();
    let missing = bus.socket_dir.0.join("no-such-bus");
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
    assert_eq!(watch.settings_set("the settings").len(), 4);

    mock.set_consumers(1);
    let v = watch.next_json("a consumers change");
    assert_eq!(v["type"], "state", "{v}");
    assert_eq!(v["changed"], serde_json::json!(["consumers"]));
    assert_eq!(v["consumers"], 1);
    assert_eq!(v["state"], "idle");
    assert_eq!(v["active_camera_name"], "First Camera");
    let v = watch.next_json("the loopback count");
    assert_eq!(v["changed"], serde_json::json!(["loopback_consumers"]));
    assert_eq!(v["loopback_consumers"], 1);
    assert_eq!(v["pipewire_consumers"], 0);

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
    assert_eq!(v["denoise"], false);
    let out = bus.run(&["denoise", "on"]);
    assert!(out.status.success(), "{}", out.stderr);
    let v = watch.next_json("a denoise change");
    assert_eq!(v["changed"], serde_json::json!(["denoise"]));
    assert_eq!(v["denoise"], true);
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
    assert_eq!(watch.settings_set("the settings after the outage").len(), 4);

    mock.set_consumers(0);
    let v = watch.next_json("a change after the outage");
    assert_eq!(v["type"], "state", "{v}");
    assert_eq!(v["consumers"], 0);
    let v = watch.next_json("the loopback count after the outage");
    assert_eq!(v["loopback_consumers"], 0);

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
    watch.settings_set("the settings once the daemon appears");

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
    watch.settings_set("the settings once the daemon answers");

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
    // Like `omarchy-broadcast watch --json | head -1`: the reader takes one line
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
        "omarchy-broadcastd: state idle | enabled yes | camera First Camera (1234:abcd) \
         | consumers 0 | output NV12 1920x1080@30 | effect none | denoise off\n"
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
    let out = bus.run(&["denoise", "on"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(watch.next("a denoise change"), "denoise: on\n");
    mock.release_name();
    assert!(
        watch
            .next("the outage")
            .starts_with("omarchy-broadcastd is not running"),
    );

    watch.child.signal(libc::SIGTERM);
    let (status, stderr) = watch.finish("watch after SIGTERM");
    assert_eq!(status.code(), Some(0), "{status:?}");
    assert_eq!(stderr, "");
}

// ---------------------------------------------------------------------------
// preview

/// Waits until `done` holds of the mock's leases.
fn until_leases(mock: &MockDaemon, what: &str, done: impl Fn(&[(String, u32)]) -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let leases = mock.leases();
        if done(&leases) {
            return;
        }
        assert!(Instant::now() < deadline, "{what}: leases {leases:?}");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn preview_json_holds_a_lease_prints_its_frames_and_releases_it_on_sigterm() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    let preview = bus.watch(&["preview", "--json"], None);

    let v = preview.next_json("the lease");
    assert_eq!(v["type"], "preview", "{v}");
    assert_eq!(v["active"], true);
    assert_eq!(v["fps"], 15, "the default rate");
    assert_eq!(v["path"], PREVIEW_PATH);
    let leases = mock.leases();
    assert_eq!(leases.len(), 1, "{leases:?}");
    assert!(leases[0].0.starts_with(':'), "keyed by its unique name");
    assert_eq!(leases[0].1, 15);

    for seq in [1, 2, 3] {
        mock.emit_frame(seq);
        let v = preview.next_json("a frame");
        assert_eq!(v["type"], "frame", "{v}");
        assert_eq!(v["seq"], seq);
        assert_eq!(v["path"], PREVIEW_PATH);
    }

    let mut preview = preview;
    preview.child.signal(libc::SIGTERM);
    let (status, stderr) = preview.finish("preview after SIGTERM");
    assert_eq!(status.code(), Some(0), "{status:?}");
    assert_eq!(stderr, "");
    assert_eq!(
        mock.take_calls(),
        ["AcquirePreview(15)", "ReleasePreview"],
        "released before it exited"
    );
    // Both carried NO_AUTO_START: a daemon gone in between is not started
    // by them.
    assert_eq!(mock.auto_start_calls(), Vec::<String>::new());
    assert!(mock.leases().is_empty());
    assert!(
        !bus.activation_attempted(),
        "preview must not start the daemon"
    );
}

#[test]
fn preview_fps_30_is_asked_for_and_any_other_rate_is_a_usage_error() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    for bad in ["20", "0", "10", "60", "fifteen", "15.0", "4294967295"] {
        let out = bus.run(&["preview", "--json", "--fps", bad]);
        assert_eq!(out.status.code(), Some(2), "--fps {bad}: {}", out.stderr);
        assert_eq!(out.stdout, "", "--fps {bad}");
        assert!(out.stderr.contains("--fps"), "--fps {bad}: {}", out.stderr);
    }
    assert!(mock.take_calls().is_empty(), "no call for a usage error");

    let mut preview = bus.watch(&["preview", "--json", "--fps", "30"], None);
    let v = preview.next_json("the lease");
    assert_eq!(
        (v["type"].as_str(), v["fps"].as_u64()),
        (Some("preview"), Some(30))
    );
    assert_eq!(mock.leases()[0].1, 30);
    // SIGINT and SIGHUP end it as SIGTERM does.
    preview.child.signal(libc::SIGHUP);
    let (status, _) = preview.finish("preview after SIGHUP");
    assert_eq!(status.code(), Some(0), "{status:?}");
    assert!(mock.leases().is_empty());
    let mut preview = bus.watch(&["preview", "--json", "--fps", "15"], None);
    preview.next_json("the lease");
    preview.child.signal(libc::SIGINT);
    let (status, _) = preview.finish("preview after SIGINT");
    assert_eq!(status.code(), Some(0), "{status:?}");
    assert!(mock.leases().is_empty());
}

#[test]
fn preview_json_follows_the_daemon_through_an_outage() {
    let bus = Bus::start();
    let preview = bus.watch(&["preview", "--json"], None);
    let v = preview.next_json("the not-running line");
    assert_eq!(v["type"], "error", "{v}");
    assert_eq!(v["code"], "daemon-not-running");
    preview.quiet_for(Duration::from_millis(2500), "while nothing runs");

    let mock = MockDaemon::start(&bus);
    let v = preview.next_json("the lease once the daemon appears");
    assert_eq!(
        (v["type"].as_str(), v["active"].as_bool()),
        (Some("preview"), Some(true))
    );
    until_leases(&mock, "the lease", |l| l.len() == 1);

    // The daemon goes away: the lease is lost, then one outage line.
    mock.release_name();
    let v = preview.next_json("the lease lost");
    assert_eq!(
        v,
        serde_json::json!({"v": 1, "type": "preview", "active": false})
    );
    let v = preview.next_json("the outage");
    assert_eq!(v["type"], "error", "{v}");
    assert_eq!(v["code"], "daemon-not-running");
    preview.quiet_for(Duration::from_millis(2500), "during the outage");

    // Back: the lease is asked for again.
    mock.request_name();
    let v = preview.next_json("the lease again");
    assert_eq!(
        (v["type"].as_str(), v["active"].as_bool()),
        (Some("preview"), Some(true))
    );
    let acquired = mock
        .take_calls()
        .iter()
        .filter(|c| c.starts_with("AcquirePreview"))
        .count();
    assert_eq!(acquired, 2);
    mock.emit_frame(9);
    let v = preview.next_json("a frame after the outage");
    assert_eq!(
        (v["type"].as_str(), v["seq"].as_u64()),
        (Some("frame"), Some(9))
    );
    assert!(!bus.activation_attempted());
}

#[test]
fn preview_against_an_older_daemon_says_it_is_unsupported_and_waits() {
    let bus = Bus::start();
    let _old = OldDaemon::start(&bus, Rev3);
    let preview = bus.watch(&["preview", "--json"], None);
    let v = preview.next_json("the unsupported line");
    assert_eq!(v["type"], "error", "{v}");
    assert_eq!(v["code"], "unsupported-daemon");
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains("interface revision is 3"),
        "{v}"
    );
    preview.quiet_for(Duration::from_millis(2500), "waiting for another daemon");
}

#[test]
fn preview_releases_and_exits_quietly_when_its_reader_goes_away() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    // The reader closes the pipe after the first line.
    let preview = bus.watch(&["preview", "--json"], Some(1));
    let v = preview.next_json("the lease");
    assert_eq!(v["type"], "preview", "{v}");
    let (status, stderr) = preview.finish("preview without a reader");
    assert_eq!(status.code(), Some(0), "{status:?}");
    assert_eq!(stderr, "");
    assert!(mock.take_calls().contains(&"ReleasePreview".to_string()));
    assert!(mock.leases().is_empty());
}

#[test]
fn preview_json_says_the_lease_is_lost_when_the_bus_goes_away() {
    let mut bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    let preview = bus.watch(&["preview", "--json"], None);
    let v = preview.next_json("the lease");
    assert_eq!(
        (v["type"].as_str(), v["active"].as_bool()),
        (Some("preview"), Some(true))
    );
    until_leases(&mock, "the lease", |l| l.len() == 1);

    // The session bus ends: the connection holding the lease closes, so the
    // lease is gone; that is said before the outage.
    bus.daemon.signal(libc::SIGKILL);
    let v = preview.next_json("the lease lost");
    assert_eq!(
        v,
        serde_json::json!({"v": 1, "type": "preview", "active": false})
    );
    let v = preview.next_json("the outage");
    assert_eq!(v["type"], "error", "{v}");
    assert_eq!(v["code"], "dbus-error");
    preview.quiet_for(Duration::from_millis(2500), "during the outage");
    let mut preview = preview;
    preview.child.signal(libc::SIGTERM);
    let (status, stderr) = preview.finish("preview after SIGTERM");
    assert_eq!(status.code(), Some(0), "{status:?}");
    assert_eq!(stderr, "");
}

#[test]
fn preview_json_refuses_a_frame_path_too_long_for_a_line() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    let long = format!("/run/{}/omarchy-broadcast/preview.jpg", "d".repeat(5000));
    mock.set_preview_path(&long);
    let preview = bus.watch(&["preview", "--json"], None);
    let v = preview.next_json("the refusal");
    assert_eq!(v["type"], "error", "{v}");
    assert_eq!(v["code"], "preview-path-too-long");
    assert!(v["error"].as_str().unwrap().contains("too long"), "{v}");
    // Not held for frames nobody can find: released at once.
    until_leases(&mock, "the lease released", <[_]>::is_empty);
    assert_eq!(mock.take_calls(), ["AcquirePreview(15)", "ReleasePreview"]);
    // No frame line, clipped or not, and the daemon is not asked again.
    mock.emit_frame(1);
    preview.quiet_for(
        Duration::from_millis(2500),
        "after the refusal, with the same daemon",
    );
    assert!(mock.take_calls().is_empty());
    assert_eq!(mock.auto_start_calls(), Vec::<String>::new());

    // Another daemon (with a short path) is picked up.
    drop(mock);
    let mock = MockDaemon::start(&bus);
    let v = preview.next_json("the lease from the new daemon");
    assert_eq!(
        (v["type"].as_str(), v["path"].as_str()),
        (Some("preview"), Some(PREVIEW_PATH))
    );
    mock.emit_frame(2);
    let v = preview.next_json("a frame");
    assert_eq!(
        (v["type"].as_str(), v["seq"].as_u64()),
        (Some("frame"), Some(2))
    );
}

// ---------------------------------------------------------------------------
// controls

/// `controls` of the mock's first camera, untouched, for a person.
const CONTROLS_TEXT: &str = "Camera:     First Camera (1234:abcd)\n  \
                             brightness    0  -64..64        default 0\n  \
                             saturation   50  0..100 step 2  default 50\n  \
                             sharpness   128  0..255         default 128\n";
const NOT_IN_USE: &str = "The camera is not in use: changes apply the next time it opens.\n";
/// What `settings` ends with when a revision-9 daemon has no effect module.
const NO_MODULES: &str = "All cameras: No effect modules installed\n";

#[test]
fn controls_human_and_json() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);

    let out = bus.run(&["controls"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(out.stdout, format!("{CONTROLS_TEXT}{NOT_IN_USE}"));
    assert_eq!(out.stderr, "");
    // Read from the device, for the active camera's id, never starting the
    // daemon.
    assert_eq!(mock.take_calls(), ["GetCameraControls(usb-1234_abcd)"]);

    let out = bus.run(&["controls", "--json"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(out.stdout.lines().count(), 1, "{}", out.stdout);
    let v = json_line(&out.stdout);
    assert_eq!(
        v,
        serde_json::json!({
            "v": 1, "type": "controls", "camera": "usb-1234_abcd",
            "controls": [
                {"name": "brightness", "min": -64, "max": 64, "step": 1,
                 "default": 0, "value": 0, "stored": false},
                {"name": "saturation", "min": 0, "max": 100, "step": 2,
                 "default": 50, "value": 50, "stored": false},
                {"name": "sharpness", "min": 0, "max": 255, "step": 1,
                 "default": 128, "value": 128, "stored": false},
            ],
            "controls_truncated": false,
        })
    );
    assert_eq!(mock.take_calls(), ["GetCameraControls(usb-1234_abcd)"]);

    // While the camera streams, a change applies at once.
    mock.set_state(State::Active, "");
    let out = bus.run(&["controls"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(out.stdout, format!("{CONTROLS_TEXT}Applied live.\n"));

    // A camera without image controls.
    let out = bus.run(&["camera", "platform-video2"]);
    assert!(out.status.success(), "{}", out.stderr);
    let out = bus.run(&["controls"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        "Camera:     Second Camera\nNo image controls.\nApplied live.\n"
    );
    let v = json_line(&bus.run(&["controls", "--json"]).stdout);
    assert_eq!(v["camera"], "platform-video2");
    assert_eq!(v["controls"], serde_json::json!([]));

    // `status --json` carries the property.
    let v = json_line(&bus.run(&["status", "--json"]).stdout);
    assert_eq!(v["camera_controls"]["camera"], "platform-video2");
    assert_eq!(v["camera_controls"]["controls"], serde_json::json!([]));
    assert_eq!(v["camera_controls"]["controls_truncated"], false);

    assert!(
        mock.auto_start_calls().is_empty(),
        "{:?}",
        mock.auto_start_calls()
    );
    assert!(!bus.activation_attempted());
}

/// No camera present: nothing to read, so no call, and the empty controls
/// of the `CameraControls` property, exit 0.
#[test]
fn controls_without_a_camera_are_empty_not_an_error() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    mock.remove_cameras();

    let out = bus.run(&["controls"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        format!("Camera:     none\nNo image controls.\n{NOT_IN_USE}")
    );
    assert_eq!(out.stderr, "");

    let out = bus.run(&["controls", "--json"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        json_line(&out.stdout),
        serde_json::json!({
            "v": 1, "type": "controls", "camera": "", "controls": [],
            "controls_truncated": false,
        })
    );
    assert_eq!(mock.take_calls(), Vec::<String>::new());

    // The same as the property says.
    let v = json_line(&bus.run(&["status", "--json"]).stdout);
    assert_eq!(v["camera_controls"]["camera"], "");
    assert_eq!(v["camera_controls"]["controls"], serde_json::json!([]));
}

/// The active camera changes between the properties and the call: one
/// retry with the new id.
#[test]
fn controls_retry_once_when_the_active_camera_changed() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    mock.rename_first_camera_on_next_get("usb-new_id");

    let out = bus.run(&["controls"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(out.stdout, format!("{CONTROLS_TEXT}{NOT_IN_USE}"));
    assert_eq!(
        mock.take_calls(),
        [
            "GetCameraControls(usb-1234_abcd)",
            "GetCameraControls(usb-new_id)"
        ]
    );
}

/// A change the daemon accepted, whose read-back failed: still a failure
/// status, but the message says the change was made.
#[test]
fn controls_set_says_so_when_only_the_read_back_failed() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    mock.fail_get_camera_controls(true);

    let out = bus.run(&["controls", "set", "brightness", "-10"]);
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr);
    assert_eq!(out.stdout, "");
    assert!(
        out.stderr.contains("the change was made")
            && out.stderr.contains("the device cannot be read"),
        "{}",
        out.stderr
    );
    mock.fail_get_camera_controls(false);
    let v = json_line(&bus.run(&["controls", "--json"]).stdout);
    assert_eq!(v["controls"][0]["value"], -10);
    assert_eq!(v["controls"][0]["stored"], true);

    // A change that fails keeps the plain message.
    let out = bus.run(&["controls", "set", "brightness", "1000"]);
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr);
    assert!(
        !out.stderr.contains("the change was made"),
        "{}",
        out.stderr
    );
}

#[test]
fn controls_set_and_reset_print_the_result() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);

    let out = bus.run(&["controls", "set", "brightness", "-10"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        format!(
            "Camera:     First Camera (1234:abcd)\n  \
             brightness  -10  -64..64        default 0    (stored)\n  \
             saturation   50  0..100 step 2  default 50\n  \
             sharpness   128  0..255         default 128\n{NOT_IN_USE}"
        )
    );
    assert_eq!(out.stderr, "");
    // On the active camera (`""`), then read back from the device.
    assert_eq!(
        mock.take_calls(),
        [
            "SetCameraControl(,brightness,-10)",
            "GetCameraControls(usb-1234_abcd)"
        ]
    );
    let out = bus.run(&["controls", "set", "saturation", "+52"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(mock.take_calls()[0], "SetCameraControl(,saturation,52)");
    let v = json_line(&bus.run(&["controls", "--json"]).stdout);
    assert_eq!(v["controls"][0]["value"], -10);
    assert_eq!(v["controls"][0]["stored"], true);
    assert_eq!(v["controls"][1]["value"], 52);
    mock.take_calls();

    mock.set_state(State::Active, "");
    let out = bus.run(&["controls", "reset"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(out.stdout, format!("{CONTROLS_TEXT}Applied live.\n"));
    assert_eq!(
        mock.take_calls(),
        ["ResetCameraControls()", "GetCameraControls(usb-1234_abcd)"]
    );
}

#[test]
fn controls_usage_errors_exit_2_before_any_call() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    for args in [
        &["controls", "set", "hue", "1"][..],
        &["controls", "set", "Brightness", "1"],
        &["controls", "set", "", "1"],
        &["controls", "set", "brightness", "1.5"],
        &["controls", "set", "brightness", "ten"],
        &["controls", "set", "brightness", "0x10"],
        &["controls", "set", "brightness", ""],
        &["controls", "set", "brightness", "2147483648"],
        &["controls", "set", "brightness", "-2147483649"],
        &["controls", "set", "brightness", "99999999999999999999"],
        &["controls", "set", "brightness"],
        &["controls", "set", "brightness", "1", "2"],
        &["controls", "set"],
        &["controls", "--json", "set", "brightness", "1"],
        &["controls", "set", "brightness", "1", "--json"],
        &["controls", "reset", "--json"],
        &["controls", "reset", "brightness"],
        &["controls", "sharpen"],
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
    let out = bus.run(&["controls", "set", "hue", "1"]);
    assert!(
        out.stderr.contains("backlight_compensation"),
        "{}",
        out.stderr
    );
    assert!(mock.take_calls().is_empty());
    assert!(!bus.activation_attempted());
}

#[test]
fn controls_rejected_by_the_daemon_exit_1_with_its_message() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    for (args, message) in [
        (
            &["controls", "set", "brightness", "65"][..],
            "brightness 65 is outside its range -64..=64",
        ),
        (
            &["controls", "set", "saturation", "51"],
            "saturation 51 is not on its step of 2 from 0",
        ),
        (
            &["controls", "set", "contrast", "5"],
            "the camera has no control \"contrast\"",
        ),
    ] {
        let out = bus.run(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", out.stderr);
        assert_eq!(out.stdout, "", "{args:?}");
        assert_eq!(out.stderr, format!("omarchy-broadcast: {message}\n"));
        assert_eq!(mock.take_calls().len(), 1, "{args:?}: nothing read back");
    }
    // Nothing was changed.
    let out = bus.run(&["controls"]);
    assert_eq!(out.stdout, format!("{CONTROLS_TEXT}{NOT_IN_USE}"));
}

#[test]
fn controls_never_start_the_daemon_but_set_and_reset_may() {
    let bus = Bus::start();
    let out = bus.run(&["controls"]);
    assert_eq!(out.status.code(), Some(3), "{}", out.stderr);
    assert!(out.stderr.contains("is not running"), "{}", out.stderr);
    let out = bus.run(&["controls", "--json"]);
    assert_eq!(out.status.code(), Some(3), "{}", out.stderr);
    let v = json_line(&out.stdout);
    assert_eq!(v["type"], "error");
    assert_eq!(v["code"], "daemon-not-running");
    assert!(!bus.activation_attempted(), "`controls` must not start it");

    for args in [
        &["controls", "set", "sharpness", "1"][..],
        &["controls", "reset"],
    ] {
        let out = bus.run(args);
        assert_eq!(out.status.code(), Some(3), "{args:?}: {}", out.stderr);
        assert!(
            out.stderr.contains("D-Bus could not start it"),
            "{}",
            out.stderr
        );
    }
    assert!(bus.activation_attempted(), "`controls set` may start it");
}

#[test]
fn controls_against_a_revision_5_daemon_say_they_are_unsupported() {
    let bus = Bus::start();
    let _daemon = OldDaemon::start(&bus, Rev5);
    let expected = "omarchy-broadcast: the daemon does not support camera controls (its \
                    interface revision is 5, camera controls need 6); update omarchy-broadcastd\n";
    for args in [
        &["controls"][..],
        &["controls", "set", "brightness", "1"],
        &["controls", "reset"],
    ] {
        let out = bus.run(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", out.stderr);
        assert_eq!(out.stdout, "", "{args:?}");
        assert_eq!(out.stderr, expected, "{args:?}");
    }
    let out = bus.run(&["controls", "--json"]);
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr);
    assert_eq!(out.stderr, "");
    assert_eq!(out.stdout.lines().count(), 1, "{}", out.stdout);
    let v = json_line(&out.stdout);
    assert_eq!(v["type"], "error");
    assert_eq!(v["code"], "unsupported-daemon");
    assert_eq!(
        v["error"],
        expected
            .trim_start_matches("omarchy-broadcast: ")
            .trim_end_matches('\n')
    );

    // Its snapshot has none: `null`, with the key present.
    let v = json_line(&bus.run(&["status", "--json"]).stdout);
    assert_eq!(v["interface_revision"], 5);
    assert!(v.as_object().unwrap().contains_key("camera_controls"));
    assert_eq!(v["camera_controls"], Value::Null);
    let mut watch = bus.watch(&["watch", "--json"], None);
    let v = watch.next_json("the snapshot of a revision-5 daemon");
    assert_eq!(v["type"], "snapshot", "{v}");
    assert!(v.as_object().unwrap().contains_key("camera_controls"));
    assert_eq!(v["camera_controls"], Value::Null);
    watch.child.signal(libc::SIGTERM);
    let (status, _) = watch.finish("watch --json");
    assert_eq!(status.code(), Some(0), "{status:?}");
}

#[test]
fn watch_prints_the_controls_when_they_change() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);

    let watch = bus.watch(&["watch", "--json"], None);
    let v = watch.next_json("the first snapshot");
    assert_eq!(v["type"], "snapshot", "{v}");
    assert_eq!(v["camera_controls"]["camera"], "usb-1234_abcd");
    assert_eq!(
        v["camera_controls"]["controls"][2],
        serde_json::json!({"name": "sharpness", "min": 0, "max": 255, "step": 1,
                           "default": 128, "value": 128, "stored": false})
    );
    assert_eq!(v["camera_controls"]["controls_truncated"], false);
    watch.settings_set("the settings");

    let out = bus.run(&["controls", "set", "sharpness", "200"]);
    assert!(out.status.success(), "{}", out.stderr);
    let v = watch.next_json("a controls change");
    assert_eq!(v["type"], "controls", "{v}");
    assert_eq!(v["camera"], "usb-1234_abcd");
    assert_eq!(v["controls"].as_array().unwrap().len(), 3);
    assert_eq!(v["controls"][2]["value"], 200);
    assert_eq!(v["controls"][2]["stored"], true);
    assert_eq!(v["controls_truncated"], false);

    // An announcement that changes nothing prints nothing.
    mock.announce_controls();
    watch.quiet_for(Duration::from_millis(500), "an unchanged CameraControls");
    let out = bus.run(&["controls", "reset"]);
    assert!(out.status.success(), "{}", out.stderr);
    let v = watch.next_json("the reset");
    assert_eq!(v["type"], "controls", "{v}");
    assert_eq!(v["controls"][2]["value"], 128);
    assert_eq!(v["controls"][2]["stored"], false);
    let mut watch = watch;
    watch.child.signal(libc::SIGTERM);
    let (status, stderr) = watch.finish("watch --json");
    assert_eq!(status.code(), Some(0), "{status:?}");
    assert_eq!(stderr, "");

    // For a person.
    let mut watch = bus.watch(&["watch"], None);
    assert!(
        watch
            .next("the summary")
            .starts_with("omarchy-broadcastd: state idle")
    );
    let out = bus.run(&["controls", "set", "brightness", "-64"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        watch.next("a controls change"),
        "camera controls: brightness -64 (stored), saturation 50, sharpness 128\n"
    );
    mock.announce_controls();
    watch.quiet_for(Duration::from_millis(500), "an unchanged CameraControls");
    mock.set_controls(Vec::new());
    assert_eq!(watch.next("no controls"), "camera controls: none\n");
    watch.child.signal(libc::SIGTERM);
    let (status, stderr) = watch.finish("watch");
    assert_eq!(status.code(), Some(0), "{status:?}");
    assert_eq!(stderr, "");
    assert!(!bus.activation_attempted());
}

#[test]
fn controls_lines_stay_under_the_cap_with_a_long_camera_id() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    // A hostile daemon: a long id full of escapes, and too many controls
    // with long names that carry escapes too.
    let evil = "\u{1b}]0;x\u{7}\u{202e}\"\\";
    let id = format!("usb-{}", evil.repeat(60));
    mock.set_first_camera_id(&id);
    let many: Vec<CameraControl> = (0..20)
        .map(|i| {
            control(
                &format!("{i}{evil}{}", "n".repeat(100)),
                i32::MIN,
                i32::MAX,
                1,
                i32::MIN,
            )
        })
        .collect();
    mock.set_controls(many);

    let out = bus.run(&["controls", "--json"]);
    assert!(out.status.success(), "{}", out.stderr);
    let v = json_line(&out.stdout);
    let camera = v["camera"].as_str().unwrap();
    assert!(
        camera.starts_with("usb-") && camera.ends_with('\u{2026}'),
        "{camera}"
    );
    assert!(
        serde_json::to_string(camera).unwrap().len() - 2 <= 96,
        "{camera}"
    );
    assert_eq!(v["controls"].as_array().unwrap().len(), 16);
    assert_eq!(v["controls_truncated"], true);
    for c in v["controls"].as_array().unwrap() {
        let name = c["name"].as_str().unwrap();
        assert!(name.ends_with('\u{2026}') && name.len() <= 32, "{name}");
    }

    let v = json_line(&bus.run(&["status", "--json"]).stdout);
    assert_eq!(v["camera_controls"]["camera"].as_str().unwrap(), camera);
    assert_eq!(v["camera_controls"]["controls_truncated"], true);
    let watch = bus.watch(&["watch", "--json"], Some(1));
    let v = watch.next_json("the snapshot");
    assert_eq!(v["camera_controls"]["controls_truncated"], true);
    let _ = watch.finish("watch --json | head -1");

    // For a person, the id is escaped and each control is one line.
    let out = bus.run(&["controls"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(out.stdout.lines().count(), 1 + 20 + 1, "{}", out.stdout);
    assert!(
        !out.stdout
            .chars()
            .any(|c| (c.is_control() && c != '\n') || c == '\u{202e}'),
        "{:?}",
        out.stdout
    );
    // The camera is listed: its name, not its id.
    assert!(
        out.stdout
            .starts_with("Camera:     First Camera (1234:abcd)\n  0\\x1b]0;x\\x07\\u{202e}\"\\"),
        "{}",
        out.stdout
    );
}

// ---------------------------------------------------------------------------
// settings

/// `settings` of the mock's first camera, untouched, for a person (without
/// the closing line on where a change applies).
const SETTINGS_TEXT: &str = "\
Camera:     First Camera (1234:abcd)
Image
  Brightness                128           camera.image.brightness                         0..255
  Contrast                  128           camera.image.contrast                           0..255
  Saturation                128           camera.image.saturation                         0..255
  Sharpness                 128           camera.image.sharpness                          0..255
  Backlight compensation    1             camera.image.backlight_compensation             0..1
White balance
  Automatic white balance   on            camera.white_balance.white_balance_automatic    on|off
  Temperature               2975 K        camera.white_balance.white_balance_temperature  2000..7500                (Turn off automatic white balance to change this)
Exposure
  Exposure mode             Auto          camera.exposure.auto_exposure                   manual|aperture_priority
  Exposure time             1250 \u{d7}100 \u{b5}s  camera.exposure.exposure_time_absolute          3..2047                   (Set exposure to manual to change this)
  Gain                      0             camera.exposure.gain                            0..255
  Keep 30 fps in low light  off           camera.exposure.keep_framerate                  on|off
  Anti-flicker              60 Hz         camera.exposure.power_line_frequency            disabled|50hz|60hz
Focus & zoom
  Continuous autofocus      on            camera.focus.focus_automatic_continuous         on|off
  Focus                     40            camera.focus.focus_absolute                     0..255 step 5             (Turn off continuous autofocus to change this)
  Zoom                      100           camera.focus.zoom_absolute                      100..500
  Pan                       14400         camera.focus.pan_absolute                       -36000..36000 step 3600
  Tilt                      -14400        camera.focus.tilt_absolute                      -36000..36000 step 3600
  Recentre pan              action        camera.focus.pan_reset
  Field of view             65\u{b0}           camera.focus.field_of_view                      90|78|65
";

/// The `settings_group` line of the mock's exposure group, untouched.
fn exposure_group_line(rev: u32, serial: u32) -> Value {
    serde_json::json!({
        "v": 1, "type": "settings_group", "schema": 1, "rev": rev, "serial": serial,
        "camera": "usb-1234_abcd", "index": 2, "count": 4, "part": 1, "parts": 1,
        "group": {"id": "exposure", "label": "Exposure", "settings": [
            {"id": "camera.exposure.auto_exposure", "scope": "camera", "label": "Exposure mode", "type": "enum",
             "value": "aperture_priority", "default": "aperture_priority", "stored": false,
             "enabled": true, "disabled_reason": "", "disabled_kind": "", "enabled_when": [],
             "options": [{"value": "manual", "label": "Manual"},
                         {"value": "aperture_priority", "label": "Auto"}],
             "hint": "segmented"},
            {"id": "camera.exposure.exposure_time_absolute", "scope": "camera", "label": "Exposure time",
             "type": "int", "value": 1250, "default": 156, "stored": false, "enabled": false,
             "disabled_reason": "Set exposure to manual to change this", "disabled_kind": "parent_off",
             "enabled_when": [{"id": "camera.exposure.auto_exposure", "op": "in",
                               "value": ["manual", "shutter_priority"]}],
             "min": 3, "max": 2047, "step": 1, "unit": "\u{d7}100 \u{b5}s", "hint": "slider"},
            {"id": "camera.exposure.gain", "scope": "camera", "label": "Gain", "type": "int", "value": 0,
             "default": 0, "stored": false, "enabled": true, "disabled_reason": "", "disabled_kind": "",
             "enabled_when": [], "min": 0, "max": 255, "step": 1, "hint": "slider"},
            {"id": "camera.exposure.keep_framerate", "scope": "camera", "label": "Keep 30 fps in low light",
             "type": "bool", "value": false, "default": true, "stored": false, "enabled": true,
             "disabled_reason": "", "disabled_kind": "", "enabled_when": [], "hint": "switch"},
            {"id": "camera.exposure.power_line_frequency", "scope": "camera", "label": "Anti-flicker",
             "type": "enum", "value": "60hz", "default": "60hz", "stored": false,
             "enabled": true, "disabled_reason": "", "disabled_kind": "", "enabled_when": [],
             "options": [{"value": "disabled", "label": "Off"}, {"value": "50hz", "label": "50 Hz"},
                         {"value": "60hz", "label": "60 Hz"}],
             "hint": "segmented"},
        ]},
    })
}

/// The lines of `settings --json` (or of a set in `watch --json`): their
/// groups' ids, checking `index`, `count` and that each group is whole.
fn set_groups(lines: &[Value]) -> Vec<String> {
    lines
        .iter()
        .enumerate()
        .map(|(i, v)| {
            assert_eq!(v["type"], "settings_group", "{v}");
            assert_eq!(v["index"], i, "{v}");
            assert_eq!(v["count"], lines.len(), "{v}");
            assert_eq!((&v["part"], &v["parts"]), (&1.into(), &1.into()), "{v}");
            v["group"]["id"].as_str().unwrap().to_string()
        })
        .collect()
}

fn json_lines(stdout: &str) -> Vec<Value> {
    stdout.split_inclusive('\n').map(json_line).collect()
}

/// The setting `id` in a set of lines.
fn find_setting<'a>(lines: &'a [Value], id: &str) -> &'a Value {
    lines
        .iter()
        .flat_map(|v| v["group"]["settings"].as_array().unwrap())
        .find(|s| s["id"] == id)
        .unwrap_or_else(|| panic!("no setting {id}"))
}

#[test]
fn settings_human_and_json() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);

    let out = bus.run(&["settings"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        format!("{SETTINGS_TEXT}{NOT_IN_USE}{NO_MODULES}")
    );
    assert_eq!(out.stderr, "");
    // The active camera's, read without starting the daemon.
    assert_eq!(mock.take_calls(), ["GetSettings()"]);

    let out = bus.run(&["settings", "--json"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(out.stderr, "");
    let lines = json_lines(&out.stdout);
    assert_eq!(
        set_groups(&lines),
        ["image", "white_balance", "exposure", "focus"]
    );
    assert_eq!(lines[2], exposure_group_line(1, 1));
    assert_eq!(lines[0]["group"]["label"], "Image");
    assert_eq!(lines[3]["group"]["label"], "Focus & zoom");
    assert_eq!(
        find_setting(&lines, "camera.focus.pan_reset"),
        &serde_json::json!({"id": "camera.focus.pan_reset", "scope": "camera", "label": "Recentre pan",
                            "type": "action", "stored": false, "enabled": true,
                            "disabled_reason": "", "disabled_kind": "", "enabled_when": []})
    );
    assert_eq!(
        find_setting(&lines, "camera.focus.field_of_view")["options"],
        serde_json::json!([{"value": "90", "label": "90\u{b0}"},
                           {"value": "78", "label": "78\u{b0}"},
                           {"value": "65", "label": "65\u{b0}"}])
    );
    assert_eq!(mock.take_calls(), ["GetSettings()"]);

    // Another camera, by its id: it has none.
    let out = bus.run(&["settings", "--json", "--camera=platform-video2"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        json_lines(&out.stdout),
        [serde_json::json!({"v": 1, "type": "settings_clear", "rev": 1, "serial": 1})]
    );
    let out = bus.run(&["settings", "--camera", "platform-video2"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        format!("Camera:     Second Camera\nNo camera settings.\n{NOT_IN_USE}{NO_MODULES}")
    );
    assert_eq!(
        mock.take_calls(),
        [
            "GetSettings(platform-video2)",
            "GetSettings(platform-video2)"
        ]
    );

    // The active camera, by its id, while it streams: live.
    mock.set_state(State::Active, "");
    let out = bus.run(&["settings", "--camera=usb-1234_abcd"]);
    assert_eq!(
        out.stdout,
        format!("{SETTINGS_TEXT}Applied live.\n{NO_MODULES}")
    );
    // Another camera is not open, whatever the state.
    let out = bus.run(&["settings", "--camera=platform-video2"]);
    assert!(
        out.stdout.ends_with(&format!("{NOT_IN_USE}{NO_MODULES}")),
        "{}",
        out.stdout
    );

    // A camera that is not there: the daemon's rejection.
    for json in [false, true] {
        let mut args = vec!["settings", "--camera=nope"];
        if json {
            args.push("--json");
        }
        let out = bus.run(&args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", out.stderr);
        if json {
            let v = json_line(&out.stdout);
            assert_eq!(v["type"], "error");
            assert_eq!(v["code"], "dbus-error");
            assert_eq!(v["error"], "no camera \"nope\" is present");
        } else {
            assert_eq!(
                out.stderr,
                "omarchy-broadcast: no camera \"nope\" is present\n"
            );
        }
    }

    // The snapshot carries the revision and the schema; a state line does not.
    let v = json_line(&bus.run(&["status", "--json"]).stdout);
    assert_eq!(v["settings_revision"], 1);
    assert_eq!(v["settings_schema"], 1);

    assert!(
        mock.auto_start_calls().is_empty(),
        "{:?}",
        mock.auto_start_calls()
    );
    assert!(!bus.activation_attempted());
}

#[test]
fn settings_without_a_camera_are_a_clear_line() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    mock.remove_cameras();
    let out = bus.run(&["settings", "--json"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        json_lines(&out.stdout),
        [serde_json::json!({"v": 1, "type": "settings_clear", "rev": 1, "serial": 1})]
    );
    let out = bus.run(&["settings"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        format!("Camera:     none\nNo camera settings.\n{NOT_IN_USE}{NO_MODULES}")
    );
    // Setting something without a camera: rejected before any write.
    let out = bus.run(&["settings", "set", "camera.image.brightness", "10"]);
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr);
    assert_eq!(out.stderr, "omarchy-broadcast: no camera is present\n");
    assert_eq!(
        mock.take_calls(),
        ["GetSettings()", "GetSettings()", "GetSettings()"]
    );
}

#[test]
fn settings_set_types_the_value_and_prints_the_setting() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);

    let out = bus.run(&["settings", "set", "camera.exposure.auto_exposure", "manual"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        format!(
            "Camera:     First Camera (1234:abcd)\n\
             Exposure\n  \
             Exposure mode  Manual  camera.exposure.auto_exposure  manual|aperture_priority  \
             (stored)\n{NOT_IN_USE}"
        )
    );
    assert_eq!(out.stderr, "");
    // The tree first, to type the value; then the write; then read back.
    assert_eq!(
        mock.take_calls(),
        [
            "GetSettings()",
            "SetSetting(,camera.exposure.auto_exposure,Str(\"manual\"))",
            "GetSettings()"
        ]
    );
    // Its dependent can be changed now.
    let out = bus.run(&["settings"]);
    assert!(
        out.stdout.contains(
            "\n  Exposure time             1250 \u{d7}100 \u{b5}s  \
             camera.exposure.exposure_time_absolute          3..2047\n"
        ),
        "{}",
        out.stdout
    );
    mock.take_calls();

    // Each type, each spelling.
    for (args, sent) in [
        (
            &[
                "settings",
                "set",
                "--",
                "camera.focus.pan_absolute",
                "-3600",
            ][..],
            "SetSetting(,camera.focus.pan_absolute,Int(-3600))",
        ),
        (
            &["settings", "set", "camera.focus.tilt_absolute", "-7200"],
            "SetSetting(,camera.focus.tilt_absolute,Int(-7200))",
        ),
        (
            &["settings", "set", "camera.image.brightness", "+40"],
            "SetSetting(,camera.image.brightness,Int(40))",
        ),
        (
            &["settings", "set", "camera.exposure.keep_framerate", "on"],
            "SetSetting(,camera.exposure.keep_framerate,Bool(true))",
        ),
        (
            &["settings", "set", "camera.exposure.keep_framerate", "false"],
            "SetSetting(,camera.exposure.keep_framerate,Bool(false))",
        ),
        (
            &["settings", "set", "camera.exposure.keep_framerate", "true"],
            "SetSetting(,camera.exposure.keep_framerate,Bool(true))",
        ),
        (
            &[
                "settings",
                "set",
                "camera.focus.focus_automatic_continuous",
                "off",
            ],
            "SetSetting(,camera.focus.focus_automatic_continuous,Bool(false))",
        ),
        (
            &["settings", "set", "camera.focus.pan_reset"],
            "SetSetting(,camera.focus.pan_reset,Bool(true))",
        ),
        (
            &["settings", "set", "camera.focus.pan_reset", "true"],
            "SetSetting(,camera.focus.pan_reset,Bool(true))",
        ),
        (
            &["settings", "set", "camera.focus.field_of_view", "90"],
            "SetSetting(,camera.focus.field_of_view,Str(\"90\"))",
        ),
        (
            &[
                "settings",
                "set",
                "--camera=usb-1234_abcd",
                "camera.exposure.power_line_frequency",
                "50hz",
            ],
            "SetSetting(usb-1234_abcd,camera.exposure.power_line_frequency,Str(\"50hz\"))",
        ),
    ] {
        let out = bus.run(args);
        assert!(out.status.success(), "{args:?}: {}", out.stderr);
        let calls = mock.take_calls();
        assert_eq!(calls.len(), 3, "{args:?}: {calls:?}");
        assert_eq!(calls[1], sent, "{args:?}");
    }
    let lines = json_lines(&bus.run(&["settings", "--json"]).stdout);
    for (id, value) in [
        ("camera.focus.pan_absolute", serde_json::json!(-3600)),
        ("camera.focus.tilt_absolute", serde_json::json!(-7200)),
        ("camera.image.brightness", serde_json::json!(40)),
        ("camera.exposure.keep_framerate", serde_json::json!(true)),
        ("camera.focus.field_of_view", serde_json::json!("90")),
        (
            "camera.exposure.power_line_frequency",
            serde_json::json!("50hz"),
        ),
    ] {
        let s = find_setting(&lines, id);
        assert_eq!((&s["value"], &s["stored"]), (&value, &true.into()), "{s}");
    }
    // Continuous autofocus off: the focus can be set.
    assert_eq!(
        find_setting(&lines, "camera.focus.focus_absolute")["enabled"],
        true
    );

    // The keep-30-fps switch as on/off; the action applied live while open.
    mock.set_state(State::Active, "");
    let out = bus.run(&["settings", "set", "camera.exposure.keep_framerate", "off"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        "Camera:     First Camera (1234:abcd)\n\
         Exposure\n  \
         Keep 30 fps in low light  off  camera.exposure.keep_framerate  on|off  (stored)\n\
         Applied live.\n"
    );
    let out = bus.run(&["settings", "set", "camera.focus.pan_reset"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        "Camera:     First Camera (1234:abcd)\n\
         Focus & zoom\n  \
         Recentre pan  action  camera.focus.pan_reset\n\
         Applied live.\n"
    );
    assert!(!bus.activation_attempted());
}

#[test]
fn settings_reset_by_camera_group_or_setting() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    for (id, value) in [
        ("camera.exposure.auto_exposure", "manual"),
        ("camera.exposure.gain", "30"),
        ("camera.image.sharpness", "200"),
        ("camera.image.contrast", "100"),
    ] {
        assert!(bus.run(&["settings", "set", id, value]).status.success());
    }
    mock.take_calls();

    let out = bus.run(&["settings", "reset", "--setting=camera.image.contrast"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        format!(
            "Camera:     First Camera (1234:abcd)\nImage\n  \
             Contrast  128  camera.image.contrast  0..255\n{NOT_IN_USE}"
        )
    );
    assert_eq!(
        mock.take_calls(),
        [
            "ResetSettings(setting:camera.image.contrast,)",
            "GetSettings()"
        ]
    );

    let out = bus.run(&["settings", "reset", "--group", "exposure"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        format!(
            "Camera:     First Camera (1234:abcd)\nExposure\n  \
             Exposure mode             Auto         camera.exposure.auto_exposure           \
             manual|aperture_priority\n  \
             Exposure time             156 \u{d7}100 \u{b5}s  camera.exposure.exposure_time_absolute  \
             3..2047                   (Set exposure to manual to change this)\n  \
             Gain                      0            camera.exposure.gain                    0..255\n  \
             Keep 30 fps in low light  on           camera.exposure.keep_framerate          on|off\n  \
             Anti-flicker              60 Hz        camera.exposure.power_line_frequency    \
             disabled|50hz|60hz\n{NOT_IN_USE}"
        )
    );
    assert_eq!(
        mock.take_calls(),
        ["ResetSettings(group:exposure,)", "GetSettings()"]
    );
    // The image group keeps its stored sharpness until the camera reset.
    let lines = json_lines(&bus.run(&["settings", "--json"]).stdout);
    assert_eq!(find_setting(&lines, "camera.image.sharpness")["value"], 200);
    mock.take_calls();

    mock.set_state(State::Active, "");
    let out = bus.run(&["settings", "reset", "--camera=usb-1234_abcd"]);
    assert!(out.status.success(), "{}", out.stderr);
    // Every setting is listed, back at its default; live, as it streams.
    assert!(
        out.stdout
            .starts_with("Camera:     First Camera (1234:abcd)\nImage\n")
    );
    assert!(
        out.stdout
            .ends_with(&format!("90|78|65\nApplied live.\n{NO_MODULES}")),
        "{}",
        out.stdout
    );
    assert_eq!(
        out.stdout.lines().count(),
        SETTINGS_TEXT.lines().count() + 2
    );
    assert!(!out.stdout.contains("(stored)"), "{}", out.stdout);
    assert_eq!(
        mock.take_calls(),
        [
            "ResetSettings(camera,usb-1234_abcd)",
            "GetSettings(usb-1234_abcd)"
        ]
    );
    let lines = json_lines(&bus.run(&["settings", "--json"]).stdout);
    for s in lines
        .iter()
        .flat_map(|v| v["group"]["settings"].as_array().unwrap())
    {
        assert_eq!(s["stored"], false, "{s}");
        assert_eq!(s.get("value"), s.get("default"), "{s}");
    }
    mock.take_calls();
    let out = bus.run(&["settings", "reset"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(mock.take_calls()[0], "ResetSettings(camera,)");
}

#[test]
fn the_mock_introspects_get_settings_like_the_daemon() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    let xml = mock.introspect(&bus);
    let start = xml
        .find("<method name=\"GetSettings\">")
        .unwrap_or_else(|| panic!("no GetSettings in {xml}"));
    let get = &xml[start..start + xml[start..].find("</method>").unwrap()];
    let outs: Vec<&str> = get
        .split("<arg ")
        .filter(|a| a.contains("direction=\"out\""))
        .collect();
    assert_eq!(outs.len(), 4, "{get}");
    for (arg, (name, ty)) in outs.iter().zip([
        ("revision", "u"),
        ("serial", "u"),
        ("camera", "s"),
        ("groups", "aa{sv}"),
    ]) {
        assert!(
            arg.contains(&format!("name=\"{name}\"")) && arg.contains(&format!("type=\"{ty}\"")),
            "{name} {ty} not in {arg}"
        );
    }
}

#[test]
fn settings_camera_goes_before_or_after_the_subcommand() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    let cam = "--camera=usb-1234_abcd";
    let id = "camera.focus.pan_absolute";
    for args in [
        &["settings", cam, "set", "--", id, "-3600"][..],
        &["settings", "set", cam, "--", id, "-3600"],
        &[
            "settings",
            "--camera",
            "usb-1234_abcd",
            "set",
            "--",
            id,
            "-3600",
        ],
        &["settings", "set", id, cam, "-3600"],
        &["settings", "set", id, "-3600", cam],
    ] {
        let out = bus.run(args);
        assert!(out.status.success(), "{args:?}: {}", out.stderr);
        assert_eq!(
            mock.take_calls(),
            [
                "GetSettings(usb-1234_abcd)",
                "SetSetting(usb-1234_abcd,camera.focus.pan_absolute,Int(-3600))",
                "GetSettings(usb-1234_abcd)"
            ],
            "{args:?}"
        );
    }
    for (args, sent) in [
        (
            &["settings", cam, "reset"][..],
            "ResetSettings(camera,usb-1234_abcd)",
        ),
        (
            &["settings", "reset", cam],
            "ResetSettings(camera,usb-1234_abcd)",
        ),
        (
            &["settings", cam, "reset", "--group=exposure"],
            "ResetSettings(group:exposure,usb-1234_abcd)",
        ),
        (
            &["settings", "reset", "--group=exposure", cam],
            "ResetSettings(group:exposure,usb-1234_abcd)",
        ),
    ] {
        let out = bus.run(args);
        assert!(out.status.success(), "{args:?}: {}", out.stderr);
        assert_eq!(
            mock.take_calls(),
            [sent, "GetSettings(usb-1234_abcd)"],
            "{args:?}"
        );
    }
    // `--` still ends the options: what follows is the id and the value,
    // however much it looks like `--camera`.
    let out = bus.run(&["settings", "set", "--", cam, "camera.image.brightness"]);
    assert_eq!(out.status.code(), Some(2), "{}", out.stderr);
    assert!(out.stderr.contains("not a setting id"), "{}", out.stderr);
    assert!(mock.take_calls().is_empty());
    let out = bus.run(&[
        "settings",
        cam,
        "set",
        "--",
        "camera.image.brightness",
        "--camera=x",
    ]);
    assert_eq!(out.status.code(), Some(2), "{}", out.stderr);
    assert_eq!(mock.take_calls(), ["GetSettings(usb-1234_abcd)"]);
    // `--json` with a subcommand stays refused, wherever `--camera` is.
    for args in [
        &[
            "settings",
            "--json",
            cam,
            "set",
            "camera.image.brightness",
            "1",
        ][..],
        &["settings", cam, "--json", "reset"],
        &["settings", "--json", "reset", cam],
    ] {
        let out = bus.run(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", out.stderr);
        assert!(
            out.stderr
                .starts_with("error: the argument '--json' cannot be used with a subcommand"),
            "{args:?}: {}",
            out.stderr
        );
        assert_eq!(out.stdout, "");
    }
    assert!(mock.take_calls().is_empty());
}

#[test]
fn settings_usage_errors_exit_2_before_any_write() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    // The command line alone is wrong: no call at all.
    for args in [
        &["settings", "set", "Camera.image.brightness", "1"][..],
        &["settings", "set", "camera", "1"],
        &["settings", "set", "camera.image.bright-ness", "1"],
        &["settings", "set", "", "1"],
        &["settings", "set", "camera.a.b.c.d", "1"],
        &["settings", "set"],
        &["settings", "set", "camera.image.brightness", "1", "2"],
        &["settings", "--json", "set", "camera.image.brightness", "1"],
        &["settings", "set", "--json", "camera.image.brightness", "1"],
        &["settings", "set", "camera.image.brightness", "1", "--json"],
        &["settings", "reset", "--json"],
        &["settings", "--json", "reset"],
        &["settings", "reset", "camera.image.brightness"],
        &["settings", "reset", "--group=Exposure"],
        &["settings", "reset", "--group="],
        &["settings", "reset", "--group=mic."],
        &["settings", "reset", "--group=mic.no.ise"],
        &["settings", "reset", "--setting=exposure"],
        &[
            "settings",
            "reset",
            "--group=exposure",
            "--setting=camera.image.brightness",
        ],
        &["settings", "frob"],
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
    assert!(mock.take_calls().is_empty());
    assert!(!bus.activation_attempted());

    // The value does not fit the setting's type: the tree is read, nothing
    // is written.
    for (args, message) in [
        (
            &[
                "settings",
                "set",
                "camera.exposure.auto_exposure",
                "shutter_priority",
            ][..],
            "error: invalid value 'shutter_priority' for camera.exposure.auto_exposure: one of \
             manual, aperture_priority\n",
        ),
        (
            &["settings", "set", "camera.exposure.auto_exposure"],
            "error: camera.exposure.auto_exposure needs a value: one of manual, \
             aperture_priority\n",
        ),
        (
            &["settings", "set", "camera.image.brightness", "1.5"],
            "error: invalid value '1.5' for camera.image.brightness: a whole number from 0 to \
             255\n",
        ),
        (
            &["settings", "set", "camera.image.brightness", "0x10"],
            "error: invalid value '0x10' for camera.image.brightness",
        ),
        (
            &["settings", "set", "camera.image.brightness", "ten"],
            "error: invalid value 'ten' for camera.image.brightness",
        ),
        (
            &["settings", "set", "camera.image.brightness", ""],
            "error: invalid value '' for camera.image.brightness",
        ),
        (
            &[
                "settings",
                "set",
                "camera.image.brightness",
                "99999999999999999999",
            ],
            "error: invalid value '99999999999999999999' for camera.image.brightness",
        ),
        (
            &["settings", "set", "camera.image.brightness"],
            "error: camera.image.brightness needs a value: a whole number from 0 to 255\n",
        ),
        (
            &["settings", "set", "camera.exposure.keep_framerate", "yes"],
            "error: invalid value 'yes' for camera.exposure.keep_framerate: it is a switch (on, \
             off, true or false)\n",
        ),
        (
            &["settings", "set", "camera.exposure.keep_framerate"],
            "error: camera.exposure.keep_framerate needs a value: on, off, true or false\n",
        ),
        (
            &["settings", "set", "camera.focus.pan_reset", "false"],
            "error: invalid value 'false' for camera.focus.pan_reset: it is an action (give no \
             value, or true)\n",
        ),
    ] {
        let out = bus.run(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", out.stderr);
        assert_eq!(out.stdout, "", "{args:?}");
        assert!(out.stderr.starts_with(message), "{args:?}: {}", out.stderr);
        assert_eq!(mock.take_calls(), ["GetSettings()"], "{args:?}: no write");
    }
    // Nothing was changed.
    let out = bus.run(&["settings"]);
    assert_eq!(
        out.stdout,
        format!("{SETTINGS_TEXT}{NOT_IN_USE}{NO_MODULES}")
    );
}

#[test]
fn settings_rejected_by_the_daemon_exit_1_with_its_message() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    let set = |id: &str, v: &str| {
        vec![
            "GetSettings()".to_string(),
            format!("SetSetting(,{id},{v})"),
        ]
    };
    for (args, message, calls) in [
        (
            &[
                "settings",
                "set",
                "camera.white_balance.white_balance_temperature",
                "4000",
            ][..],
            "Turn off automatic white balance to change this",
            set(
                "camera.white_balance.white_balance_temperature",
                "Int(4000)",
            ),
        ),
        (
            &["settings", "set", "camera.image.brightness", "300"],
            "camera.image.brightness: 300 is outside 0..=255",
            set("camera.image.brightness", "Int(300)"),
        ),
        (
            &[
                "settings",
                "set",
                "--",
                "camera.focus.pan_absolute",
                "-1000",
            ],
            "camera.focus.pan_absolute: -1000 is not on its step of 3600 from -36000",
            set("camera.focus.pan_absolute", "Int(-1000)"),
        ),
        (
            &[
                "settings",
                "set",
                "--camera=nope",
                "camera.image.brightness",
                "1",
            ],
            "no camera \"nope\" is present",
            vec!["GetSettings(nope)".to_string()],
        ),
        (
            &["settings", "reset", "--setting=camera.image.hue"],
            "the camera has no setting \"camera.image.hue\"",
            vec!["ResetSettings(setting:camera.image.hue,)".to_string()],
        ),
        (
            &["settings", "reset", "--group=other"],
            "no group \"other\"",
            vec!["ResetSettings(group:other,)".to_string()],
        ),
    ] {
        let out = bus.run(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", out.stderr);
        assert_eq!(out.stdout, "", "{args:?}");
        assert_eq!(
            out.stderr,
            format!("omarchy-broadcast: {message}\n"),
            "{args:?}"
        );
        // Nothing read back.
        assert_eq!(mock.take_calls(), calls, "{args:?}");
    }
    // A setting the camera does not have: found before any write.
    let out = bus.run(&["settings", "set", "camera.image.hue", "1"]);
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr);
    assert_eq!(
        out.stderr,
        "omarchy-broadcast: the camera \"usb-1234_abcd\" has no setting \"camera.image.hue\"\n"
    );
    assert_eq!(mock.take_calls(), ["GetSettings()"]);
    // The daemon's own refusals of a valid value.
    mock.fail_next_set("RateLimited");
    let out = bus.run(&["settings", "set", "camera.image.brightness", "10"]);
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr);
    assert_eq!(
        out.stderr,
        "omarchy-broadcast: too many changes to the camera: try again in a moment\n"
    );
    mock.fail_next_set("UnknownSetting");
    let out = bus.run(&["settings", "set", "camera.image.brightness", "10"]);
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr);
    assert_eq!(
        out.stderr,
        "omarchy-broadcast: the camera has no setting \"camera.image.brightness\"\n"
    );
    assert_eq!(
        mock.take_calls(),
        [
            "GetSettings()",
            "SetSetting(,camera.image.brightness,Int(10))",
            "GetSettings()",
            "SetSetting(,camera.image.brightness,Int(10))"
        ]
    );
    // Nothing was changed.
    let out = bus.run(&["settings"]);
    assert_eq!(
        out.stdout,
        format!("{SETTINGS_TEXT}{NOT_IN_USE}{NO_MODULES}")
    );
}

#[test]
fn settings_never_start_the_daemon_but_set_and_reset_may() {
    let bus = Bus::start();
    let out = bus.run(&["settings"]);
    assert_eq!(out.status.code(), Some(3), "{}", out.stderr);
    assert!(out.stderr.contains("is not running"), "{}", out.stderr);
    let out = bus.run(&["settings", "--json", "--camera=x"]);
    assert_eq!(out.status.code(), Some(3), "{}", out.stderr);
    let v = json_line(&out.stdout);
    assert_eq!(v["type"], "error");
    assert_eq!(v["code"], "daemon-not-running");
    assert!(!bus.activation_attempted(), "`settings` must not start it");

    for args in [
        &["settings", "set", "camera.image.brightness", "1"][..],
        &["settings", "reset"],
    ] {
        let out = bus.run(args);
        assert_eq!(out.status.code(), Some(3), "{args:?}: {}", out.stderr);
        assert!(
            out.stderr.contains("D-Bus could not start it"),
            "{}",
            out.stderr
        );
    }
    assert!(bus.activation_attempted(), "`settings set` may start it");
}

#[test]
fn settings_against_a_revision_6_daemon_say_they_are_unsupported() {
    let bus = Bus::start();
    let _daemon = OldDaemon::start(&bus, Rev6);
    let expected = "omarchy-broadcast: the daemon does not support camera settings (its \
                    interface revision is 6, camera settings need 7); update omarchy-broadcastd\n";
    for args in [
        &["settings"][..],
        &["settings", "set", "camera.image.brightness", "1"],
        &["settings", "reset", "--group=image"],
    ] {
        let out = bus.run(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", out.stderr);
        assert_eq!(out.stdout, "", "{args:?}");
        assert_eq!(out.stderr, expected, "{args:?}");
    }
    let out = bus.run(&["settings", "--json"]);
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr);
    assert_eq!(out.stderr, "");
    let v = json_line(&out.stdout);
    assert_eq!(v["type"], "error");
    assert_eq!(v["code"], "unsupported-daemon");
    assert_eq!(
        v["error"],
        expected
            .trim_start_matches("omarchy-broadcast: ")
            .trim_end_matches('\n')
    );

    // Its snapshot has neither: `null`, with the keys present; `watch`
    // prints nothing of the settings.
    let v = json_line(&bus.run(&["status", "--json"]).stdout);
    assert_eq!(v["interface_revision"], 6);
    for key in ["settings_revision", "settings_schema"] {
        assert_eq!(v.get(key), Some(&Value::Null), "{key}: {v}");
    }
    let mut watch = bus.watch(&["watch", "--json"], None);
    let v = watch.next_json("the snapshot of a revision-6 daemon");
    assert_eq!(v["type"], "snapshot", "{v}");
    assert_eq!(v["settings_schema"], Value::Null);
    watch.quiet_for(Duration::from_millis(500), "no settings from revision 6");
    watch.child.signal(libc::SIGTERM);
    let (status, stderr) = watch.finish("watch --json");
    assert_eq!(status.code(), Some(0), "{status:?}");
    assert_eq!(stderr, "");
}

/// The next `n` lines of `watch`, sorted by their type (lines from
/// different signals may come in either order).
fn next_lines_by_type(watch: &Watch, n: usize, what: &str) -> Vec<Value> {
    let mut lines: Vec<Value> = (0..n).map(|_| watch.next_json(what)).collect();
    lines.sort_by_key(|v| v["type"].as_str().unwrap().to_string());
    lines
}

#[test]
fn watch_json_follows_the_settings() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    let watch = bus.watch(&["watch", "--json"], None);

    let v = watch.next_json("the snapshot");
    assert_eq!(v["type"], "snapshot", "{v}");
    assert_eq!(
        (&v["settings_revision"], &v["settings_schema"]),
        (&1.into(), &1.into())
    );
    let set = watch.settings_set("the settings after the snapshot");
    assert_eq!(
        set_groups(&set),
        ["image", "white_balance", "exposure", "focus"]
    );
    assert_eq!(set[2], exposure_group_line(1, 1));

    // A change, and the dependency it flips, in one delta.
    let out = bus.run(&["settings", "set", "camera.exposure.auto_exposure", "manual"]);
    assert!(out.status.success(), "{}", out.stderr);
    let v = watch.next_json("the delta");
    assert_eq!(
        v,
        serde_json::json!({
            "v": 1, "type": "settings_delta", "schema": 1, "rev": 2, "serial": 1,
            "camera": "usb-1234_abcd",
            "changes": [
                {"id": "camera.exposure.auto_exposure", "value": "manual", "stored": true,
                 "enabled": true, "disabled_reason": "", "disabled_kind": ""},
                {"id": "camera.exposure.exposure_time_absolute", "value": 1250,
                 "stored": false, "enabled": true, "disabled_reason": "", "disabled_kind": ""},
            ],
        })
    );
    // An action has no value.
    assert!(
        bus.run(&["settings", "set", "camera.focus.pan_reset"])
            .status
            .success()
    );
    let v = watch.next_json("the action's delta");
    assert_eq!(
        v["changes"],
        serde_json::json!([{"id": "camera.focus.pan_reset", "stored": false,
                            "enabled": true, "disabled_reason": "", "disabled_kind": ""}])
    );
    assert_eq!(v["rev"], 3);

    // A structural change: the set again, in full.
    mock.restructure_settings();
    let set = watch.settings_set("the set after a serial change");
    assert_eq!(set.len(), 4);
    assert!(
        set.iter().all(|v| v["rev"] == 4 && v["serial"] == 2),
        "{set:?}"
    );
    assert_eq!(
        find_setting(&set, "camera.exposure.auto_exposure")["value"],
        "manual"
    );

    // A missed signal: the next revision is not the one after the last.
    mock.change_setting("camera.exposure.gain", 30, true);
    let set = watch.settings_set("the set after a revision gap");
    assert!(
        set.iter().all(|v| v["rev"] == 6 && v["serial"] == 2),
        "{set:?}"
    );
    assert_eq!(find_setting(&set, "camera.exposure.gain")["value"], 30);
    // And the next one in order is a delta again.
    mock.change_setting("camera.exposure.gain", 40, false);
    let v = watch.next_json("a delta after the gap");
    assert_eq!(v["type"], "settings_delta", "{v}");
    assert_eq!((&v["rev"], &v["serial"]), (&7.into(), &2.into()));
    assert_eq!(v["changes"][0]["value"], 40);
    assert_eq!(mock.settings_marks(), (7, 2));

    // Another camera (the daemon bumps the serial): its set, here none.
    assert!(bus.run(&["camera", "platform-video2"]).status.success());
    let lines = next_lines_by_type(&watch, 2, "the camera switch");
    assert_eq!(lines[0]["type"], "settings_clear", "{lines:?}");
    assert_eq!(
        (&lines[0]["rev"], &lines[0]["serial"]),
        (&8.into(), &3.into())
    );
    assert_eq!(lines[1]["type"], "state", "{lines:?}");
    assert!(lines[1].get("settings_revision").is_none(), "{lines:?}");

    // Through an outage: a fresh snapshot, then the set.
    mock.release_name();
    assert_eq!(watch.next_json("the outage")["type"], "error");
    mock.request_name();
    assert_eq!(watch.next_json("the snapshot after it")["type"], "snapshot");
    assert_eq!(
        watch.settings_set("the set after the outage"),
        [serde_json::json!({"v": 1, "type": "settings_clear", "rev": 8, "serial": 3})]
    );
    assert!(bus.run(&["camera", "next"]).status.success());
    // The state line and the set (four lines) of the first camera.
    let mut seen = Vec::new();
    for _ in 0..5 {
        seen.push(watch.next_json("the switch back"));
    }
    let set: Vec<Value> = seen
        .iter()
        .filter(|v| v["type"] == "settings_group")
        .cloned()
        .collect();
    assert_eq!(
        set_groups(&set),
        ["image", "white_balance", "exposure", "focus"]
    );
    assert!(set.iter().all(|v| v["serial"] == 4));
    assert_eq!(seen.iter().filter(|v| v["type"] == "state").count(), 1);

    // No camera: a clear line.
    let mut watch = watch;
    watch.child.signal(libc::SIGTERM);
    let (status, stderr) = watch.finish("watch --json");
    assert_eq!(status.code(), Some(0), "{status:?}");
    assert_eq!(stderr, "");
    mock.remove_cameras();
    let watch = bus.watch(&["watch", "--json"], None);
    assert_eq!(watch.next_json("the snapshot")["type"], "snapshot");
    assert_eq!(watch.settings_set("no camera")[0]["type"], "settings_clear");
    let mut watch = watch;
    watch.child.signal(libc::SIGTERM);
    let _ = watch.finish("watch --json");
    assert!(
        !bus.activation_attempted(),
        "watch must not start the daemon"
    );
}

#[test]
fn watch_for_a_person_prints_nothing_of_the_settings() {
    let bus = Bus::start();
    let _mock = MockDaemon::start(&bus);
    let mut watch = bus.watch(&["watch"], None);
    assert!(
        watch
            .next("the summary")
            .starts_with("omarchy-broadcastd: state idle")
    );
    assert!(
        bus.run(&["settings", "set", "camera.image.brightness", "1"])
            .status
            .success()
    );
    watch.quiet_for(Duration::from_millis(500), "a settings change");
    watch.child.signal(libc::SIGTERM);
    let (status, stderr) = watch.finish("watch");
    assert_eq!(status.code(), Some(0), "{status:?}");
    assert_eq!(stderr, "");
}

#[test]
fn settings_lines_stay_under_the_cap_with_hostile_strings() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    let evil = "\u{1b}]0;x\u{7}\u{202e}\"\\".repeat(60);
    let hostile = |i: usize| {
        let mut s = choice(&format!("camera.other.s{i}"), &evil, &[], &evil, &evil);
        if let SettingKind::Enum { options, .. } = &mut s.kind {
            *options = (0..30)
                .map(|j| (format!("{j}{evil}"), evil.clone()))
                .collect();
        }
        s.enabled = false;
        s.disabled_reason = evil.clone();
        s.enabled_when = (0..9)
            .map(|_| Condition {
                id: evil.clone(),
                op: ConditionOp::In,
                value: SettingValue::List(
                    (0..30).map(|_| SettingValue::Str(evil.clone())).collect(),
                ),
            })
            .collect();
        s
    };
    mock.set_settings(vec![SettingsGroup {
        id: "other".into(),
        label: evil.clone(),
        settings: (0..30).map(hostile).collect(),
        surface: None,
    }]);
    let out = bus.run(&["settings", "--json"]);
    assert!(out.status.success(), "{}", out.stderr);
    // `json_line` checks every line is one valid object of at most 4096 bytes.
    let lines = json_lines(&out.stdout);
    let mut n = 0;
    for (i, v) in lines.iter().enumerate() {
        assert_eq!(v["index"], i);
        assert_eq!(v["count"], lines.len());
        assert_eq!(v["parts"], lines.len());
        assert_eq!(v["part"], i + 1);
        n += v["group"]["settings"].as_array().unwrap().len();
    }
    assert_eq!(n, 30);
    // For a person: escaped, one line per setting.
    let out = bus.run(&["settings"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout.lines().count(),
        1 + 1 + 30 + 1 + 1,
        "{}",
        out.stdout
    );
    assert!(
        !out.stdout
            .chars()
            .any(|c| (c.is_control() && c != '\n') || c == '\u{202e}'),
        "{:?}",
        out.stdout
    );
}

// ---- the global settings (revision 8): the video effects ----

const BLUR_REASON: &str = "Choose Blur to change this";

/// The global groups a revision-8 daemon publishes after the camera's.
fn mock_global() -> Vec<SettingsGroup> {
    let global = |mut s: Setting| {
        s.scope = "global".into();
        s
    };
    vec![
        SettingsGroup {
            id: "background".into(),
            label: "Background effect".into(),
            settings: vec![
                global(choice(
                    "effect.background.mode",
                    "Effect",
                    &[("none", "Off"), ("blur", "Blur"), ("replace", "Replace")],
                    "none",
                    "none",
                )),
                global(with_unit(
                    depends(
                        int(
                            "effect.background.blur_strength",
                            "Blur strength",
                            (0, 100, 1),
                            50,
                            50,
                        ),
                        "effect.background.mode",
                        ConditionOp::Eq,
                        SettingValue::Str("blur".into()),
                    ),
                    "%",
                )),
            ],
            surface: None,
        },
        SettingsGroup {
            id: "video".into(),
            label: "Video".into(),
            settings: vec![global(switch(
                "effect.video.denoise",
                "Noise removal",
                false,
                false,
            ))],
            surface: None,
        },
    ]
}

const GLOBAL_TEXT: &str = "All cameras:\n\
    Background effect\n  \
    Effect         Off   effect.background.mode           none|blur|replace\n  \
    Blur strength  50 %  effect.background.blur_strength  0..100             (Choose Blur to change this)\n\
    Video\n  \
    Noise removal  off   effect.video.denoise             on|off\n";

#[test]
fn settings_show_the_global_settings_after_the_cameras_and_without_one() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    mock.set_global(mock_global());
    let out = bus.run(&["settings"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        format!("{SETTINGS_TEXT}{NOT_IN_USE}{GLOBAL_TEXT}")
    );
    let out = bus.run(&["settings", "--json"]);
    assert!(out.status.success(), "{}", out.stderr);
    let lines = json_lines(&out.stdout);
    let groups: Vec<&str> = lines
        .iter()
        .map(|v| v["group"]["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        groups,
        [
            "image",
            "white_balance",
            "exposure",
            "focus",
            "background",
            "video"
        ]
    );
    assert_eq!(
        find_setting(&lines, "effect.background.blur_strength"),
        &serde_json::json!({"id": "effect.background.blur_strength", "scope": "global", "label": "Blur strength",
            "type": "int", "value": 50, "default": 50, "stored": false, "enabled": false,
            "disabled_reason": BLUR_REASON, "disabled_kind": "parent_off",
            "enabled_when": [{"id": "effect.background.mode", "op": "eq", "value": "blur"}],
            "min": 0, "max": 100, "step": 1, "unit": "%", "hint": "slider"})
    );

    // No camera: the global settings alone; no clear line.
    mock.remove_cameras();
    let out = bus.run(&["settings", "--json"]);
    assert!(out.status.success(), "{}", out.stderr);
    let lines = json_lines(&out.stdout);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines
            .iter()
            .all(|v| v["type"] == "settings_group" && v["camera"] == ""),
        "{lines:?}"
    );
    let out = bus.run(&["settings"]);
    assert_eq!(
        out.stdout,
        format!("Camera:     none\nNo camera settings.\n{NOT_IN_USE}{GLOBAL_TEXT}")
    );
}

#[test]
fn settings_set_and_reset_take_global_ids() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    mock.set_global(mock_global());
    let out = bus.run(&["settings", "set", "--", "effect.video.denoise", "on"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        "All cameras:\nVideo\n  \
         Noise removal  on  effect.video.denoise  on|off  (stored)\n"
    );
    assert_eq!(
        mock.take_calls(),
        [
            "GetSettings()",
            "SetSetting(,effect.video.denoise,Bool(true))",
            "GetSettings()"
        ]
    );
    // Typed from the tree: a choice, then the number it enables.
    let out = bus.run(&["settings", "set", "effect.background.mode", "blur"]);
    assert!(out.status.success(), "{}", out.stderr);
    let out = bus.run(&["settings", "set", "effect.background.blur_strength", "70"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert!(
        out.stdout
            .contains("Blur strength  70 %  effect.background.blur_strength"),
        "{}",
        out.stdout
    );
    let out = bus.run(&["settings", "set", "effect.background.mode", "sepia"]);
    assert_eq!(out.status.code(), Some(2), "{}", out.stderr);
    assert!(
        out.stderr.contains("one of none, blur, replace"),
        "{}",
        out.stderr
    );
    mock.take_calls();
    // A reset of one global setting and of a global group.
    let out = bus.run(&[
        "settings",
        "reset",
        "--setting=effect.background.blur_strength",
    ]);
    assert!(out.status.success(), "{}", out.stderr);
    assert!(
        out.stdout
            .starts_with("All cameras:\nBackground effect\n  Blur strength  50 %"),
        "{}",
        out.stdout
    );
    let out = bus.run(&["settings", "reset", "--group=video"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        "All cameras:\nVideo\n  \
         Noise removal  off  effect.video.denoise  on|off\n"
    );
    assert_eq!(
        mock.take_calls(),
        [
            "ResetSettings(setting:effect.background.blur_strength,)",
            "GetSettings()",
            "ResetSettings(group:video,)",
            "GetSettings()"
        ]
    );
}

#[test]
fn watch_json_carries_the_global_groups_in_the_set_and_their_deltas() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    mock.set_global(mock_global());
    let watch = bus.watch(&["watch", "--json"], None);
    assert_eq!(watch.next_json("the snapshot")["type"], "snapshot");
    let set = watch.settings_set("the settings after the snapshot");
    assert_eq!(
        set_groups(&set),
        [
            "image",
            "white_balance",
            "exposure",
            "focus",
            "background",
            "video"
        ]
    );
    let out = bus.run(&["settings", "set", "effect.background.mode", "blur"]);
    assert!(out.status.success(), "{}", out.stderr);
    let v = watch.next_json("the delta");
    assert_eq!(v["type"], "settings_delta", "{v}");
    let ids: Vec<&str> = v["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        ["effect.background.mode", "effect.background.blur_strength"]
    );
}

#[test]
fn global_ids_ignore_the_camera_option_and_need_no_camera() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    mock.set_global(mock_global());
    // A camera that is not there: typed and read back from the global
    // groups, the camera passed on (the daemon ignores it).
    let out = bus.run(&[
        "settings",
        "set",
        "--camera=usb-gone",
        "effect.video.denoise",
        "on",
    ]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        "All cameras:\nVideo\n  \
         Noise removal  on  effect.video.denoise  on|off  (stored)\n"
    );
    assert_eq!(
        mock.take_calls(),
        [
            "GetSettings()",
            "SetSetting(usb-gone,effect.video.denoise,Bool(true))",
            "GetSettings()"
        ]
    );
    let out = bus.run(&[
        "settings",
        "reset",
        "--camera=usb-gone",
        "--setting=effect.video.denoise",
    ]);
    assert!(out.status.success(), "{}", out.stderr);
    assert!(out.stdout.contains("Noise removal  off"), "{}", out.stdout);
    mock.take_calls();
    // No camera at all: an unknown global id is unknown, not "no camera".
    mock.remove_cameras();
    let out = bus.run(&["settings", "set", "effect.video.nope", "on"]);
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr);
    assert_eq!(
        out.stderr,
        "omarchy-broadcast: there is no setting \"effect.video.nope\"\n"
    );
    assert_eq!(mock.take_calls(), ["GetSettings()"], "nothing written");
    // ... and a known one works.
    let out = bus.run(&["settings", "set", "effect.background.mode", "blur"]);
    assert!(out.status.success(), "{}", out.stderr);
    // A camera id is still a camera's: with no camera, no camera is there.
    let out = bus.run(&["settings", "set", "camera.image.brightness", "10"]);
    assert_eq!(out.stderr, "omarchy-broadcast: no camera is present\n");
}

/// [`mock_global`] with the enhance switch after noise removal, greyed or
/// not as `reason` says.
fn mock_global_with_enhance(reason: &str) -> Vec<SettingsGroup> {
    let mut groups = mock_global();
    let mut enhance = switch(
        "effect.video.enhance",
        "Enhance low-resolution camera",
        false,
        false,
    );
    enhance.scope = "global".into();
    enhance.enabled = reason.is_empty();
    enhance.disabled_reason = reason.into();
    enhance.disabled_kind = if reason.is_empty() {
        ""
    } else {
        "not_applicable"
    }
    .into();
    groups[1].settings.push(enhance);
    groups
}

#[test]
fn settings_show_and_set_the_enhance_switch() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    mock.set_global(mock_global_with_enhance(""));
    let out = bus.run(&["settings", "--json"]);
    assert!(out.status.success(), "{}", out.stderr);
    let lines = json_lines(&out.stdout);
    assert_eq!(
        find_setting(&lines, "effect.video.enhance"),
        &serde_json::json!({"id": "effect.video.enhance", "scope": "global",
            "label": "Enhance low-resolution camera", "type": "bool", "value": false,
            "default": false, "stored": false, "enabled": true, "disabled_reason": "", "disabled_kind": "",
            "enabled_when": [], "hint": "switch"})
    );
    let out = bus.run(&["settings", "set", "--", "effect.video.enhance", "on"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        "All cameras:\nVideo\n  \
         Enhance low-resolution camera  on  effect.video.enhance  on|off  (stored)\n"
    );
    assert_eq!(
        mock.take_calls(),
        [
            "GetSettings()",
            "GetSettings()",
            "SetSetting(,effect.video.enhance,Bool(true))",
            "GetSettings()"
        ]
    );
    // Greyed for a camera that needs none: the reason shows.
    mock.set_global(mock_global_with_enhance(
        "The camera already captures at full resolution",
    ));
    let out = bus.run(&["settings"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert!(
        out.stdout
            .contains("(The camera already captures at full resolution)"),
        "{}",
        out.stdout
    );
}

/// [`mock_global`] with the lighting group after the video one, its light
/// settings enabled only while relighting is `on`.
fn mock_global_with_lighting(on: bool) -> Vec<SettingsGroup> {
    let mut groups = mock_global();
    let global = |mut s: Setting| {
        s.scope = "global".into();
        if s.id != "effect.lighting.relight" {
            s = depends(
                s,
                "effect.lighting.relight",
                ConditionOp::Eq,
                SettingValue::Bool(true),
            );
            s.enabled = on;
            s.disabled_reason = if on {
                String::new()
            } else {
                LIGHT_REASON.into()
            };
            s.disabled_kind = if on { "" } else { "parent_off" }.into();
        }
        s
    };
    groups.push(SettingsGroup {
        id: "lighting".into(),
        label: "Lighting".into(),
        settings: vec![
            global(switch("effect.lighting.relight", "Relighting", false, on)),
            global(choice(
                "effect.lighting.light",
                "Light",
                &[("warm", "Warm"), ("neutral", "Neutral"), ("cool", "Cool")],
                "neutral",
                "neutral",
            )),
            global(with_unit(
                int(
                    "effect.lighting.direction",
                    "Light direction",
                    (-90, 90, 5),
                    0,
                    0,
                ),
                "°",
            )),
            global(with_unit(
                int(
                    "effect.lighting.brightness",
                    "Brightness",
                    (50, 200, 5),
                    100,
                    100,
                ),
                "%",
            )),
        ],
        surface: None,
    });
    groups
}

const LIGHT_REASON: &str = "Turn on relighting to change this";

#[test]
fn settings_show_the_lighting_group_and_set_a_negative_direction() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    mock.set_global(mock_global_with_lighting(false));
    let out = bus.run(&["settings"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert!(
        out.stdout.ends_with(
            "Lighting\n  \
             Relighting       off      effect.lighting.relight          on|off\n  \
             Light            Neutral  effect.lighting.light            warm|neutral|cool  \
             (Turn on relighting to change this)\n  \
             Light direction  0 °      effect.lighting.direction        -90..90 step 5     \
             (Turn on relighting to change this)\n  \
             Brightness       100 %    effect.lighting.brightness       50..200 step 5     \
             (Turn on relighting to change this)\n"
        ),
        "{}",
        out.stdout
    );
    let out = bus.run(&["settings", "--json"]);
    assert!(out.status.success(), "{}", out.stderr);
    let lines = json_lines(&out.stdout);
    let groups: Vec<&str> = lines
        .iter()
        .map(|v| v["group"]["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        groups,
        [
            "image",
            "white_balance",
            "exposure",
            "focus",
            "background",
            "video",
            "lighting"
        ]
    );
    assert_eq!(
        find_setting(&lines, "effect.lighting.direction"),
        &serde_json::json!({"id": "effect.lighting.direction", "scope": "global",
            "label": "Light direction", "type": "int", "value": 0, "default": 0,
            "stored": false, "enabled": false, "disabled_reason": LIGHT_REASON, "disabled_kind": "parent_off",
            "enabled_when": [{"id": "effect.lighting.relight", "op": "eq", "value": true}],
            "min": -90, "max": 90, "step": 5, "unit": "°", "hint": "slider"})
    );
    for id in [
        "effect.lighting.relight",
        "effect.lighting.light",
        "effect.lighting.brightness",
    ] {
        assert_eq!(find_setting(&lines, id)["scope"], "global", "{id}");
    }
    // On: a negative direction after `--`, typed from the tree.
    mock.set_global(mock_global_with_lighting(true));
    mock.take_calls();
    let out = bus.run(&["settings", "set", "--", "effect.lighting.direction", "-30"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        "All cameras:\nLighting\n  \
         Light direction  -30 °  effect.lighting.direction  -90..90 step 5  (stored)\n"
    );
    assert_eq!(
        mock.take_calls(),
        [
            "GetSettings()",
            "SetSetting(,effect.lighting.direction,Int(-30))",
            "GetSettings()"
        ]
    );
    // The light by its option id, the brightness, and a light that is none.
    let out = bus.run(&["settings", "set", "effect.lighting.light", "warm"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert!(out.stdout.contains("Light  Warm"), "{}", out.stdout);
    let out = bus.run(&["settings", "set", "effect.lighting.brightness", "150"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert!(out.stdout.contains("150 %"), "{}", out.stdout);
    let out = bus.run(&["settings", "set", "effect.lighting.light", "pink"]);
    assert_eq!(out.status.code(), Some(2), "{}", out.stderr);
    assert!(
        out.stderr.contains("one of warm, neutral, cool"),
        "{}",
        out.stderr
    );
}

/// `modules` and `order` against a revision-9 daemon: none installed, then
/// one; the reads start nothing, the changes are the methods, an invalid
/// order is the daemon's rejection.
#[test]
fn modules_and_order_list_change_and_refuse() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    let out = bus.run(&["modules"]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    assert_eq!(out.stdout, "No effect modules installed\n");
    let out = bus.run(&["modules", "list", "--json"]);
    assert_eq!(
        json_lines(&out.stdout),
        [
            serde_json::json!({"v": 1, "type": "modules", "serial": 1, "profile": "quality", "count": 0, "profile_overridden_by": ""})
        ]
    );
    mock.with(|m| {
        m.modules = vec![obc_ipc::ModuleInfo {
            id: "br.com.dias.maxine-vfx".into(),
            name: "NVIDIA Maxine video effects".into(),
            version: "0.24.0".into(),
            enabled: true,
            status: "available".into(),
            stages: vec![obc_ipc::StageInfo {
                id: "background".into(),
                slot: "matte".into(),
                rank: 300,
                device: "nvidia-gpu".into(),
                available: true,
                selected: true,
                ..obc_ipc::StageInfo::default()
            }],
            ..obc_ipc::ModuleInfo::default()
        }];
        m.order = vec!["denoise".into(), "background".into(), "relight".into()];
    });
    mock.take_calls();
    let out = bus.run(&["modules", "list"]);
    assert_eq!(
        out.stdout,
        "Profile: quality\nbr.com.dias.maxine-vfx  NVIDIA Maxine video effects 0.24.0  available\n\
         \x20   background (matte, nvidia-gpu, rank 300) selected\n"
    );
    let out = bus.run(&["modules", "info", "br.com.dias.maxine-vfx", "--json"]);
    let v = json_line(&out.stdout);
    assert_eq!(v["type"], "module");
    assert_eq!(v["module"]["stages"][0]["id"], "background");
    let out = bus.run(&["modules", "info", "org.x.none"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        out.stderr.contains("No module org.x.none is installed"),
        "{}",
        out.stderr
    );
    // A daemon of revision 10: the config schema says what a later file
    // sets.
    assert_eq!(
        mock.take_calls(),
        [
            "GetModules()",
            "GetConfigSchema()",
            "GetModules()",
            "GetConfigSchema()",
            "GetModules()",
            "GetConfigSchema()"
        ]
    );
    let out = bus.run(&["modules", "disable", "br.com.dias.maxine-vfx"]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    assert!(
        out.stdout.contains("Status:      disabled: Disabled\n"),
        "{}",
        out.stdout
    );
    let out = bus.run(&["modules", "profile", "battery"]);
    assert_eq!(out.stdout, "Profile: battery\n");
    let out = bus.run(&["modules", "pin", "background", "--clear"]);
    assert_eq!(
        out.stdout,
        "background: provided by NVIDIA Maxine video effects (br.com.dias.maxine-vfx)\n"
    );
    assert_eq!(
        mock.take_calls(),
        [
            "SetModuleEnabled(br.com.dias.maxine-vfx, false)",
            "GetModules()",
            "GetConfigSchema()",
            "SetBackendProfile(battery)",
            "GetModules()",
            "GetConfigSchema()",
            "SetStagePin(background, )",
            "GetModules()",
            "GetConfigSchema()"
        ]
    );
    let out = bus.run(&["order"]);
    assert_eq!(out.stdout, "1. denoise\n2. background\n3. relight\n");
    let out = bus.run(&["order", "show", "--json"]);
    assert_eq!(json_line(&out.stdout)["order"][1], "background");
    let out = bus.run(&["order", "set", "relight,denoise,background"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stderr.contains("cannot come before"), "{}", out.stderr);
    let out = bus.run(&["order", "set", "background,denoise,relight"]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    assert_eq!(out.stdout, "1. background\n2. denoise\n3. relight\n");
    let out = bus.run(&["order", "reset"]);
    assert_eq!(out.stdout, "1. denoise\n2. background\n3. relight\n");
    assert_eq!(
        mock.take_calls(),
        [
            "GetStageOrderMoves()",
            "GetConfigSchema()",
            "GetStageOrderMoves()",
            "GetConfigSchema()",
            "SetStageOrder(relight,denoise,background)",
            "SetStageOrder(background,denoise,relight)",
            "GetStageOrderMoves()",
            "GetConfigSchema()",
            "SetStageOrder()",
            "GetStageOrderMoves()",
            "GetConfigSchema()"
        ]
    );
}

/// `modules enable --force`, the `forced` field of the list and of `info`,
/// and the plain enable that clears a force.
#[test]
fn modules_force_is_shown_set_cleared_and_refused() {
    const ID: &str = "br.com.dias.maxine-vfx";
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    mock.with(|m| {
        m.modules = vec![
            obc_ipc::ModuleInfo {
                id: ID.into(),
                name: "NVIDIA Maxine video effects".into(),
                version: "0.24.0".into(),
                enabled: false,
                status: "disabled".into(),
                reason: "Disabled".into(),
                ..obc_ipc::ModuleInfo::default()
            },
            obc_ipc::ModuleInfo {
                id: "org.x.audio".into(),
                name: "Audio".into(),
                enabled: false,
                status: "disabled".into(),
                ..obc_ipc::ModuleInfo::default()
            },
        ];
        m.config[2].rows.push(config_row(
            &format!("modules.forced.{ID}"),
            "Forced",
            obc_ipc::ConfigKind::Bool,
            obc_ipc::ConfigValue::Bool(false),
        ));
    });
    // Not forced: nothing says so.
    let out = bus.run(&["modules", "list"]);
    assert!(!out.stdout.contains("forced"), "{}", out.stdout);
    let out = bus.run(&["modules", "list", "--json"]);
    let lines = json_lines(&out.stdout);
    assert_eq!(lines[1]["module"]["forced"], false);
    mock.take_calls();

    let out = bus.run(&["modules", "enable", ID, "--force"]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    assert!(
        out.stdout.contains(&format!(
            "Forced: conflicts are not enforced (modules.forced.{ID})\n"
        )),
        "{}",
        out.stdout
    );
    assert_eq!(
        mock.take_calls(),
        [
            "GetModules()",
            format!("SetConfig(modules.forced.{ID}, Some(Bool(true)))").as_str(),
            &format!("SetModuleEnabled({ID}, true)"),
            "GetModules()",
            "GetConfigSchema()"
        ]
    );
    let out = bus.run(&["modules", "list"]);
    assert!(
        out.stdout.contains("available forced\n"),
        "list: {}",
        out.stdout
    );
    let out = bus.run(&["modules", "list", "--json"]);
    assert_eq!(json_lines(&out.stdout)[1]["module"]["forced"], true);
    let out = bus.run(&["modules", "info", ID, "--json"]);
    assert_eq!(json_line(&out.stdout)["module"]["forced"], true);
    let out = bus.run(&["modules", "info", ID]);
    assert!(
        out.stdout.contains(&format!(
            "Forced: conflicts are not enforced (modules.forced.{ID})\n"
        )),
        "{}",
        out.stdout
    );

    // Disable leaves the force alone.
    mock.take_calls();
    let out = bus.run(&["modules", "disable", ID]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    assert_eq!(
        mock.take_calls(),
        [
            &format!("SetModuleEnabled({ID}, false)")[..],
            "GetModules()",
            "GetConfigSchema()"
        ]
    );

    // A plain enable clears the force.
    let out = bus.run(&["modules", "enable", ID]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    assert!(!out.stdout.contains("Forced:"), "{}", out.stdout);
    assert_eq!(
        mock.take_calls(),
        [
            "GetModules()",
            &format!("SetModuleEnabled({ID}, true)")[..],
            &format!("SetConfig(modules.forced.{ID}, Some(Bool(false)))"),
            "GetModules()",
            "GetConfigSchema()"
        ]
    );

    // A plain enable of a module that is not forced writes no config.
    let out = bus.run(&["modules", "enable", ID]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    assert_eq!(
        mock.take_calls(),
        [
            "GetModules()",
            &format!("SetModuleEnabled({ID}, true)")[..],
            "GetModules()",
            "GetConfigSchema()"
        ]
    );

    // The daemon refuses the key (an audio module has none): its error, the
    // exit code of a refused `config set`.
    let refused = bus.run(&["config", "set", "modules.forced.org.x.audio", "true"]);
    let out = bus.run(&["modules", "enable", "org.x.audio", "--force"]);
    assert_ne!(out.status.code(), Some(0), "{}", out.stdout);
    assert_eq!(out.status.code(), refused.status.code(), "{}", out.stderr);
    assert!(
        out.stderr
            .contains("No configuration key modules.forced.org.x.audio"),
        "{}",
        out.stderr
    );
    // Refused, so nothing changed: the module is still disabled.
    let list = bus.run(&["modules", "info", "org.x.audio", "--json"]);
    assert_eq!(
        json_line(&list.stdout)["module"]["enabled"],
        false,
        "{}",
        list.stdout
    );
    assert!(
        !mock
            .take_calls()
            .iter()
            .any(|c| c == "SetModuleEnabled(org.x.audio, true)"),
        "enabled despite the refusal"
    );

    // A module the list says is audio is refused before any call.
    mock.with(|m| m.modules[1].kind = "audio-filter".into());
    mock.take_calls();
    let out = bus.run(&["modules", "enable", "org.x.audio", "--force"]);
    assert_ne!(out.status.code(), Some(0), "{}", out.stdout);
    assert!(
        out.stderr.contains("[mic] force_conflicts"),
        "{}",
        out.stderr
    );
    assert!(
        mock.take_calls()
            .iter()
            .all(|c| c == "GetModules()" || c == "GetConfigSchema()"),
        "a change was sent"
    );
}

/// `modules` against a daemon before revision 9 says it is unsupported.
#[test]
fn modules_against_an_older_daemon_say_they_are_unsupported() {
    let bus = Bus::start();
    let _daemon = OldDaemon::start(&bus, Rev1);
    for args in [&["modules"][..], &["order"], &["modules", "enable", "a.b"]] {
        let out = bus.run(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", out.stderr);
        assert!(
            out.stderr.contains("the daemon does not support modules"),
            "{args:?}: {}",
            out.stderr
        );
    }
}

// ---------------------------------------------------------------------------
// The configuration (revision 10)

/// Sets `overridden_by` (and what follows from it) on the mock's row `id`.
fn override_row(mock: &MockDaemon, id: &str, file: &str) {
    mock.with(|m| {
        let row = m
            .config
            .iter_mut()
            .flat_map(|g| g.rows.iter_mut())
            .find(|r| r.id == id)
            .unwrap();
        row.overridden_by = file.into();
        row.source = file.into();
        row.enabled = false;
        row.disabled_reason = format!("Overridden by {file}");
    });
}

#[test]
fn config_shows_gets_sets_resets_and_lists_the_files() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    let out = bus.run(&["config"]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    assert!(
        out.stdout.starts_with(
            "Output\n  output.mode = 1920x1080@30  (Output mode; default; read only)\n  \
             output.hold_timeout_s = 2 s  (Hold timeout; default; applies at the next camera \
             start)\n\nLifecycle\n  lifecycle.idle_grace_s = 5 s  (Idle grace; from \
             config.toml; applies at once)\n"
        ),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout
            .contains("  modules.order = \"\"  (Stage order; default; read only)\n")
    );
    for args in [&["config", "--json"][..], &["config", "show", "--json"]] {
        let out = bus.run(args);
        assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
        let lines = json_lines(&out.stdout);
        let groups: Vec<&str> = lines
            .iter()
            .map(|v| v["group"]["id"].as_str().unwrap())
            .collect();
        assert_eq!(groups, ["output", "lifecycle", "modules"], "{args:?}");
        for (i, v) in lines.iter().enumerate() {
            assert_eq!(v["type"], "config_group");
            assert_eq!(
                (&v["schema"], &v["serial"], &v["index"], &v["count"]),
                (&1.into(), &1.into(), &i.into(), &3.into())
            );
            assert_eq!((&v["part"], &v["parts"]), (&1.into(), &1.into()));
        }
        let grace = &lines[1]["group"]["settings"][0];
        assert_eq!(
            *grace,
            serde_json::json!({
                "id": "lifecycle.idle_grace_s", "label": "Idle grace",
                "description": "Help for lifecycle.idle_grace_s", "type": "int",
                "value": 5, "default": 5, "min": 0, "max": 3600, "step": 1, "unit": "s",
                "editable": true, "source": "config.toml", "overridden_by": "",
                "apply": "live", "enabled": true, "disabled_reason": "", "notice": ""
            })
        );
        let modules = &lines[2]["group"]["settings"];
        assert_eq!(
            modules[0]["options"][1],
            serde_json::json!({"value": "battery", "label": "Battery"})
        );
        assert_eq!(modules[2]["value"], serde_json::json!([]));
        assert_eq!(lines[0]["group"]["settings"][1]["value"], 2.0);
    }
    mock.take_calls();

    let out = bus.run(&["config", "get", "lifecycle.idle_grace_s"]);
    assert_eq!((out.status.code(), out.stdout.as_str()), (Some(0), "5\n"));
    let out = bus.run(&["config", "get", "no.such"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        out.stderr.contains("No configuration key no.such"),
        "{}",
        out.stderr
    );

    let out = bus.run(&["config", "set", "--", "lifecycle.idle_grace_s", "10"]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        "  lifecycle.idle_grace_s = 10 s  (Idle grace; from config.d/50-panel.toml; applies at \
         once)\n"
    );
    let out = bus.run(&["config", "set", "--", "output.hold_timeout_s", "2.5"]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    assert!(out.stdout.contains("= 2.5 s"), "{}", out.stdout);
    // The value's type is checked before any write.
    let out = bus.run(&["config", "set", "--", "lifecycle.idle_grace_s", "soon"]);
    assert_eq!(out.status.code(), Some(2), "{}", out.stderr);
    assert!(out.stderr.contains("a whole number"), "{}", out.stderr);
    // The daemon's refusals: exit 1 with its reason.
    let out = bus.run(&["config", "set", "--", "output.mode", "1280x720@30"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        out.stderr.contains("cannot be changed here"),
        "{}",
        out.stderr
    );
    override_row(&mock, "modules.profile", "config.d/90-x.toml");
    let out = bus.run(&["config", "set", "--", "modules.profile", "battery"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        out.stderr.contains("Overridden by config.d/90-x.toml"),
        "{}",
        out.stderr
    );
    let out = bus.run(&["config", "reset", "--", "lifecycle.idle_grace_s"]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        "  lifecycle.idle_grace_s = 5 s  (Idle grace; default; applies at once)\n"
    );
    assert_eq!(
        mock.take_calls(),
        [
            "GetConfigSchema()",
            "GetConfigSchema()",
            "GetConfigSchema()",
            "SetConfig(lifecycle.idle_grace_s, Some(Int(10)))",
            "GetConfigSchema()",
            "GetConfigSchema()",
            "SetConfig(output.hold_timeout_s, Some(Float(2.5)))",
            "GetConfigSchema()",
            "GetConfigSchema()",
            "GetConfigSchema()",
            "SetConfig(output.mode, Some(Str(\"1280x720@30\")))",
            "GetConfigSchema()",
            "SetConfig(modules.profile, Some(Str(\"battery\")))",
            "ResetConfig(lifecycle.idle_grace_s)",
            "GetConfigSchema()"
        ]
    );

    let out = bus.run(&["config", "files"]);
    assert_eq!(
        out.stdout,
        "Directory: /home/u/.config/omarchy-broadcast\n  config.toml             loaded\n  \
         config.d/50-panel.toml  absent (written by the daemon)\n"
    );
    let out = bus.run(&["config", "files", "--json"]);
    assert_eq!(
        json_line(&out.stdout),
        serde_json::json!({
            "v": 1, "type": "config_files", "serial": 4,
            "dir": "/home/u/.config/omarchy-broadcast",
            "files": [
                {"path": "config.toml", "status": "loaded", "managed": false, "reason": ""},
                {"path": "config.d/50-panel.toml", "status": "absent", "managed": true, "reason": ""}
            ],
            "files_truncated": false
        })
    );
    // `--json` goes with `config` or `show` only.
    let out = bus.run(&["config", "--json", "get", "a.b"]);
    assert_eq!(out.status.code(), Some(2), "{}", out.stderr);
}

#[test]
fn config_against_an_older_daemon_says_it_is_unsupported() {
    let bus = Bus::start();
    let _daemon = OldDaemon::start(&bus, Rev1);
    for args in [
        &["config"][..],
        &["config", "get", "a.b"],
        &["config", "files"],
        &["config", "set", "--", "a.b", "1"],
        &["config", "reset", "--", "a.b"],
    ] {
        let out = bus.run(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", out.stderr);
        assert!(
            out.stderr
                .contains("the daemon does not support the configuration methods"),
            "{args:?}: {}",
            out.stderr
        );
    }
    for args in [&["config", "--json"][..], &["config", "files", "--json"]] {
        let out = bus.run(args);
        assert_eq!(out.status.code(), Some(1));
        let v = json_line(&out.stdout);
        assert_eq!(
            (&v["type"], &v["code"]),
            (&"error".into(), &"unsupported-daemon".into())
        );
    }
}

#[test]
fn watch_json_carries_the_serials_and_the_config_and_modules_lines() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    let watch = bus.watch(&["watch", "--json"], None);
    let v = watch.next_json("the first snapshot");
    assert_eq!(v["type"], "snapshot", "{v}");
    assert_eq!(
        (&v["config_serial"], &v["modules_serial"]),
        (&1.into(), &1.into())
    );
    watch.settings_set("the settings");
    let out = bus.run(&["config", "set", "--", "lifecycle.idle_grace_s", "10"]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    assert_eq!(
        watch.next_json("ConfigChanged"),
        serde_json::json!({"v": 1, "type": "config_changed", "serial": 2})
    );
    let iface = mock.iface();
    mock.rt.block_on(async {
        iface.get_mut().await.modules_serial = 5;
        let m = iface.get().await;
        m.modules_serial_changed(iface.signal_emitter())
            .await
            .unwrap();
    });
    assert_eq!(
        watch.next_json("ModulesSerial"),
        serde_json::json!({"v": 1, "type": "modules_changed", "serial": 5})
    );
    watch.quiet_for(Duration::from_millis(200), "nothing else");
    // A daemon before revision 10 (or 9): the serials are 0.
    let old = Bus::start();
    let _daemon = OldDaemon::start(&old, Rev1);
    let out = old.run(&["status", "--json"]);
    let v = json_line(&out.stdout);
    assert_eq!(
        (&v["config_serial"], &v["modules_serial"]),
        (&0.into(), &0.into())
    );
}

#[test]
fn order_and_modules_json_carry_the_moves_and_the_overrides() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    mock.with(|m| {
        m.modules = vec![obc_ipc::ModuleInfo {
            id: "br.com.dias.maxine-vfx".into(),
            name: "NVIDIA Maxine video effects".into(),
            enabled: true,
            status: "available".into(),
            ..obc_ipc::ModuleInfo::default()
        }];
        m.order = vec!["denoise".into(), "background".into(), "relight".into()];
    });
    let out = bus.run(&["order", "show", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    assert_eq!(
        json_line(&out.stdout),
        serde_json::json!({
            "v": 1, "type": "order", "order": ["denoise", "background", "relight"],
            "order_truncated": false,
            "stages": [
                {"id": "denoise", "slot": "pre-matte", "up": false, "down": true},
                {"id": "background", "slot": "matte", "up": true, "down": false},
                {"id": "relight", "slot": "post-matte", "up": false, "down": false}
            ],
            "overridden_by": ""
        })
    );
    override_row(&mock, "modules.order", "config.d/90-x.toml");
    override_row(&mock, "modules.profile", "config.d/90-x.toml");
    override_row(
        &mock,
        "modules.enabled.br.com.dias.maxine-vfx",
        "config.d/91-y.toml",
    );
    let out = bus.run(&["order", "show", "--json"]);
    assert_eq!(
        json_line(&out.stdout)["overridden_by"],
        "config.d/90-x.toml"
    );
    let out = bus.run(&["order"]);
    assert_eq!(
        out.stdout,
        "1. denoise\n2. background\n3. relight\nThe order is set by config.d/90-x.toml: change \
         it there.\n"
    );
    let out = bus.run(&["modules", "list", "--json"]);
    let lines = json_lines(&out.stdout);
    assert_eq!(lines[0]["profile_overridden_by"], "config.d/90-x.toml");
    assert_eq!(lines[1]["module"]["overridden_by"], "config.d/91-y.toml");
    let out = bus.run(&["modules"]);
    assert!(
        out.stdout
            .starts_with("Profile: quality (set by config.d/90-x.toml)\n"),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout
            .contains("available (set by config.d/91-y.toml)\n"),
        "{}",
        out.stdout
    );
}

// ---------------------------------------------------------------------------
// The capture and output modes (revision 12)

#[test]
fn the_capture_mode_and_the_output_mode_are_set_through_the_generic_paths() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    // The capture mode: an enum setting of the camera, first group.
    let mut groups = mock_settings();
    groups.insert(
        0,
        SettingsGroup {
            id: "capture".into(),
            label: "Capture".into(),
            settings: vec![choice(
                "camera.capture.mode",
                "Resolution and frame rate",
                &[
                    ("auto", "Automatic"),
                    ("3840x2160@30", "3840x2160 \u{b7} 30 fps"),
                    ("1920x1080@60", "1920x1080 \u{b7} 60 fps"),
                ],
                "auto",
                "auto",
            )],
            surface: None,
        },
    );
    mock.set_settings(groups);
    let out = bus.run(&["settings", "set", "camera.capture.mode", "3840x2160@30"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert!(
        out.stdout.contains("Resolution and frame rate"),
        "{}",
        out.stdout
    );
    assert!(
        mock.take_calls()
            .contains(&"SetSetting(,camera.capture.mode,Str(\"3840x2160@30\"))".to_string())
    );
    // The output mode: an editable enum row of the config schema.
    mock.with(|m| {
        let row = m
            .config
            .iter_mut()
            .flat_map(|g| g.rows.iter_mut())
            .find(|r| r.id == "output.mode")
            .unwrap();
        row.editable = true;
        row.apply = "reload".into();
        row.kind = obc_ipc::ConfigKind::Enum {
            options: vec![
                ("1920x1080@30".into(), "1920x1080 \u{b7} 30 fps".into()),
                ("1280x720@30".into(), "1280x720 \u{b7} 30 fps".into()),
            ],
        };
    });
    let out = bus.run(&["config", "set", "output.mode", "1280x720@30"]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    assert!(
        out.stdout.contains("output.mode = 1280x720@30"),
        "{}",
        out.stdout
    );
    assert!(
        mock.take_calls()
            .contains(&"SetConfig(output.mode, Some(Str(\"1280x720@30\")))".to_string())
    );
    // Not an option: the daemon's refusal.
    let out = bus.run(&["config", "set", "output.mode", "800x600@30"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        out.stderr.contains("not one of its options"),
        "{}",
        out.stderr
    );
}

// ---------------------------------------------------------------------------
// The microphone (revision 13)

/// The daemon's `mic.input` group, as the mock serves it among the global
/// ones.
fn mic_input_group() -> SettingsGroup {
    let scoped = |mut s: Setting| {
        s.scope = "global".into();
        s
    };
    SettingsGroup {
        id: "input".into(),
        label: "Microphone".into(),
        settings: vec![
            scoped(switch("mic.input.enabled", "Processing", true, true)),
            scoped(choice(
                "mic.input.device",
                "Input device",
                &[("default", "System default"), ("alsa_input.usb-a", "USB A")],
                "default",
                "default",
            )),
            scoped(setting(
                "mic.input.make_default",
                "Use as default microphone",
                SettingKind::Action,
                None,
            )),
        ],
        surface: Some("mic".into()),
    }
}

#[test]
fn mic_status_shows_the_microphone_without_starting_the_daemon() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    let out = bus.run(&["mic"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(out.stdout, "Microphone: off\nConsumers:  0\n");
    mock.set_mic(MicStatus {
        state: "active".into(),
        consumers: 2,
        provider: "mic-denoise: br.com.dias.voice-cpu".into(),
        latency_ms: 20.0,
        error: String::new(),
    });
    let out = bus.run(&["mic", "status"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        "Microphone: active\nConsumers:  2\nProvider:   mic-denoise: br.com.dias.voice-cpu\n\
         Latency:    20.0 ms\n"
    );
    for args in [&["mic", "--json"][..], &["mic", "status", "--json"]] {
        let out = bus.run(args);
        assert!(out.status.success(), "{}", out.stderr);
        assert_eq!(out.stdout.lines().count(), 1, "{}", out.stdout);
        let v = json_line(&out.stdout);
        assert_eq!(
            v,
            serde_json::json!({
                "v": 1, "type": "mic", "state": "active", "consumers": 2,
                "provider": "mic-denoise: br.com.dias.voice-cpu", "latency_ms": 20.0,
                "error": "",
            })
        );
    }
    // The snapshot carries it too.
    let v = json_line(&bus.run(&["status", "--json"]).stdout);
    assert_eq!(v["mic"]["state"], "active", "{v}");
    assert!(!bus.activation_attempted());
    // `--json` with a subcommand is a usage error.
    let out = bus.run(&["mic", "--json", "on"]);
    assert_eq!(out.status.code(), Some(2), "{}", out.stderr);
}

#[test]
fn mic_status_of_an_older_daemon_or_none() {
    let bus = Bus::start();
    let out = bus.run(&["mic", "--json"]);
    assert_eq!(out.status.code(), Some(3), "{}", out.stderr);
    assert_eq!(json_line(&out.stdout)["code"], "daemon-not-running");
    assert!(!bus.activation_attempted());
    let _old = OldDaemon::start(&bus, Rev1);
    let out = bus.run(&["mic"]);
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr);
    assert!(
        out.stderr.contains("does not support the microphone"),
        "{}",
        out.stderr
    );
}

#[test]
fn mic_on_and_off_call_set_mic_host_and_print_the_state() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    let out = bus.run(&["mic", "on"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        "Microphone: passthrough\nConsumers:  0\nLatency:    0.0 ms\n"
    );
    let out = bus.run(&["mic", "off"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(out.stdout, "Microphone: off\nConsumers:  0\n");
    assert_eq!(mock.take_calls(), ["SetMicHost(true)", "SetMicHost(false)"]);
    // A method call may start the daemon.
    mock.with(|m| {
        assert_eq!(m.auto_start, ["SetMicHost(true)", "SetMicHost(false)"]);
    });
    // Refused: the reason, exit 1.
    let unit = "omarchy-broadcast-mic@rnnoise.service is running: stop it first";
    mock.with(|m| m.mic_refusal = Some(unit.into()));
    let out = bus.run(&["mic", "on"]);
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr);
    assert!(out.stderr.contains(unit), "{}", out.stderr);
    assert_eq!(out.stdout, "");
}

#[test]
fn mic_device_and_default_set_the_input_settings() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    mock.with(|m| m.global = vec![mic_input_group()]);
    let out = bus.run(&["mic", "device", "alsa_input.usb-a"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(out.stdout, "Microphone: off\nConsumers:  0\n");
    let out = bus.run(&["mic", "default"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        mock.take_calls(),
        [
            "SetSetting(,mic.input.device,Str(\"alsa_input.usb-a\"))",
            "SetSetting(,mic.input.make_default,Bool(true))"
        ]
    );
    // A device the daemon does not offer: its refusal.
    let out = bus.run(&["mic", "device", "nope"]);
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr);
    assert!(
        out.stderr.contains("not one of its options"),
        "{}",
        out.stderr
    );
    // No restore action in this tree: the daemon's refusal.
    let out = bus.run(&["mic", "restore-default"]);
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr);
}

/// A PipeWire node name of 52 bytes, past the 32-byte bound of an option
/// value in the JSON lines.
const LONG_NODE_NAME: &str = "alsa_input.usb-K66_K66_20190805V001-00.analog-stereo";

#[test]
fn a_long_microphone_device_is_a_short_token_in_the_json_and_a_node_name_to_set() {
    assert!(LONG_NODE_NAME.len() > 32);
    // What the daemon offers for it: `pw-` and 12 hex characters, the
    // description as the label (the node name stays in the config).
    let token = "pw-0123456789ab";
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    let mut group = mic_input_group();
    group.settings[1] = choice(
        "mic.input.device",
        "Input device",
        &[("default", "System default"), (token, "K66 Analog Stereo")],
        "default",
        token,
    );
    group.settings[1].scope = "global".into();
    mock.with(|m| m.global = vec![group.clone()]);
    let watch = bus.watch(&["watch", "--json"], None);
    assert_eq!(watch.next_json("the snapshot")["type"], "snapshot");
    let set = watch.settings_set("the settings");
    let input = set
        .iter()
        .find(|v| v["group"]["id"] == "input")
        .expect("the input group");
    let device = input["group"]["settings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "mic.input.device")
        .unwrap();
    // The token and its label are carried whole, the value is the token.
    assert_eq!(device["value"], token);
    assert_eq!(device["options"][1]["value"], token);
    assert_eq!(device["options"][1]["label"], "K66 Analog Stereo");
    assert_eq!(device["default"], "default");

    // By node name: the CLI sends the whole name. The daemon resolves it
    // to the device (`mic::check_input`); the mock stands in for that by
    // listing the name as an option.
    if let SettingKind::Enum { options, .. } = &mut group.settings[1].kind {
        options.push((LONG_NODE_NAME.into(), "K66 by name".into()));
    }
    mock.with(|m| m.global = vec![group]);
    mock.take_calls();
    let out = bus.run(&["mic", "device", LONG_NODE_NAME]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        mock.take_calls(),
        [format!(
            "SetSetting(,mic.input.device,Str({LONG_NODE_NAME:?}))"
        )]
    );
}

#[test]
fn watch_json_carries_the_microphone_and_its_changes() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    let watch = bus.watch(&["watch", "--json"], None);
    let v = watch.next_json("the first snapshot");
    assert_eq!(v["type"], "snapshot", "{v}");
    assert_eq!(
        v["mic"],
        serde_json::json!({
            "state": "off", "consumers": 0, "provider": "", "latency_ms": 0.0, "error": "",
        })
    );
    watch.settings_set("the settings");
    mock.set_mic(MicStatus {
        state: "active".into(),
        consumers: 1,
        provider: "mic-denoise: voice".into(),
        latency_ms: 10.0,
        error: String::new(),
    });
    // A line per property that changed (the error did not), each with
    // every field in force; the state comes last.
    let mut lines = 0;
    let last = loop {
        let v = watch.next_json("a mic line");
        assert_eq!(v["type"], "mic", "{v}");
        lines += 1;
        if v["state"] == "active" {
            break v;
        }
    };
    assert_eq!(lines, 4);
    assert_eq!(last["consumers"], 1);
    assert_eq!(last["provider"], "mic-denoise: voice");
    assert_eq!(last["latency_ms"], 10.0);
}

#[test]
fn settings_set_of_a_mic_setting_has_no_all_cameras_heading() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    mock.with(|m| m.global = vec![mic_input_group()]);
    let out = bus.run(&["settings", "set", "mic.input.enabled", "off"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert!(!out.stdout.contains("All cameras"), "{}", out.stdout);
    assert!(out.stdout.contains("mic.input.enabled"), "{}", out.stdout);
}

#[test]
fn modules_pin_words_the_pin_it_set_not_the_stale_provider() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    let stage = |selected| obc_ipc::StageInfo {
        id: "mic-denoise".into(),
        available: true,
        selected,
        ..obc_ipc::StageInfo::default()
    };
    mock.with(|m| {
        m.modules = vec![
            obc_ipc::ModuleInfo {
                id: "br.com.dias.maxine-afx".into(),
                name: "NVIDIA Maxine audio effects".into(),
                kind: "audio-filter".into(),
                enabled: true,
                status: "available".into(),
                // The catalogue still names the old provider.
                stages: vec![stage(true)],
                ..obc_ipc::ModuleInfo::default()
            },
            obc_ipc::ModuleInfo {
                id: "br.com.dias.voice-cpu".into(),
                name: "Voice on the CPU".into(),
                kind: "audio-filter".into(),
                enabled: true,
                status: "available".into(),
                stages: vec![stage(false)],
                ..obc_ipc::ModuleInfo::default()
            },
        ];
    });
    let out = bus.run(&["modules", "pin", "mic-denoise", "br.com.dias.voice-cpu"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        "mic-denoise: pinned to Voice on the CPU (br.com.dias.voice-cpu)\n"
    );
    let out = bus.run(&["modules", "pin", "mic-denoise", "--clear"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(out.stdout, "mic-denoise: pin cleared\n");
}

// ---------------------------------------------------------------------------
// The media gallery (revision 15)

#[test]
fn media_lists_the_gallery_and_never_starts_the_daemon() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    mock.with(|m| {
        m.media.get_mut("background").unwrap().push(media_item(
            "background",
            "office.png",
            "Office",
            false,
        ));
    });
    let user = "/home/u/.local/share/omarchy-broadcast/media/backgrounds";
    let human = format!(
        "Bundled: /usr/share/omarchy-broadcast/media/backgrounds\n\
         Yours:   {user}\n\
         Videos:  {USER_VIDEOS}\n\
         Dusk  image  3840x2160  bundled  /usr/share/omarchy-broadcast/media/backgrounds/dusk.webp\n\
         Office  image  3840x2160  yours  {user}/office.png\n"
    );
    for args in [&["media"][..], &["media", "background"], &["media", "list"]] {
        let out = bus.run(args);
        assert_eq!(out.status.code(), Some(0), "{args:?}: {}", out.stderr);
        assert_eq!(out.stdout, human, "{args:?}");
    }
    let out = bus.run(&["media", "list", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    let lines = json_lines(&out.stdout);
    assert_eq!(lines.len(), 3, "{}", out.stdout);
    let v = &lines[0];
    assert_eq!(v["type"], "media");
    assert_eq!(v["kind"], "background");
    assert_eq!(v["serial"], 1);
    assert_eq!(v["user_dir"], user);
    assert_eq!(v["video_dir"], USER_VIDEOS);
    assert_eq!(v["items"], 2);
    assert_eq!(v["items_truncated"], false);
    let items = &lines[1..];
    assert!(
        items
            .iter()
            .all(|i| i["type"] == "media_item" && i["kind"] == "background")
    );
    assert_eq!(items[1]["path"], format!("{user}/office.png"));
    assert_eq!(items[1]["bundled"], false);
    assert_eq!(items[0]["media_type"], "image");
    assert_eq!(items[0]["width"], 3840);
    assert_eq!(items[0]["duration_ms"], 0);
    assert_eq!(items[0]["poster"], "");

    for args in [
        &["media", "away", "--json"][..],
        &["media", "list", "away", "--json"],
    ] {
        let out = bus.run(args);
        assert_eq!(out.status.code(), Some(0), "{args:?}: {}", out.stderr);
        let v = json_line(&out.stdout);
        assert_eq!(v["kind"], "away", "{args:?}");
        assert_eq!(v["video_dir"], "", "{args:?}");
        assert_eq!(v["items"], 0);
    }
    let out = bus.run(&["media", "away"]);
    assert!(
        out.stdout.ends_with("No away items in the gallery\n"),
        "{}",
        out.stdout
    );
    // Away has no videos folder: no Videos line.
    assert!(!out.stdout.contains("Videos:"), "{}", out.stdout);

    // Every read was made without starting the daemon.
    let mut auto = Vec::new();
    mock.with(|m| auto = m.auto_start.clone());
    assert_eq!(auto, Vec::<String>::new());
    let calls = mock.take_calls();
    for method in ["GetMedia(", "GetMediaDirs("] {
        assert_eq!(
            calls.iter().filter(|c| c.starts_with(method)).count(),
            7,
            "{method} {calls:?}"
        );
    }

    // Usage errors, before any call.
    for args in [
        &["media", "wallpaper"][..],
        &["media", "list", "away", "background"],
        &["media", "away", "list"],
        &["media", "import", "background"],
        &["media", "remove"],
    ] {
        let out = bus.run(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", out.stderr);
    }
    assert!(mock.take_calls().is_empty());
}

#[test]
fn media_list_names_the_videos_folder_from_revision_19_only() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    let user = "/home/u/.local/share/omarchy-broadcast/media/backgrounds";
    let clip = format!("{USER_VIDEOS}/beach.webm");
    mock.with(|m| {
        m.media
            .get_mut("background")
            .unwrap()
            .push(obc_ipc::MediaEntry {
                path: clip.clone(),
                media_type: "video".into(),
                cpu_ready: true,
                ..media_item("background", "beach.webm", "Beach", false)
            });
    });
    // Revision 19: the videos folder under Yours, and `video_dir` on the
    // `media` line; the video in it is listed.
    let out = bus.run(&["media"]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    assert!(
        out.stdout.starts_with(&format!(
            "Bundled: /usr/share/omarchy-broadcast/media/backgrounds\n\
             Yours:   {user}\n\
             Videos:  {USER_VIDEOS}\n"
        )),
        "{}",
        out.stdout
    );
    assert!(out.stdout.contains(&clip), "{}", out.stdout);
    let out = bus.run(&["media", "--json"]);
    let lines = json_lines(&out.stdout);
    assert_eq!(lines[0]["video_dir"], USER_VIDEOS);
    assert_eq!(lines[0]["items"], 2);
    assert_eq!(lines[2]["path"], clip.as_str());
    assert_eq!(
        mock.take_calls(),
        [
            "GetMedia(background)",
            "GetMediaDirs(background)",
            "GetMedia(background)",
            "GetMediaDirs(background)"
        ]
    );

    // A videos folder over the line's bound: "" and its items left out;
    // `user_dir` stays and the line keeps within the cap (`json_line`).
    let long = format!("/home/{}", "v/".repeat(1950));
    mock.with(|m| {
        m.media_videos = long.clone();
        m.media.get_mut("background").unwrap()[1].path = format!("{long}/beach.webm");
    });
    let out = bus.run(&["media", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    let lines = json_lines(&out.stdout);
    assert_eq!(lines.len(), 2, "{}", out.stdout);
    assert_eq!(lines[0]["video_dir"], "");
    assert_eq!(lines[0]["user_dir"], user);
    assert_eq!(lines[0]["items"], 1);
    assert_eq!(lines[0]["items_truncated"], true);
    // The human list is not bounded.
    let out = bus.run(&["media"]);
    assert!(
        out.stdout.contains(&format!("Videos:  {long}\n")),
        "{}",
        out.stdout
    );

    // Revision 18: GetMediaDirs is not asked, no key, no Videos line.
    mock.with(|m| m.revision = 18);
    mock.take_calls();
    let out = bus.run(&["media", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    let lines = json_lines(&out.stdout);
    assert!(lines[0].get("video_dir").is_none(), "{}", out.stdout);
    assert_eq!(lines[0]["user_dir"], user);
    let out = bus.run(&["media"]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    assert!(!out.stdout.contains("Videos:"), "{}", out.stdout);
    assert_eq!(
        mock.take_calls(),
        ["GetMedia(background)", "GetMedia(background)"]
    );
}

#[test]
fn media_convert_command_prints_the_daemons_command_alone() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    let user = bus.dir.join("gallery").display().to_string();
    mock.with(|m| m.media_user = user.clone());
    // A relative path is given to the daemon made absolute against the
    // current directory; the command alone is printed, on one line.
    let cwd = bus.dir.join("videos");
    std::fs::create_dir_all(&cwd).unwrap();
    let args = ["media", "convert-command", "beach day.mp4"];
    let mut cmd = bus.cli(&args);
    cmd.current_dir(&cwd);
    let out = run_cmd(cmd, &args);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    let input = cwd.join("beach day.mp4").display().to_string();
    assert_eq!(
        out.stdout,
        format!(
            "ffmpeg -nostdin -n -protocol_whitelist file -i '{input}' -f webm \
             '{user}/beach day-cpu.webm'\n"
        )
    );
    assert_eq!(out.stderr, "");
    assert_eq!(mock.take_calls(), [format!("GetConvertCommand({input})")]);
    // The daemon's refusals: on stderr, exit 1, nothing on stdout.
    for (path, why) in [
        ("/v/ready.webm", "already plays without an NVIDIA GPU"),
        ("/v/office.png", "not an MP4, MOV, MKV or WebM file"),
    ] {
        let out = bus.run(&["media", "convert-command", path]);
        assert_eq!(out.status.code(), Some(1), "{path}: {}", out.stderr);
        assert_eq!(out.stdout, "", "{path}");
        assert!(
            out.stderr.starts_with("omarchy-broadcast: ") && out.stderr.contains(why),
            "{path}: {}",
            out.stderr
        );
    }
    // A usage error: no path, or an option it does not take.
    for args in [
        &["media", "convert-command"][..],
        &["media", "convert-command", "/v/a.mp4", "--json"],
    ] {
        let out = bus.run(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", out.stderr);
    }
}

#[test]
fn media_convert_command_needs_revision_19_and_may_start_the_daemon() {
    let bus = Bus::start();
    let out = bus.run(&["media", "convert-command", "/v/a.mp4"]);
    assert_eq!(out.status.code(), Some(3), "{}", out.stderr);
    assert!(bus.activation_attempted());
    let _old = OldDaemon::start(&bus, Rev15);
    let out = bus.run(&["media", "convert-command", "/v/a.mp4"]);
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr);
    assert!(
        out.stderr.contains(
            "the daemon does not give the conversion command (its interface revision is 15, \
             that needs 19)"
        ),
        "{}",
        out.stderr
    );
}

#[test]
fn media_import_and_remove_change_the_gallery() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    let root = bus.dir.join("media");
    for kind in ["backgrounds", "away"] {
        std::fs::create_dir_all(root.join(kind)).unwrap();
    }
    let user = root.display().to_string();
    mock.with(|m| m.media_user = user.clone());

    // A relative path is given to the daemon made absolute against the
    // current directory; the stored path is printed.
    let cwd = bus.dir.join("pictures");
    std::fs::create_dir_all(&cwd).unwrap();
    let args = ["media", "import", "away", "me.png"];
    let mut cmd = bus.cli(&args);
    cmd.current_dir(&cwd);
    let out = run_cmd(cmd, &args);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    assert_eq!(out.stdout, format!("{user}/away/me.png\n"));
    assert_eq!(
        mock.take_calls(),
        [format!("ImportMedia(away, {}/me.png)", cwd.display())]
    );

    // --select: then the background and `replace`, as `effect replace
    // --image` does.
    let out = bus.run(&[
        "media",
        "import",
        "background",
        "/pics/office.jpg",
        "--select",
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    let stored = format!("{user}/backgrounds/office.jpg");
    let v = json_line(&out.stdout);
    assert_eq!(
        v,
        serde_json::json!({"v": 1, "type": "media_import", "kind": "background",
            "path": stored, "path_truncated": false, "selected": true})
    );
    assert_eq!(
        mock.take_calls(),
        [
            "ImportMedia(background, /pics/office.jpg)".to_string(),
            format!("SetBackgroundImage({stored})"),
            "SetEffect(replace)".to_string(),
        ]
    );
    let out = bus.run(&["media", "import", "away", "/pics/b.png", "--json"]);
    assert_eq!(json_line(&out.stdout)["selected"], false);
    let out = bus.run(&["media", "--json"]);
    assert_eq!(json_lines(&out.stdout)[0]["serial"], 4);

    // --select of an away image: the stored path becomes the away image
    // (`effect.fallback_image`, through SetConfig).
    mock.take_calls();
    mock.with(|m| {
        m.config[0].rows.push(config_row(
            "effect.fallback_image",
            "Away image",
            obc_ipc::ConfigKind::Path,
            obc_ipc::ConfigValue::Str(String::new()),
        ))
    });
    let out = bus.run(&[
        "media",
        "import",
        "away",
        "/pics/c.png",
        "--select",
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    let stored_away = format!("{user}/away/c.png");
    assert_eq!(
        json_line(&out.stdout),
        serde_json::json!({"v": 1, "type": "media_import", "kind": "away",
            "path": stored_away, "path_truncated": false, "selected": true})
    );
    assert_eq!(
        mock.take_calls(),
        [
            "ImportMedia(away, /pics/c.png)".to_string(),
            format!("SetConfig(effect.fallback_image, Some(Str({stored_away:?})))"),
        ]
    );
    let mut row = None;
    mock.with(|m| {
        row = m.config[0]
            .rows
            .iter()
            .find(|r| r.id == "effect.fallback_image")
            .cloned()
    });
    assert_eq!(
        row.unwrap().value,
        obc_ipc::ConfigValue::Str(stored_away.clone())
    );
    // Without --json: the stored path.
    let out = bus.run(&["media", "import", "away", "/pics/d.png", "--select"]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    assert_eq!(out.stdout, format!("{user}/away/d.png\n"));
    mock.take_calls();

    // A refusal: exit 1 with the daemon's reason (on stdout with --json).
    let out = bus.run(&["media", "import", "background", "/pics/notes.txt"]);
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr);
    assert_eq!(
        out.stderr,
        "omarchy-broadcast: cannot import /pics/notes.txt: not a PNG, JPEG or WebP image\n"
    );
    let out = bus.run(&["media", "import", "background", "/pics/notes.txt", "--json"]);
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr);
    assert_eq!(json_line(&out.stdout)["type"], "error");

    // The background in force under `replace` cannot be removed; under
    // another effect it is cleared and removed.
    let out = bus.run(&["media", "remove", &stored]);
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr);
    assert!(
        out.stderr.contains("select another effect"),
        "{}",
        out.stderr
    );
    assert_eq!(bus.run(&["effect", "none"]).status.code(), Some(0));
    let out = bus.run(&["media", "remove", &stored]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    assert_eq!(out.stdout, format!("Removed {stored}\n"));
    let out = bus.run(&["effect", "--json"]);
    assert_eq!(json_line(&out.stdout)["background_image"], "");
    let bundled = "/usr/share/omarchy-broadcast/media/backgrounds/dusk.webp";
    let out = bus.run(&["media", "remove", bundled]);
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr);
    assert!(
        out.stderr
            .contains("is not an item of the user's media gallery"),
        "{}",
        out.stderr
    );
}

#[test]
fn media_json_lines_stay_within_the_cap_and_long_paths_are_left_out() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    let long = format!("/home/u/{}x.png", "deep/".repeat(800));
    mock.with(|m| {
        let items = m.media.get_mut("background").unwrap();
        items.push(media_item("background", "office.png", "Office", false));
        items.push(obc_ipc::MediaEntry {
            path: long.clone(),
            ..media_item("background", "x.png", "Deep", false)
        });
    });
    let out = bus.run(&["media", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    // `json_line` holds every line to 4096 bytes.
    let lines = json_lines(&out.stdout);
    assert_eq!(lines.len(), 3, "{}", out.stdout);
    assert_eq!(lines[0]["items"], 2);
    assert_eq!(lines[0]["items_truncated"], true);
    assert!(lines[1..].iter().all(|l| l["path"] != long.as_str()));
    // The human list is not bounded: every item is shown.
    let out = bus.run(&["media"]);
    assert!(out.stdout.contains(&long), "{}", out.stdout);

    // An import stored under a path over the bound: printed as "", with
    // path_truncated, and the import succeeded.
    let user = format!("/home/{}", "u/".repeat(1950));
    mock.with(|m| m.media_user = user.clone());
    let out = bus.run(&["media", "import", "away", "/pics/b.png", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    let v = json_line(&out.stdout);
    assert_eq!(
        v,
        serde_json::json!({"v": 1, "type": "media_import", "kind": "away",
            "path": "", "path_truncated": true, "selected": false})
    );
    let out = bus.run(&["media", "away", "--json"]);
    let lines = json_lines(&out.stdout);
    assert_eq!(lines[0]["user_dir"], "");
    assert_eq!(lines[0]["items"], 0);
    assert_eq!(lines[0]["items_truncated"], true);
}

#[test]
fn media_against_an_older_daemon_says_it_is_unsupported() {
    let bus = Bus::start();
    let _daemon = OldDaemon::start(&bus, Rev6);
    for args in [
        &["media"][..],
        &["media", "list", "--json"],
        &["media", "import", "background", "/pics/a.png"],
        &["media", "remove", "/pics/a.png"],
    ] {
        let out = bus.run(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", out.stderr);
        let text = if args.contains(&"--json") {
            let v = json_line(&out.stdout);
            assert_eq!(v["code"], "unsupported-daemon");
            v["error"].as_str().unwrap().to_string()
        } else {
            out.stderr
        };
        assert!(
            text.contains(
                "the daemon does not support the media gallery (its interface revision is 6, \
                 the gallery needs 15)"
            ),
            "{args:?}: {text}"
        );
    }
}

#[test]
fn media_list_never_starts_the_daemon_but_import_and_remove_may() {
    let bus = Bus::start();
    for args in [&["media"][..], &["media", "list", "away", "--json"]] {
        let out = bus.run(args);
        assert_eq!(out.status.code(), Some(3), "{args:?}: {}", out.stderr);
    }
    assert!(!bus.activation_attempted());
    let out = bus.run(&["media", "import", "background", "/pics/a.png"]);
    assert_eq!(out.status.code(), Some(3), "{}", out.stderr);
    assert!(
        out.stderr.contains("D-Bus could not start it"),
        "{}",
        out.stderr
    );
    assert!(bus.activation_attempted());
}

// ---------------------------------------------------------------------------
// The camera's health (revision 20)

/// The first camera delivering short, with damaged frames, a warning, a
/// suggestion and one recommended change.
fn short_health() -> obc_ipc::CameraHealth {
    obc_ipc::CameraHealth {
        camera: "usb-1234_abcd".into(),
        delivered_fps: 19.5,
        nominal_fps: 30.0,
        corrupt_frames: 12,
        warnings: vec!["Plug First Camera into its own USB port.".into()],
        suggestions: vec![obc_ipc::Suggestion {
            setting: "effect.video.denoise".into(),
            value: "on".into(),
            reason: "This camera's picture is grainy: noise removal makes it cleaner.".into(),
        }],
        has_recommendation: true,
        recommended: vec![obc_ipc::Recommended {
            setting: "camera.image.sharpness".into(),
            label: "Sharpness".into(),
            value: "5".into(),
            current: "3".into(),
        }],
        shutter: obc_ipc::SHUTTER_CLOSED.into(),
        show_mode: obc_ipc::SHOW_MODE_NORMAL.into(),
        status: vec!["Shutter closed".into()],
    }
}

/// `status` adds the health lines (the suggestion named by its label in
/// the settings tree), `status --json` the `camera_health` object; a
/// healthy camera adds nothing to `status`.
#[test]
fn status_shows_the_camera_health() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    let out = bus.run(&["status"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert!(!out.stdout.contains("Delivers:"), "{}", out.stdout);
    let v = json_line(&bus.run(&["status", "--json"]).stdout);
    assert_eq!(
        v["camera_health"],
        serde_json::json!({
            "camera": "usb-1234_abcd",
            "delivered_fps": -1.0,
            "nominal_fps": 30.0,
            "corrupt_frames": 0,
            "warnings": [],
            "suggestions": [],
            "has_recommendation": false,
            "recommended": [],
            "shutter": "",
            "show_mode": "",
            "status": []
        }),
        "{v}"
    );

    mock.set_global(mock_global());
    mock.set_camera_health(short_health());
    let out = bus.run(&["status"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert!(
        out.stdout.ends_with(
            "Delivers:   19.5 of 30 fps\n\
             Damaged:    12 frames dropped\n\
             Status:     Shutter closed\n\
             Warning:    Plug First Camera into its own USB port.\n\
             Suggest:    Noise removal \u{2014} This camera's picture is grainy: noise removal \
             makes it cleaner.\n"
        ),
        "{}",
        out.stdout
    );
    let v = json_line(&bus.run(&["status", "--json"]).stdout);
    let h = &v["camera_health"];
    assert_eq!(h["delivered_fps"], 19.5, "{v}");
    assert_eq!(h["corrupt_frames"], 12, "{v}");
    assert_eq!(
        h["suggestions"][0]["setting"], "effect.video.denoise",
        "{v}"
    );
    assert_eq!(h["suggestions"][0]["value"], "on", "{v}");
    assert_eq!(h["has_recommendation"], true, "{v}");
    assert_eq!(h["shutter"], "closed", "{v}");
    assert_eq!(h["show_mode"], "normal", "{v}");
    assert_eq!(h["status"], serde_json::json!(["Shutter closed"]), "{v}");
    assert_eq!(
        h["recommended"],
        serde_json::json!([{
            "setting": "camera.image.sharpness",
            "label": "Sharpness",
            "value": "5",
            "current": "3"
        }]),
        "{v}"
    );
    // Reading never starts the daemon.
    assert!(!bus.activation_attempted());
}

/// `watch --json` prints a `camera_health` line when it changes, never in
/// `state` lines; `watch` a `camera health:` line.
#[test]
fn watch_prints_a_camera_health_line_when_it_changes() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    let watch = bus.watch(&["watch", "--json"], None);
    let v = watch.next_json("the first snapshot");
    assert_eq!(v["type"], "snapshot", "{v}");
    assert_eq!(v["camera_health"]["delivered_fps"], -1.0, "{v}");
    watch.settings_set("the settings after the snapshot");
    mock.set_camera_health(short_health());
    let v = watch.next_json("the camera health");
    assert_eq!(v["type"], "camera_health", "{v}");
    assert_eq!(v["camera"], "usb-1234_abcd", "{v}");
    assert_eq!(v["delivered_fps"], 19.5, "{v}");
    assert_eq!(v["warnings"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v["shutter"], "closed", "{v}");
    assert_eq!(v["status"][0], "Shutter closed", "{v}");
    assert!(v.get("changed").is_none(), "{v}");
    watch.quiet_for(Duration::from_millis(300), "nothing else changed");

    let watch = bus.watch(&["watch"], None);
    watch.next("the summary");
    let mut h = short_health();
    h.corrupt_frames = 13;
    mock.set_camera_health(h);
    let line = watch.next("the camera health");
    assert!(
        line.starts_with(
            "camera health: delivers 19.5 of 30 fps | 13 damaged frames dropped | Shutter closed \
             | warning:"
        ),
        "{line}"
    );
}

#[test]
fn recommended_shows_and_applies_the_recommended_settings() {
    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    // No row: said so, and apply is refused.
    let out = bus.run(&["recommended"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(out.stdout, "This camera has no recommended settings.\n");
    let out = bus.run(&["recommended", "apply"]);
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr);
    assert!(
        out.stderr.contains("has no recommended settings"),
        "{}",
        out.stderr
    );
    let out = bus.run(&["recommended", "apply", "--json"]);
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr);
    let v = json_line(&out.stdout);
    assert_eq!(v["type"], "error", "{v}");

    mock.set_camera_health(short_health());
    let out = bus.run(&["recommended"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        "Recommended settings for First Camera (1234:abcd):\n  Sharpness: 3 \u{2192} 5\n\
         Use them with: omarchy-broadcast recommended apply\n"
    );
    let out = bus.run(&["recommended", "--json"]);
    assert!(out.status.success(), "{}", out.stderr);
    let v = json_line(&out.stdout);
    assert_eq!(v["type"], "camera_health", "{v}");
    assert_eq!(v["recommended"][0]["value"], "5", "{v}");
    assert!(!bus.activation_attempted());
    mock.take_calls();

    let out = bus.run(&["recommended", "apply"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        "Recommended settings applied to First Camera (1234:abcd).\n"
    );
    assert_eq!(mock.take_calls(), ["ApplyRecommendedSettings()"]);
    let out = bus.run(&["recommended"]);
    assert_eq!(out.stdout, "Already at the recommended settings.\n");
    mock.set_camera_health(short_health());
    let out = bus.run(&["recommended", "apply", "--json"]);
    assert!(out.status.success(), "{}", out.stderr);
    let v = json_line(&out.stdout);
    assert_eq!(v["type"], "camera_health", "{v}");
    assert_eq!(v["recommended"], serde_json::json!([]), "{v}");
    assert_eq!(v["has_recommendation"], true, "{v}");
    // `--json` goes with no subcommand but `apply`'s own.
    let out = bus.run(&["recommended", "--json", "apply"]);
    assert_eq!(out.status.code(), Some(2), "{}", out.stderr);
}

/// A daemon before revision 20: `camera_health` is null (even when it
/// serves something by that name), and `recommended` says it is too old.
#[test]
fn an_older_daemon_has_no_camera_health() {
    let bus = Bus::start();
    let _old = OldDaemon::start(&bus, Rev15);
    let v = json_line(&bus.run(&["status", "--json"]).stdout);
    assert_eq!(v.get("camera_health"), Some(&Value::Null), "{v}");
    let out = bus.run(&["recommended"]);
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr);
    assert!(
        out.stderr.contains("does not support recommended settings"),
        "{}",
        out.stderr
    );
    let out = bus.run(&["recommended", "--json"]);
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr);
    assert_eq!(json_line(&out.stdout)["code"], "unsupported-daemon");
    drop(_old);

    let bus = Bus::start();
    let mock = MockDaemon::start(&bus);
    mock.with(|m| m.revision = 19);
    mock.set_camera_health(short_health());
    let v = json_line(&bus.run(&["status", "--json"]).stdout);
    assert_eq!(v["camera_health"], Value::Null, "{v}");
    let out = bus.run(&["status"]);
    assert!(!out.stdout.contains("Delivers:"), "{}", out.stdout);
    let out = bus.run(&["recommended", "apply"]);
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr);
    assert!(
        out.stderr.contains("update omarchy-broadcastd"),
        "{}",
        out.stderr
    );
    assert!(mock.take_calls().is_empty());
}
