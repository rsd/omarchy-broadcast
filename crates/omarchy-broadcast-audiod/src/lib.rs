//! `omarchy-broadcast-audiod`, the audio worker of Omarchy Broadcast: it
//! hosts the `audio-filter` modules (noise removal, room echo removal,
//! voice enhancement) for the Omarchy Broadcast Mic, in a process of its
//! own, so that neither a module's crash nor the video daemon's can take
//! the microphone away (the mic plugin plays raw audio whenever the worker
//! is not there).
//!
//! * [`listener`]: the socket, handed over by systemd or bound here;
//! * [`engine`]: the processing thread, one mic plugin at a time;
//! * [`chain`]: a loaded chain of module instances, failing open;
//! * [`host`]: the modules, the crash ledger and the chain's plan;
//! * [`snapshot`]: the audio settings the daemon pushes (and writes);
//! * [`dbus`]: the internal control interface;
//! * [`pipewire`]: the PipeWire side, behind a trait;
//! * [`mixer`]: the microphone's hardware capture gain (ALSA), behind a
//!   trait;
//! * [`status`]: what `GetStatus` says.

pub mod chain;
pub mod dbus;
pub mod engine;
pub mod host;
pub mod listener;
pub mod log;
pub mod mixer;
pub mod pipewire;
pub mod snapshot;
pub mod status;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use engine::{BusCmd, Engine, Exit, Shared};
use snapshot::Snapshot;

/// The command line.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Debug lines.
    pub verbose: bool,
    /// The configuration file (default: the daemon's).
    pub config: Option<PathBuf>,
    /// The snapshot file (default: beside the daemon's state file).
    pub snapshot: Option<PathBuf>,
    /// The socket to bind when none is handed over (default:
    /// [`obc_audio::default_socket_path`]).
    pub socket: Option<PathBuf>,
    /// Never connect to PipeWire (no device list, default source or
    /// consumers).
    pub no_pipewire: bool,
    /// The only module directories to scan, instead of the system's, the
    /// user's, the configuration's and `OBC_MODULE_PATH` (tests,
    /// development).
    pub module_dirs: Vec<PathBuf>,
}

/// The usage text.
pub const USAGE: &str = "usage: omarchy-broadcast-audiod [-v] [--config FILE] [--snapshot FILE] \
                         [--socket PATH] [--no-pipewire] [--module-dir DIR]...";

impl Options {
    /// Parses the arguments (without the program name).
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
        let mut o = Options::default();
        let mut it = args.into_iter();
        while let Some(a) = it.next() {
            let mut value = |name: &str| {
                it.next()
                    .map(PathBuf::from)
                    .ok_or_else(|| format!("{name} needs a value"))
            };
            match a.as_str() {
                "-v" | "--verbose" => o.verbose = true,
                "--config" => o.config = Some(value("--config")?),
                "--snapshot" => o.snapshot = Some(value("--snapshot")?),
                "--socket" => o.socket = Some(value("--socket")?),
                "--no-pipewire" => o.no_pipewire = true,
                "--module-dir" => o.module_dirs.push(value("--module-dir")?),
                "-h" | "--help" => return Err(USAGE.into()),
                other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
            }
        }
        Ok(o)
    }
}

/// The configuration: the daemon's files, read only (the module
/// directories and tables); the defaults when they cannot be used.
fn load_config(path: Option<PathBuf>) -> obc_core::config::Config {
    let Some(path) = path.or_else(obc_core::config::config_path) else {
        return obc_core::config::Config::default();
    };
    match obc_core::config_d::ConfigSet::load(&path) {
        Ok(set) => set.config,
        Err(e) => {
            warning!(
                "the configuration {} cannot be used ({e}): using the defaults",
                path.display()
            );
            obc_core::config::Config::default()
        }
    }
}

