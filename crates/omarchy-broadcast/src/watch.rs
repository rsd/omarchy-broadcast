//! `watch`: follows the daemon's `PropertiesChanged` and `Stats` signals and
//! the owner of its bus name, printing a line per event, until a signal or a
//! closed stdout ends it.
//!
//! The subscriptions are made before the properties are read, so no change is
//! lost in between. While the daemon (or the session bus) is absent, one
//! error line is printed for the outage, the name is re-checked every
//! [`RETRY`] (and at once on `NameOwnerChanged`), and a fresh snapshot is
//! printed when it is back. A daemon that holds its name but does not
//! answer a read within [`READ_TIMEOUT`] is an outage too (a `dbus-error`
//! line), retried every [`RETRY`]. Nothing here starts the daemon.

use std::time::Duration;

use futures_util::StreamExt;
use obc_ipc::{BUS_NAME, INTERFACE, OBJECT_PATH, Snapshot, Stats};
use tokio::signal::unix::{SignalKind, signal};
use zbus::Connection;
use zbus::fdo::{DBusProxy, PropertiesProxy};
use zbus::names::BusName;
use zbus::proxy::CacheProperties;

use crate::bus::{self, EXIT_OK, Failure, READ_TIMEOUT};
use crate::model::{self, Changes};
use crate::{human, json, out};

/// Interval of the presence re-check during an outage.
pub const RETRY: Duration = Duration::from_secs(2);

/// Prints the event lines, in JSON or for a person.
struct Printer {
    json: bool,
    /// An outage has been announced and no snapshot has followed yet.
    in_outage: bool,
}

impl Printer {
    fn snapshot(&mut self, s: &Snapshot) {
        self.in_outage = false;
        if self.json {
            out::line(&json::snapshot_line(s));
        } else {
            out::line(&format!("omarchy-broadcastd: {}", human::summary_line(s)));
        }
    }

    fn changes(&self, s: &Snapshot, changes: &Changes) {
        if self.json {
            if !changes.fields.is_empty() {
                out::line(&json::state_line(s, &changes.fields));
            }
            if changes.cameras {
                out::line(&json::cameras_line(s));
            }
        } else {
            for line in human::change_lines(s, changes) {
                out::line(&line);
            }
        }
    }

    fn stats(&self, st: &Stats) {
        if self.json {
            out::line(&json::stats_line(st));
        } else {
            out::line(&human::stats_text(st));
        }
    }

    /// Announces an outage, once until the next snapshot.
    fn outage(&mut self, f: &Failure) {
        if self.in_outage {
            return;
        }
        self.in_outage = true;
        if self.json {
            out::line(&json::error_line(f.json_code(), &f.message()));
        } else {
            out::line(&format!(
                "{}; waiting for it",
                human::printable(&f.message())
            ));
        }
    }
}

/// How one connected session of [`follow`] ended.
enum Ended {
    /// The name lost its owner, or never had one.
    NotRunning(Failure),
    /// The name moved to a new owner: read everything again.
    NewOwner,
    /// The daemon is there but its properties cannot be read: retry after
    /// [`RETRY`].
    Unreadable(Failure),
    /// The bus connection failed or closed.
    BusLost(Failure),
}

fn lost(e: zbus::Error) -> Ended {
    Ended::BusLost(Failure::Other(format!("lost the D-Bus session bus: {e}")))
}

fn stream_ended() -> Ended {
    Ended::BusLost(Failure::Other(
        "the D-Bus session bus connection closed".into(),
    ))
}

/// The bus itself did not answer within [`READ_TIMEOUT`].
pub(crate) fn bus_silent() -> Failure {
    Failure::Other(format!(
        "the D-Bus session bus did not answer within {} s",
        READ_TIMEOUT.as_secs()
    ))
}

/// [`BUS_NAME`] as a bus name.
pub(crate) fn daemon_name() -> BusName<'static> {
    BusName::from_static_str(BUS_NAME).expect("BUS_NAME is a valid bus name")
}

fn fetch_failed(f: Failure) -> Ended {
    if f.is_not_running() {
        Ended::NotRunning(f)
    } else {
        Ended::Unreadable(f)
    }
}

/// The subscriptions of one [`follow`] session and the name's owner at the
/// time they were made.
struct Subscriptions {
    owner: zbus::names::OwnedUniqueName,
    owners: zbus::fdo::NameOwnerChangedStream,
    changed: zbus::fdo::PropertiesChangedStream,
    stats: obc_ipc::client::StatsStream,
}

/// Subscribes to the name's owner changes, `PropertiesChanged` and `Stats`,
/// then asks for the current owner.
async fn subscribe(conn: &Connection) -> Result<Subscriptions, Ended> {
    let dbus = DBusProxy::new(conn).await.map_err(lost)?;
    let owners = dbus
        .receive_name_owner_changed_with_args(&[(0, BUS_NAME)])
        .await
        .map_err(lost)?;
    let props = PropertiesProxy::builder(conn)
        .destination(BUS_NAME)
        .path(OBJECT_PATH)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(lost)?;
    let changed = props.receive_properties_changed().await.map_err(lost)?;
    let proxy = bus::proxy(conn).await.map_err(Ended::BusLost)?;
    let stats = proxy.receive_stats().await.map_err(lost)?;
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
        changed,
        stats,
    })
}

