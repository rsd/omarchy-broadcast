//! Talking to `omarchy-broadcastd`: the session-bus connection, property reads
//! that never start the daemon, and the classification of failures into the
//! CLI's exit codes.
//!
//! Properties are read with [`Snapshot::fetch_no_autostart`] (one
//! `org.freedesktop.DBus.Properties.GetAll` carrying the `NO_AUTO_START`
//! flag), so `status`, `cameras` and `watch` never trigger D-Bus activation;
//! `controls` reads the image controls with a `GetCameraControls` carrying
//! the same flag ([`camera_controls`]), and `settings` and `watch` read the
//! camera settings with a `GetSettings` carrying it ([`settings_tree`]),
//! and `media` reads the gallery with a `GetMedia` carrying it ([`media`]).
//! The generated property getters of [`obc_ipc::OmarchyBroadcastProxy`] (and its
//! cache, filled by a `GetAll` without that flag) would. Methods go through
//! the proxy, without the flag: `on`, `off`, `camera`, `effect <effect>`,
//! `denoise`, `controls set`, `controls reset`, `settings set`,
//! `settings reset`, `media import` and `media remove` may start the daemon
//! when its activation file is installed.

use std::time::Duration;

use obc_ipc::{BUS_NAME, CameraControl, IpcError, OmarchyBroadcastProxy, SettingsTree, Snapshot};
use zbus::proxy::{CacheProperties, MethodFlags};
use zbus::{Connection, DBusError};

use crate::json;

/// Time allowed for one read of the properties, and for `status` and
/// `cameras` as a whole.
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Exit status: success.
pub const EXIT_OK: u8 = 0;
/// Exit status: the daemon rejected the request (`NoSuchCamera`,
/// `InvalidConfig`, `InvalidArgument`, `Unavailable`, `Failed`,
/// `UnknownSetting`, `Disabled`, `RateLimited`), or is too old for it.
pub const EXIT_REJECTED: u8 = 1;
/// Exit status: invalid command line (clap's own status).
pub const EXIT_USAGE: u8 = 2;
/// Exit status: the daemon is not running, or there is no session bus.
pub const EXIT_NOT_RUNNING: u8 = 3;
/// Exit status: any other D-Bus failure, a timeout, or a failed write.
pub const EXIT_FAILURE: u8 = 4;

/// The first `InterfaceRevision` with the effect controls (`BlurStrength`,
/// `BackgroundImage`, `SetEffect` and its siblings).
pub const EFFECTS_REVISION: u32 = 2;

/// The first `InterfaceRevision` with noise removal (`Denoise`,
/// `SetDenoise`).
pub use obc_ipc::DENOISE_REVISION;

/// The first `InterfaceRevision` with the live preview (`AcquirePreview`,
/// `ReleasePreview`, `PreviewFrame`).
pub use obc_ipc::PREVIEW_REVISION;

/// The first `InterfaceRevision` with the camera image controls
/// (`CameraControls`, `GetCameraControls`, `SetCameraControl`,
/// `ResetCameraControls`).
pub use obc_ipc::CONTROLS_REVISION;

/// The first `InterfaceRevision` with the camera settings (`GetSettings`,
/// `SetSetting`, `ResetSettings`, `SettingsChanged`, `SettingsRevision`,
/// `SettingsSchemaSerial`).
pub use obc_ipc::SETTINGS_REVISION;

/// How to start the daemon, quoted in the not-running message.
pub const START_HINT: &str = "systemctl --user start omarchy-broadcastd";

/// Why a request did not succeed.
#[derive(Debug)]
pub enum Failure {
    /// The name has no owner (and, for methods, activation failed).
    NotRunning(String),
    /// No session bus to connect to.
    NoBus(String),
    /// The daemon replied with one of its errors.
    Rejected(IpcError),
    /// The daemon is too old for the request (its `InterfaceRevision`).
    Unsupported(String),
    /// The daemon's preview frame path is too long for a JSON line
    /// ([`json::MAX_PREVIEW_PATH_BYTES`]).
    PreviewPathTooLong(String),
    /// Anything else.
    Other(String),
}

impl Failure {
    /// The not-running failure of a read that did not try to start it.
    pub fn not_running() -> Failure {
        Failure::NotRunning(format!(
            "omarchy-broadcastd is not running (no owner for {BUS_NAME} on the session bus); \
             start it with: {START_HINT}"
        ))
    }

