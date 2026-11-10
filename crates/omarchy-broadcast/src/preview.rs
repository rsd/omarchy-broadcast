//! `preview`: holds a live preview lease on the daemon and prints a line per
//! frame it writes, until a signal or a closed stdout ends it.
//!
//! The lease (`AcquirePreview`, interface revision 5) belongs to this
//! process's own bus connection: when the process ends, however it ends,
//! the connection closes and the daemon ends the lease. On `SIGTERM`,
//! `SIGINT`, `SIGHUP`, `SIGPIPE` or a closed stdout it calls
//! `ReleasePreview` first (best effort, within [`RELEASE_TIMEOUT`]) and
//! exits 0.
//!
//! The `PreviewFrame` and `NameOwnerChanged` subscriptions are made before
//! the lease is asked for, so no frame is missed. The daemon writes each
//! frame to the file whose path `AcquirePreview` returned and announces it;
//! each announcement becomes one `frame` line with that path. While the
//! daemon (or the session bus) is absent, one error line is printed for the
//! outage and the daemon is waited for as `watch` does (every [`RETRY`], at
//! once on `NameOwnerChanged`); when it is back the lease is asked for
//! again. A daemon too old for the preview is reported once
//! (`unsupported-daemon`) and waited for until another one owns the name;
//! so is one whose frame path does not fit a JSON line
//! (`preview-path-too-long`, `--json` only: a path is never clipped), after
//! its lease is released at once. A lease lost with the bus connection is
//! announced before the outage. Nothing here starts the daemon: every call
//! carries `NO_AUTO_START`.

use std::sync::Mutex;
use std::time::Duration;

use futures_util::StreamExt;
use obc_ipc::BUS_NAME;
use tokio::signal::unix::{SignalKind, signal};
use zbus::Connection;
use zbus::fdo::DBusProxy;
use zbus::names::OwnedUniqueName;
use zbus::proxy::MethodFlags;

use crate::bus::{self, EXIT_FAILURE, EXIT_OK, Failure, READ_TIMEOUT};
use crate::watch::{RETRY, bus_silent, daemon_name, wait_for_owner};
use crate::{human, json, out};

/// How long the release at exit may take.
pub const RELEASE_TIMEOUT: Duration = Duration::from_secs(1);

/// The connection that holds the lease, for the release at exit.
type Held = Mutex<Option<Connection>>;

/// The exit status of a failed write to stdout: 0 for a reader that went
/// away, [`EXIT_FAILURE`] otherwise (with a message).
fn write_failed(e: &std::io::Error) -> u8 {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        EXIT_OK
    } else {
        eprintln!("omarchy-broadcast: cannot write to stdout: {e}");
        EXIT_FAILURE
    }
}

/// Prints the lines, in JSON or for a person. A failed write is an `Err`
/// carrying the exit status.
struct Printer {
    json: bool,
    /// An outage has been announced and no lease has followed yet.
    in_outage: bool,
    /// A lease is held (and was announced).
    active: bool,
}

impl Printer {
    fn emit(&self, text: &str) -> Result<(), u8> {
        out::try_line(text).map_err(|e| write_failed(&e))
    }

    fn held(&mut self, fps: u32, path: &str) -> Result<(), u8> {
        self.in_outage = false;
        self.active = true;
        if self.json {
            self.emit(&json::preview_line(Some((fps, path))))
        } else {
            self.emit(&format!("preview: {} at {fps} fps", human::printable(path)))
        }
    }

    /// The lease is gone (the daemon left); announced once per lease.
    fn lost(&mut self) -> Result<(), u8> {
        if !std::mem::take(&mut self.active) {
            return Ok(());
        }
        if self.json {
            self.emit(&json::preview_line(None))
        } else {
            self.emit("preview: stopped")
        }
    }

    fn frame(&self, seq: u32, path: &str) -> Result<(), u8> {
        if self.json {
            self.emit(&json::frame_line(seq, path))
        } else {
            self.emit(&format!("frame {seq}"))
        }
    }

    /// Announces an outage, once until the next lease.
    fn outage(&mut self, f: &Failure) -> Result<(), u8> {
        if self.in_outage {
            return Ok(());
        }
        self.in_outage = true;
        if self.json {
            self.emit(&json::error_line(f.json_code(), &f.message()))
        } else {
            self.emit(&format!(
                "{}; waiting for it",
                human::printable(&f.message())
            ))
        }
    }
}

/// How one connected session of [`follow`] ended.
enum Ended {
    /// The name lost its owner, or never had one.
    NotRunning(Failure),
    /// The name moved to a new owner: ask for the lease again.
    NewOwner,
    /// The daemon is there but cannot be read or refused the lease: retry
    /// after [`RETRY`].
    Unreadable(Failure),
    /// The daemon (owned by that name) cannot serve this preview (too old,
    /// or its frame path does not fit a JSON line): wait for another owner.
    Unsupported(Failure, OwnedUniqueName),
    /// The bus connection failed or closed.
    BusLost(Failure),
    /// Stdout failed: exit with this status.
    Output(u8),
}