/// Follows the daemon on `conn` until it goes away.
async fn follow(conn: &Connection, p: &mut Printer) -> Ended {
    // Every call here goes to the bus itself; a bus that does not answer in
    // time counts as lost.
    let subscribed = match tokio::time::timeout(READ_TIMEOUT, subscribe(conn)).await {
        Ok(Ok(s)) => s,
        Ok(Err(ended)) => return ended,
        Err(_) => return Ended::BusLost(bus_silent()),
    };
    let Subscriptions {
        owner,
        mut owners,
        mut changed,
        mut stats,
    } = subscribed;
    // Bounded by READ_TIMEOUT: a daemon that holds the name but does not
    // answer is an outage like any other.
    let mut snap = match bus::fetch(conn).await {
        Ok(s) => s,
        Err(f) => return fetch_failed(f),
    };
    p.snapshot(&snap);

    loop {
        tokio::select! {
            o = owners.next() => {
                let Some(signal) = o else { return stream_ended() };
                let Ok(args) = signal.args() else { continue };
                match args.new_owner().as_ref() {
                    None => return Ended::NotRunning(Failure::not_running()),
                    Some(new) if *new != owner.as_ref() => return Ended::NewOwner,
                    Some(_) => {}
                }
            }
            c = changed.next() => {
                let Some(signal) = c else { return stream_ended() };
                let Ok(args) = signal.args() else { continue };
                if args.interface_name().as_str() != INTERFACE {
                    continue;
                }
                let new = if args.invalidated_properties().is_empty() {
                    let mut new = snap.clone();
                    let applied = args
                        .changed_properties()
                        .iter()
                        .try_for_each(|(k, v)| new.apply_property(k, v));
                    match applied {
                        Ok(()) => new,
                        // A value of an unexpected type: read everything.
                        Err(_) => match bus::fetch(conn).await {
                            Ok(s) => s,
                            Err(f) => return fetch_failed(f),
                        },
                    }
                } else {
                    match bus::fetch(conn).await {
                        Ok(s) => s,
                        Err(f) => return fetch_failed(f),
                    }
                };
                let changes = model::diff(&snap, &new);
                if !changes.is_empty() {
                    p.changes(&new, &changes);
                }
                snap = new;
            }
            s = stats.next() => {
                let Some(signal) = s else { return stream_ended() };
                let Ok(args) = signal.args() else { continue };
                p.stats(&args.to_stats());
            }
        }
    }
}

/// Waits until the name has an owner. `Err` if the bus is lost.
pub(crate) async fn wait_for_owner(conn: &Connection) -> Result<(), Failure> {
    let bus_err = |e: zbus::fdo::Error| Failure::Other(format!("lost the D-Bus session bus: {e}"));
    let dbus = DBusProxy::new(conn).await.map_err(|e| bus_err(e.into()))?;
    let mut owners = dbus
        .receive_name_owner_changed_with_args(&[(0, BUS_NAME)])
        .await
        .map_err(|e| bus_err(e.into()))?;
    let name = daemon_name();
    loop {
        let has_owner = tokio::time::timeout(READ_TIMEOUT, dbus.name_has_owner(name.clone()))
            .await
            .map_err(|_| bus_silent())?;
        if has_owner.map_err(bus_err)? {
            return Ok(());
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

/// The watch loop; returns only through [`std::process::exit`].
async fn watch_loop(json: bool) {
    let mut p = Printer {
        json,
        in_outage: false,
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
                    p.outage(&f);
                    tokio::time::sleep(RETRY).await;
                    continue;
                }
            },
        };
        match follow(&c, &mut p).await {
            Ended::NewOwner => {}
            Ended::NotRunning(f) => {
                p.outage(&f);
                if let Err(f) = wait_for_owner(&c).await {
                    conn = None;
                    p.outage(&f);
                    tokio::time::sleep(RETRY).await;
                }
            }
            Ended::Unreadable(f) => {
                p.outage(&f);
                tokio::time::sleep(RETRY).await;
            }
            Ended::BusLost(f) => {
                conn = None;
                p.outage(&f);
                tokio::time::sleep(RETRY).await;
            }
        }
    }
}

/// Runs `watch` until `SIGTERM`, `SIGINT`, `SIGPIPE` or a closed stdout,
/// each of which exits with status 0 and no output.
pub async fn run(json: bool) -> u8 {
    let signals = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
        signal(SignalKind::pipe()),
    );
    let (Ok(mut term), Ok(mut int), Ok(mut pipe)) = signals else {
        eprintln!("omarchy-broadcast: cannot install signal handlers");
        return bus::EXIT_FAILURE;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
        _ = pipe.recv() => {}
        () = out::closed() => {}
        () = watch_loop(json) => {}
    }
    EXIT_OK
}