    fn activation_failed(detail: &str) -> Failure {
        Failure::NotRunning(format!(
            "omarchy-broadcastd is not running and D-Bus could not start it ({detail}); \
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
            Failure::Rejected(_) | Failure::Unsupported(_) => EXIT_REJECTED,
            Failure::PreviewPathTooLong(_) | Failure::Other(_) => EXIT_FAILURE,
        }
    }

    /// The `code` of the JSON error line.
    pub fn json_code(&self) -> &'static str {
        match self {
            Failure::NotRunning(_) | Failure::NoBus(_) => json::CODE_NOT_RUNNING,
            Failure::Unsupported(_) => json::CODE_UNSUPPORTED,
            Failure::PreviewPathTooLong(_) => json::CODE_PREVIEW_PATH_TOO_LONG,
            Failure::Rejected(_) | Failure::Other(_) => json::CODE_DBUS,
        }
    }

    /// One line for a person.
    pub fn message(&self) -> String {
        match self {
            Failure::NotRunning(m)
            | Failure::NoBus(m)
            | Failure::Other(m)
            | Failure::Unsupported(m)
            | Failure::PreviewPathTooLong(m) => m.clone(),
            Failure::Rejected(e) => e.message(),
        }
    }
}

/// Fails unless a daemon of interface revision `revision` has the effect
/// controls: an older one has no `SetEffect`, and its `Effect` is always
/// `none`.
pub fn require_effects(revision: u32) -> Result<(), Failure> {
    if revision >= EFFECTS_REVISION {
        return Ok(());
    }
    Err(Failure::Unsupported(format!(
        "the daemon does not support effects (its interface revision is {revision}, \
         effects need {EFFECTS_REVISION}); update omarchy-broadcastd"
    )))
}

/// Fails unless a daemon of interface revision `revision` has noise
/// removal: an older one has no `SetDenoise`.
pub fn require_denoise(revision: u32) -> Result<(), Failure> {
    if revision >= DENOISE_REVISION {
        return Ok(());
    }
    Err(Failure::Unsupported(format!(
        "the daemon does not support noise removal (its interface revision is {revision}, \
         noise removal needs {DENOISE_REVISION}); update omarchy-broadcastd"
    )))
}

/// Fails unless a daemon of interface revision `revision` has the live
/// preview.
pub fn require_preview(revision: u32) -> Result<(), Failure> {
    if revision >= PREVIEW_REVISION {
        return Ok(());
    }
    Err(Failure::Unsupported(format!(
        "the daemon does not support the live preview (its interface revision is {revision}, \
         the preview needs {PREVIEW_REVISION}); update omarchy-broadcastd"
    )))
}

/// Fails unless a daemon of interface revision `revision` has the camera
/// image controls.
pub fn require_controls(revision: u32) -> Result<(), Failure> {
    if revision >= CONTROLS_REVISION {
        return Ok(());
    }
    Err(Failure::Unsupported(format!(
        "the daemon does not support camera controls (its interface revision is {revision}, \
         camera controls need {CONTROLS_REVISION}); update omarchy-broadcastd"
    )))
}

/// Fails unless a daemon of interface revision `revision` has the camera
/// settings.
pub fn require_settings(revision: u32) -> Result<(), Failure> {
    if revision >= SETTINGS_REVISION {
        return Ok(());
    }
    Err(Failure::Unsupported(format!(
        "the daemon does not support camera settings (its interface revision is {revision}, \
         camera settings need {SETTINGS_REVISION}); update omarchy-broadcastd"
    )))
}

