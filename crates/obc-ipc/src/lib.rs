//! D-Bus interface `br.com.dias.OmarchyBroadcast1` (zbus), shared by the daemon
//! and its clients (see `docs/adr/0005-dbus-zbus-control-plane.md` and
//! `docs/adr/0011-daemon-lifecycle-and-dbus-api.md`; the reference for
//! client authors is `docs/dbus-api.md`).
//!
//! This crate holds the names, the value types carried on the bus, the error
//! type the daemon replies with and the client proxy ([`client`]). The
//! server-side `#[interface]` implementation belongs to the daemon.
//!
//! # Reading state without starting the daemon
//!
//! When the daemon's D-Bus activation file is installed, any message
//! addressed to [`BUS_NAME`] without the `NO_AUTO_START` flag starts the
//! daemon if it is not running. The generated proxy sends such messages for
//! reads too: a proxy that caches properties (the default of
//! [`OmarchyBroadcastProxy::new`] and of [`OmarchyBroadcastProxy::builder`]) fills
//! its cache with a `GetAll` without the flag, and the property getters
//! (`state()`, `cameras()`, ...) fall back to a `Get` without it.
//!
//! A client that only looks (a status bar, a `status` command) reads with
//! [`Snapshot::fetch_no_autostart`] instead: one `GetAll` carrying
//! `NO_AUTO_START`, which yields `None` when the daemon is not running. It
//! follows changes with `PropertiesChanged`, applied with
//! [`Snapshot::apply_property`], and the daemon's presence with
//! `org.freedesktop.DBus.NameOwnerChanged` or [`daemon_is_running`]. Signal
//! subscriptions never start the daemon. Method calls through the proxy
//! (build it with `cache_properties(CacheProperties::No)` so building it
//! sends nothing) may start it, which is what a control action wants.
//!
//! zbus sets no time limit on a method call by default: a daemon that holds
//! its name but does not answer keeps the call pending. Bound reads with
//! `zbus::connection::Builder::method_timeout` or with the runtime's own
//! timeout.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use zbus::as_variant::deserialize_for_property;
use zbus::fdo::DBusProxy;
use zbus::names::BusName;
use zbus::proxy::{CacheProperties, MethodFlags};
use zbus::{Connection, DBusError, OwnedVariant, Type, Variant};

pub mod client;

pub use client::OmarchyBroadcastProxy;

/// Well-known name of the daemon on the session bus.
pub const BUS_NAME: &str = "br.com.dias.OmarchyBroadcast";
/// Path of the one object the daemon exports.
pub const OBJECT_PATH: &str = "/br/com/dias/OmarchyBroadcast";
/// Interface name. Breaking changes add `br.com.dias.OmarchyBroadcast2`.
pub const INTERFACE: &str = "br.com.dias.OmarchyBroadcast1";
/// Revision of [`INTERFACE`] this crate describes, exposed by the daemon as
/// the `InterfaceRevision` property. Raised for every backwards-compatible
/// addition (a new property, method or signal). Revision 2 added the
/// effect controls: `BlurStrength`, `BackgroundImage`, `SetEffect`,
/// `SetBlurStrength`, `SetBackgroundImage` and the `InvalidArgument` error.
/// Revision 3 added the optional PipeWire output: `LoopbackConsumers`,
/// `PipeWireConsumers` and `PipeWireOutput`, with `Consumers` the total of
/// both kinds of consumer. Revision 4 added video noise removal: `Denoise`
/// and `SetDenoise`. Revision 5 added the live preview: `AcquirePreview`,
/// `ReleasePreview`, the `PreviewFrame` signal and `PreviewConsumers`, with
/// `Consumers` counting the preview leases too.
pub const INTERFACE_REVISION: u32 = 5;
/// The first [`INTERFACE_REVISION`] with `LoopbackConsumers`,
/// `PipeWireConsumers` and `PipeWireOutput`.
pub const PIPEWIRE_REVISION: u32 = 3;
/// The first [`INTERFACE_REVISION`] with `Denoise` and `SetDenoise`.
pub const DENOISE_REVISION: u32 = 4;
/// The first [`INTERFACE_REVISION`] with the live preview:
/// `AcquirePreview`, `ReleasePreview`, `PreviewFrame` and
/// `PreviewConsumers`.
pub const PREVIEW_REVISION: u32 = 5;
/// The frame rates `AcquirePreview` accepts.
pub const PREVIEW_FPS: [u32; 2] = [15, 30];
/// `PipeWireOutput` of a daemon whose PipeWire output is not configured, or
/// that has none (before [`PIPEWIRE_REVISION`]).
pub const PIPEWIRE_OFF: &str = "off";
/// Prefix of every D-Bus error name the daemon replies with
/// (`br.com.dias.OmarchyBroadcast1.Error.NoSuchCamera`, ...).
pub const ERROR_PREFIX: &str = "br.com.dias.OmarchyBroadcast1.Error";

