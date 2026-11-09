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

    /// Selects the video effect: `none`, `blur` or `replace`. Persists
    /// across restarts. Fails with `InvalidArgument` for an unknown effect,
    /// or `replace` with no background image set. Revision 2.
    fn set_effect(&self, effect: &str) -> Result<(), IpcError>;

    /// Sets the strength of `blur`, from 0.0 to 1.0. Persists across
    /// restarts. Fails with `InvalidArgument` outside that range.
    /// Revision 2.
    fn set_blur_strength(&self, strength: f64) -> Result<(), IpcError>;

    /// Sets the background image of `replace`: an absolute path to a
    /// readable regular file, read by the daemon. Persists across restarts.
    /// Fails with `InvalidArgument` otherwise. Revision 2.
    fn set_background_image(&self, path: &str) -> Result<(), IpcError>;

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

    /// Effect in use: `none`, `blur` or `replace`.
    #[zbus(property)]
    fn effect(&self) -> zbus::Result<String>;

    /// Strength of `blur`, from 0.0 to 1.0. Revision 2.
    #[zbus(property)]
    fn blur_strength(&self) -> zbus::Result<f64>;

    /// Absolute path of the background image of `replace`; empty if none.
    /// Revision 2.
    #[zbus(property)]
    fn background_image(&self) -> zbus::Result<String>;

    /// Number of consumers: the processes holding the loopback plus the
    /// links to the PipeWire output (before revision 3, the loopback's
    /// only).
    #[zbus(property)]
    fn consumers(&self) -> zbus::Result<u32>;

    /// Number of processes holding the loopback device open. Revision 3.
    #[zbus(property)]
    fn loopback_consumers(&self) -> zbus::Result<u32>;

    /// Number of links to the PipeWire output; 0 when it is off.
    /// Revision 3.
    #[zbus(property, name = "PipeWireConsumers")]
    fn pipewire_consumers(&self) -> zbus::Result<u32>;

    /// State of the PipeWire output: `off`, `unsupported`, `connecting`,
    /// `ready`, `streaming` or `failed`. Revision 3.
    #[zbus(property, name = "PipeWireOutput")]
    fn pipewire_output(&self) -> zbus::Result<String>;

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
