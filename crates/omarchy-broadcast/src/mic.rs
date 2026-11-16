//! `mic`: the Omarchy Broadcast Mic (interface revision 13). `mic` and
//! `mic status` show its state, never starting the daemon; `on` and `off`
//! turn the mic host on and off (`SetMicHost`), `device` chooses the
//! microphone it captures and `default` / `restore-default` make it, or the
//! one before, the default microphone (`SetSetting` of the `mic.input.*`
//! settings). Those may start the daemon through D-Bus activation, and
//! print the state read back after the change.

use clap::Subcommand;
use obc_ipc::{MicStatus, SettingValue, Snapshot};
use zbus::Connection;

use crate::bus::{self, EXIT_OK, Failure, READ_TIMEOUT};
use crate::{CALL_TIMEOUT, human, json, out, report, within};

/// `mic.input.device`.
const DEVICE: &str = "mic.input.device";
/// `mic.input.make_default`.
const MAKE_DEFAULT: &str = "mic.input.make_default";
/// `mic.input.restore_default`.
const RESTORE_DEFAULT: &str = "mic.input.restore_default";

/// The subcommands of `mic`.
#[derive(Subcommand, Clone)]
pub enum MicAction {
    /// Show the microphone: its state, the applications recording from
    /// it, the modules processing it, the delay added and any error.
    Status {
        /// Print one JSON object (`"type":"mic"`).
        #[arg(long)]
        json: bool,
    },
    /// Turn the microphone on: its service is enabled and started, and
    /// starts again at the next login.
    On,
    /// Turn the microphone off: the previous default microphone is made
    /// the default again (if this one is the default), then its service is
    /// stopped and disabled.
    Off,
    /// Choose the microphone it captures: `default` (the system's choice)
    /// or a PipeWire node name, as the Mic panel lists them.
    ///
    /// The choice takes effect at once while the microphone is on,
    /// whether or not the Omarchy Broadcast Mic is the default microphone.
    /// With `default` it captures the system's default microphone while
    /// the Omarchy Broadcast Mic is not the default.
    Device {
        /// `default`, or the node name, e.g. `alsa_input.usb-...`.
        name: String,
    },
    /// Make the Omarchy Broadcast Mic the default microphone (the one
    /// before, and the microphone in use at that moment, are remembered).
    Default,
    /// Make the microphone that was the default before `mic default` the
    /// default again; when it is unplugged, the microphone that was in
    /// use before comes back.
    RestoreDefault,
}

/// The microphone's properties, without starting the daemon.
async fn read(conn: &Connection) -> Result<MicStatus, Failure> {
    let snap: Snapshot = bus::fetch(conn).await?;
    bus::require_mic(snap.interface_revision)?;
    snap.mic
        .ok_or_else(|| Failure::Unsupported("the daemon does not report the microphone".into()))
}

fn print(m: &MicStatus, json: bool) {
    if json {
        out::line(&json::mic_line(m));
    } else {
        out::print(&human::mic_text(m));
    }
}

/// Runs `mic` (`None`: `mic status`).
pub async fn run(action: Option<&MicAction>, json: bool) -> u8 {
    let action = action.cloned().unwrap_or(MicAction::Status { json });
    if let MicAction::Status { json } = action {
        let res = within(READ_TIMEOUT, async {
            let conn = bus::connect().await?;
            read(&conn).await
        })
        .await;
        return match res {
            Ok(m) => {
                print(&m, json);
                EXIT_OK
            }
            Err(f) => report(&f, json),
        };
    }
    // Whether the daemon accepted the change: a failure after that is only
    // the read-back's, and the message says so.
    let changed = std::cell::Cell::new(false);
    let res = within(CALL_TIMEOUT, async {
        let conn = bus::connect().await?;
        let proxy = bus::proxy(&conn).await?;
        let revision = proxy
            .interface_revision()
            .await
            .map_err(|e| bus::classify(&e, true))?;
        bus::require_mic(revision)?;
        let set = |id: &'static str, v: SettingValue| {
            let proxy = proxy.clone();
            async move { proxy.set_setting_value("", id, &v).await }
        };
        match &action {
            MicAction::On => proxy.set_mic_host(true).await,
            MicAction::Off => proxy.set_mic_host(false).await,
            MicAction::Device { name } => set(DEVICE, SettingValue::Str(name.clone())).await,
            MicAction::Default => set(MAKE_DEFAULT, SettingValue::Bool(true)).await,
            MicAction::RestoreDefault => set(RESTORE_DEFAULT, SettingValue::Bool(true)).await,
            MicAction::Status { .. } => unreachable!("handled above"),
        }
        .map_err(bus::classify_ipc)?;
        changed.set(true);
        read(&conn).await
    })
    .await;
    match res {
        Ok(m) => {
            print(&m, false);
            EXIT_OK
        }
        Err(f) if changed.get() => {
            eprintln!(
                "omarchy-broadcast: the change was made, but reading the microphone back failed: {}",
                human::printable(&f.message())
            );
            f.exit_code()
        }
        Err(f) => report(&f, false),
    }
}
