//! The `br.com.dias.OmaBroadcast1` object (`docs/dbus-api.md`).
//!
//! [`Server`] answers property reads from the controller's published
//! snapshot and forwards each method to the controller, replying with its
//! result. [`run_publisher`] turns the controller's notes into
//! `PropertiesChanged` signals (one per changed property, with the value of
//! that step, so a short-lived state such as `starting` is announced too)
//! and `Stats` signals.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::mpsc::Sender;

use obc_ipc::{CameraInfo, INTERFACE, IpcError, OBJECT_PATH, Snapshot, State};
use tokio::sync::mpsc::UnboundedReceiver;
use zbus::names::InterfaceName;
use zbus::object_server::SignalEmitter;
use zbus::{Connection, interface};

use crate::controller::{Change, Command, Note, Published, Reply};

/// The D-Bus object (see the module docs).
pub struct Server {
    published: Published,
    commands: Sender<Command>,
}

impl Server {
    pub fn new(published: Published, commands: Sender<Command>) -> Self {
        Server {
            published,
            commands,
        }
    }

    fn snap(&self) -> Snapshot {
        self.published.snapshot()
    }

    async fn call(&self, make: impl FnOnce(Reply) -> Command) -> Result<(), IpcError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.commands
            .send(make(tx))
            .map_err(|_| IpcError::Failed("the daemon is shutting down".into()))?;
        rx.await
            .unwrap_or_else(|_| Err(IpcError::Failed("the daemon is shutting down".into())))
    }
}

#[interface(name = "br.com.dias.OmaBroadcast1")]
impl Server {
    /// Enables (`true`) or disables (`false`, the privacy state `off`) the
    /// camera. Persists across restarts.
    async fn set_enabled(&self, enabled: bool) -> Result<(), IpcError> {
        if !enabled {
            // Before queueing: a session start in progress then opens no
            // camera.
            self.published.request_disable();
        }
        self.call(|r| Command::SetEnabled(enabled, r)).await
    }

    /// Selects a camera by id, card-name substring, `vid:pid` or
    /// `/dev/videoN`. Persists across restarts.
    async fn select_camera(&self, selector: String) -> Result<(), IpcError> {
        self.call(|r| Command::SelectCamera(selector, r)).await
    }

    /// Selects the camera after `ActiveCamera`, wrapping around.
    async fn next_camera(&self) -> Result<(), IpcError> {
        self.call(Command::NextCamera).await
    }

    /// Re-reads the configuration file (same as SIGHUP).
    async fn reload_config(&self) -> Result<(), IpcError> {
        self.call(|r| Command::ReloadConfig(Some(r))).await
    }

    /// Selects the effect: `none`, `blur` or `replace` (which needs a
    /// background image). Applies to a running stream at once; persists
    /// across restarts.
    async fn set_effect(&self, effect: String) -> Result<(), IpcError> {
        self.call(|r| Command::SetEffect(effect, r)).await
    }

    /// Sets the blur strength, 0.0 to 1.0. Persists across restarts.
    async fn set_blur_strength(&self, strength: f64) -> Result<(), IpcError> {
        self.call(|r| Command::SetBlurStrength(strength, r)).await
    }

    /// Sets the background image of `replace`: an absolute path to a
    /// readable regular file of at most 32 MiB; empty clears it. Persists
    /// across restarts.
    async fn set_background_image(&self, path: String) -> Result<(), IpcError> {
        self.call(|r| Command::SetBackgroundImage(path, r)).await
    }

    /// About once per second while `active`; `-1` marks a value that cannot
    /// be measured.
    #[zbus(signal)]
    pub async fn stats(
        emitter: &SignalEmitter<'_>,
        fps: f64,
        latency_p50_ms: f64,
        cpu_percent: f64,
        gpu_percent: f64,
    ) -> zbus::Result<()>;

    #[zbus(property)]
    fn state(&self) -> State {
        self.snap().state
    }

    #[zbus(property)]
    fn error(&self) -> String {
        self.snap().error
    }