/// Lifecycle state of the daemon, carried as a lower-case string (`s`).
///
/// The set is fixed for [`INTERFACE`]: a new state would be a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
#[zbus(signature = "s")]
pub enum State {
    /// Disabled by the user (`Enabled` = false): camera closed, no CUDA
    /// context, black placeholder frames on the loopback.
    Off,
    /// Enabled, no consumer: camera closed, no CUDA context, placeholder.
    Idle,
    /// A consumer is present: the camera and the GPU pipeline are starting.
    Starting,
    /// Camera frames are streaming (includes the grace period after the last
    /// consumer leaves).
    Active,
    /// A lifecycle failure (no loopback, no camera, camera lost); the
    /// `Error` property says which.
    Error,
}

impl State {
    /// Every state, in lifecycle order.
    pub const ALL: [State; 5] = [
        State::Off,
        State::Idle,
        State::Starting,
        State::Active,
        State::Error,
    ];

    /// The wire form: `off`, `idle`, `starting`, `active`, `error`.
    pub fn as_str(self) -> &'static str {
        match self {
            State::Off => "off",
            State::Idle => "idle",
            State::Starting => "starting",
            State::Active => "active",
            State::Error => "error",
        }
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A string that is not one of the [`State`] names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownState(pub String);

impl fmt::Display for UnknownState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown state {:?} (expected off, idle, starting, active or error)",
            self.0
        )
    }
}

impl std::error::Error for UnknownState {}

impl FromStr for State {
    type Err = UnknownState;

    /// Parses the exact lower-case wire form.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        State::ALL
            .into_iter()
            .find(|st| st.as_str() == s)
            .ok_or_else(|| UnknownState(s.to_string()))
    }
}

/// One camera the daemon can stream from, as listed by the `Cameras`
/// property: a D-Bus struct `(sss)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
pub struct CameraInfo {
    /// Stable identifier, accepted by `SelectCamera` and reported by
    /// `ActiveCamera`.
    pub id: String,
    /// Human-readable name: the V4L2 card name (a repeated `X: X` reduced
    /// to `X`), unique among the cameras present (` (2)`, ` (3)`, ...).
    pub name: String,
    /// USB `vid:pid` in lower-case hex (`1234:abcd`), empty if not a USB
    /// device.
    pub usb: String,
}

/// One sample of the `Stats` signal. A value that cannot be measured is
/// [`Stats::NOT_MEASURABLE`] (`-1.0`).
///
/// This struct is a convenience for code that handles the four values
/// together. Its own D-Bus signature is the struct `(dddd)`, but the `Stats`
/// signal carries four separate arguments, signature `dddd`: emit the signal
/// with the four fields as arguments, never with this value as one argument.
/// On the client side, [`client::StatsArgs::to_stats`] builds it from the
/// signal's arguments.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Type)]
pub struct Stats {
    /// Frames delivered to the loopback device per second, as counted by
    /// the daemon's loopback writer: camera frames, repeats and black
    /// placeholder frames alike.
    pub fps: f64,
    /// Median capture-to-output latency, in milliseconds.
    pub latency_p50_ms: f64,
    /// CPU use of the daemon, in percent of one core.
    pub cpu_percent: f64,
    /// GPU utilisation, in percent.
    pub gpu_percent: f64,
}

impl Stats {
    /// The value of a field that cannot be measured.
    pub const NOT_MEASURABLE: f64 = -1.0;

