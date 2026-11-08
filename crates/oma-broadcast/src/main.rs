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
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use obc_ipc::IpcError;

use crate::bus::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Failure, READ_TIMEOUT};

/// Time allowed for `on`, `off` and `camera`, which may wait for D-Bus
/// activation of the daemon.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

const AFTER_HELP: &str = "\
Exit status:
  0  success
  1  the daemon rejected the request (no such camera, invalid config, unavailable, failed)
  2  invalid command line
  3  oma-broadcastd is not running, or there is no D-Bus session bus
  4  any other D-Bus failure, a timeout, or a failed write to stdout

`status`, `cameras` and `watch` never start the daemon. `on`, `off` and `camera` may start
it through D-Bus activation when its activation file is installed.

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
    /// Follow state changes and stats until interrupted.
    Watch {
        /// Print one JSON object per line (NDJSON).
        #[arg(long)]
        json: bool,
    },
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

/// Reports `f` and returns its exit status: on stderr, or as a JSON error
/// line on stdout.
fn report(f: &Failure, json: bool) -> u8 {
    if json {
        out::line(&json::error_line(f.json_code(), &f.message()));
    } else {
        eprintln!("oma-broadcast: {}", f.message());
    }
    f.exit_code()
}

/// `status` and `cameras`.
async fn show(json: bool, cameras: bool) -> u8 {
    let snap = within(READ_TIMEOUT, async {
        let conn = bus::connect().await?;
        bus::fetch(&conn).await
    })
    .await;
    match snap {
        Ok(s) => {
            match (json, cameras) {
                (true, false) => out::line(&json::snapshot_line(&s)),
                (true, true) => out::line(&json::cameras_line(&s)),
                (false, false) => out::print(&human::status_text(&s)),
                (false, true) => out::print(&human::cameras_text(&s)),
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

async fn run(cli: Cli) -> u8 {
    match &cli.command {
        Command::Status { json } => show(*json, false).await,
        Command::Cameras { json } => show(*json, true).await,
        Command::Watch { json } => watch::run(*json).await,
        command @ (Command::On | Command::Off | Command::Camera { .. }) => control(command).await,
    }
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
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
    let code = runtime.block_on(run(cli));
    // Exit at once: background threads (the stdout watcher of `watch`, the
    // bus connection's tasks) need no orderly shutdown.
    std::process::exit(i32::from(code));
}