    #[zbus(property)]
    fn enabled(&self) -> bool {
        self.snap().enabled
    }

    #[zbus(property)]
    fn cameras(&self) -> Vec<CameraInfo> {
        self.snap().cameras
    }

    #[zbus(property)]
    fn active_camera(&self) -> String {
        self.snap().active_camera
    }

    #[zbus(property)]
    fn output_mode(&self) -> String {
        self.snap().output_mode
    }

    #[zbus(property)]
    fn effect(&self) -> String {
        self.snap().effect
    }

    #[zbus(property)]
    fn blur_strength(&self) -> f64 {
        self.snap().blur_strength
    }

    #[zbus(property)]
    fn background_image(&self) -> String {
        self.snap().background_image
    }

    /// Every consumer: the processes holding the loopback plus the links
    /// to the PipeWire output.
    #[zbus(property)]
    fn consumers(&self) -> u32 {
        self.snap().consumers
    }

    /// The processes holding the loopback device open (revision 3).
    #[zbus(property)]
    fn loopback_consumers(&self) -> u32 {
        self.snap().loopback_consumers
    }

    /// The links to the PipeWire output; 0 when it is off (revision 3).
    #[zbus(property, name = "PipeWireConsumers")]
    fn pipewire_consumers(&self) -> u32 {
        self.snap().pipewire_consumers
    }

    /// The PipeWire output's state: `off`, `unsupported`, `connecting`,
    /// `ready`, `streaming` or `failed` (revision 3).
    #[zbus(property, name = "PipeWireOutput")]
    fn pipewire_output(&self) -> String {
        self.snap().pipewire_output
    }

    #[zbus(property)]
    fn interface_revision(&self) -> u32 {
        self.snap().interface_revision
    }
}

/// Emits `PropertiesChanged` for one property with `value`.
async fn emit_changed<T>(emitter: &SignalEmitter<'_>, name: &str, value: &T) -> zbus::Result<()>
where
    T: serde::Serialize + zbus::Type,
{
    let mut changed = HashMap::new();
    changed.insert(name, zbus::as_variant::Serialize(value));
    emitter
        .emit(
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &(
                InterfaceName::from_static_str_unchecked(INTERFACE),
                changed,
                Cow::<[&str]>::Borrowed(&[]),
            ),
        )
        .await
}

async fn emit_change(emitter: &SignalEmitter<'_>, change: &Change) -> zbus::Result<()> {
    let name = change.name();
    match change {
        Change::State(v) => emit_changed(emitter, name, v).await,
        Change::Error(v)
        | Change::ActiveCamera(v)
        | Change::OutputMode(v)
        | Change::Effect(v)
        | Change::BackgroundImage(v)
        | Change::PipeWireOutput(v) => emit_changed(emitter, name, v).await,
        Change::BlurStrength(v) => emit_changed(emitter, name, v).await,
        Change::Enabled(v) => emit_changed(emitter, name, v).await,
        Change::Cameras(v) => emit_changed(emitter, name, v).await,
        Change::Consumers(v) | Change::LoopbackConsumers(v) | Change::PipeWireConsumers(v) => {
            emit_changed(emitter, name, v).await
        }
    }
}

/// Emits the controller's notes on `conn` until the controller is gone.
pub async fn run_publisher(conn: Connection, mut notes: UnboundedReceiver<Note>) {
    let emitter = match SignalEmitter::new(&conn, OBJECT_PATH) {
        Ok(e) => e,
        Err(e) => {
            crate::error!("D-Bus signals unavailable: {e}");
            return;
        }
    };
    while let Some(note) = notes.recv().await {
        let res = match &note {
            Note::Changed(changes) => {
                let mut res = Ok(());
                for c in changes {
                    res = res.and(emit_change(&emitter, c).await);
                }
                res
            }
            Note::Stats(s) => {
                Server::stats(
                    &emitter,
                    s.fps,
                    s.latency_p50_ms,
                    s.cpu_percent,
                    s.gpu_percent,
                )
                .await
            }
        };
        if let Err(e) = res {
            crate::debug!("emitting a signal failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests;
