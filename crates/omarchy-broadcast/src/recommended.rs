//! `recommended`: the recommended settings of the active camera's model
//! (interface revision 20). `recommended` shows what they would change,
//! never starting the daemon; `recommended apply` puts the camera's
//! settings back to their defaults and sets the recommended values
//! (`ApplyRecommendedSettings`), which may start it through D-Bus
//! activation, and prints the result read back after it.

use clap::Subcommand;
use obc_ipc::{CameraHealth, Snapshot};
use zbus::Connection;

use crate::bus::{self, EXIT_OK, Failure, READ_TIMEOUT};
use crate::{CALL_TIMEOUT, human, json, out, report, within};

/// The subcommands of `recommended`.
#[derive(Subcommand, Clone)]
pub enum RecommendedAction {
    /// Put the active camera's settings back to their defaults, then set
    /// the recommended values (and the anti-flicker of your time zone),
    /// stored for that camera. The capture mode and the effects are not
    /// changed.
    Apply {
        /// Print one JSON object: a `"type":"camera_health"` line after
        /// the change, or an `"type":"error"` line.
        #[arg(long)]
        json: bool,
    },
}

/// The properties and the active camera's health, without starting the
/// daemon.
async fn read(conn: &Connection) -> Result<(Snapshot, CameraHealth), Failure> {
    let snap = bus::fetch(conn).await?;
    bus::require_camera_health(snap.interface_revision)?;
    let health = snap.camera_health.clone().ok_or_else(|| {
        Failure::Unsupported("the daemon does not report the camera's health".into())
    })?;
    Ok((snap, health))
}

/// Runs `recommended` (`None`: show them).
pub async fn run(action: Option<&RecommendedAction>, json: bool) -> u8 {
    let Some(RecommendedAction::Apply { json }) = action.cloned() else {
        let res = within(READ_TIMEOUT, async {
            let conn = bus::connect().await?;
            read(&conn).await
        })
        .await;
        return match res {
            Ok((s, h)) => {
                if json {
                    out::line(&json::camera_health_line(&h));
                } else {
                    out::print(&human::recommended_text(&s, &h));
                }
                EXIT_OK
            }
            Err(f) => report(&f, json),
        };
    };
    // Whether the daemon applied them: a failure after that is only the
    // read-back's, and the message says so.
    let changed = std::cell::Cell::new(false);
    let res = within(CALL_TIMEOUT, async {
        let conn = bus::connect().await?;
        let proxy = bus::proxy(&conn).await?;
        let revision = proxy
            .interface_revision()
            .await
            .map_err(|e| bus::classify(&e, true))?;
        bus::require_camera_health(revision)?;
        proxy
            .apply_recommended_settings("")
            .await
            .map_err(bus::classify_ipc)?;
        changed.set(true);
        read(&conn).await
    })
    .await;
    match res {
        Ok((s, h)) => {
            if json {
                out::line(&json::camera_health_line(&h));
            } else {
                out::print(&human::recommended_applied_text(&s, &h));
            }
            EXIT_OK
        }
        Err(f) if changed.get() && !json => {
            eprintln!(
                "omarchy-broadcast: the recommended settings were applied, but reading them back \
                 failed: {}",
                human::printable(&f.message())
            );
            f.exit_code()
        }
        Err(f) => report(&f, json),
    }
}