    /// Whether `value` is a measurement rather than [`Self::NOT_MEASURABLE`]
    /// (any negative value counts as not measurable).
    pub fn is_measured(value: f64) -> bool {
        value >= 0.0
    }
}

impl Default for Stats {
    /// Every field not measurable.
    fn default() -> Self {
        Stats {
            fps: Stats::NOT_MEASURABLE,
            latency_p50_ms: Stats::NOT_MEASURABLE,
            cpu_percent: Stats::NOT_MEASURABLE,
            gpu_percent: Stats::NOT_MEASURABLE,
        }
    }
}

/// The errors the daemon's methods reply with, named
/// `br.com.dias.OmarchyBroadcast1.Error.<Variant>`.
///
/// On the client side a reply carrying one of these names converts back to
/// its variant; any other failure is [`IpcError::ZBus`].
#[derive(Debug, DBusError)]
#[zbus(prefix = "br.com.dias.OmarchyBroadcast1.Error")]
pub enum IpcError {
    /// `SelectCamera` named no camera that is present.
    NoSuchCamera(String),
    /// The configuration file is invalid; the previous config stays in force.
    InvalidConfig(String),
    /// The request cannot be served right now (e.g. no camera at all).
    Unavailable(String),
    /// An argument is out of its domain: an unknown effect, a blur strength
    /// outside 0 to 1, a background image that is not an absolute path to a
    /// readable regular file, or `replace` with no background image
    /// (revision 2); a preview frame rate other than 15 or 30 (revision 5).
    InvalidArgument(String),
    /// Any other failure.
    Failed(String),
    /// A transport or protocol error (not a daemon reply).
    #[zbus(error)]
    ZBus(zbus::Error),
}

impl IpcError {
    /// The human-readable message, without the error name.
    pub fn message(&self) -> String {
        match self {
            IpcError::NoSuchCamera(m)
            | IpcError::InvalidConfig(m)
            | IpcError::Unavailable(m)
            | IpcError::InvalidArgument(m)
            | IpcError::Failed(m) => m.clone(),
            IpcError::ZBus(e) => e.to_string(),
        }
    }
}

/// Every property of the interface at one moment, for `status`-style output.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Snapshot {
    pub state: State,
    pub error: String,
    pub enabled: bool,
    pub cameras: Vec<CameraInfo>,
    pub active_camera: String,
    pub output_mode: String,
    pub effect: String,
    /// `BlurStrength` (revision 2); `0.0` when the daemon does not offer it.
    pub blur_strength: f64,
    /// `BackgroundImage` (revision 2); empty when the daemon does not offer
    /// it.
    pub background_image: String,
    /// `Denoise` (revision 4): video noise removal is requested; `false`
    /// when the daemon does not offer it (tell that case by
    /// `interface_revision` below [`DENOISE_REVISION`]).
    pub denoise: bool,
    /// `Consumers`: every consumer, those holding the loopback, those
    /// linked to the PipeWire output and the preview leases.
    pub consumers: u32,
    /// `LoopbackConsumers` (revision 3): the processes holding the
    /// loopback. From a daemon without it, `Consumers` (which then counts
    /// only those).
    pub loopback_consumers: u32,
    /// `PipeWireConsumers` (revision 3): the links to the PipeWire output;
    /// `0` when the daemon does not offer it.
    pub pipewire_consumers: u32,
    /// `PreviewConsumers` (revision 5): the preview leases held; `0` when
    /// the daemon does not offer it.
    pub preview_consumers: u32,
    /// `PipeWireOutput` (revision 3): `off`, `unsupported`, `connecting`,
    /// `ready`, `streaming` or `failed`; `off` when the daemon does not
    /// offer it.
    pub pipewire_output: String,
    pub interface_revision: u32,
}

impl Snapshot {
    /// A snapshot with nothing read yet: the starting point of
    /// [`Snapshot::from_properties`].
    pub fn empty() -> Snapshot {
        Snapshot {
            state: State::Off,
            error: String::new(),
            enabled: false,
            cameras: Vec::new(),
            active_camera: String::new(),
            output_mode: String::new(),
            effect: String::new(),
            blur_strength: 0.0,
            background_image: String::new(),
            denoise: false,
            consumers: 0,
            loopback_consumers: 0,
            pipewire_consumers: 0,
            preview_consumers: 0,
            pipewire_output: PIPEWIRE_OFF.into(),
            interface_revision: 0,
        }
    }

