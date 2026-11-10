//! `omarchy-broadcast`: the command-line remote control of `omarchy-broadcastd`,
//! over the D-Bus interface `br.com.dias.OmarchyBroadcast1` (`docs/dbus-api.md`,
//! ADR-0011). `--json` output is the contract of `docs/cli-json.md`.

mod bus;
mod human;
mod json;
mod model;
mod modules;
mod out;
mod preview;
mod watch;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::builder::PossibleValuesParser;
use clap::error::ErrorKind;
use clap::{CommandFactory, Parser, Subcommand};
use obc_ipc::{
    CameraControls, IpcError, ResetScope, Setting, SettingKind, SettingValue, SettingsTree,
    Snapshot,
};
use zbus::Connection;

use crate::bus::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Failure, READ_TIMEOUT};

/// Time allowed for `on`, `off`, `camera`, `effect <effect>`, `denoise`,
/// `controls set`, `controls reset`, `settings set` and `settings reset`,
/// which may wait for D-Bus activation of the daemon.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

const AFTER_HELP: &str = "\
Exit status:
  0  success
  1  the daemon rejected the request (no such camera, invalid config, invalid argument,
     unavailable, failed, unknown setting, setting disabled, rate limited), or is too old
     for it
  2  invalid command line
  3  omarchy-broadcastd is not running, or there is no D-Bus session bus
  4  any other D-Bus failure, a timeout, or a failed write to stdout

`status`, `cameras`, `watch`, `preview`, `effect` without an effect, `controls` and
`settings` without a subcommand, `modules list`, `modules info`, `modules profile` without
a profile and `order show` never start the daemon. `on`, `off`, `camera`,
`effect <effect>`, `denoise`, `controls set`, `controls reset`, `settings set`,
`settings reset`, `modules enable|disable|profile <profile>|pin` and `order set|reset` may
start it through D-Bus activation when its activation file is installed.

`--json` output is documented in docs/cli-json.md; the D-Bus interface in docs/dbus-api.md.";

