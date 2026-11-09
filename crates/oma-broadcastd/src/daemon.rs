//! Process wiring: the session-bus connection and name, the watcher
//! threads, the controller thread and the signal handlers.
//!
//! Start-up order:
//!
//! 1. the object is served (with a snapshot read from the files alone), on
//!    a connection that does not own the well-known name yet;
//! 2. `NameHasOwner`: if another process owns the name, the daemon exits
//!    ([`EXIT_NAME_TAKEN`]) before it could disturb the running one's
//!    loopback;
//! 3. the controller starts and makes its first loopback writer attempt
//!    (the writer streams, or the failure is in `State`/`Error`);
//! 4. only then is the name requested (`DO_NOT_QUEUE`; losing a race to
//!    another instance between 2 and 4 stops the writer and exits
//!    [`EXIT_NAME_TAKEN`] as well).
//!
//! With `Type=dbus`, systemd considers the service ready when the name is
//! taken, so a unit ordered after `oma-broadcastd.service` (WirePlumber,
//! with the drop-in in `dist/`) finds the loopback already advertised as a
//! capture device. A D-Bus activated start finds the object in place when
//! the bus hands over the activating call, which it queues until the name
//! is owned.
//!
//! A lost bus connection (the bus went away) ends the daemon with
//! [`EXIT_FAILURE`], so the service manager restarts it.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};
use std::time::Instant;

use obc_ipc::{BUS_NAME, OBJECT_PATH};
use tokio::signal::unix::{SignalKind, signal};
use zbus::fdo::{RequestNameFlags, RequestNameReply};

use crate::backend::PipeWireReporter;
use crate::backend::real::GstBackend;
use crate::consumers::ConsumerWatcher;
use crate::controller::{self, Command, Controller, Paths, Published, preliminary_snapshot};
use crate::dbus::{Server, run_publisher};
use crate::hotplug::{CameraWatcher, SECOND_SCAN, SETTLE};
use crate::{error, info, warning};

/// Exit status: a failure (no session bus, a thread could not start, ...).
pub const EXIT_FAILURE: u8 = 1;
/// Exit status: another process owns `br.com.dias.OmaBroadcast` (the
/// daemon is already running). `oma-broadcastd.service` does not restart
/// on it.
pub const EXIT_NAME_TAKEN: u8 = 3;

/// Command-line options.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Config file (default `$XDG_CONFIG_HOME/oma-broadcast/config.toml`).
    pub config: Option<PathBuf>,
    /// State file (default `$XDG_STATE_HOME/oma-broadcast/state.toml`).
    pub state: Option<PathBuf>,
    pub verbose: bool,
}

#[derive(Debug, thiserror::Error)]
enum DaemonError {
    #[error("{BUS_NAME} is already owned on the session bus: oma-broadcastd is already running")]
    NameTaken,
    #[error("D-Bus: {0}")]
    Bus(#[from] zbus::Error),
    #[error("{0}")]
    Other(String),
}

fn resolve_paths(opts: &Options) -> Result<Paths, String> {
    let config = opts
        .config
        .clone()
        .or_else(obc_core::config::config_path)
        .ok_or("neither XDG_CONFIG_HOME nor HOME is set: pass --config")?;
    let state = opts
        .state
        .clone()
        .or_else(obc_core::config::state_path)
        .ok_or("neither XDG_STATE_HOME nor HOME is set: pass --state")?;
    Ok(Paths { config, state })
}

/// Runs the daemon until SIGTERM or SIGINT.
pub fn run(opts: Options) -> ExitCode {
    crate::log::set_verbose(opts.verbose);
    let paths = match resolve_paths(&opts) {
        Ok(p) => p,
        Err(e) => {
            error!("{e}");
            return ExitCode::from(EXIT_FAILURE);
        }
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!("cannot start the async runtime: {e}");
            return ExitCode::from(EXIT_FAILURE);
        }
    };
    match rt.block_on(serve(paths)) {
        Ok(()) => {
            info!("stopped");
            ExitCode::SUCCESS
        }
        Err(e @ DaemonError::NameTaken) => {
            error!("{e}");
            ExitCode::from(EXIT_NAME_TAKEN)
        }
        Err(e) => {
            error!("{e}");
            ExitCode::from(EXIT_FAILURE)
        }
    }
}

/// The consumer watcher, and the hook through which the controller tells it
/// the loopback node.
fn consumer_watcher(commands: &Sender<Command>) -> Result<controller::SinkHook, DaemonError> {
    let tx = commands.clone();
    let watcher = ConsumerWatcher::spawn(PathBuf::from("/proc"), std::process::id(), move |n| {
        let _ = tx.send(Command::Consumers(n));
    })
    .map_err(|e| DaemonError::Other(format!("cannot start the consumer watcher: {e}")))?;
    Ok(Box::new(move |node: Option<&Path>| {
        watcher.watch(node.map(Path::to_path_buf));
    }))
}

/// The PipeWire output's reporter: its events go to the controller, with
/// the generation of the output that sent them, on the same command channel
/// as the consumer watcher's counts.
fn pipewire_reporter(commands: &Sender<Command>) -> PipeWireReporter {
    let tx = commands.clone();
    Arc::new(move |generation, event| {
        let _ = tx.send(Command::PipeWire(generation, event));
    })
}