    /// Sets the field that the D-Bus property `name` maps to, from a
    /// `GetAll` reply or a `PropertiesChanged` signal. Properties this crate
    /// does not know (added by a later revision) are ignored; a value of the
    /// wrong type, or a `State` that is not a [`State`] name, is an error.
    ///
    /// For a daemon known to predate [`PIPEWIRE_REVISION`]
    /// (`interface_revision` read and below it), `Consumers` also sets
    /// `loopback_consumers`: such a daemon counts only the loopback.
    pub fn apply_property(&mut self, name: &str, value: &Variant<'_>) -> zbus::Result<()> {
        match name {
            "State" => {
                let s: String = deserialize_for_property(value)?;
                self.state = s
                    .parse()
                    .map_err(|e: UnknownState| zbus::Error::Failure(e.to_string()))?;
            }
            "Error" => self.error = deserialize_for_property(value)?,
            "Enabled" => self.enabled = deserialize_for_property(value)?,
            "Cameras" => self.cameras = deserialize_for_property::<Vec<CameraInfo>>(value)?,
            "ActiveCamera" => self.active_camera = deserialize_for_property(value)?,
            "OutputMode" => self.output_mode = deserialize_for_property(value)?,
            "Effect" => self.effect = deserialize_for_property(value)?,
            "BlurStrength" => self.blur_strength = deserialize_for_property(value)?,
            "BackgroundImage" => self.background_image = deserialize_for_property(value)?,
            "Denoise" => self.denoise = deserialize_for_property(value)?,
            "Consumers" => {
                self.consumers = deserialize_for_property(value)?;
                if (1..PIPEWIRE_REVISION).contains(&self.interface_revision) {
                    self.loopback_consumers = self.consumers;
                }
            }
            "LoopbackConsumers" => self.loopback_consumers = deserialize_for_property(value)?,
            "PipeWireConsumers" => self.pipewire_consumers = deserialize_for_property(value)?,
            "PreviewConsumers" => self.preview_consumers = deserialize_for_property(value)?,
            "PipeWireOutput" => self.pipewire_output = deserialize_for_property(value)?,
            "InterfaceRevision" => self.interface_revision = deserialize_for_property(value)?,
            _ => {}
        }
        Ok(())
    }