/// Remote control for omarchy-broadcastd, the Omarchy virtual-camera daemon.
#[derive(Parser)]
#[command(name = "omarchy-broadcast", version, after_help = AFTER_HELP)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show the state, the active camera, consumers, output mode and error.
    Status {
        /// Print one JSON object (`"type":"snapshot"`).
        #[arg(long)]
        json: bool,
    },
    /// Enable the camera (leave the privacy state).
    On,
    /// Disable the camera: the privacy state, camera closed, black frames.
    Off,
    /// List the cameras, the active one marked with `*`.
    Cameras {
        /// Print one JSON object (`"type":"cameras"`).
        #[arg(long)]
        json: bool,
    },
    /// Select a camera: `next`, or a camera id, a name substring, a USB
    /// `vid:pid` or a `/dev/videoN` path.
    Camera {
        /// `next`, or the camera to select.
        #[arg(value_name = "next|NAME|VID:PID|ID|/dev/videoN")]
        target: String,
    },
    /// Show or select the video effect: `none`, `blur` or `replace` (the
    /// background replaced by an image).
    ///
    /// Without an effect, prints the current effect, blur strength and
    /// background image. With one, sets the option given (`--strength` with
    /// `blur`, `--image` with `replace`), then selects the effect and prints
    /// the result. The settings persist across restarts of the daemon.
    ///
    /// Examples: `omarchy-broadcast effect blur --strength 0.7`,
    /// `omarchy-broadcast effect replace --image ~/Pictures/office.jpg`,
    /// `omarchy-broadcast effect none`.
    Effect {
        /// The effect to select; omitted, the current one is printed.
        #[arg(value_parser = ["none", "blur", "replace"])]
        effect: Option<String>,
        /// Blur strength, from 0.0 to 1.0 (with `blur`).
        #[arg(long, value_name = "X", value_parser = parse_strength, requires = "effect")]
        strength: Option<f64>,
        /// Background image (with `replace`). A relative path is taken from
        /// the current directory; the daemon is given the absolute path.
        #[arg(long, value_name = "PATH", requires = "effect")]
        image: Option<PathBuf>,
        /// Print one JSON object (`"type":"snapshot"`); only without an
        /// effect.
        #[arg(long, conflicts_with = "effect")]
        json: bool,
    },
    /// Switch video noise removal on or off.
    ///
    /// It reduces the grain of the camera image and runs before the effect
    /// (or alone with `none`). If it cannot run, the camera image is shown
    /// without it and `status` says why. The setting persists across
    /// restarts of the daemon.
    Denoise {
        /// `on` or `off`.
        #[arg(value_parser = ["on", "off"])]
        value: String,
    },
    /// Show or change the image controls of the active camera: brightness,
    /// contrast, saturation, sharpness and backlight compensation, those
    /// the camera has.
    ///
    /// Without a subcommand, prints each control's value, range and
    /// default; `(stored)` marks a value set here, which the daemon gives
    /// the camera whenever it opens. A change applies at once while the
    /// camera is open (state `starting` or `active`); otherwise it is stored and applied
    /// the next time the camera opens. The values persist across restarts
    /// of the daemon, for each camera.
    ///
    /// Examples: `omarchy-broadcast controls set brightness 10`,
    /// `omarchy-broadcast controls reset`.
    #[command(args_conflicts_with_subcommands = true)]
    Controls {
        /// Print one JSON object (`"type":"controls"`); only without a
        /// subcommand.
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        action: Option<ControlsAction>,
    },
    /// Show or change the settings of a camera: image, white balance,
    /// exposure, focus and zoom, those the camera has; and the video
    /// effects, the same for every camera.
    ///
    /// Without a subcommand, prints the settings of the active camera (or
    /// of `--camera`) by group: each one's label, value, id, the values it
    /// takes, `(stored)` for a value set here, and why it cannot be changed
    /// now when it cannot. A change applies at once while the camera is
    /// open (state `starting` or `active`); otherwise it is stored and
    /// applied the next time the camera opens. The values persist across
    /// restarts of the daemon, for each camera. The effects (`effect.*`,
    /// under "All cameras") apply at once whatever the camera, and a reset
    /// puts back the configuration's value.
    ///
    /// Examples: `omarchy-broadcast settings set camera.exposure.auto_exposure manual`,
    /// `omarchy-broadcast settings set -- camera.focus.pan_absolute -3600`,
    /// `omarchy-broadcast settings set effect.video.denoise on`,
    /// `omarchy-broadcast settings reset --group=exposure`.
    // Not `args_conflicts_with_subcommands`: `--camera` goes before or after
    // `set`/`reset`; `--json` with a subcommand is refused in `job`.
    Settings {
        /// Print the settings as JSON lines (`"type":"settings_group"`, or
        /// `"type":"settings_clear"` when there is nothing to show: no
        /// camera and no effect setting, or no setting this version can
        /// show); only without a subcommand.
        #[arg(long)]
        json: bool,
        /// The camera, by its id (as `omarchy-broadcast cameras` lists it);
        /// the active camera when omitted. Before or after `set` and
        /// `reset`.
        #[arg(long, value_name = "ID", global = true)]
        camera: Option<String>,
        #[command(subcommand)]
        action: Option<SettingsAction>,
    },
    /// Show or manage the effect modules: list them with their status and
    /// stages, show one, enable or disable one, set the backend profile,
    /// pin the module that provides a stage. Changes take effect when the
    /// camera next starts.
    ///
    /// Without a subcommand, lists the modules. Examples:
    /// `omarchy-broadcast modules info br.com.dias.maxine-vfx`,
    /// `omarchy-broadcast modules profile battery`,
    /// `omarchy-broadcast modules pin background br.com.dias.maxine-vfx`.
    Modules {
        #[command(subcommand)]
        action: Option<modules::ModulesAction>,
    },
    /// Show or set the order of the effect stages (within the limits each
    /// stage declares). Changes take effect when the camera next starts.
    ///
    /// Without a subcommand, prints the order. Example:
    /// `omarchy-broadcast order set enhance,denoise,background,relight`.
    Order {
        #[command(subcommand)]
        action: Option<modules::OrderAction>,
    },
    /// Follow state changes and stats until interrupted.
    Watch {
        /// Print one JSON object per line (NDJSON).
        #[arg(long)]
        json: bool,
    },
    /// Hold a live preview of the virtual camera until interrupted: the
    /// daemon writes small JPEG frames to a file in $XDG_RUNTIME_DIR, and
    /// one line is printed per frame.
    ///
    /// The preview counts as an application using the camera: it starts
    /// the camera when nothing else does. It ends when this command does.
    Preview {
        /// Print one JSON object per line (NDJSON).
        #[arg(long)]
        json: bool,
        /// Frames per second: 15 or 30.
        #[arg(long, value_name = "15|30", default_value_t = 15, value_parser = parse_fps)]
        fps: u32,
    },
}