fn lost(e: zbus::Error) -> Ended {
    Ended::BusLost(Failure::Other(format!("lost the D-Bus session bus: {e}")))
}

fn stream_ended() -> Ended {
    Ended::BusLost(Failure::Other(
        "the D-Bus session bus connection closed".into(),
    ))
}

fn failed(f: Failure) -> Ended {
    if f.is_not_running() {
        Ended::NotRunning(f)
    } else {
        Ended::Unreadable(f)
    }
}

/// `AcquirePreview(fps)` without starting the daemon: the frame file's path.
async fn acquire(conn: &Connection, fps: u32) -> Result<String, Failure> {
    let proxy = bus::proxy(conn).await?;
    let reply: Option<String> = proxy
        .inner()
        .call_with_flags("AcquirePreview", MethodFlags::NO_AUTO_START, &(fps,))
        .await
        .map_err(bus::classify_no_autostart)?;
    reply.ok_or_else(|| Failure::Other("AcquirePreview returned no reply".into()))
}

/// `ReleasePreview()` on the connection that holds the lease, if any,
/// within [`RELEASE_TIMEOUT`]; failures are ignored (the lease ends with
/// the connection anyway).
async fn release(held: &Held) {
    let conn = held.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(conn) = conn {
        release_on(&conn).await;
    }
}

/// `ReleasePreview()` on `conn` without starting the daemon, within
/// [`RELEASE_TIMEOUT`]; failures are ignored.
async fn release_on(conn: &Connection) {
    let _ = tokio::time::timeout(RELEASE_TIMEOUT, async {
        let proxy = bus::proxy(conn).await.ok()?;
        proxy
            .inner()
            .call_with_flags::<_, _, ()>("ReleasePreview", MethodFlags::NO_AUTO_START, &())
            .await
            .ok()
    })
    .await;
}

/// The bus connection closed while the lease was held: the lease is gone
/// with it, which is said before the outage.
fn closed_while_held(p: &mut Printer, held: &Held) -> Ended {
    held.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Err(code) = p.lost() {
        return Ended::Output(code);
    }
    stream_ended()
}

/// The subscriptions of one [`follow`] session and the name's owner at the
/// time they were made.
struct Subscriptions {
    owner: OwnedUniqueName,
    owners: zbus::fdo::NameOwnerChangedStream,
    frames: obc_ipc::client::PreviewFrameStream,
}

/// Subscribes to the name's owner changes and `PreviewFrame`, then asks for
/// the current owner.
async fn subscribe(conn: &Connection) -> Result<Subscriptions, Ended> {
    let dbus = DBusProxy::new(conn).await.map_err(lost)?;
    let owners = dbus
        .receive_name_owner_changed_with_args(&[(0, BUS_NAME)])
        .await
        .map_err(lost)?;
    let proxy = bus::proxy(conn).await.map_err(Ended::BusLost)?;
    let frames = proxy.receive_preview_frame().await.map_err(lost)?;
    let owner = match dbus.get_name_owner(daemon_name()).await {
        Ok(o) => o,
        Err(zbus::fdo::Error::NameHasNoOwner(_)) => {
            return Err(Ended::NotRunning(Failure::not_running()));
        }
        Err(e) => return Err(lost(e.into())),
    };
    Ok(Subscriptions {
        owner,
        owners,
        frames,
    })
}

/// Holds the lease on `conn` and prints its frames until it is lost.
async fn follow(conn: &Connection, p: &mut Printer, fps: u32, held: &Held) -> Ended {
    let subscribed = match tokio::time::timeout(READ_TIMEOUT, subscribe(conn)).await {
        Ok(Ok(s)) => s,
        Ok(Err(ended)) => return ended,
        Err(_) => return Ended::BusLost(bus_silent()),
    };
    let Subscriptions {
        owner,
        mut owners,
        mut frames,
    } = subscribed;
    // The revision first: an older daemon has no AcquirePreview.
    let snap = match bus::fetch(conn).await {
        Ok(s) => s,
        Err(f) => return failed(f),
    };
    if let Err(f) = bus::require_preview(snap.interface_revision) {
        return Ended::Unsupported(f, owner);
    }
    let path = match tokio::time::timeout(READ_TIMEOUT, acquire(conn, fps)).await {
        Ok(Ok(path)) => path,
        Ok(Err(f)) => return failed(f),
        Err(_) => return Ended::Unreadable(bus::timed_out(READ_TIMEOUT)),
    };
    if p.json && !json::preview_path_fits(&path) {
        // Every frame line would carry a clipped path, which names no file:
        // the lease serves nobody.
        release_on(conn).await;
        return Ended::Unsupported(bus::preview_path_too_long(&path), owner);
    }
    *held.lock().unwrap_or_else(|e| e.into_inner()) = Some(conn.clone());
    if let Err(code) = p.held(fps, &path) {
        return Ended::Output(code);
    }

    loop {
        tokio::select! {
            o = owners.next() => {
                let Some(signal) = o else { return closed_while_held(p, held) };
                let Ok(args) = signal.args() else { continue };
                let ended = match args.new_owner().as_ref() {
                    None => Ended::NotRunning(Failure::not_running()),
                    Some(new) if *new != owner.as_ref() => Ended::NewOwner,
                    Some(_) => continue,
                };
                // The daemon that held the lease is gone, and the lease with it.
                held.lock().unwrap_or_else(|e| e.into_inner()).take();
                if let Err(code) = p.lost() {
                    return Ended::Output(code);
                }
                return ended;
            }
            f = frames.next() => {
                let Some(signal) = f else { return closed_while_held(p, held) };
                let Ok(args) = signal.args() else { continue };
                if let Err(code) = p.frame(args.seq, &path) {
                    return Ended::Output(code);
                }
            }
        }
    }
}