/// The daemon's preview frame `path` does not fit a JSON line (it is never
/// clipped: a clipped path names no file).
pub fn preview_path_too_long(path: &str) -> Failure {
    Failure::PreviewPathTooLong(format!(
        "the daemon's preview frame path is too long for a JSON line ({} bytes, at most {} \
         encoded): set a shorter XDG_RUNTIME_DIR for omarchy-broadcastd",
        path.len(),
        json::MAX_PREVIEW_PATH_BYTES
    ))
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
        return Failure::Other(format!("D-Bus error from omarchy-broadcastd: {text}"));
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

/// Classifies the reply of a method call made with `NO_AUTO_START`: the
/// daemon's own error by name, or a transport error.
pub fn classify_no_autostart(e: zbus::Error) -> Failure {
    match IpcError::from(e) {
        IpcError::ZBus(z) => classify(&z, false),
        rejected => Failure::Rejected(rejected),
    }
}

/// `GetCameraControls(camera)` without starting the daemon, within
/// [`READ_TIMEOUT`]: the image controls of `camera` (an id; `""` is the
/// active camera), read from the device.
pub async fn camera_controls(
    conn: &Connection,
    camera: &str,
) -> Result<Vec<CameraControl>, Failure> {
    let call = async {
        let proxy = proxy(conn).await?;
        let reply: Option<Vec<CameraControl>> = proxy
            .inner()
            .call_with_flags("GetCameraControls", MethodFlags::NO_AUTO_START, &(camera,))
            .await
            .map_err(classify_no_autostart)?;
        reply.ok_or_else(|| Failure::Other("GetCameraControls returned no reply".into()))
    };
    tokio::time::timeout(READ_TIMEOUT, call)
        .await
        .unwrap_or_else(|_| Err(timed_out(READ_TIMEOUT)))
}

/// `GetSettings` for `camera` (an id; `""` is the active camera) without
/// starting the daemon, within [`READ_TIMEOUT`]. With no camera present the
/// tree has camera `""` and no group.
pub async fn settings_tree(conn: &Connection, camera: &str) -> Result<SettingsTree, Failure> {
    let call = async {
        let proxy = proxy(conn).await?;
        proxy
            .settings_tree_no_autostart(camera)
            .await
            .map_err(|e| match e {
                IpcError::ZBus(z) => classify(&z, false),
                rejected => Failure::Rejected(rejected),
            })
    };
    tokio::time::timeout(READ_TIMEOUT, call)
        .await
        .unwrap_or_else(|_| Err(timed_out(READ_TIMEOUT)))
}

/// Fails unless a daemon of interface revision `revision` has the modules.
pub fn require_modules(revision: u32) -> Result<(), Failure> {
    if revision >= obc_ipc::MODULES_REVISION {
        return Ok(());
    }
    Err(Failure::Unsupported(format!(
        "the daemon does not support modules (its interface revision is {revision}, \
         modules need {}); update omarchy-broadcastd",
        obc_ipc::MODULES_REVISION
    )))
}

/// Fails unless a daemon of interface revision `revision` can force a
/// module past its conflicts (`modules.forced.<id>`).
pub fn require_forced(revision: u32) -> Result<(), Failure> {
    if revision >= obc_ipc::FORCED_REVISION {
        return Ok(());
    }
    Err(Failure::Unsupported(format!(
        "the daemon cannot force a module past its conflicts (its interface revision is \
         {revision}, forcing needs {}); update omarchy-broadcastd",
        obc_ipc::FORCED_REVISION
    )))
}

/// Fails unless a daemon of interface revision `revision` has the
/// microphone.
pub fn require_mic(revision: u32) -> Result<(), Failure> {
    if revision >= obc_ipc::MIC_REVISION {
        return Ok(());
    }
    Err(Failure::Unsupported(format!(
        "the daemon does not support the microphone (its interface revision is {revision}, \
         the microphone needs {}); update omarchy-broadcastd",
        obc_ipc::MIC_REVISION
    )))
}

/// Fails unless a daemon of interface revision `revision` has the
/// configuration methods.
pub fn require_config(revision: u32) -> Result<(), Failure> {
    if revision >= obc_ipc::CONFIG_REVISION {
        return Ok(());
    }
    Err(Failure::Unsupported(format!(
        "the daemon does not support the configuration methods (its interface revision is \
         {revision}, they need {}); update omarchy-broadcastd",
        obc_ipc::CONFIG_REVISION
    )))
}

/// Fails unless a daemon of interface revision `revision` has the media
/// gallery (`GetMedia`, `ImportMedia`, `RemoveMedia`).
pub fn require_media(revision: u32) -> Result<(), Failure> {
    if revision >= obc_ipc::MEDIA_REVISION {
        return Ok(());
    }
    Err(Failure::Unsupported(format!(
        "the daemon does not support the media gallery (its interface revision is {revision}, \
         the gallery needs {}); update omarchy-broadcastd",
        obc_ipc::MEDIA_REVISION
    )))
}

/// Fails unless a daemon of interface revision `revision` takes the away
/// image (`effect.fallback_image`) from `SetConfig`.
pub fn require_away_image(revision: u32) -> Result<(), Failure> {
    if revision >= obc_ipc::PLACEHOLDER_REVISION {
        return Ok(());
    }
    Err(Failure::Unsupported(format!(
        "the daemon cannot select an away image (its interface revision is {revision}, that \
         needs {}); update omarchy-broadcastd",
        obc_ipc::PLACEHOLDER_REVISION
    )))
}

/// `GetMedia(kind)` without starting the daemon, within [`READ_TIMEOUT`].
pub async fn media(conn: &Connection, kind: &str) -> Result<obc_ipc::MediaReply, Failure> {
    let call = async {
        let proxy = proxy(conn).await?;
        proxy
            .media_no_autostart(kind)
            .await
            .map_err(no_autostart_error)
    };
    tokio::time::timeout(READ_TIMEOUT, call)
        .await
        .unwrap_or_else(|_| Err(timed_out(READ_TIMEOUT)))
}

/// `GetConfigSchema` without starting the daemon, within [`READ_TIMEOUT`].
pub async fn config_schema(conn: &Connection) -> Result<obc_ipc::ConfigSchema, Failure> {
    let call = async {
        let proxy = proxy(conn).await?;
        proxy
            .config_schema_no_autostart()
            .await
            .map_err(no_autostart_error)
    };
    tokio::time::timeout(READ_TIMEOUT, call)
        .await
        .unwrap_or_else(|_| Err(timed_out(READ_TIMEOUT)))
}

/// `GetConfigFiles` without starting the daemon, within [`READ_TIMEOUT`].
pub async fn config_files(conn: &Connection) -> Result<obc_ipc::ConfigFiles, Failure> {
    let call = async {
        let proxy = proxy(conn).await?;
        proxy
            .config_files_no_autostart()
            .await
            .map_err(no_autostart_error)
    };
    tokio::time::timeout(READ_TIMEOUT, call)
        .await
        .unwrap_or_else(|_| Err(timed_out(READ_TIMEOUT)))
}

/// `GetStageOrderMoves` without starting the daemon, within
/// [`READ_TIMEOUT`]: each stage `(id, slot, up, down)`.
pub async fn stage_moves(conn: &Connection) -> Result<Vec<(String, String, bool, bool)>, Failure> {
    let call = async {
        let proxy = proxy(conn).await?;
        proxy
            .stage_order_moves_no_autostart()
            .await
            .map_err(no_autostart_error)
    };
    tokio::time::timeout(READ_TIMEOUT, call)
        .await
        .unwrap_or_else(|_| Err(timed_out(READ_TIMEOUT)))
}

/// Maps an error of a call made without starting the daemon.
fn no_autostart_error(e: IpcError) -> Failure {
    match e {
        IpcError::ZBus(z) => classify(&z, false),
        rejected => Failure::Rejected(rejected),
    }
}

/// `GetModules` without starting the daemon, within [`READ_TIMEOUT`].
pub async fn modules(conn: &Connection) -> Result<obc_ipc::ModuleList, Failure> {
    let call = async {
        let proxy = proxy(conn).await?;
        proxy
            .module_list_no_autostart()
            .await
            .map_err(no_autostart_error)
    };
    tokio::time::timeout(READ_TIMEOUT, call)
        .await
        .unwrap_or_else(|_| Err(timed_out(READ_TIMEOUT)))
}

/// `GetStageOrder` without starting the daemon, within [`READ_TIMEOUT`].
pub async fn stage_order(conn: &Connection) -> Result<Vec<String>, Failure> {
    let call = async {
        let proxy = proxy(conn).await?;
        proxy
            .stage_order_no_autostart()
            .await
            .map_err(no_autostart_error)
    };
    tokio::time::timeout(READ_TIMEOUT, call)
        .await
        .unwrap_or_else(|_| Err(timed_out(READ_TIMEOUT)))
}

/// Connects to the session bus.
pub async fn connect() -> Result<Connection, Failure> {
    Connection::session().await.map_err(|e| {
        Failure::NoBus(format!(
            "cannot reach omarchy-broadcastd: no D-Bus session bus ({e})"
        ))
    })
}

/// The failure of a read that did not finish within [`READ_TIMEOUT`].
pub fn timed_out(limit: Duration) -> Failure {
    Failure::Other(format!(
        "timed out after {} s waiting for omarchy-broadcastd",
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
            "unexpected properties from omarchy-broadcastd: {e}"
        ))),
        Ok(Err(e)) => Err(classify(&e, false)),
    }
}