/// The subcommands of `controls`.
#[derive(Subcommand)]
enum ControlsAction {
    /// Set an image control of the active camera, and store the value for
    /// that camera; then print the controls.
    Set {
        /// The control.
        #[arg(value_parser = PossibleValuesParser::new(obc_ipc::CAMERA_CONTROL_NAMES))]
        name: String,
        /// A whole number within the control's range and on its step (see
        /// `omarchy-broadcast controls`); the daemon checks it.
        #[arg(allow_negative_numbers = true)]
        value: i32,
    },
    /// Forget the values stored for the active camera and put every
    /// control back to the camera's default; then print the controls.
    Reset,
}

/// The subcommands of `settings`.
#[derive(Subcommand)]
enum SettingsAction {
    /// Set a setting of the active camera (or of `--camera`), and store the
    /// value for that camera; then print the setting.
    ///
    /// The value is read by the setting's type: `on`, `off`, `true` or
    /// `false` for a switch; a whole number for a number (a negative one
    /// after `--`, or written right after the id); one of the option ids
    /// `omarchy-broadcast settings` lists for a choice; nothing (or `true`)
    /// to run an action. The daemon checks the range and whether the
    /// setting can be changed now.
    Set {
        /// The setting's id, e.g. `camera.image.brightness`.
        #[arg(value_name = "ID", value_parser = parse_setting_id)]
        id: String,
        /// The value; omitted only for an action.
        #[arg(value_name = "VALUE", allow_negative_numbers = true)]
        value: Option<String>,
    },
    /// Forget the values stored for the active camera (or `--camera`), all
    /// of them, a group's or one setting's, and put those settings back to
    /// the camera's default; then print them.
    Reset {
        /// Reset only the settings of this group, e.g. `exposure`.
        #[arg(long, value_name = "GROUP", value_parser = parse_group_id, conflicts_with = "setting")]
        group: Option<String>,
        /// Reset only this setting, e.g. `camera.image.brightness`.
        #[arg(long, value_name = "ID", value_parser = parse_setting_id)]
        setting: Option<String>,
    },
}

/// A setting id ([`obc_ipc::valid_setting_id`]).
fn parse_setting_id(s: &str) -> Result<String, String> {
    if obc_ipc::valid_setting_id(s) {
        Ok(s.to_string())
    } else {
        Err(format!(
            "not a setting id (dot-separated lowercase words such as \
             camera.image.brightness, at most {} bytes)",
            obc_ipc::settings::SETTING_ID_MAX
        ))
    }
}

/// A group id: lowercase letters, digits and `_`, at most
/// [`json::MAX_GROUP_ID_BYTES`] bytes.
fn parse_group_id(s: &str) -> Result<String, String> {
    let ok = !s.is_empty()
        && s.len() <= json::MAX_GROUP_ID_BYTES
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if ok {
        Ok(s.to_string())
    } else {
        Err(format!(
            "not a group id (lowercase letters, digits and `_`, such as white_balance, at most \
             {} bytes)",
            json::MAX_GROUP_ID_BYTES
        ))
    }
}

/// The value of `--fps`: 15 or 30.
fn parse_fps(s: &str) -> Result<u32, String> {
    match s.parse::<u32>() {
        Ok(v) if obc_ipc::PREVIEW_FPS.contains(&v) => Ok(v),
        _ => Err("the preview frame rate must be 15 or 30".into()),
    }
}

/// The value of `--strength`: a number from 0.0 to 1.0.
fn parse_strength(s: &str) -> Result<f64, String> {
    match s.parse::<f64>() {
        Ok(v) if (0.0..=1.0).contains(&v) => Ok(v),
        Ok(_) => Err("the strength must be from 0.0 to 1.0".into()),
        Err(_) => Err("not a number (expected 0.0 to 1.0)".into()),
    }
}

