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
//!    [`EXIT_NAME_TAKEN`] as well);
//! 5. with the name taken, the controller loads the modules (counting the
//!    crash ledger for this start) before it handles any command: an
//!    instance that loses the name never touches the ledger; and the
//!    microphone's agent starts ([`crate::mic::agent`]), so an instance
//!    that loses the name never writes the audio settings either.
//!
//! Between 2 and 3, preview frame files a crashed daemon left in
//! `$XDG_RUNTIME_DIR/omarchy-broadcast` are removed, and the subscription
//! to the clients leaving the bus (which ends their preview leases) is
//! made. At exit the controller ends every lease and the files are removed
//! again.
//!
//! With `Type=dbus`, systemd considers the service ready when the name is
//! taken, so a unit ordered after `omarchy-broadcastd.service` (WirePlumber,
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
use std::time::{Duration, Instant};

use obc_core::config_d::ConfigSet;
use obc_ipc::{BUS_NAME, OBJECT_PATH};
use tokio::signal::unix::{SignalKind, signal};
use zbus::fdo::{RequestNameFlags, RequestNameReply};

use crate::backend::real::GstBackend;
use crate::backend::{
    ControlEventReporter, OrientationReporter, PipeWireReporter, PreviewReporter,
};
use crate::consumers::ConsumerWatcher;
use crate::controller::{
    self, Command, Controller, GalleryVideos, Paths, Published, preliminary_snapshot,
};
use crate::dbus::{Server, run_owner_watch, run_publisher, subscribe_owners};
use crate::hotplug::{CameraWatcher, SECOND_SCAN, SETTLE};
use crate::media::MediaStore;
use crate::power::{ProfileCache, run_profile_watch};
use crate::preview::PreviewDir;
use crate::{error, info, warning};

/// Exit status: a failure (no session bus, a thread could not start, ...).
pub const EXIT_FAILURE: u8 = 1;
/// Exit status: another process owns `br.com.dias.OmarchyBroadcast` (the
/// daemon is already running). `omarchy-broadcastd.service` does not restart
/// on it.
pub const EXIT_NAME_TAKEN: u8 = 3;

/// How long the start waits for the first read of power-profiles-daemon's
/// profile, under a battery saver mode that reads it
/// ([`ProfileCache::follow_and_read`]); past it the start carries on with
/// the profile unavailable, as before the read, and the watcher's read
/// that follows is taken as any change of the profile.
pub const FIRST_PROFILE_READ: Duration = Duration::from_millis(500);

/// Whether the battery saver's mode in the configuration at `config` reads
/// power-profiles-daemon's profile: as the controller loads it, the
/// defaults when it cannot be loaded.
fn saver_reads_profile(config: &Path) -> bool {
    ConfigSet::load(config)
        .map(|s| s.config.video.battery_saver)
        .unwrap_or_default()
        .reads_power_profile()
}

/// Command-line options.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Config file (default `$XDG_CONFIG_HOME/omarchy-broadcast/config.toml`).
    pub config: Option<PathBuf>,
    /// State file (default `$XDG_STATE_HOME/omarchy-broadcast/state.toml`).
    pub state: Option<PathBuf>,
    pub verbose: bool,
}

#[derive(Debug, thiserror::Error)]
enum DaemonError {
    #[error(
        "{BUS_NAME} is already owned on the session bus: omarchy-broadcastd is already running"
    )]
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
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let ledger = state.parent().map(|d| d.join(crate::modules::LEDGER_FILE));
    Ok(Paths {
        config,
        state,
        cache: obc_core::config::cache_dir(),
        runtime,
        ledger,
    })
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

/// The preview's reporter: every frame written becomes a `PreviewFrame`
/// signal, through the publisher's channel (the controller is not woken).
fn preview_reporter(published: &Published) -> PreviewReporter {
    let published = published.clone();
    Arc::new(move |seq| published.preview_frame(seq))
}