/// Runs the worker until it is idle or told to stop.
pub fn run(opts: Options) -> ExitCode {
    log::set_verbose(opts.verbose);
    let socket_path = opts.socket.clone().or_else(obc_audio::default_socket_path);
    let socket = match listener::open(socket_path.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            error!("cannot listen for the mic plugin: {e}");
            return ExitCode::FAILURE;
        }
    };
    if socket.activated {
        info!("listening on the socket handed over by systemd");
    } else if let Some(p) = &socket.bound {
        info!("listening on {}", p.display());
    }
    let config = load_config(opts.config);
    let snapshot_path = opts.snapshot.or_else(snapshot::default_path);
    let (snap, note) = match &snapshot_path {
        Some(p) => Snapshot::load(p),
        None => (
            Snapshot::default(),
            Some("neither XDG_STATE_HOME nor HOME is set: using the default audio settings".into()),
        ),
    };
    if let Some(n) = note {
        warning!("{n}");
    }
    let ledger = snapshot_path
        .as_ref()
        .and_then(|p| p.parent().map(|d| d.join(snapshot::LEDGER_FILE)));
    let dirs = if opts.module_dirs.is_empty() {
        obc_module_host::default_dirs(&config.modules.dirs)
    } else {
        opts.module_dirs.clone()
    };
    let host = host::Host::load(config, &dirs, ledger.as_deref(), &snap, log::module_sink());
    let control: Box<dyn pipewire::PipeWireControl> = if opts.no_pipewire {
        Box::new(pipewire::Unavailable)
    } else {
        Box::new(pipewire::Live::new())
    };
    let shared =
        Arc::new(Shared::new(host, snap, control).with_mixer(Box::new(mixer::Alsa::default())));
    let engine = Engine::new(shared.clone(), socket);
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<Exit>();
    let thread = std::thread::Builder::new()
        .name("obc-audio-engine".into())
        .spawn(move || {
            let exit = engine.run();
            let _ = done_tx.send(exit);
        });
    let thread = match thread {
        Ok(t) => t,
        Err(e) => {
            error!("cannot start the processing thread: {e}");
            return ExitCode::FAILURE;
        }
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!("cannot start the async runtime: {e}");
            shared.shutdown.store(true, Ordering::SeqCst);
            let _ = thread.join();
            return ExitCode::FAILURE;
        }
    };
    let exit = rt.block_on(async {
        use tokio::signal::unix::{SignalKind, signal};
        let conn = dbus::serve(shared.clone()).await;
        let (bus_tx, mut bus_rx) = tokio::sync::mpsc::unbounded_channel::<BusCmd>();
        if conn.is_some() {
            *engine::lock(&shared.bus) = Some(bus_tx.clone());
        }
        let mut term = signal(SignalKind::terminate()).ok();
        let mut int = signal(SignalKind::interrupt()).ok();
        let stop = async {
            tokio::select! {
                _ = async { match &mut term { Some(s) => { s.recv().await; } None => std::future::pending().await } } => {}
                _ = async { match &mut int { Some(s) => { s.recv().await; } None => std::future::pending().await } } => {}
            }
        };
        tokio::pin!(stop);
        let mut done_rx = done_rx;
        loop {
            tokio::select! {
                r = &mut done_rx => break r.unwrap_or(Exit::Shutdown),
                _ = &mut stop => {
                    info!("stopping");
                    shared.shutdown.store(true, Ordering::SeqCst);
                    break (&mut done_rx).await.unwrap_or(Exit::Shutdown);
                }
                Some(cmd) = bus_rx.recv() => match (cmd, &conn) {
                    (BusCmd::Release(done), Some(c)) => {
                        dbus::release(c).await;
                        let _ = done.send(());
                    }
                    (BusCmd::Reacquire, Some(c)) => dbus::reacquire(c).await,
                    (BusCmd::Release(done), None) => {
                        let _ = done.send(());
                    }
                    (BusCmd::Reacquire, None) => {}
                },
            }
        }
    });
    let _ = thread.join();
    shared.pipewire.shutdown();
    match exit {
        Exit::Idle => info!("idle: exiting"),
        Exit::Shutdown => info!("stopped"),
    }
    ExitCode::SUCCESS
}