    /// A snapshot from the properties returned by `GetAll`. `State` is
    /// required; any other missing property keeps its [`Snapshot::empty`]
    /// value, except `LoopbackConsumers`, which is then `Consumers` (a
    /// daemon without it counts only the loopback).
    pub fn from_properties<'a, I>(props: I) -> zbus::Result<Snapshot>
    where
        I: IntoIterator<Item = (&'a str, &'a Variant<'a>)>,
    {
        let mut snap = Snapshot::empty();
        let mut has_state = false;
        let mut has_loopback = false;
        for (name, value) in props {
            has_state |= name == "State";
            has_loopback |= name == "LoopbackConsumers";
            snap.apply_property(name, value)?;
        }
        if !has_loopback {
            snap.loopback_consumers = snap.consumers;
        }
        if !has_state {
            return Err(zbus::Error::Failure(
                "the daemon's properties have no State".into(),
            ));
        }
        Ok(snap)
    }

    /// Reads every property with one `org.freedesktop.DBus.Properties.GetAll`
    /// carrying the `NO_AUTO_START` flag, so the read never starts the
    /// daemon: the way for clients to read the daemon's state.
    ///
    /// `Ok(None)` means the daemon is not running (the bus reports that
    /// [`BUS_NAME`] has no owner). Any other failure, including a reply that
    /// does not map to a [`Snapshot`], is an error. No time limit is applied
    /// (see the crate documentation).
    pub async fn fetch_no_autostart(conn: &Connection) -> zbus::Result<Option<Snapshot>> {
        let props: zbus::Proxy<'_> = zbus::proxy::Builder::new(conn)
            .destination(BUS_NAME)
            .path(OBJECT_PATH)
            .interface("org.freedesktop.DBus.Properties")
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        let reply: Option<HashMap<String, OwnedVariant>> = match props
            .call_with_flags("GetAll", MethodFlags::NO_AUTO_START, &(INTERFACE,))
            .await
        {
            Ok(reply) => reply,
            Err(e) if is_not_running_error(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        let reply = reply.ok_or_else(|| zbus::Error::Failure("GetAll returned no reply".into()))?;
        Snapshot::from_properties(reply.iter().map(|(k, v)| (k.as_str(), &**v))).map(Some)
    }

    /// Reads every property through `proxy` (from its cache when the proxy
    /// caches properties, which is the default).
    ///
    /// This read may start the daemon by D-Bus activation: a caching proxy
    /// built on a bus connection has already sent a `GetAll` without
    /// `NO_AUTO_START`, and the getters of an uncached one send `Get`
    /// without it. Clients that only look use
    /// [`Snapshot::fetch_no_autostart`].
    ///
    /// The properties added after revision 1 (`BlurStrength`,
    /// `BackgroundImage`, `PipeWireConsumers`, `PipeWireOutput`, `Denoise`,
    /// `PreviewConsumers`)
    /// keep their [`Snapshot::empty`] value when the daemon does not offer
    /// them (see [`is_unknown_property_error`]), and `LoopbackConsumers` is then
    /// `Consumers`, as [`Snapshot::from_properties`] does for a property
    /// missing from `GetAll`; any other failure to read them is an error.
    pub async fn fetch(proxy: &OmarchyBroadcastProxy<'_>) -> zbus::Result<Snapshot> {
        let empty = Snapshot::empty();
        let consumers = proxy.consumers().await?;
        Ok(Snapshot {
            state: proxy.state().await?,
            error: proxy.error().await?,
            enabled: proxy.enabled().await?,
            cameras: proxy.cameras().await?,
            active_camera: proxy.active_camera().await?,
            output_mode: proxy.output_mode().await?,
            effect: proxy.effect().await?,
            blur_strength: optional(proxy.blur_strength().await, empty.blur_strength)?,
            background_image: optional(proxy.background_image().await, empty.background_image)?,
            denoise: optional(proxy.denoise().await, empty.denoise)?,
            consumers,
            loopback_consumers: optional(proxy.loopback_consumers().await, consumers)?,
            pipewire_consumers: optional(
                proxy.pipewire_consumers().await,
                empty.pipewire_consumers,
            )?,
            preview_consumers: optional(proxy.preview_consumers().await, empty.preview_consumers)?,
            pipewire_output: optional(proxy.pipewire_output().await, empty.pipewire_output)?,
            interface_revision: proxy.interface_revision().await?,
        })
    }
}

/// `value`, or `default` when the read failed because the daemon does not
/// have the property.
fn optional<T>(value: zbus::Result<T>, default: T) -> zbus::Result<T> {
    match value {
        Err(e) if is_unknown_property_error(&e) => Ok(default),
        other => other,
    }
}

/// Whether `e` is the reply to `Properties.Get` for a property the object
/// does not have: `org.freedesktop.DBus.Error.UnknownProperty`, or
/// `org.freedesktop.DBus.Error.InvalidArgs`, which some implementations
/// send instead. A daemon of an older [`INTERFACE_REVISION`] replies so for
/// the properties added since.
pub fn is_unknown_property_error(e: &zbus::Error) -> bool {
    const NAMES: [&str; 2] = [
        "org.freedesktop.DBus.Error.UnknownProperty",
        "org.freedesktop.DBus.Error.InvalidArgs",
    ];
    match e {
        zbus::Error::MethodError(name, _, _) => NAMES.contains(&name.as_str()),
        zbus::Error::FDO(f) => NAMES.contains(&f.name().as_str()),
        _ => false,
    }
}

/// Whether `e` is the bus's reply that [`BUS_NAME`] has no owner (and, for
/// a call that allowed activation, cannot be started): the daemon is not
/// running. `org.freedesktop.DBus.Error.NameHasNoOwner` and
/// `org.freedesktop.DBus.Error.ServiceUnknown` qualify.
pub fn is_not_running_error(e: &zbus::Error) -> bool {
    const NAMES: [&str; 2] = [
        "org.freedesktop.DBus.Error.NameHasNoOwner",
        "org.freedesktop.DBus.Error.ServiceUnknown",
    ];
    match e {
        zbus::Error::MethodError(name, _, _) => NAMES.contains(&name.as_str()),
        zbus::Error::FDO(f) => NAMES.contains(&f.name().as_str()),
        _ => false,
    }
}

/// Whether the daemon is running: asks the bus whether [`BUS_NAME`] has an
/// owner (`org.freedesktop.DBus.NameHasOwner`). Never starts the daemon.
/// Needs a bus connection (not a peer-to-peer one).
pub async fn daemon_is_running(conn: &Connection) -> zbus::Result<bool> {
    let dbus = DBusProxy::builder(conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    let name = BusName::try_from(BUS_NAME)?;
    Ok(dbus.name_has_owner(name).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_consistent() {
        assert!(INTERFACE.starts_with(BUS_NAME));
        assert!(INTERFACE.ends_with('1'));
        assert_eq!(ERROR_PREFIX, format!("{INTERFACE}.Error"));
        assert_eq!(OBJECT_PATH, format!("/{}", BUS_NAME.replace('.', "/")));
        let e = IpcError::NoSuchCamera("x".into());
        assert_eq!(
            zbus::DBusError::name(&e).as_str(),
            format!("{ERROR_PREFIX}.NoSuchCamera")
        );
        let e = IpcError::InvalidArgument("bad".into());
        assert_eq!(
            zbus::DBusError::name(&e).as_str(),
            format!("{ERROR_PREFIX}.InvalidArgument")
        );
        assert_eq!(e.message(), "bad");
    }

    #[test]
    fn state_round_trips() {
        for st in State::ALL {
            assert_eq!(st.as_str().parse::<State>(), Ok(st));
            assert_eq!(st.to_string(), st.as_str());
            let json = serde_json::to_string(&st).unwrap();
            assert_eq!(json, format!("{:?}", st.as_str()));
            assert_eq!(serde_json::from_str::<State>(&json).unwrap(), st);
        }
        assert!("Active".parse::<State>().is_err());
        assert!("".parse::<State>().is_err());
        assert_eq!(<State as Type>::SIGNATURE.to_string(), "s");
    }

    #[test]
    fn wire_signatures() {
        assert_eq!(<CameraInfo as Type>::SIGNATURE.to_string(), "(sss)");
        assert_eq!(<Vec<CameraInfo> as Type>::SIGNATURE.to_string(), "a(sss)");
        assert_eq!(<Stats as Type>::SIGNATURE.to_string(), "(dddd)");
    }

    fn cam(id: &str, name: &str) -> CameraInfo {
        CameraInfo {
            id: id.into(),
            name: name.into(),
            usb: String::new(),
        }
    }

    #[test]
    fn properties_build_a_snapshot() {
        let state = Variant::from("active");
        let enabled = Variant::from(true);
        let consumers = Variant::from(3u32);
        let cameras = zbus::as_variant::to_owned_variant(&vec![cam("a", "A")]).unwrap();
        let future = Variant::from(7u32);
        let props: Vec<(&str, &Variant<'_>)> = vec![
            ("State", &state),
            ("Enabled", &enabled),
            ("Consumers", &consumers),
            ("Cameras", &cameras),
            ("SomethingNew", &future),
        ];
        let snap = Snapshot::from_properties(props).unwrap();
        assert_eq!(snap.state, State::Active);
        // Properties of a later revision missing from GetAll keep the empty
        // values.
        assert_eq!(snap.blur_strength, 0.0);
        assert_eq!(snap.background_image, "");
        assert!(snap.enabled);
        assert_eq!(snap.consumers, 3);
        assert_eq!(snap.cameras, vec![cam("a", "A")]);
        assert_eq!(snap.error, "");

        let bad = Variant::from("Active");
        assert!(Snapshot::from_properties([("State", &bad)]).is_err());
        assert!(Snapshot::from_properties([("Enabled", &enabled)]).is_err());
        let wrong_type = Variant::from(1u32);
        assert!(Snapshot::from_properties([("State", &state), ("Enabled", &wrong_type)]).is_err());

        // A change applies to one field and leaves the others.
        let mut changed = snap.clone();
        let error = Variant::from("camera lost");
        changed.apply_property("Error", &error).unwrap();
        assert_eq!(changed.error, "camera lost");
        assert_eq!(changed.state, snap.state);
        assert!(changed.apply_property("Consumers", &state).is_err());

        // The revision-2 properties.
        let strength = Variant::from(0.75f64);
        let image = Variant::from("/home/u/bg.png");
        changed.apply_property("BlurStrength", &strength).unwrap();
        changed.apply_property("BackgroundImage", &image).unwrap();
        assert_eq!(changed.blur_strength, 0.75);
        assert_eq!(changed.background_image, "/home/u/bg.png");
        assert!(changed.apply_property("BlurStrength", &image).is_err());
        assert!(
            changed
                .apply_property("BackgroundImage", &strength)
                .is_err()
        );
    }

    #[test]
    fn the_revision_3_properties_and_an_older_daemon() {
        let state = Variant::from("active");
        let total = Variant::from(3u32);
        let loopback = Variant::from(1u32);
        let pipewire = Variant::from(2u32);
        let output = Variant::from("streaming");
        let rev3 = Variant::from(3u32);
        // In GetAll order whatever it is.
        let snap = Snapshot::from_properties([
            ("Consumers", &total),
            ("PipeWireOutput", &output),
            ("State", &state),
            ("PipeWireConsumers", &pipewire),
            ("InterfaceRevision", &rev3),
            ("LoopbackConsumers", &loopback),
        ])
        .unwrap();
        assert_eq!(
            (
                snap.consumers,
                snap.loopback_consumers,
                snap.pipewire_consumers,
                snap.pipewire_output.as_str()
            ),
            (3, 1, 2, "streaming")
        );
        // A change of the total leaves the split to its own properties.
        let mut changed = snap.clone();
        changed
            .apply_property("Consumers", &Variant::from(2u32))
            .unwrap();
        changed
            .apply_property("PipeWireConsumers", &Variant::from(1u32))
            .unwrap();
        changed
            .apply_property("PipeWireOutput", &Variant::from("ready"))
            .unwrap();
        assert_eq!(
            (
                changed.consumers,
                changed.loopback_consumers,
                changed.pipewire_consumers,
                changed.pipewire_output.as_str()
            ),
            (2, 1, 1, "ready")
        );
        assert!(changed.apply_property("PipeWireOutput", &total).is_err());
        assert!(
            changed
                .apply_property("LoopbackConsumers", &output)
                .is_err()
        );
        assert!(
            changed
                .apply_property("PipeWireConsumers", &output)
                .is_err()
        );

        // A revision-2 daemon: Consumers counts the loopback only, before
        // and after InterfaceRevision in the reply, and in later changes.
        let rev2 = Variant::from(2u32);
        for props in [
            [("Consumers", &total), ("InterfaceRevision", &rev2)],
            [("InterfaceRevision", &rev2), ("Consumers", &total)],
        ] {
            let snap =
                Snapshot::from_properties([("State", &state)].into_iter().chain(props)).unwrap();
            assert_eq!(snap.loopback_consumers, 3);
            assert_eq!(snap.pipewire_consumers, 0);
            assert_eq!(snap.pipewire_output, PIPEWIRE_OFF);
            let mut changed = snap.clone();
            changed
                .apply_property("Consumers", &Variant::from(1u32))
                .unwrap();
            assert_eq!((changed.consumers, changed.loopback_consumers), (1, 1));
        }
        assert_eq!(Snapshot::empty().pipewire_output, "off");
    }

    #[test]
    fn the_revision_4_property_and_an_older_daemon() {
        let state = Variant::from("idle");
        let on = Variant::from(true);
        let rev4 = Variant::from(DENOISE_REVISION);
        let snap = Snapshot::from_properties([
            ("State", &state),
            ("Denoise", &on),
            ("InterfaceRevision", &rev4),
        ])
        .unwrap();
        assert!(snap.denoise);
        let mut changed = snap.clone();
        changed
            .apply_property("Denoise", &Variant::from(false))
            .unwrap();
        assert!(!changed.denoise);
        assert!(changed.apply_property("Denoise", &state).is_err());
        // A revision-3 daemon has no Denoise: false, told apart by the
        // revision.
        let rev3 = Variant::from(3u32);
        let old =
            Snapshot::from_properties([("State", &state), ("InterfaceRevision", &rev3)]).unwrap();
        assert!(!old.denoise);
        assert!(old.interface_revision < DENOISE_REVISION);
        assert!(!Snapshot::empty().denoise);
    }

    #[test]
    fn the_revision_5_property_and_an_older_daemon() {
        let state = Variant::from("idle");
        let total = Variant::from(3u32);
        let leases = Variant::from(2u32);
        let rev5 = Variant::from(PREVIEW_REVISION);
        let snap = Snapshot::from_properties([
            ("State", &state),
            ("Consumers", &total),
            ("PreviewConsumers", &leases),
            ("LoopbackConsumers", &Variant::from(1u32)),
            ("InterfaceRevision", &rev5),
        ])
        .unwrap();
        assert_eq!(
            (
                snap.consumers,
                snap.loopback_consumers,
                snap.pipewire_consumers,
                snap.preview_consumers
            ),
            (3, 1, 0, 2)
        );
        let mut changed = snap.clone();
        changed
            .apply_property("PreviewConsumers", &Variant::from(0u32))
            .unwrap();
        assert_eq!(changed.preview_consumers, 0);
        assert!(changed.apply_property("PreviewConsumers", &state).is_err());
        // A revision-4 daemon has none: 0, told apart by the revision.
        let rev4 = Variant::from(DENOISE_REVISION);
        let old =
            Snapshot::from_properties([("State", &state), ("InterfaceRevision", &rev4)]).unwrap();
        assert_eq!(old.preview_consumers, 0);
        assert!(old.interface_revision < PREVIEW_REVISION);
        assert_eq!(Snapshot::empty().preview_consumers, 0);
        assert_eq!(INTERFACE_REVISION, PREVIEW_REVISION);
        assert_eq!(PREVIEW_FPS, [15, 30]);
    }

    #[test]
    fn unknown_property_replies_are_recognised() {
        use zbus::fdo;
        let fdo_error = |e: fdo::Error| zbus::Error::FDO(Box::new(e));
        assert!(is_unknown_property_error(&fdo_error(
            fdo::Error::UnknownProperty("BlurStrength".into())
        )));
        assert!(is_unknown_property_error(&fdo_error(
            fdo::Error::InvalidArgs("no such property".into())
        )));
        assert!(!is_unknown_property_error(&fdo_error(
            fdo::Error::AccessDenied("no".into())
        )));
        assert!(!is_unknown_property_error(&fdo_error(
            fdo::Error::NameHasNoOwner("gone".into())
        )));
        assert!(!is_unknown_property_error(&zbus::Error::Failure(
            "x".into()
        )));
        assert_eq!(optional(Ok(1), 0).unwrap(), 1);
        assert_eq!(
            optional(Err(fdo_error(fdo::Error::UnknownProperty("x".into()))), 7).unwrap(),
            7
        );
        assert!(optional(Err(fdo_error(fdo::Error::AccessDenied("x".into()))), 7).is_err());
    }

    #[test]
    fn absent_names_mean_not_running() {
        use zbus::fdo;
        let fdo_error = |e: fdo::Error| zbus::Error::FDO(Box::new(e));
        assert!(is_not_running_error(&fdo_error(
            fdo::Error::NameHasNoOwner("gone".into())
        )));
        assert!(is_not_running_error(&fdo_error(
            fdo::Error::ServiceUnknown("not activatable".into())
        )));
        assert!(!is_not_running_error(&fdo_error(fdo::Error::AccessDenied(
            "no".into()
        ))));
        assert!(!is_not_running_error(&fdo_error(
            fdo::Error::UnknownObject("no object".into())
        )));
        assert!(!is_not_running_error(&zbus::Error::Failure("x".into())));
    }

    #[test]
    fn stats_default_is_not_measurable() {
        let s = Stats::default();
        assert!(!Stats::is_measured(s.fps));
        assert!(Stats::is_measured(0.0));
    }
}