/// Removes the preview frame files under `runtime`, if any (a daemon that
/// crashed may have left them; the controller removes them otherwise).
fn remove_preview_files(runtime: Option<&Path>) {
    if let Ok(dir) = PreviewDir::under(runtime) {
        dir.remove_files();
    }
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

/// The control-event watcher's reporter: the open camera's control
/// changes go to the controller, with the generation of the watcher that
/// saw them, on the same command channel.
fn control_reporter(commands: &Sender<Command>) -> ControlEventReporter {
    let tx = commands.clone();
    Arc::new(move |generation, event| {
        let _ = tx.send(Command::ControlEvent(generation, event));
    })
}

/// The orientation watcher's reporter: which way the active camera faces
/// goes to the controller, with the generation of the watcher that read
/// it, on the same command channel.
fn orientation_reporter(commands: &Sender<Command>) -> OrientationReporter {
    let tx = commands.clone();
    Arc::new(move |generation, event| {
        let _ = tx.send(Command::Orientation(generation, event));
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
    let media =
        MediaStore::from_env().inspect_err(|e| warning!("the media gallery is unavailable: {e}"));
    let store = media.as_ref().ok().cloned();
    let conn = zbus::connection::Builder::session()
        .serve_at(
            OBJECT_PATH,
            Server::new(published.clone(), commands.clone(), media),
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

    // The gallery of an earlier version moves to the pictures directory
    // now: past the check above (which two daemons started at once may
    // both pass: the move holds a lock of the old directory, so only one
    // of them moves it), before the name is requested (no GetMedia is
    // addressed to the daemon yet) and before the controller thread
    // starts (its configuration is loaded after the managed file's paths
    // are rewritten; the cache is pruned after, and the posters of videos
    // a deferred migration left may go and are remade). Off the executor, and bounded in time (systemd's start
    // timeout): a copy across file systems of large videos takes a
    // moment, and what is left waits for the next start.
    if let Some(store) = store.clone() {
        let (config, state) = (paths.config.clone(), paths.state.clone());
        let moved = tokio::task::spawn_blocking(move || {
            crate::media::migrate::at_start(&store, &config, &state)
        });
        if let Err(e) = moved.await {
            warning!("media: moving the old gallery failed: {e}");
        }
    }
    // Video backgrounds: the posters of the gallery's cache. Its prune
    // (the playback copies of earlier versions go, and the posters no video
    // needs) starts once the controller names what the background in force
    // needs: after the move above, never before it.
    let videos = GalleryVideos::new(store, commands.clone());

    // No other instance: what is in the preview directory is stale.
    let runtime = paths.runtime.clone();
    remove_preview_files(runtime.as_deref());
    let owners = subscribe_owners(&conn).await?;
    tokio::spawn(run_owner_watch(owners, commands.clone()));
    // The battery saver's power profile: followed on the system bus into
    // a cache the controller reads without waiting, only while the mode
    // reads it (`power_profile`, `auto`; the controller says so through
    // the backend), and never starting power-profiles-daemon.
    let profiles = ProfileCache::default();
    let tx = commands.clone();
    tokio::spawn(run_profile_watch(profiles.clone(), move || {
        let _ = tx.send(Command::PowerChanged);
    }));
    // The controller's first decision of the saver (the memory the modules
    // open with) is taken from the profile as read, not from an empty
    // cache: the first read is waited for, briefly.
    if saver_reads_profile(&paths.config) && !profiles.follow_and_read(FIRST_PROFILE_READ).await {
        info!(
            "battery saver: power-profiles-daemon's profile was not read within {} ms: \
             following the battery until it is",
            FIRST_PROFILE_READ.as_millis()
        );
    }

    let hook = consumer_watcher(&commands)?;
    let reporter = pipewire_reporter(&commands);
    let previews = preview_reporter(&published);
    let controls = control_reporter(&commands);
    let orientation = orientation_reporter(&commands);
    let tx = commands.clone();
    let cameras = CameraWatcher::spawn(PathBuf::from("/dev"), SETTLE, SECOND_SCAN, move || {
        let _ = tx.send(Command::Rescan);
    })
    .inspect_err(|e| warning!("camera hotplug detection is unavailable: {e}"))
    .ok();

    // The microphone's agent: the controller sends to it from the name on.
    let (mic_tx, mic_rx) = tokio::sync::mpsc::unbounded_channel();
    let mic_snapshot = paths
        .state
        .parent()
        .map(|d| d.join(crate::mic::SNAPSHOT_FILE));
    let mic_published = published.clone();
    let mut mic_rx = Some(mic_rx);

    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
    // The modules are loaded (and the crash ledger counted) once the name
    // is ours: an instance that loses it never touches the ledger.
    let (name_tx, name_rx) = tokio::sync::oneshot::channel::<()>();
    let mut name_tx = Some(name_tx);
    let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
    let thread = std::thread::Builder::new()
        .name("obc-controller".into())
        .spawn(move || {
            let res = GstBackend::new(Some(reporter), Some(previews)).map(|backend| {
                let backend = backend
                    .with_control_reporter(controls)
                    .with_orientation_reporter(orientation)
                    .with_power_profiles(profiles);
                // Controller::new_without_modules makes the first writer
                // attempt.
                let mut ctrl = Controller::new_without_modules(
                    backend,
                    paths,
                    published,
                    Some(hook),
                    Instant::now(),
                );
                let _ = ready_tx.send(());
                // Commands wait in the channel meanwhile; without the name
                // the daemon stops (the sender is dropped).
                if name_rx.blocking_recv().is_ok() {
                    ctrl.set_mic(mic_tx);
                    // Once the name is ours: its prune must not race
                    // another instance's posters in the same cache.
                    ctrl.set_videos(Box::new(videos));
                    ctrl.load_modules(Instant::now());
                }
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
                if let Some(rx) = mic_rx.take() {
                    crate::mic::agent::spawn(
                        conn.clone(),
                        rx,
                        commands.clone(),
                        mic_published.clone(),
                        mic_snapshot.clone(),
                    );
                }
                if let Some(tx) = name_tx.take() {
                    let _ = tx.send(());
                }
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
    // A controller still waiting for the name goes straight to the
    // shutdown, without the modules.
    drop(name_tx.take());
    let _ = commands.send(Command::Shutdown);
    let res = match ended {
        Some(r) => r,
        None => done_rx
            .await
            .unwrap_or_else(|_| Err("the controller thread panicked".into())),
    };
    let _ = thread.join();
    remove_preview_files(runtime.as_deref());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_start_waits_for_the_profile_only_under_a_mode_that_reads_it() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        // No file, and the defaults: `auto` reads it.
        assert!(saver_reads_profile(&config));
        for (mode, reads) in [
            ("off", false),
            ("auto", true),
            ("on_battery", false),
            ("power_profile", true),
            ("always", false),
        ] {
            std::fs::write(&config, format!("[video]\nbattery_saver = \"{mode}\"\n")).unwrap();
            assert_eq!(saver_reads_profile(&config), reads, "{mode}");
        }
        // A configuration that cannot be loaded: the defaults, as the
        // controller runs on.
        std::fs::write(&config, "[video\n").unwrap();
        assert!(saver_reads_profile(&config));
        assert!(FIRST_PROFILE_READ <= Duration::from_millis(500));
    }
}
