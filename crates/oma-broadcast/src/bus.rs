//! Talking to `oma-broadcastd`: the session-bus connection, property reads
//! that never start the daemon, and the classification of failures into the
//! CLI's exit codes.
//!
//! Properties are read with [`Snapshot::fetch_no_autostart`] (one
//! `org.freedesktop.DBus.Properties.GetAll` carrying the `NO_AUTO_START`
//! flag), so `status`, `cameras` and `watch` never trigger D-Bus activation.
//! The generated property getters of [`obc_ipc::OmaBroadcastProxy`] (and its
//! cache, filled by a `GetAll` without that flag) would. Methods go through
//! the proxy, without the flag: `on`, `off` and `camera` may start the daemon
//! when its activation file is installed.

use std::time::Duration;

use obc_ipc::{BUS_NAME, IpcError, OmaBroadcastProxy, Snapshot};
use zbus::proxy::CacheProperties;
use zbus::{Connection, DBusError};

use crate::json;

/// Time allowed for one read of the properties, and for `status` and
/// `cameras` as a whole.
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Exit status: success.
pub const EXIT_OK: u8 = 0;
/// Exit status: the daemon rejected the request (`NoSuchCamera`,
/// `InvalidConfig`, `Unavailable`, `Failed`).
pub const EXIT_REJECTED: u8 = 1;
/// Exit status: invalid command line (clap's own status).
pub const EXIT_USAGE: u8 = 2;
/// Exit status: the daemon is not running, or there is no session bus.
pub const EXIT_NOT_RUNNING: u8 = 3;
/// Exit status: any other D-Bus failure, a timeout, or a failed write.
pub const EXIT_FAILURE: u8 = 4;

/// How to start the daemon, quoted in the not-running message.
pub const START_HINT: &str = "systemctl --user start oma-broadcastd";

/// Why a request did not succeed.
#[derive(Debug)]
pub enum Failure {
    /// The name has no owner (and, for methods, activation failed).
    NotRunning(String),
    /// No session bus to connect to.
    NoBus(String),
    /// The daemon replied with one of its errors.
    Rejected(IpcError),
    /// Anything else.
    Other(String),
}

impl Failure {
    /// The not-running failure of a read that did not try to start it.
    pub fn not_running() -> Failure {
        Failure::NotRunning(format!(
            "oma-broadcastd is not running (no owner for {BUS_NAME} on the session bus); \
             start it with: {START_HINT}"
        ))
    }

    fn activation_failed(detail: &str) -> Failure {
        Failure::NotRunning(format!(
            "oma-broadcastd is not running and D-Bus could not start it ({detail}); \
             start it with: {START_HINT}"
        ))
    }

    /// Whether the daemon is absent (or the bus is), as opposed to a failed
    /// request.
    pub fn is_not_running(&self) -> bool {
        matches!(self, Failure::NotRunning(_) | Failure::NoBus(_))
    }

    pub fn exit_code(&self) -> u8 {
        match self {
            Failure::NotRunning(_) | Failure::NoBus(_) => EXIT_NOT_RUNNING,
            Failure::Rejected(_) => EXIT_REJECTED,
            Failure::Other(_) => EXIT_FAILURE,
        }
    }

    /// The `code` of the JSON error line.
    pub fn json_code(&self) -> &'static str {
        if self.is_not_running() {
            json::CODE_NOT_RUNNING
        } else {
            json::CODE_DBUS
        }
    }

    /// One line for a person.
    pub fn message(&self) -> String {
        match self {
            Failure::NotRunning(m) | Failure::NoBus(m) | Failure::Other(m) => m.clone(),
            Failure::Rejected(e) => e.message(),
        }
    }
}

/// The D-Bus error name and description carried by `e`, if it is an error
/// reply.
fn error_reply(e: &zbus::Error) -> Option<(String, String)> {
    match e {
        zbus::Error::MethodError(name, desc, _) => {
            Some((name.to_string(), desc.clone().unwrap_or_default()))
        }
        zbus::Error::FDO(f) => Some((
            f.name().to_string(),
            f.description().unwrap_or_default().to_string(),
        )),
        _ => None,
    }
}

/// Classifies a transport or protocol error. `activating` says whether the
/// call allowed D-Bus activation.
pub fn classify(e: &zbus::Error, activating: bool) -> Failure {
    if let Some((name, desc)) = error_reply(e) {
        let absent = name == "org.freedesktop.DBus.Error.NameHasNoOwner"
            || name == "org.freedesktop.DBus.Error.ServiceUnknown";
        // Activation failures: the program could not run, failed, or did not
        // claim the name in time.
        let spawn = name.starts_with("org.freedesktop.DBus.Error.Spawn.")
            || (activating && name == "org.freedesktop.DBus.Error.TimedOut");
        if absent && !activating {
            return Failure::not_running();
        }
        if absent || spawn {
            let detail = if desc.is_empty() { name } else { desc };
            return Failure::activation_failed(&detail);
        }
        let text = if desc.is_empty() {
            name
        } else {
            format!("{name}: {desc}")
        };
        return Failure::Other(format!("D-Bus error from oma-broadcastd: {text}"));
    }
    Failure::Other(format!("D-Bus failure: {e}"))
}

