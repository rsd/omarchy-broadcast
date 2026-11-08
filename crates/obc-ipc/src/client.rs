//! Client proxy for `br.com.dias.OmaBroadcast1`.
//!
//! [`OmaBroadcastProxy::new`] targets the daemon's well-known name and
//! object path on the given connection; use [`OmaBroadcastProxy::builder`] to
//! point it elsewhere (a peer-to-peer connection, a test server).
//!
//! The `Stats` signal yields the message wrapper [`Stats`] (this module's,
//! not [`crate::Stats`]); [`StatsArgs::to_stats`] turns its arguments into
//! the value type.

use crate::{CameraInfo, IpcError, State};

/// The daemon's interface, as seen by a client. Every property is read-only
/// and emits `org.freedesktop.DBus.Properties.PropertiesChanged`.
#[zbus::proxy(
    interface = "br.com.dias.OmaBroadcast1",
    default_service = "br.com.dias.OmaBroadcast",
    default_path = "/br/com/dias/OmaBroadcast"
)]
pub trait OmaBroadcast {
    /// Enables (`true`) or disables (`false`, the privacy state `off`) the
    /// camera. Persists across restarts.
    fn set_enabled(&self, enabled: bool) -> Result<(), IpcError>;

    /// Selects a camera by id, card-name substring, `vid:pid` or
    /// `/dev/videoN`. Persists across restarts.
    fn select_camera(&self, selector: &str) -> Result<(), IpcError>;

    /// Selects the next camera in order.
    fn next_camera(&self) -> Result<(), IpcError>;

    /// Re-reads the configuration file.
    fn reload_config(&self) -> Result<(), IpcError>;

    /// Roughly once per second while `active`; `-1` marks a value that
    /// cannot be measured.
    #[zbus(signal)]
    fn stats(
        &self,
        fps: f64,
        latency_p50_ms: f64,
        cpu_percent: f64,
        gpu_percent: f64,
    ) -> zbus::Result<()>;

    /// Lifecycle state.
    #[zbus(property)]
    fn state(&self) -> zbus::Result<State>;

    /// Text of the latest error, empty if none.
    #[zbus(property)]
    fn error(&self) -> zbus::Result<String>;

    /// Whether the camera is enabled (false = `off`).
    #[zbus(property)]
    fn enabled(&self) -> zbus::Result<bool>;

    /// Cameras present, in selection order.
    #[zbus(property)]
    fn cameras(&self) -> zbus::Result<Vec<CameraInfo>>;

    /// Id of the camera that streams or would stream; empty if none.
    #[zbus(property)]
    fn active_camera(&self) -> zbus::Result<String>;

    /// Output contract, e.g. `NV12 1920x1080@30`.
    #[zbus(property)]
    fn output_mode(&self) -> zbus::Result<String>;

    /// Effect in use; reserved, always `none` in this revision.
    #[zbus(property)]
    fn effect(&self) -> zbus::Result<String>;

    /// Number of processes holding the virtual camera open.
    #[zbus(property)]
    fn consumers(&self) -> zbus::Result<u32>;

    /// Revision of the interface the daemon implements.
    #[zbus(property)]
    fn interface_revision(&self) -> zbus::Result<u32>;
}

impl StatsArgs<'_> {
    /// The signal's arguments as a [`crate::Stats`] value.
    pub fn to_stats(&self) -> crate::Stats {
        crate::Stats {
            fps: self.fps,
            latency_p50_ms: self.latency_p50_ms,
            cpu_percent: self.cpu_percent,
            gpu_percent: self.gpu_percent,
        }
    }
}