async fn serve(paths: Paths) -> Result<(), DaemonError> {
    let io = |what: &str| {
        let what = what.to_string();
        move |e: std::io::Error| DaemonError::Other(format!("{what}: {e}"))
    };
    let mut hup = signal(SignalKind::hangup()).map_err(io("SIGHUP handler"))?;
    let mut term = signal(SignalKind::terminate()).map_err(io("SIGTERM handler"))?;
    let mut int = signal(SignalKind::interrupt()).map_err(io("SIGINT handler"))?;

    let (published, notes) = Published::new(preliminary_snapshot(&paths));
    let (commands, command_rx) = channel::<Command>();
    let conn = zbus::connection::Builder::session()
        .serve_at(
            OBJECT_PATH,
            Server::new(published.clone(), commands.clone()),
        )
        .build()
        .await?;
    // Another instance runs: stop before touching any device.
    let bus = zbus::fdo::DBusProxy::new(&conn).await?;
    let name: zbus::names::BusName<'_> = BUS_NAME
        .try_into()
        .map_err(|e| DaemonError::Other(format!("{BUS_NAME}: {e}")))?;
    if bus.name_has_owner(name).await.map_err(zbus::Error::from)? {
        return Err(DaemonError::NameTaken);
    }

    let hook = consumer_watcher(&commands)?;
    let reporter = pipewire_reporter(&commands);
    let tx = commands.clone();
    let cameras = CameraWatcher::spawn(PathBuf::from("/dev"), SETTLE, SECOND_SCAN, move || {
        let _ = tx.send(Command::Rescan);
    })
    .inspect_err(|e| warning!("camera hotplug detection is unavailable: {e}"))
    .ok();

    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
    let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
    let thread = std::thread::Builder::new()
        .name("obc-controller".into())
        .spawn(move || {
            let res = GstBackend::new(Some(reporter)).map(|backend| {
                // Controller::new makes the first writer attempt.
                let ctrl = Controller::new(backend, paths, published, Some(hook), Instant::now());
                let _ = ready_tx.send(());
                controller::run(ctrl, command_rx);
            });
            let _ = done_tx.send(res);
        })
        .map_err(io("controller thread"))?;

    let mut ended: Option<Result<(), String>> = None;
    let mut failure: Option<DaemonError> = None;
    // Wait for the first writer attempt before taking the name.
    let mut ready = false;
    tokio::select! {
        r = ready_rx => ready = r.is_ok(),
        res = &mut done_rx => {
            ended = Some(res.unwrap_or_else(|_| Err("the controller thread panicked".into())));
        }
        _ = term.recv() => info!("SIGTERM"),
        _ = int.recv() => info!("SIGINT"),
        _ = conn.closed() => failure = Some(lost_bus()),
    }
    if ready {
        // Neither replacing an owner nor letting another instance replace
        // us (zbus's default flags allow both), and no queueing.
        match conn
            .request_name_with_flags(BUS_NAME, RequestNameFlags::DO_NOT_QUEUE)
            .await
        {
            Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {
                info!(
                    "{BUS_NAME} on the session bus (version {})",
                    env!("CARGO_PKG_VERSION")
                );
                tokio::spawn(run_publisher(conn.clone(), notes));
            }
            Ok(RequestNameReply::Exists | RequestNameReply::InQueue)
            | Err(zbus::Error::NameTaken) => {
                warning!("another instance took {BUS_NAME} meanwhile: stopping");
                failure = Some(DaemonError::NameTaken);
            }
            Err(e) => failure = Some(e.into()),
        }
    }
    while ready && failure.is_none() && ended.is_none() {
        tokio::select! {
            _ = hup.recv() => {
                info!("SIGHUP: reloading the configuration");
                let _ = commands.send(Command::ReloadConfig(None));
            }
            _ = term.recv() => {
                info!("SIGTERM");
                break;
            }
            _ = int.recv() => {
                info!("SIGINT");
                break;
            }
            res = &mut done_rx => {
                ended = Some(res.unwrap_or_else(|_| Err("the controller thread panicked".into())));
                break;
            }
            _ = conn.closed() => {
                failure = Some(lost_bus());
                break;
            }
        }
    }
    let _ = commands.send(Command::Shutdown);
    let res = match ended {
        Some(r) => r,
        None => done_rx
            .await
            .unwrap_or_else(|_| Err("the controller thread panicked".into())),
    };
    let _ = thread.join();
    if let Some(c) = cameras {
        c.stop();
    }
    if !conn.is_closed() {
        let _ = conn.release_name(BUS_NAME).await;
    }
    if let Some(f) = failure {
        return Err(f);
    }
    res.map_err(DaemonError::Other)
}

/// The bus connection closed under the daemon (the bus went away): it can
/// no longer be controlled, so it exits with a failure and the service
/// manager restarts it.
fn lost_bus() -> DaemonError {
    DaemonError::Other(
        "lost the session bus connection: exiting (the service manager restarts the daemon)".into(),
    )
}