/// Classifies the reply of a method call (which allows activation).
pub fn classify_ipc(e: IpcError) -> Failure {
    match e {
        IpcError::ZBus(z) => classify(&z, true),
        rejected => Failure::Rejected(rejected),
    }
}

/// Connects to the session bus.
pub async fn connect() -> Result<Connection, Failure> {
    Connection::session().await.map_err(|e| {
        Failure::NoBus(format!(
            "cannot reach oma-broadcastd: no D-Bus session bus ({e})"
        ))
    })
}

/// The failure of a read that did not finish within [`READ_TIMEOUT`].
pub fn timed_out(limit: Duration) -> Failure {
    Failure::Other(format!(
        "timed out after {} s waiting for oma-broadcastd",
        limit.as_secs()
    ))
}

/// Reads every property in one `GetAll`, without starting the daemon, within
/// [`READ_TIMEOUT`].
pub async fn fetch(conn: &Connection) -> Result<Snapshot, Failure> {
    match tokio::time::timeout(READ_TIMEOUT, Snapshot::fetch_no_autostart(conn)).await {
        Err(_) => Err(timed_out(READ_TIMEOUT)),
        Ok(Ok(Some(snap))) => Ok(snap),
        Ok(Ok(None)) => Err(Failure::not_running()),
        Ok(Err(e @ zbus::Error::Failure(_))) => Err(Failure::Other(format!(
            "unexpected properties from oma-broadcastd: {e}"
        ))),
        Ok(Err(e)) => Err(classify(&e, false)),
    }
}

/// The interface proxy for method calls and signals. Uncached: its property
/// getters are not used, and an uncached proxy makes no call when built.
pub async fn proxy(conn: &Connection) -> Result<OmaBroadcastProxy<'static>, Failure> {
    OmaBroadcastProxy::builder(conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(|e| classify(&e, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::fdo;

    fn fdo_error(e: fdo::Error) -> zbus::Error {
        zbus::Error::FDO(Box::new(e))
    }

    #[test]
    fn exit_codes_are_distinct() {
        let codes = [
            EXIT_OK,
            EXIT_REJECTED,
            EXIT_USAGE,
            EXIT_NOT_RUNNING,
            EXIT_FAILURE,
        ];
        for (i, a) in codes.iter().enumerate() {
            for b in &codes[i + 1..] {
                assert_ne!(a, b);
            }
        }
        assert_eq!(Failure::not_running().exit_code(), EXIT_NOT_RUNNING);
        assert_eq!(Failure::not_running().json_code(), "daemon-not-running");
        assert_eq!(Failure::NoBus("x".into()).json_code(), "daemon-not-running");
        assert_eq!(
            Failure::Rejected(IpcError::NoSuchCamera("x".into())).exit_code(),
            EXIT_REJECTED
        );
        assert_eq!(Failure::Other("x".into()).exit_code(), EXIT_FAILURE);
        assert_eq!(Failure::Other("x".into()).json_code(), "dbus-error");
    }

    #[test]
    fn absent_names_and_spawn_failures_mean_not_running() {
        let no_owner = fdo_error(fdo::Error::NameHasNoOwner("gone".into()));
        let f = classify(&no_owner, false);
        assert!(f.is_not_running(), "{f:?}");
        assert!(f.message().contains(START_HINT), "{}", f.message());
        assert!(f.message().contains(BUS_NAME), "{}", f.message());

        let unknown = fdo_error(fdo::Error::ServiceUnknown("not activatable".into()));
        let f = classify(&unknown, true);
        assert!(f.is_not_running(), "{f:?}");
        assert!(f.message().contains("not activatable"), "{}", f.message());

        let spawn = fdo_error(fdo::Error::SpawnChildExited("exited with 0".into()));
        let f = classify_ipc(IpcError::ZBus(spawn));
        assert_eq!(f.exit_code(), EXIT_NOT_RUNNING);
        assert!(f.message().contains("could not start"), "{}", f.message());

        let timed_out = fdo_error(fdo::Error::TimedOut("activation timed out".into()));
        assert!(classify(&timed_out, true).is_not_running());
        assert!(!classify(&timed_out, false).is_not_running());

        let other = fdo_error(fdo::Error::AccessDenied("no".into()));
        let f = classify(&other, false);
        assert_eq!(f.exit_code(), EXIT_FAILURE);
        assert!(f.message().contains("AccessDenied"), "{}", f.message());

        let f = classify_ipc(IpcError::Unavailable("no camera".into()));
        assert_eq!(f.exit_code(), EXIT_REJECTED);
        assert_eq!(f.message(), "no camera");
    }
}