/// Waits until another process than `old` owns the name. `Err` if the bus
/// is lost.
async fn wait_for_another_owner(conn: &Connection, old: &OwnedUniqueName) -> Result<(), Failure> {
    let bus_err = |e: zbus::fdo::Error| Failure::Other(format!("lost the D-Bus session bus: {e}"));
    let dbus = DBusProxy::new(conn).await.map_err(|e| bus_err(e.into()))?;
    let mut owners = dbus
        .receive_name_owner_changed_with_args(&[(0, BUS_NAME)])
        .await
        .map_err(|e| bus_err(e.into()))?;
    loop {
        let owner = tokio::time::timeout(READ_TIMEOUT, dbus.get_name_owner(daemon_name()))
            .await
            .map_err(|_| bus_silent())?;
        match owner {
            Ok(o) if o != *old => return Ok(()),
            Ok(_) | Err(zbus::fdo::Error::NameHasNoOwner(_)) => {}
            Err(e) => return Err(bus_err(e)),
        }
        tokio::select! {
            o = owners.next() => {
                if o.is_none() {
                    return Err(Failure::Other("the D-Bus session bus connection closed".into()));
                }
            }
            () = tokio::time::sleep(RETRY) => {}
        }
    }
}

/// The preview loop; returns only when stdout fails, with the exit status.
async fn preview_loop(json: bool, fps: u32, held: &Held) -> u8 {
    // A line that cannot be written ends the loop with its exit status.
    macro_rules! print_or_exit {
        ($line:expr) => {
            if let Err(code) = $line {
                return code;
            }
        };
    }
    let mut p = Printer {
        json,
        in_outage: false,
        active: false,
    };
    let mut conn: Option<Connection> = None;
    loop {
        let c = match &conn {
            Some(c) => c.clone(),
            None => match bus::connect().await {
                Ok(c) => {
                    conn = Some(c.clone());
                    c
                }
                Err(f) => {
                    print_or_exit!(p.outage(&f));
                    tokio::time::sleep(RETRY).await;
                    continue;
                }
            },
        };
        match follow(&c, &mut p, fps, held).await {
            Ended::Output(code) => return code,
            Ended::NewOwner => {}
            Ended::NotRunning(f) => {
                print_or_exit!(p.outage(&f));
                if let Err(f) = wait_for_owner(&c).await {
                    conn = None;
                    print_or_exit!(p.outage(&f));
                    tokio::time::sleep(RETRY).await;
                }
            }
            Ended::Unsupported(f, old) => {
                print_or_exit!(p.outage(&f));
                if let Err(f) = wait_for_another_owner(&c, &old).await {
                    conn = None;
                    print_or_exit!(p.outage(&f));
                    tokio::time::sleep(RETRY).await;
                }
            }
            Ended::Unreadable(f) => {
                print_or_exit!(p.outage(&f));
                tokio::time::sleep(RETRY).await;
            }
            Ended::BusLost(f) => {
                conn = None;
                print_or_exit!(p.outage(&f));
                tokio::time::sleep(RETRY).await;
            }
        }
    }
}

/// Runs `preview` until `SIGTERM`, `SIGINT`, `SIGHUP`, `SIGPIPE` or a closed
/// stdout, each of which releases the lease and exits with status 0.
pub async fn run(json: bool, fps: u32) -> u8 {
    let signals = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
        signal(SignalKind::hangup()),
        signal(SignalKind::pipe()),
    );
    let (Ok(mut term), Ok(mut int), Ok(mut hup), Ok(mut pipe)) = signals else {
        eprintln!("omarchy-broadcast: cannot install signal handlers");
        return EXIT_FAILURE;
    };
    let held = Held::default();
    let code = tokio::select! {
        _ = term.recv() => EXIT_OK,
        _ = int.recv() => EXIT_OK,
        _ = hup.recv() => EXIT_OK,
        _ = pipe.recv() => EXIT_OK,
        () = out::closed() => EXIT_OK,
        code = preview_loop(json, fps, &held) => code,
    };
    release(&held).await;
    code
}