/// The value of `--image` as given to the daemon: absolute, with symbolic
/// links resolved (the daemon's working directory is not the caller's), and
/// naming an existing regular file.
fn background_path(path: &Path) -> Result<String, String> {
    let shown = path.display();
    let abs = std::fs::canonicalize(path).map_err(|e| format!("--image {shown}: {e}"))?;
    let meta = std::fs::metadata(&abs).map_err(|e| format!("--image {shown}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("--image {shown}: not a regular file"));
    }
    abs.into_os_string().into_string().map_err(|p| {
        format!(
            "--image {}: the path is not valid UTF-8, which D-Bus cannot carry",
            Path::new(&p).display()
        )
    })
}

/// A checked `effect <effect>` request.
struct EffectRequest {
    effect: String,
    strength: Option<f64>,
    image: Option<String>,
}

/// Checks the options of `effect <effect>` against the effect and resolves
/// the image path. The error is a usage error of the `effect` subcommand.
fn effect_request(
    effect: &str,
    strength: Option<f64>,
    image: Option<&Path>,
) -> Result<EffectRequest, clap::Error> {
    let usage = |kind: ErrorKind, msg: String| {
        let mut cmd = Cli::command();
        cmd.build();
        match cmd.find_subcommand_mut("effect") {
            Some(sub) => sub.error(kind, msg),
            None => cmd.error(kind, msg),
        }
    };
    if strength.is_some() && effect != "blur" {
        return Err(usage(
            ErrorKind::ArgumentConflict,
            format!("--strength applies to `blur`, not `{effect}`"),
        ));
    }
    if image.is_some() && effect != "replace" {
        return Err(usage(
            ErrorKind::ArgumentConflict,
            format!("--image applies to `replace`, not `{effect}`"),
        ));
    }
    let image = image
        .map(background_path)
        .transpose()
        .map_err(|m| usage(ErrorKind::ValueValidation, m))?;
    Ok(EffectRequest {
        effect: effect.to_string(),
        strength,
        image,
    })
}

/// Runs `f` with a time limit.
async fn within<T>(
    limit: Duration,
    f: impl Future<Output = Result<T, Failure>>,
) -> Result<T, Failure> {
    tokio::time::timeout(limit, f)
        .await
        .unwrap_or_else(|_| Err(bus::timed_out(limit)))
}

/// Reports `f` and returns its exit status: on stderr (the daemon's text
/// made [`human::printable`]), or as a JSON error line on stdout.
fn report(f: &Failure, json: bool) -> u8 {
    if json {
        out::line(&json::error_line(f.json_code(), &f.message()));
    } else {
        eprintln!("omarchy-broadcast: {}", human::printable(&f.message()));
    }
    f.exit_code()
}

/// What a read-only command prints.
#[derive(Clone, Copy)]
enum View {
    Status,
    Cameras,
    Effect,
}

/// `status`, `cameras` and `effect` without an effect.
async fn show(json: bool, view: View) -> u8 {
    let snap = within(READ_TIMEOUT, async {
        let conn = bus::connect().await?;
        bus::fetch(&conn).await
    })
    .await;
    // A daemon without the effect controls would read as "none"; say so
    // instead. `--json` prints the snapshot, which carries the revision.
    let snap = snap.and_then(|s| match (json, view) {
        (false, View::Effect) => bus::require_effects(s.interface_revision).map(|()| s),
        _ => Ok(s),
    });
    match snap {
        Ok(s) => {
            match (json, view) {
                // `effect --json` prints the whole snapshot, as `status
                // --json` does: it carries every effect field.
                (true, View::Status | View::Effect) => out::line(&json::snapshot_line(&s)),
                (true, View::Cameras) => out::line(&json::cameras_line(&s)),
                (false, View::Status) => out::print(&human::status_text(&s)),
                (false, View::Cameras) => out::print(&human::cameras_text(&s)),
                (false, View::Effect) => out::print(&human::effect_text(&s)),
            }
            EXIT_OK
        }
        Err(f) => report(&f, json),
    }
}

/// The method calls: `on`, `off`, `camera ...`. Prints the result read back
/// after the call.
async fn control(command: &Command) -> u8 {
    let result = within(CALL_TIMEOUT, async {
        let conn = bus::connect().await?;
        let proxy = bus::proxy(&conn).await?;
        let reply: Result<(), IpcError> = match command {
            Command::On => proxy.set_enabled(true).await,
            Command::Off => proxy.set_enabled(false).await,
            Command::Camera { target } if target == "next" => proxy.next_camera().await,
            Command::Camera { target } => proxy.select_camera(target).await,
            _ => unreachable!("not a control command"),
        };
        reply.map_err(bus::classify_ipc)?;
        // Read back with an uncached GetAll: a cache could still hold the
        // value from before the call.
        bus::fetch(&conn).await
    })
    .await;
    match result {
        Ok(s) => {
            if matches!(command, Command::Camera { .. }) {
                out::print(&human::camera_text(&s));
            } else {
                out::print(&human::enabled_text(&s));
            }
            EXIT_OK
        }
        Err(f) => report(&f, false),
    }
}

/// `effect <effect>`: checks that the daemon has the effect controls, sets
/// the option first, so that the effect starts with it, then the effect;
/// prints the result read back after the calls.
async fn set_effect(req: &EffectRequest) -> u8 {
    let result = within(CALL_TIMEOUT, async {
        let conn = bus::connect().await?;
        let proxy = bus::proxy(&conn).await?;
        // An uncached proxy's getter is a `Get` call, which (like the
        // methods) may start the daemon.
        let revision = proxy
            .interface_revision()
            .await
            .map_err(|e| bus::classify(&e, true))?;
        bus::require_effects(revision)?;
        if let Some(strength) = req.strength {
            proxy
                .set_blur_strength(strength)
                .await
                .map_err(bus::classify_ipc)?;
        }
        if let Some(image) = &req.image {
            proxy
                .set_background_image(image)
                .await
                .map_err(bus::classify_ipc)?;
        }
        proxy
            .set_effect(&req.effect)
            .await
            .map_err(bus::classify_ipc)?;
        bus::fetch(&conn).await
    })
    .await;
    match result {
        Ok(s) => {
            out::print(&human::effect_text(&s));
            EXIT_OK
        }
        Err(f) => report(&f, false),
    }
}

/// `denoise on|off`: checks that the daemon has noise removal, sets it, and
/// prints the result read back after the call.
async fn set_denoise(on: bool) -> u8 {
    let result = within(CALL_TIMEOUT, async {
        let conn = bus::connect().await?;
        let proxy = bus::proxy(&conn).await?;
        let revision = proxy
            .interface_revision()
            .await
            .map_err(|e| bus::classify(&e, true))?;
        bus::require_denoise(revision)?;
        proxy.set_denoise(on).await.map_err(bus::classify_ipc)?;
        bus::fetch(&conn).await
    })
    .await;
    match result {
        Ok(s) => {
            out::print(&human::denoise_text(&s));
            EXIT_OK
        }
        Err(f) => report(&f, false),
    }
}

/// The image controls of the active camera, without starting the daemon:
/// the properties first (for the revision, the state and the camera's
/// name), then `GetCameraControls` for that camera's id, which the daemon
/// reads from the device. With no camera present there is nothing to ask
/// (the daemon would answer `Unavailable`): the controls are empty, as in
/// the `CameraControls` property.
async fn read_controls(conn: &Connection) -> Result<(Snapshot, CameraControls), Failure> {
    let mut snap = bus::fetch(conn).await?;
    bus::require_controls(snap.interface_revision)?;
    // The active camera can change (or go away) between the properties and
    // the call: a `NoSuchCamera` for the id just read is answered by reading
    // the properties once more and asking again, once.
    let mut retried = false;
    loop {
        if snap.active_camera.is_empty() {
            return Ok((snap, CameraControls::default()));
        }
        match bus::camera_controls(conn, &snap.active_camera).await {
            Ok(controls) => {
                let camera = snap.active_camera.clone();
                return Ok((snap, CameraControls { camera, controls }));
            }
            Err(Failure::Rejected(IpcError::NoSuchCamera(_))) if !retried => {
                retried = true;
                snap = bus::fetch(conn).await?;
            }
            Err(f) => return Err(f),
        }
    }
}

/// `controls`: prints the image controls of the active camera.
async fn show_controls(json: bool) -> u8 {
    let result = within(READ_TIMEOUT, async {
        let conn = bus::connect().await?;
        read_controls(&conn).await
    })
    .await;
    match result {
        Ok((s, c)) => {
            if json {
                out::line(&json::controls_line(&c));
            } else {
                out::print(&human::controls_text(&s, &c));
            }
            EXIT_OK
        }
        Err(f) => report(&f, json),
    }
}

/// `controls set|reset`: checks that the daemon has the image controls,
/// makes the change on the active camera, and prints the controls read
/// back after it. The value's range is the daemon's to check.
async fn change_controls(action: &ControlsAction) -> u8 {
    // Whether the daemon accepted the change: a failure after that is only
    // the read-back's, and the message says so.
    let changed = std::cell::Cell::new(false);
    let result = within(CALL_TIMEOUT, async {
        let conn = bus::connect().await?;
        let proxy = bus::proxy(&conn).await?;
        let revision = proxy
            .interface_revision()
            .await
            .map_err(|e| bus::classify(&e, true))?;
        bus::require_controls(revision)?;
        match action {
            ControlsAction::Set { name, value } => proxy.set_camera_control("", name, *value).await,
            ControlsAction::Reset => proxy.reset_camera_controls("").await,
        }
        .map_err(bus::classify_ipc)?;
        changed.set(true);
        read_controls(&conn).await
    })
    .await;
    match result {
        Ok((s, c)) => {
            out::print(&human::controls_text(&s, &c));
            EXIT_OK
        }
        Err(f) if changed.get() => {
            // Still a failure (the exit status says the result is unknown),
            // but not a failed change.
            eprintln!(
                "omarchy-broadcast: the change was made, but reading the controls back failed: {}",
                human::printable(&f.message())
            );
            f.exit_code()
        }
        Err(f) => report(&f, false),
    }
}

/// The settings of `camera` (`""`: the active camera), without starting
/// the daemon: the properties first (for the revision, the state and the
/// camera's name), then `GetSettings`.
async fn read_settings(
    conn: &Connection,
    camera: &str,
) -> Result<(Snapshot, SettingsTree), Failure> {
    let snap = bus::fetch(conn).await?;
    bus::require_settings(snap.interface_revision)?;
    let tree = bus::settings_tree(conn, camera).await?;
    Ok((snap, tree))
}

/// `settings`: prints the settings of a camera, as JSON lines (the full
/// set, or `settings_clear` with nothing to show) or for a person.
async fn show_settings(json: bool, camera: &str) -> u8 {
    let result = within(READ_TIMEOUT, async {
        let conn = bus::connect().await?;
        read_settings(&conn, camera).await
    })
    .await;
    match result {
        Ok((s, tree)) => {
            if json {
                for line in json::settings_lines(&tree) {
                    out::line(&line);
                }
            } else {
                out::print(&human::settings_text(&s, &tree, human::Only::All));
            }
            EXIT_OK
        }
        Err(f) => report(&f, json),
    }
}

/// The value `text` names for `setting`, typed by the setting's type: `on`,
/// `off`, `true` or `false` for a bool; a decimal integer for an int (its
/// range is the daemon's to check); an option value id for an enum; nothing
/// or `true` for an action. `Err` is the usage message.
fn typed_value(setting: &Setting, text: Option<&str>) -> Result<SettingValue, String> {
    let id = human::printable(&setting.id);
    let missing = || format!("{id} needs a value");
    match &setting.kind {
        SettingKind::Bool { .. } => match text {
            Some("on" | "true") => Ok(SettingValue::Bool(true)),
            Some("off" | "false") => Ok(SettingValue::Bool(false)),
            Some(other) => Err(format!(
                "invalid value '{}' for {id}: it is a switch (on, off, true or false)",
                human::printable(other)
            )),
            None => Err(format!("{}: on, off, true or false", missing())),
        },
        SettingKind::Int { min, max, .. } => {
            let range = format!("a whole number from {min} to {max}");
            match text {
                // Decimal digits only: `i64::from_str` takes a sign, and
                // nothing else that is not a digit.
                Some(t) => t.parse::<i64>().map(SettingValue::Int).map_err(|_| {
                    format!("invalid value '{}' for {id}: {range}", human::printable(t))
                }),
                None => Err(format!("{}: {range}", missing())),
            }
        }
        SettingKind::Enum { options, .. } => {
            let ids = options
                .iter()
                .map(|(v, _)| human::printable(v).into_owned())
                .collect::<Vec<_>>()
                .join(", ");
            match text {
                Some(t) if options.iter().any(|(v, _)| v == t) => {
                    Ok(SettingValue::Str(t.to_string()))
                }
                Some(t) => Err(format!(
                    "invalid value '{}' for {id}: one of {ids}",
                    human::printable(t)
                )),
                None => Err(format!("{}: one of {ids}", missing())),
            }
        }
        SettingKind::Action => match text {
            None | Some("true") => Ok(SettingValue::Bool(true)),
            Some(other) => Err(format!(
                "invalid value '{}' for {id}: it is an action (give no value, or true)",
                human::printable(other)
            )),
        },
        SettingKind::Unknown(t) => Err(format!(
            "{id} is a setting of type '{}', which this version of omarchy-broadcast cannot set",
            human::printable(t)
        )),
    }
}

/// A usage error of `settings <action>`, found after the command line was
/// read (a value the setting's type does not take).
fn settings_usage(action: &str, msg: String) -> clap::Error {
    let mut cmd = Cli::command();
    cmd.build();
    match cmd
        .find_subcommand_mut("settings")
        .and_then(|s| s.find_subcommand_mut(action))
    {
        Some(sub) => sub.error(ErrorKind::ValueValidation, msg),
        None => cmd.error(ErrorKind::ValueValidation, msg),
    }
}

/// What a `settings set|reset` run came to.
enum SettingsOutcome {
    /// Made, and read back: print these.
    Done(Box<(Snapshot, SettingsTree)>),
    /// The value does not fit the setting's type: a usage error, nothing
    /// sent.
    Usage(String),
}

/// `settings set|reset`: checks that the daemon has the settings, makes
/// the change (for `set`, after reading the tree to type the value), and
/// prints the settings changed, read back after it.
async fn change_settings(action: &SettingsAction, camera: &str) -> u8 {
    let name = match action {
        SettingsAction::Set { .. } => "set",
        SettingsAction::Reset { .. } => "reset",
    };
    // Whether the daemon accepted the change: a failure after that is only
    // the read-back's, and the message says so.
    let changed = std::cell::Cell::new(false);
    let result = within(CALL_TIMEOUT, async {
        let conn = bus::connect().await?;
        let proxy = bus::proxy(&conn).await?;
        let revision = proxy
            .interface_revision()
            .await
            .map_err(|e| bus::classify(&e, true))?;
        bus::require_settings(revision)?;
        // A global setting (the video effects) is the same whatever the
        // camera, and the daemon ignores the camera for it: it is typed and
        // read back from the global groups, which every tree has (that of
        // the active camera, or of none), so an absent --camera is no error.
        let global = match action {
            SettingsAction::Set { id, .. }
            | SettingsAction::Reset {
                setting: Some(id), ..
            } => obc_ipc::settings::is_global_id(id),
            SettingsAction::Reset { .. } => false,
        };
        let read_camera = if global { "" } else { camera };
        match action {
            SettingsAction::Set { id, value, .. } => {
                let tree = proxy
                    .settings_tree(read_camera)
                    .await
                    .map_err(bus::classify_ipc)?;
                let found = tree
                    .groups
                    .iter()
                    .filter(|g| !global || g.is_global())
                    .flat_map(|g| &g.settings)
                    .find(|s| s.id == *id);
                let Some(setting) = found else {
                    return Err(Failure::Rejected(if global {
                        IpcError::UnknownSetting(format!("there is no setting {id:?}"))
                    } else if tree.camera.is_empty() {
                        IpcError::Unavailable("no camera is present".into())
                    } else {
                        IpcError::UnknownSetting(format!(
                            "the camera {:?} has no setting {id:?}",
                            tree.camera
                        ))
                    }));
                };
                let value = match typed_value(setting, value.as_deref()) {
                    Ok(v) => v,
                    Err(msg) => return Ok(SettingsOutcome::Usage(msg)),
                };
                proxy
                    .set_setting_value(camera, id, &value)
                    .await
                    .map_err(bus::classify_ipc)?;
            }
            SettingsAction::Reset { group, setting, .. } => {
                let scope = match (group, setting) {
                    (Some(g), _) => ResetScope::Group(g.clone()),
                    (None, Some(id)) => ResetScope::Setting(id.clone()),
                    (None, None) => ResetScope::Camera,
                };
                proxy
                    .reset_settings_scope(camera, &scope)
                    .await
                    .map_err(bus::classify_ipc)?;
            }
        }
        changed.set(true);
        let (snap, tree) = read_settings(&conn, read_camera).await?;
        Ok(SettingsOutcome::Done(Box::new((snap, tree))))
    })
    .await;
    match result {
        Ok(SettingsOutcome::Done(done)) => {
            let (s, tree) = *done;
            let only = match action {
                SettingsAction::Set { id, .. } => human::Only::Setting(id),
                SettingsAction::Reset { group: Some(g), .. } => human::Only::Group(g),
                SettingsAction::Reset {
                    setting: Some(id), ..
                } => human::Only::Setting(id),
                SettingsAction::Reset { .. } => human::Only::All,
            };
            out::print(&human::settings_text(&s, &tree, only));
            EXIT_OK
        }
        Ok(SettingsOutcome::Usage(msg)) => {
            let _ = settings_usage(name, msg).print();
            EXIT_USAGE
        }
        Err(f) if changed.get() => {
            eprintln!(
                "omarchy-broadcast: the change was made, but reading the settings back failed: {}",
                human::printable(&f.message())
            );
            f.exit_code()
        }
        Err(f) => report(&f, false),
    }
}

/// What `main` runs once the command line is checked.
enum Job {
    Command(Command),
    SetEffect(EffectRequest),
}

/// Checks what clap cannot: the options of `effect <effect>`.
fn job(cli: Cli) -> Result<Job, clap::Error> {
    match cli.command {
        Command::Effect {
            effect: Some(effect),
            strength,
            image,
            ..
        } => effect_request(&effect, strength, image.as_deref()).map(Job::SetEffect),
        Command::Settings {
            json: true,
            action: Some(_),
            ..
        } => {
            let mut cmd = Cli::command();
            cmd.build();
            let msg = "the argument '--json' cannot be used with a subcommand";
            Err(match cmd.find_subcommand_mut("settings") {
                Some(sub) => sub.error(ErrorKind::ArgumentConflict, msg),
                None => cmd.error(ErrorKind::ArgumentConflict, msg),
            })
        }
        command => Ok(Job::Command(command)),
    }
}

async fn run(job: Job) -> u8 {
    match &job {
        Job::SetEffect(req) => set_effect(req).await,
        Job::Command(Command::Status { json }) => show(*json, View::Status).await,
        Job::Command(Command::Cameras { json }) => show(*json, View::Cameras).await,
        Job::Command(Command::Effect { json, .. }) => show(*json, View::Effect).await,
        Job::Command(Command::Watch { json }) => watch::run(*json).await,
        Job::Command(Command::Preview { json, fps }) => preview::run(*json, *fps).await,
        Job::Command(Command::Denoise { value }) => set_denoise(value == "on").await,
        Job::Command(Command::Modules { action }) => {
            let list = modules::ModulesAction::List { json: false };
            modules::run_modules(action.as_ref().unwrap_or(&list)).await
        }
        Job::Command(Command::Order { action }) => {
            let show = modules::OrderAction::Show { json: false };
            modules::run_order(action.as_ref().unwrap_or(&show)).await
        }
        Job::Command(Command::Controls { json, action: None }) => show_controls(*json).await,
        Job::Command(Command::Controls {
            action: Some(action),
            ..
        }) => change_controls(action).await,
        Job::Command(Command::Settings {
            json,
            camera,
            action: None,
        }) => show_settings(*json, camera.as_deref().unwrap_or("")).await,
        Job::Command(Command::Settings {
            camera,
            action: Some(action),
            ..
        }) => change_settings(action, camera.as_deref().unwrap_or("")).await,
        Job::Command(command @ (Command::On | Command::Off | Command::Camera { .. })) => {
            control(command).await
        }
    }
}

fn main() -> ExitCode {
    let job = match Cli::try_parse().and_then(job) {
        Ok(job) => job,
        Err(e) => {
            // `--help` and `--version` are "errors" printed to stdout.
            let code = if e.use_stderr() { EXIT_USAGE } else { EXIT_OK };
            let _ = e.print();
            return ExitCode::from(code);
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("omarchy-broadcast: cannot start the async runtime: {e}");
            return ExitCode::from(EXIT_FAILURE);
        }
    };
    let code = runtime.block_on(run(job));
    // Exit at once: background threads (the stdout watcher of `watch`, the
    // bus connection's tasks) need no orderly shutdown.
    std::process::exit(i32::from(code));
}
