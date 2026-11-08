//! `oma-broadcast`: the command-line remote control of `oma-broadcastd`,
//! over the D-Bus interface `br.com.dias.OmaBroadcast1` (`docs/dbus-api.md`,
//! ADR-0011). `--json` output is the contract of `docs/cli-json.md`.

mod bus;
mod human;
mod json;
mod model;
mod out;
mod watch;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::error::ErrorKind;
use clap::{CommandFactory, Parser, Subcommand};
use obc_ipc::IpcError;

use crate::bus::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Failure, READ_TIMEOUT};

/// Time allowed for `on`, `off`, `camera` and `effect <effect>`, which may
/// wait for D-Bus activation of the daemon.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

const AFTER_HELP: &str = "\
Exit status:
  0  success
  1  the daemon rejected the request (no such camera, invalid config, invalid argument,
     unavailable, failed)
  2  invalid command line
  3  oma-broadcastd is not running, or there is no D-Bus session bus
  4  any other D-Bus failure, a timeout, or a failed write to stdout

`status`, `cameras`, `watch` and `effect` without an effect never start the daemon. `on`,
`off`, `camera` and `effect <effect>` may start it through D-Bus activation when its
activation file is installed.

`--json` output is documented in docs/cli-json.md; the D-Bus interface in docs/dbus-api.md.";

/// Remote control for oma-broadcastd, the Omarchy virtual-camera daemon.
#[derive(Parser)]
#[command(name = "oma-broadcast", version, after_help = AFTER_HELP)]
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
    /// Examples: `oma-broadcast effect blur --strength 0.7`,
    /// `oma-broadcast effect replace --image ~/Pictures/office.jpg`,
    /// `oma-broadcast effect none`.
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
    /// Follow state changes and stats until interrupted.
    Watch {
        /// Print one JSON object per line (NDJSON).
        #[arg(long)]
        json: bool,
    },
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
        eprintln!("oma-broadcast: {}", human::printable(&f.message()));
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
            eprintln!("oma-broadcast: cannot start the async runtime: {e}");
            return ExitCode::from(EXIT_FAILURE);
        }
    };
    let code = runtime.block_on(run(job));
    // Exit at once: background threads (the stdout watcher of `watch`, the
    // bus connection's tasks) need no orderly shutdown.
    std::process::exit(i32::from(code));
}