/// The interface proxy for method calls and signals. Uncached: an uncached
/// proxy makes no call when built, and its property getters each make a
/// fresh `Get` call (as `effect` needs for `InterfaceRevision`) instead of
/// reading a cache that may be stale.
pub async fn proxy(conn: &Connection) -> Result<OmarchyBroadcastProxy<'static>, Failure> {
    OmarchyBroadcastProxy::builder(conn)
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
    fn selecting_an_away_image_needs_revision_17() {
        assert!(require_away_image(obc_ipc::PLACEHOLDER_REVISION).is_ok());
        let e = require_away_image(16).unwrap_err();
        assert_eq!(e.json_code(), "unsupported-daemon");
        assert!(e.message().contains("revision is 16"), "{}", e.message());
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
        assert_eq!(Failure::Unsupported("x".into()).exit_code(), EXIT_REJECTED);
        assert_eq!(
            Failure::Unsupported("x".into()).json_code(),
            "unsupported-daemon"
        );
        assert_eq!(
            Failure::Rejected(IpcError::Failed("x".into())).json_code(),
            "dbus-error"
        );
    }

    #[test]
    fn the_preview_needs_revision_5() {
        assert!(require_preview(PREVIEW_REVISION).is_ok());
        let f = require_preview(4).unwrap_err();
        assert_eq!(f.json_code(), "unsupported-daemon");
        assert_eq!(
            f.message(),
            "the daemon does not support the live preview (its interface revision is 4, the \
             preview needs 5); update omarchy-broadcastd"
        );
    }

    #[test]
    fn camera_controls_need_revision_6() {
        assert!(require_controls(CONTROLS_REVISION).is_ok());
        assert!(require_controls(CONTROLS_REVISION + 1).is_ok());
        let f = require_controls(5).unwrap_err();
        assert_eq!(f.exit_code(), EXIT_REJECTED);
        assert_eq!(f.json_code(), "unsupported-daemon");
        assert_eq!(
            f.message(),
            "the daemon does not support camera controls (its interface revision is 5, camera \
             controls need 6); update omarchy-broadcastd"
        );
    }

    #[test]
    fn camera_settings_need_revision_7() {
        assert!(require_settings(SETTINGS_REVISION).is_ok());
        assert!(require_settings(SETTINGS_REVISION + 1).is_ok());
        let f = require_settings(6).unwrap_err();
        assert_eq!(f.exit_code(), EXIT_REJECTED);
        assert_eq!(f.json_code(), "unsupported-daemon");
        assert_eq!(
            f.message(),
            "the daemon does not support camera settings (its interface revision is 6, camera \
             settings need 7); update omarchy-broadcastd"
        );
        for e in [
            IpcError::UnknownSetting("u".into()),
            IpcError::Disabled("d".into()),
            IpcError::RateLimited("r".into()),
        ] {
            let f = classify_ipc(e);
            assert_eq!(f.exit_code(), EXIT_REJECTED);
            assert_eq!(f.message().len(), 1);
        }
    }

    #[test]
    fn effects_need_revision_2() {
        assert!(require_effects(EFFECTS_REVISION).is_ok());
        assert!(require_effects(EFFECTS_REVISION + 1).is_ok());
        let f = require_effects(1).unwrap_err();
        assert_eq!(f.exit_code(), EXIT_REJECTED);
        assert!(
            f.message()
                .starts_with("the daemon does not support effects")
                && f.message().ends_with("; update omarchy-broadcastd"),
            "{}",
            f.message()
        );
    }

    #[test]
    fn denoise_needs_revision_4() {
        assert!(require_denoise(DENOISE_REVISION).is_ok());
        assert!(require_denoise(DENOISE_REVISION + 1).is_ok());
        let f = require_denoise(3).unwrap_err();
        assert_eq!(f.exit_code(), EXIT_REJECTED);
        assert_eq!(
            f.message(),
            "the daemon does not support noise removal (its interface revision is 3, noise \
             removal needs 4); update omarchy-broadcastd"
        );
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

        let f = classify_ipc(IpcError::InvalidArgument("no background image".into()));
        assert_eq!(f.exit_code(), EXIT_REJECTED);
        assert_eq!(f.message(), "no background image");
    }
}
