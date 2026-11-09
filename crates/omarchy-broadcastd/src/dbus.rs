//! The `br.com.dias.OmarchyBroadcast1` object (`docs/dbus-api.md`).
//!
//! [`Server`] answers property reads from the controller's published
//! snapshot and forwards each method to the controller, replying with its
//! result. [`run_publisher`] turns the controller's notes into
//! `PropertiesChanged` signals (one per changed property, with the value of
//! that step, so a short-lived state such as `starting` is announced too),
//! `Stats` and `PreviewFrame` signals. [`run_owner_watch`] follows the
//! unique names leaving the bus, which ends their preview leases.
//!
//! A preview lease belongs to the caller's unique bus name (the message
//! header's sender). A caller that left the bus before its lease was
//! recorded would leak it (its `NameOwnerChanged` may come before the
//! lease): so once the lease is recorded, `AcquirePreview` asks the bus
//! whether the name still has an owner, and ends the lease if not. Over a
//! peer-to-peer connection (no bus, no sender) `AcquirePreview` fails.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::mpsc::Sender;

use futures_util::StreamExt;
use obc_ipc::{CameraInfo, INTERFACE, IpcError, OBJECT_PATH, Snapshot, State};
use tokio::sync::mpsc::UnboundedReceiver;
use zbus::fdo::NameOwnerChangedStream;
use zbus::message::Header;
use zbus::names::{BusName, InterfaceName};
use zbus::object_server::SignalEmitter;
use zbus::proxy::CacheProperties;
use zbus::{Connection, interface};

use crate::controller::{Change, Command, Note, PathReply, Published, Reply};

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
        self.ask(make).await
    }

    /// Sends the command `make` builds and waits for its reply.
    async fn ask<T>(
        &self,
        make: impl FnOnce(tokio::sync::oneshot::Sender<Result<T, IpcError>>) -> Command,
    ) -> Result<T, IpcError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.commands
            .send(make(tx))
            .map_err(|_| IpcError::Failed("the daemon is shutting down".into()))?;
        rx.await
            .unwrap_or_else(|_| Err(IpcError::Failed("the daemon is shutting down".into())))
    }
}

/// The caller's unique bus name, which a preview lease is keyed by.
fn caller(header: &Header<'_>) -> Result<String, IpcError> {
    header.sender().map(|s| s.to_string()).ok_or_else(|| {
        IpcError::Failed(
            "the call has no sender: a preview lease needs a bus connection, which names its \
             holder"
                .into(),
        )
    })
}

/// Whether `name` still has an owner on the bus of `conn`; `true` when
/// the bus cannot tell (the lease then ends with the name's
/// `NameOwnerChanged`).
async fn still_there(conn: &Connection, name: &str) -> bool {
    let Ok(bus_name) = BusName::try_from(name) else {
        return false;
    };
    let dbus = match zbus::fdo::DBusProxy::builder(conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
    {
        Ok(d) => d,
        Err(_) => return true,
    };
    dbus.name_has_owner(bus_name).await.unwrap_or(true)
}

#[interface(name = "br.com.dias.OmarchyBroadcast1")]
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

    /// Switches video noise removal on or off: it runs before the effect,
    /// or alone with `none`. Applies to a running stream at once; persists
    /// across restarts (revision 4).
    async fn set_denoise(&self, denoise: bool) -> Result<(), IpcError> {
        self.call(|r| Command::SetDenoise(denoise, r)).await
    }

    /// Takes (or renews) the caller's preview lease at `fps`, 15 or 30;
    /// returns the frame file's absolute path. Works in every state
    /// (revision 5).
    async fn acquire_preview(
        &self,
        fps: u32,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<String, IpcError> {
        let name = caller(&header)?;
        let path = self
            .ask(|reply: PathReply| Command::AcquirePreview {
                name: name.clone(),
                fps,
                reply,
            })
            .await?;
        // The caller may have left before the lease was recorded, its
        // NameOwnerChanged handled first: the lease must not outlive it.
        if !still_there(conn, &name).await {
            let _ = self.commands.send(Command::PreviewOwnerGone(name));
        }
        Ok(path)
    }

    /// Ends the caller's preview lease; without one, does nothing
    /// (revision 5).
    async fn release_preview(&self, #[zbus(header)] header: Header<'_>) -> Result<(), IpcError> {
        let name = caller(&header)?;
        self.call(|r| Command::ReleasePreview(name, r)).await
    }

    /// After each replace of the preview frame file while a lease is held;
    /// `seq` increases strictly within a daemon run, from 1 (revision 5).
    #[zbus(signal)]
    pub async fn preview_frame(emitter: &SignalEmitter<'_>, seq: u32) -> zbus::Result<()>;

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

    /// Whether video noise removal is requested (revision 4).
    #[zbus(property)]
    fn denoise(&self) -> bool {
        self.snap().denoise
    }

    /// Every consumer: the processes holding the loopback, the links to the
    /// PipeWire output and the preview leases.
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

    /// The preview leases held (revision 5).
    #[zbus(property)]
    fn preview_consumers(&self) -> u32 {
        self.snap().preview_consumers
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
        Change::Enabled(v) | Change::Denoise(v) => emit_changed(emitter, name, v).await,
        Change::Cameras(v) => emit_changed(emitter, name, v).await,
        Change::Consumers(v)
        | Change::LoopbackConsumers(v)
        | Change::PipeWireConsumers(v)
        | Change::PreviewConsumers(v) => emit_changed(emitter, name, v).await,
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
            Note::PreviewFrame(seq) => Server::preview_frame(&emitter, *seq).await,
        };
        if let Err(e) = res {
            crate::debug!("emitting a signal failed: {e}");
        }
    }
}

/// Subscribes to `NameOwnerChanged` on the bus of `conn`, for
/// [`run_owner_watch`]. Done before `AcquirePreview` is served, so that no
/// departure after a lease was recorded is missed.
pub async fn subscribe_owners(conn: &Connection) -> zbus::Result<NameOwnerChangedStream> {
    let dbus = zbus::fdo::DBusProxy::new(conn).await?;
    dbus.receive_name_owner_changed().await
}

/// Tells the controller of every unique name that leaves the bus
/// (`NameOwnerChanged` with no new owner, from [`subscribe_owners`]), which
/// ends its preview lease if it holds one, until the connection closes or
/// the controller is gone.
pub async fn run_owner_watch(
    mut owners: NameOwnerChangedStream,
    commands: std::sync::mpsc::Sender<Command>,
) {
    while let Some(signal) = owners.next().await {
        let Ok(args) = signal.args() else { continue };
        if args.new_owner().is_none()
            && let BusName::Unique(name) = args.name()
            && commands
                .send(Command::PreviewOwnerGone(name.to_string()))
                .is_err()
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests;
