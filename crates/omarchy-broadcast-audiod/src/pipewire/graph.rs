//! What the worker knows of the PipeWire graph (nodes, links, the
//! `default` metadata), and the decisions taken on it: the device list, the
//! consumers of the microphone, and the metadata writes of "use as default
//! microphone", its restore and the pin of the chosen device. Pure bookkeeping, fed by the registry and
//! metadata listeners of [`super::live`], so that every decision is tested
//! without PipeWire.
//!
//! # Default source and capture pinning
//!
//! The default source is WirePlumber's `default.configured.audio.source`
//! (`{"name": <node.name>}`, `Spa:String:JSON`) in the `default` metadata.
//! The capture side of the mic host (`omarchy-broadcast-mic.capture`) is
//! pinned with `target.object` on its node id, valued with the target's
//! `object.serial` (`Spa:Id`). WirePlumber's restore-stream stores that
//! target by the stream's key (`Input/Audio:media.name:Omarchy Broadcast
//! Mic`) in its own state and sets it again on every new capture node, so
//! the pin survives a restart of the mic host, of the worker and of
//! WirePlumber, and no node is ever removed or recreated for it. It is
//! stored only when the value is a serial (a `node.name` value links, but
//! is not remembered), and it is forgotten when the key is deleted (not
//! set to `-1`, which means "link to nothing").

use std::collections::{BTreeMap, BTreeSet};

use super::{AlsaSource, AlsaSources, Consumers, PipeWireError, SourceDevice};

/// The `node.name` of the microphone (the mic host's playback side).
pub const MIC_NODE: &str = "omarchy-broadcast-mic";

/// The `node.name` of the mic host's capture side.
pub const CAPTURE_NODE: &str = "omarchy-broadcast-mic.capture";

/// The longest device list.
pub const MAX_DEVICES: usize = 16;

/// The user's default source in the `default` metadata.
pub const CONFIGURED_SOURCE_KEY: &str = "default.configured.audio.source";

/// The default source in force (WirePlumber's choice).
pub const SOURCE_KEY: &str = "default.audio.source";

/// The key that pins a stream to a node.
pub const TARGET_KEY: &str = "target.object";

/// The type of a default-node value.
pub const JSON_TYPE: &str = "Spa:String:JSON";

/// The type of a `target.object` serial.
pub const ID_TYPE: &str = "Spa:Id";

/// The reason when the mic host is not running.
pub const REASON_NO_MIC_HOST: &str =
    "Omarchy Broadcast Mic is not running (its capture node is not in PipeWire)";

/// One node of the registry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeInfo {
    /// `node.name`.
    pub name: String,
    /// `node.description` (else `node.nick`, else the name).
    pub description: String,
    /// `media.class`.
    pub media_class: String,
    /// `object.serial`.
    pub serial: Option<String>,
    /// `priority.session` (0 when absent or not a number): the rank
    /// WirePlumber gives a node when no history decides.
    pub priority: i64,
    /// Whether it is a monitor stream (`stream.monitor` or
    /// `resample.peaks`: a level meter): `None` until its properties are
    /// known.
    pub monitor: Option<bool>,
    /// Its ALSA side: `None` until its properties are known (only a
    /// device's are read).
    pub alsa: Option<AlsaProps>,
}

/// The ALSA side of a node, from its properties.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AlsaProps {
    /// The card index: `api.alsa.pcm.card`, else `alsa.card` (digits only).
    pub card: Option<String>,
    /// A USB audio device: `alsa.driver_name` is `snd_usb_audio`, or
    /// `device.bus` is `usb`.
    pub usb: bool,
}

impl AlsaProps {
    /// The ALSA side of a node whose property `key` is `get(key)`.
    pub fn of<'a>(get: impl Fn(&str) -> Option<&'a str>) -> AlsaProps {
        let card = ["api.alsa.pcm.card", "alsa.card"]
            .into_iter()
            .filter_map(&get)
            .map(str::trim)
            .find(|c| !c.is_empty() && c.len() <= 4 && c.bytes().all(|b| b.is_ascii_digit()))
            .map(str::to_string);
        let usb =
            get("alsa.driver_name") == Some("snd_usb_audio") || get("device.bus") == Some("usb");
        AlsaProps { card, usb }
    }
}

impl NodeInfo {
    /// Whether it is a source a user may record from: `Audio/Source` or
    /// `Audio/Source/Virtual`, not ours, not a monitor.
    fn is_device(&self) -> bool {
        matches!(
            self.media_class.as_str(),
            "Audio/Source" | "Audio/Source/Virtual"
        ) && !self.name.starts_with(MIC_NODE)
            && !self.name.ends_with(".monitor")
    }
}

/// Whether a node's properties make it a monitor stream.
pub fn is_monitor(stream_monitor: Option<&str>, resample_peaks: Option<&str>) -> bool {
    let yes = |v: Option<&str>| v.is_some_and(|v| v == "true" || v == "1");
    yes(stream_monitor) || yes(resample_peaks)
}

/// A write to the `default` metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetaWrite {
    /// Sets `key` of `subject`.
    Set {
        subject: u32,
        key: String,
        type_: String,
        value: String,
    },
    /// Deletes `key` of `subject`.
    Delete { subject: u32, key: String },
}

/// What [`Graph::make_default`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MadeDefault {
    /// The previous default to remember (empty when none).
    pub previous: String,
    /// The real source in force before the writes
    /// ([`Graph::source_in_force`]), to record beside `previous`.
    pub in_force: Option<String>,
    /// Ours was the configured default already, before the writes: then
    /// `previous` is the pin in force, not the default the user had, and a
    /// record of an earlier "use as default microphone" is to be kept.
    pub already: bool,
    /// The writes, in the order to execute them.
    pub writes: Vec<MetaWrite>,
}

/// The graph. See the module documentation.
#[derive(Debug, Clone, Default)]
pub struct Graph {
    nodes: BTreeMap<u32, NodeInfo>,
    /// Link id: (output node, input node).
    links: BTreeMap<u32, (u32, u32)>,
    /// (subject, key): (type, value).
    meta: BTreeMap<(u32, String), (Option<String>, String)>,
}

/// The `name` of a default-node value (`{"name": "..."}`).
pub fn name_of_json(value: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(value)
        .ok()?
        .get("name")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// A default-node value for `name`.
pub fn json_of_name(name: &str) -> String {
    serde_json::json!({ "name": name }).to_string()
}

impl Graph {
    /// A node appeared (or its properties changed: the monitor flag known
    /// is kept unless given).
    pub fn add_node(&mut self, id: u32, mut info: NodeInfo) {
        if info.monitor.is_none() {
            info.monitor = self.nodes.get(&id).and_then(|n| n.monitor);
        }
        if info.alsa.is_none() {
            info.alsa = self.nodes.get(&id).and_then(|n| n.alsa.clone());
        }
        self.nodes.insert(id, info);
    }

    /// The properties of node `id` give its ALSA side.
    pub fn set_alsa(&mut self, id: u32, alsa: AlsaProps) {
        if let Some(n) = self.nodes.get_mut(&id) {
            n.alsa = Some(alsa);
        }
    }

    /// Whether node `id` is a source a user may record from (whose
    /// properties, its ALSA side, it is bound for).
    pub fn is_device(&self, id: u32) -> bool {
        self.nodes.get(&id).is_some_and(NodeInfo::is_device)
    }

    /// The device the mic host's capture side is linked from (by the
    /// lowest link id), when it runs and is linked to one.
    pub fn captured(&self) -> Option<(u32, &NodeInfo)> {
        let (capture, _) = self.node_by_name(CAPTURE_NODE)?;
        self.links
            .values()
            .filter(|(_, input)| *input == capture)
            .find_map(|(out, _)| {
                self.nodes
                    .get(out)
                    .filter(|n| n.is_device())
                    .map(|n| (*out, n))
            })
    }

    /// The mic host's capture node: its id and `object.serial` (another
    /// node of the same name, as after a restart of the mic host or of
    /// PipeWire, differs in one of them); `None` while it is not there.
    pub fn capture_identity(&self) -> Option<(u32, Option<String>)> {
        self.node_by_name(CAPTURE_NODE)
            .map(|(id, n)| (id, n.serial.clone()))
    }

    /// The devices' ALSA side (of those whose properties are known) and
    /// what the mic host captures.
    pub fn alsa_sources(&self) -> AlsaSources {
        AlsaSources {
            capture: self.node_by_name(CAPTURE_NODE).map(|(id, _)| id),
            captured: self.captured().map(|(_, n)| n.name.clone()),
            sources: self
                .nodes
                .iter()
                .filter(|(_, n)| n.is_device())
                .filter_map(|(id, n)| {
                    let a = n.alsa.as_ref()?;
                    Some(AlsaSource {
                        id: *id,
                        name: n.name.clone(),
                        card: a.card.clone(),
                        usb: a.usb,
                    })
                })
                .collect(),
        }
    }

    /// The properties of node `id` say whether it is a monitor.
    pub fn set_monitor(&mut self, id: u32, monitor: bool) {
        if let Some(n) = self.nodes.get_mut(&id) {
            n.monitor = Some(monitor);
        }
    }

    /// A link appeared.
    pub fn add_link(&mut self, id: u32, output_node: u32, input_node: u32) {
        self.links.insert(id, (output_node, input_node));
    }

    /// A global was removed: a node (with its metadata), a link, or
    /// nothing known.
    pub fn remove(&mut self, id: u32) {
        self.nodes.remove(&id);
        self.links.remove(&id);
        self.meta.retain(|(s, _), _| *s != id);
    }

    /// A metadata property event: `key` `None` removes every property of
    /// `subject`, `value` `None` removes `key`.
    pub fn meta_event(
        &mut self,
        subject: u32,
        key: Option<&str>,
        type_: Option<&str>,
        value: Option<&str>,
    ) {
        match (key, value) {
            (None, _) => self.meta.retain(|(s, _), _| *s != subject),
            (Some(k), None) => {
                self.meta.remove(&(subject, k.to_string()));
            }
            (Some(k), Some(v)) => {
                self.meta.insert(
                    (subject, k.to_string()),
                    (type_.map(str::to_string), v.to_string()),
                );
            }
        }
    }

    /// The metadata went away (or the connection): forgets it.
    pub fn clear_meta(&mut self) {
        self.meta.clear();
    }

    /// Applies our own writes at once, so that a second request sees them
    /// before the server's echo.
    pub fn apply(&mut self, writes: &[MetaWrite]) {
        for w in writes {
            match w {
                MetaWrite::Set {
                    subject,
                    key,
                    type_,
                    value,
                } => self.meta_event(*subject, Some(key), Some(type_), Some(value)),
                MetaWrite::Delete { subject, key } => {
                    self.meta_event(*subject, Some(key), None, None)
                }
            }
        }
    }

    /// The node named `name`.
    pub fn node_by_name(&self, name: &str) -> Option<(u32, &NodeInfo)> {
        self.nodes
            .iter()
            .find(|(_, n)| n.name == name)
            .map(|(id, n)| (*id, n))
    }

    /// The node `id`.
    pub fn node(&self, id: u32) -> Option<&NodeInfo> {
        self.nodes.get(&id)
    }

    /// The sources a user may pick (see [`NodeInfo::is_device`]), in id
    /// order, at most [`MAX_DEVICES`].
    pub fn devices(&self) -> Vec<SourceDevice> {
        self.nodes
            .values()
            .filter(|n| n.is_device())
            .take(MAX_DEVICES)
            .map(|n| SourceDevice {
                name: n.name.clone(),
                description: n.description.clone(),
            })
            .collect()
    }

    /// The nodes the microphone feeds, by kind: a node linked from ours
    /// counts once however many links it has; one whose properties are not
    /// known yet counts as a real consumer.
    pub fn consumers(&self) -> Consumers {
        let Some((ours, _)) = self.node_by_name(MIC_NODE) else {
            return Consumers::default();
        };
        let inputs: BTreeSet<u32> = self
            .links
            .values()
            .filter(|(out, _)| *out == ours)
            .map(|(_, input)| *input)
            .collect();
        let mut c = Consumers::default();
        for id in inputs {
            if self.nodes.get(&id).and_then(|n| n.monitor) == Some(true) {
                c.monitors += 1;
            } else {
                c.real += 1;
            }
        }
        c
    }

    fn meta_value(&self, subject: u32, key: &str) -> Option<&str> {
        self.meta
            .get(&(subject, key.to_string()))
            .map(|(_, v)| v.as_str())
    }

    /// The user's default source, else the one in force.
    pub fn default_source(&self) -> Option<String> {
        self.meta_value(0, CONFIGURED_SOURCE_KEY)
            .and_then(name_of_json)
            .or_else(|| self.meta_value(0, SOURCE_KEY).and_then(name_of_json))
    }

    /// The user's configured default source.
    pub fn configured_source(&self) -> Option<String> {
        self.meta_value(0, CONFIGURED_SOURCE_KEY)
            .and_then(name_of_json)
    }

    /// The node the capture side is pinned to (its `node.name`), when the
    /// capture node and its pin are known.
    pub fn pin(&self) -> Option<String> {
        let (capture, _) = self.node_by_name(CAPTURE_NODE)?;
        let value = self.meta_value(capture, TARGET_KEY)?;
        self.nodes
            .values()
            .find(|n| n.serial.as_deref() == Some(value) || n.name == value)
            .map(|n| n.name.clone())
    }

    /// The `object.serial` of the device named `name`, when it is connected
    /// (see [`NodeInfo::is_device`]).
    fn device_serial(&self, name: &str) -> Option<String> {
        self.node_by_name(name)
            .filter(|(_, n)| n.is_device())
            .and_then(|(_, n)| n.serial.clone())
    }

    /// The real default source in force: `default.audio.source` when it
    /// is a connected device, never ours (see [`NodeInfo::is_device`]).
    /// Read by [`Graph::make_default`] before its writes, it is the source
    /// the system used when ours was made the default (which, with the
    /// configured default unplugged, is not that one).
    pub fn source_in_force(&self) -> Option<String> {
        self.meta_value(0, SOURCE_KEY)
            .and_then(name_of_json)
            .filter(|n| self.device_serial(n).is_some())
    }

    /// `in_force` (the source recorded at "use as default microphone",
    /// [`MadeDefault::in_force`]) when it is a connected device and not
    /// `excluded`.
    fn recorded_in_force(&self, in_force: Option<&str>, excluded: &str) -> Option<String> {
        in_force
            .filter(|n| !n.is_empty() && *n != excluded && self.device_serial(n).is_some())
            .map(str::to_string)
    }

    /// The device to write under `previous` in WirePlumber's history on a
    /// restore, never ours (see [`NodeInfo::is_device`]):
    ///
    /// 1. `in_force`, the source in force recorded when ours was made the
    ///    default, when it is a connected device and not `previous`: with
    ///    `previous` unplugged, the system goes back to the source it was
    ///    using then;
    /// 2. else the connected device that WirePlumber itself would choose
    ///    with no history (the highest `priority.session`, the lowest node
    ///    id on a tie), `previous` and the device the capture side is
    ///    pinned to left out;
    /// 3. only when there is none, the pinned device, when it is connected
    ///    and not `previous`: a real source above ours in the history, which
    ///    with `previous` unplugged would otherwise become the default by
    ///    itself;
    /// 4. else `None`.
    ///
    /// The pinned device comes last because it is the one the user chose
    /// for the microphone, not for the system: with `previous` unplugged,
    /// WirePlumber would make it the default in `previous`'s place.
    fn fallback_source(&self, previous: &str, in_force: Option<&str>) -> Option<String> {
        if let Some(recorded) = self.recorded_in_force(in_force, previous) {
            return Some(recorded);
        }
        let pin = self.pin();
        self.wireplumber_choice(|name| name != previous && Some(name) != pin.as_deref())
            .or_else(|| pin.filter(|p| p != previous && self.device_serial(p).is_some()))
    }

    /// The connected device WirePlumber itself would choose with no
    /// history among those `name` allows: the highest `priority.session`,
    /// the lowest node id on a tie; never ours (see [`NodeInfo::is_device`]).
    fn wireplumber_choice(&self, allowed: impl Fn(&str) -> bool) -> Option<String> {
        self.nodes
            .values()
            .filter(|n| n.is_device() && allowed(&n.name))
            // `max_by_key` keeps the last of equal keys: reversed, the
            // first (lowest id) of the highest priority.
            .rev()
            .max_by_key(|n| n.priority)
            .map(|n| n.name.clone())
    }

    /// The device to pin the capture side to with `[mic] device = default`
    /// while ours is the default, as [`Graph::make_default`] pins: the
    /// previous default when it is a connected device; else the default
    /// in force when it is a connected device (never ours); else the
    /// device WirePlumber would choose ([`Graph::wireplumber_choice`]).
    /// `None` only with no device connected at all.
    fn default_pin(&self) -> Option<String> {
        let connected = |n: &String| self.device_serial(n).is_some();
        Some(self.previous_default())
            .filter(connected)
            .or_else(|| {
                self.meta_value(0, SOURCE_KEY)
                    .and_then(name_of_json)
                    .filter(connected)
            })
            .or_else(|| self.wireplumber_choice(|_| true))
    }

    /// The node id of the mic host's capture side, else
    /// [`REASON_NO_MIC_HOST`].
    fn capture_node(&self) -> Result<u32, PipeWireError> {
        self.node_by_name(CAPTURE_NODE)
            .map(|(id, _)| id)
            .ok_or_else(|| PipeWireError::Failed(REASON_NO_MIC_HOST.into()))
    }

    /// Refuses a `target` that is not empty and not a device listed now.
    fn check_target(&self, target: &str) -> Result<(), PipeWireError> {
        if !target.is_empty() && !self.devices().iter().any(|d| d.name == target) {
            return Err(PipeWireError::Invalid(format!(
                "{target} is not a microphone PipeWire knows now"
            )));
        }
        Ok(())
    }

    /// Whether ours is the configured default source.
    fn ours_is_configured(&self) -> bool {
        self.configured_source()
            .is_some_and(|c| c.starts_with(MIC_NODE))
    }

    /// Whether ours is the default source: the configured one, or the one
    /// in force (WirePlumber may fall back to ours by its history).
    fn ours_is_default(&self) -> bool {
        let ours = |key| {
            self.meta_value(0, key)
                .and_then(name_of_json)
                .is_some_and(|n| n.starts_with(MIC_NODE))
        };
        ours(CONFIGURED_SOURCE_KEY) || ours(SOURCE_KEY)
    }

    /// The default to remember on "use as default microphone": the user's
    /// configured one (else the one in force); when that is ours, the pin
    /// in force (empty when none).
    fn previous_default(&self) -> String {
        match self.default_source() {
            Some(c) if !c.starts_with(MIC_NODE) => c,
            _ => self.pin().unwrap_or_default(),
        }
    }

    /// The device the capture side is pinned to while ours is (or becomes)
    /// the default: `target` when given; else `previous` when it is a
    /// connected device (or empty: nothing to pin to); else the default in
    /// force when it is a connected device and not ours; else `previous`,
    /// which [`Graph::pin_serial`] then refuses.
    fn pin_choice(&self, target: &str, previous: &str) -> String {
        if !target.is_empty() {
            target.to_string()
        } else if previous.is_empty() || self.device_serial(previous).is_some() {
            previous.to_string()
        } else {
            self.meta_value(0, SOURCE_KEY)
                .and_then(name_of_json)
                .filter(|n| !n.starts_with(MIC_NODE) && self.device_serial(n).is_some())
                .unwrap_or_else(|| previous.to_string())
        }
    }

    /// The serial to pin the capture side to the device `pin` with; fails
    /// when `pin` is not connected.
    fn pin_serial(&self, pin: &str) -> Result<String, PipeWireError> {
        self.device_serial(pin).ok_or_else(|| {
            PipeWireError::Invalid(format!(
                "{pin} is not connected: the microphone cannot be pinned to it; \
                 choose one with `omarchy-broadcast mic device`"
            ))
        })
    }

    /// "Use as default microphone": the writes that make ours the default
    /// source and pin the capture side, the previous default to remember,
    /// and the real source in force before the writes
    /// ([`Graph::source_in_force`]; `None` when ours is the default in
    /// force already, so that ours is never recorded as it either).
    ///
    /// The previous default is the user's configured one (else the one in
    /// force), so that a restore writes back exactly what the user had; when
    /// ours is the default already, it is the pin in force (empty when
    /// none): ours is never returned as the previous one. `already` tells
    /// that ours was the configured default before the writes, so that the
    /// caller keeps the previous default it recorded at the first "use as
    /// default microphone" rather than the pin in force.
    ///
    /// The pin is `target` when given; else the previous default when it is
    /// a connected device; else the default in force (`default.audio.source`)
    /// when it is a connected device and not ours: a configured default that
    /// is unplugged (WirePlumber already fell back from it) does not stop the
    /// microphone from being made the default. Else, with a previous default
    /// to pin to, it fails; with none, nothing is pinned.
    pub fn make_default(&self, target: &str) -> Result<MadeDefault, PipeWireError> {
        let capture = self.capture_node()?;
        self.check_target(target)?;
        let previous = self.previous_default();
        let in_force = self.source_in_force();
        let already = self.ours_is_configured();
        let pin = self.pin_choice(target, &previous);
        // The pin first: by the time WirePlumber sees ours as the default,
        // the capture side is already pinned away from it (never a loop).
        let mut writes = Vec::new();
        if !pin.is_empty() {
            writes.push(MetaWrite::Set {
                subject: capture,
                key: TARGET_KEY.into(),
                type_: ID_TYPE.into(),
                value: self.pin_serial(&pin)?,
            });
        }
        writes.push(MetaWrite::Set {
            subject: 0,
            key: CONFIGURED_SOURCE_KEY.into(),
            type_: JSON_TYPE.into(),
            value: json_of_name(MIC_NODE),
        });
        Ok(MadeDefault {
            previous,
            in_force,
            already,
            writes,
        })
    }

    /// The `[mic] device` chosen takes effect: the writes that pin the
    /// capture side to `target` (a device's `node.name`; empty: the
    /// default), whether or not ours is the default. Never writes the
    /// configured default, and writes nothing when the pin is in force
    /// already.
    ///
    /// While ours is the default (configured, or in force): `target`; with
    /// none, `in_force` (the source in force recorded when ours was made
    /// the default, [`MadeDefault::in_force`]) when it is a connected
    /// device, else [`Graph::default_pin`] (the previous default, which
    /// with ours the configured default is the pin in force, else the
    /// default in force, else WirePlumber's choice, never ours); never
    /// unpinned (the capture side would follow the default, ours). While
    /// it is not: pinned to `target`, or, with none, unpinned (the key
    /// deleted when set), so that the capture side follows the system
    /// default; `in_force` is not read.
    ///
    /// Fails without the mic host, and when `target` is not connected;
    /// with none, never while a device is connected.
    pub fn pin_writes(
        &self,
        target: &str,
        in_force: Option<&str>,
    ) -> Result<Vec<MetaWrite>, PipeWireError> {
        let capture = self.capture_node()?;
        self.check_target(target)?;
        let pin = if !target.is_empty() || !self.ours_is_default() {
            target.to_string()
        } else {
            // The source the system used when ours was made the default,
            // when it is still there.
            self.recorded_in_force(in_force, "")
                .or_else(|| self.default_pin())
                .unwrap_or_default()
        };
        let current = self.meta_value(capture, TARGET_KEY);
        if !pin.is_empty() {
            let serial = self.pin_serial(&pin)?;
            if current == Some(serial.as_str()) {
                return Ok(Vec::new());
            }
            return Ok(vec![MetaWrite::Set {
                subject: capture,
                key: TARGET_KEY.into(),
                type_: ID_TYPE.into(),
                value: serial,
            }]);
        }
        if self.ours_is_default() || current.is_none() {
            return Ok(Vec::new());
        }
        Ok(vec![MetaWrite::Delete {
            subject: capture,
            key: TARGET_KEY.into(),
        }])
    }

    /// Restores the default source: `previous` again (deleted, so that
    /// WirePlumber picks one, when empty), only if ours is still the
    /// configured default; and the capture side unpinned. Never makes ours
    /// the default.
    ///
    /// WirePlumber keeps a history of configured defaults (each write of
    /// `default.configured.audio.source` moves that name to its top) and,
    /// when the configured node is absent, picks the available one highest
    /// in it. Writing `previous` alone would leave ours second, so that with
    /// `previous` unplugged ours would become the default by itself on its
    /// next start. So, before `previous`, another device is written (see
    /// [`Graph::fallback_source`]: `in_force`, the source in force recorded
    /// when ours was made the default, when it is connected and not
    /// `previous`; else another connected device, the pinned one only when
    /// there is none): the history becomes `previous`, that device, ours,
    /// and with `previous` unplugged the system uses that device. With no other device connected only `previous` is
    /// written, and ours stays second in the history: with `previous`
    /// unplugged and no other source there, ours can still become the
    /// default by itself. The writes are executed in the order returned.
    ///
    /// The unpin is undone by the caller when a device is chosen
    /// (`[mic] device`): see [`Graph::pin_writes`].
    pub fn restore_default(&self, previous: &str, in_force: Option<&str>) -> Vec<MetaWrite> {
        let mut writes = Vec::new();
        if self.ours_is_configured() {
            if previous.is_empty() || previous.starts_with(MIC_NODE) {
                writes.push(MetaWrite::Delete {
                    subject: 0,
                    key: CONFIGURED_SOURCE_KEY.into(),
                });
            } else {
                if let Some(other) = self.fallback_source(previous, in_force) {
                    writes.push(MetaWrite::Set {
                        subject: 0,
                        key: CONFIGURED_SOURCE_KEY.into(),
                        type_: JSON_TYPE.into(),
                        value: json_of_name(&other),
                    });
                }
                writes.push(MetaWrite::Set {
                    subject: 0,
                    key: CONFIGURED_SOURCE_KEY.into(),
                    type_: JSON_TYPE.into(),
                    value: json_of_name(previous),
                });
            }
        }
        if let Some((capture, _)) = self.node_by_name(CAPTURE_NODE)
            && self.meta_value(capture, TARGET_KEY).is_some()
        {
            writes.push(MetaWrite::Delete {
                subject: capture,
                key: TARGET_KEY.into(),
            });
        }
        writes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, class: &str, serial: u32) -> NodeInfo {
        NodeInfo {
            name: name.into(),
            description: format!("{name} desc"),
            media_class: class.into(),
            serial: Some(serial.to_string()),
            priority: 0,
            monitor: None,
            alsa: None,
        }
    }

    /// Two USB mics of equal priority, the mic host, a recorder.
    fn graph() -> Graph {
        let mut g = Graph::default();
        g.add_node(10, node("usb-a", "Audio/Source", 110));
        g.add_node(11, node("usb-b", "Audio/Source", 111));
        g.add_node(12, node("speakers", "Audio/Sink", 112));
        g.add_node(20, node(CAPTURE_NODE, "Stream/Input/Audio", 120));
        g.add_node(21, node(MIC_NODE, "Audio/Source", 121));
        g.meta_event(
            0,
            Some(CONFIGURED_SOURCE_KEY),
            Some(JSON_TYPE),
            Some(r#"{ "name": "usb-a" }"#),
        );
        g
    }

    #[test]
    fn devices_are_sources_not_ours_nor_monitors_at_most_sixteen() {
        let mut g = graph();
        g.add_node(13, node("virt", "Audio/Source/Virtual", 113));
        g.add_node(14, node("x.monitor", "Audio/Source", 114));
        g.add_node(15, node("omarchy-broadcast-mic-old", "Audio/Source", 115));
        let names: Vec<_> = g.devices().into_iter().map(|d| d.name).collect();
        assert_eq!(names, ["usb-a", "usb-b", "virt"]);
        assert_eq!(g.devices()[0].description, "usb-a desc");
        for i in 0..30 {
            g.add_node(100 + i, node(&format!("m{i}"), "Audio/Source", 200 + i));
        }
        assert_eq!(g.devices().len(), MAX_DEVICES);
    }

    #[test]
    fn make_default_pins_to_the_previous_default_and_returns_it() {
        let g = graph();
        let MadeDefault {
            previous, writes, ..
        } = g.make_default("").unwrap();
        assert_eq!(previous, "usb-a");
        // The pin before the default.
        assert_eq!(
            writes,
            [
                MetaWrite::Set {
                    subject: 20,
                    key: TARGET_KEY.into(),
                    type_: ID_TYPE.into(),
                    value: "110".into(),
                },
                MetaWrite::Set {
                    subject: 0,
                    key: CONFIGURED_SOURCE_KEY.into(),
                    type_: JSON_TYPE.into(),
                    value: r#"{"name":"omarchy-broadcast-mic"}"#.into(),
                },
            ]
        );
    }

    #[test]
    fn make_default_twice_never_records_ours_as_previous() {
        let mut g = graph();
        let MadeDefault { writes, .. } = g.make_default("usb-b").unwrap();
        g.apply(&writes);
        assert_eq!(g.pin().as_deref(), Some("usb-b"));
        let MadeDefault {
            previous, writes, ..
        } = g.make_default("").unwrap();
        // The pin in force, not ours.
        assert_eq!(previous, "usb-b");
        g.apply(&writes);
        assert_eq!(g.pin().as_deref(), Some("usb-b"));
        // Ours the default, no pin: nothing to remember, nothing pinned.
        g.apply(&g.restore_default("usb-a", None));
        g.meta_event(
            0,
            Some(CONFIGURED_SOURCE_KEY),
            Some(JSON_TYPE),
            Some(r#"{"name":"omarchy-broadcast-mic"}"#),
        );
        let MadeDefault {
            previous, writes, ..
        } = g.make_default("").unwrap();
        assert_eq!(previous, "");
        assert_eq!(writes.len(), 1);
    }

    #[test]
    fn make_default_falls_back_to_the_default_in_force() {
        let mut g = graph();
        g.meta_event(0, Some(CONFIGURED_SOURCE_KEY), None, None);
        g.meta_event(
            0,
            Some(SOURCE_KEY),
            Some(JSON_TYPE),
            Some(r#"{"name":"usb-b"}"#),
        );
        assert_eq!(g.make_default("").unwrap().previous, "usb-b");
    }

    #[test]
    fn make_default_with_an_unplugged_configured_default_pins_to_the_one_in_force() {
        let mut g = graph();
        g.meta_event(
            0,
            Some(CONFIGURED_SOURCE_KEY),
            Some(JSON_TYPE),
            Some(r#"{"name":"unplugged"}"#),
        );
        g.meta_event(
            0,
            Some(SOURCE_KEY),
            Some(JSON_TYPE),
            Some(r#"{"name":"usb-b"}"#),
        );
        let MadeDefault {
            previous, writes, ..
        } = g.make_default("").unwrap();
        // Remembered as the user had it; pinned to the one in force.
        assert_eq!(previous, "unplugged");
        assert_eq!(
            writes,
            [
                MetaWrite::Set {
                    subject: 20,
                    key: TARGET_KEY.into(),
                    type_: ID_TYPE.into(),
                    value: "111".into(),
                },
                MetaWrite::Set {
                    subject: 0,
                    key: CONFIGURED_SOURCE_KEY.into(),
                    type_: JSON_TYPE.into(),
                    value: r#"{"name":"omarchy-broadcast-mic"}"#.into(),
                },
            ]
        );
        // A connected previous default wins over the one in force.
        g.meta_event(
            0,
            Some(CONFIGURED_SOURCE_KEY),
            Some(JSON_TYPE),
            Some(r#"{"name":"usb-a"}"#),
        );
        let MadeDefault {
            previous, writes, ..
        } = g.make_default("").unwrap();
        assert_eq!(previous, "usb-a");
        assert!(matches!(&writes[0], MetaWrite::Set { value, .. } if value == "110"));
    }

    #[test]
    fn make_default_never_pins_to_ours_or_an_absent_default_in_force() {
        let mut g = graph();
        g.meta_event(
            0,
            Some(CONFIGURED_SOURCE_KEY),
            Some(JSON_TYPE),
            Some(r#"{"name":"unplugged"}"#),
        );
        for in_force in [MIC_NODE, "gone-too", "speakers"] {
            g.meta_event(
                0,
                Some(SOURCE_KEY),
                Some(JSON_TYPE),
                Some(&json_of_name(in_force)),
            );
            match g.make_default("") {
                Err(PipeWireError::Invalid(m)) => {
                    assert!(m.starts_with("unplugged "), "{m}");
                    assert!(m.ends_with("`omarchy-broadcast mic device`"), "{m}");
                }
                other => panic!("{in_force}: {other:?}"),
            }
        }
    }

    #[test]
    fn make_default_refuses_without_mic_host_or_with_an_unknown_target() {
        let mut g = graph();
        assert!(matches!(
            g.make_default("nope"),
            Err(PipeWireError::Invalid(_))
        ));
        assert!(matches!(
            g.make_default(MIC_NODE),
            Err(PipeWireError::Invalid(_))
        ));
        // The previous default is gone: it cannot be pinned to.
        g.meta_event(
            0,
            Some(CONFIGURED_SOURCE_KEY),
            Some(JSON_TYPE),
            Some(r#"{"name":"unplugged"}"#),
        );
        assert!(matches!(g.make_default(""), Err(PipeWireError::Invalid(_))));
        assert_eq!(g.make_default("usb-b").unwrap().previous, "unplugged");
        g.remove(20);
        assert!(matches!(g.make_default(""), Err(PipeWireError::Failed(_))));
    }

    #[test]
    fn restore_puts_the_previous_back_only_if_ours_and_unpins() {
        let mut g = graph();
        let MadeDefault {
            previous, writes, ..
        } = g.make_default("usb-b").unwrap();
        g.apply(&writes);
        let writes = g.restore_default(&previous, None);
        assert_eq!(
            writes,
            [
                // The pinned device under the previous one in WirePlumber's
                // history, above ours.
                MetaWrite::Set {
                    subject: 0,
                    key: CONFIGURED_SOURCE_KEY.into(),
                    type_: JSON_TYPE.into(),
                    value: r#"{"name":"usb-b"}"#.into(),
                },
                MetaWrite::Set {
                    subject: 0,
                    key: CONFIGURED_SOURCE_KEY.into(),
                    type_: JSON_TYPE.into(),
                    value: r#"{"name":"usb-a"}"#.into(),
                },
                MetaWrite::Delete {
                    subject: 20,
                    key: TARGET_KEY.into(),
                },
            ]
        );
        g.apply(&writes);
        assert_eq!(g.configured_source().as_deref(), Some("usb-a"));
        assert_eq!(g.pin(), None);
        // Again: nothing to do.
        assert!(g.restore_default(&previous, None).is_empty());

        // The user chose another default meanwhile: kept; still unpinned.
        let MadeDefault { writes, .. } = g.make_default("").unwrap();
        g.apply(&writes);
        g.meta_event(
            0,
            Some(CONFIGURED_SOURCE_KEY),
            Some(JSON_TYPE),
            Some(r#"{"name":"usb-b"}"#),
        );
        assert_eq!(
            g.restore_default("usb-a", None),
            [MetaWrite::Delete {
                subject: 20,
                key: TARGET_KEY.into(),
            }]
        );
        // Nothing remembered: WirePlumber picks.
        g.apply(&g.make_default("").unwrap().writes);
        assert_eq!(
            g.restore_default("", None)[0],
            MetaWrite::Delete {
                subject: 0,
                key: CONFIGURED_SOURCE_KEY.into(),
            }
        );
    }

    #[test]
    fn restore_writes_another_device_not_the_pinned_one_before_the_previous_one() {
        // The incident: the configured default unplugged, WirePlumber using
        // usb-a in its place, ours the default pinned to usb-b.
        let mut g = graph();
        g.meta_event(
            0,
            Some(CONFIGURED_SOURCE_KEY),
            Some(JSON_TYPE),
            Some(r#"{"name":"unplugged"}"#),
        );
        g.meta_event(
            0,
            Some(SOURCE_KEY),
            Some(JSON_TYPE),
            Some(r#"{"name":"usb-a"}"#),
        );
        let MadeDefault {
            previous, writes, ..
        } = g.make_default("usb-b").unwrap();
        g.apply(&writes);
        assert_eq!(
            (previous.as_str(), g.pin().as_deref()),
            ("unplugged", Some("usb-b"))
        );
        // WirePlumber's history becomes unplugged, usb-a, ours: with
        // "unplugged" absent usb-a is the default again, not the pinned
        // usb-b.
        assert_eq!(
            g.restore_default(&previous, None),
            [
                set_configured("usb-a"),
                set_configured("unplugged"),
                MetaWrite::Delete {
                    subject: 20,
                    key: TARGET_KEY.into(),
                },
            ]
        );
    }

    #[test]
    fn restore_writes_the_pinned_device_only_as_a_last_resort() {
        // The configured default unplugged: pinned to the one in force,
        // the only other device.
        let mut g = graph();
        g.remove(10);
        g.meta_event(
            0,
            Some(CONFIGURED_SOURCE_KEY),
            Some(JSON_TYPE),
            Some(r#"{"name":"unplugged"}"#),
        );
        g.meta_event(
            0,
            Some(SOURCE_KEY),
            Some(JSON_TYPE),
            Some(r#"{"name":"usb-b"}"#),
        );
        let MadeDefault {
            previous, writes, ..
        } = g.make_default("").unwrap();
        g.apply(&writes);
        let writes = g.restore_default(&previous, None);
        // In this order: WirePlumber's history becomes unplugged, usb-b,
        // ours; with "unplugged" absent, usb-b wins over ours.
        // Never ours, even with nothing else.
        assert_eq!(
            writes,
            [
                MetaWrite::Set {
                    subject: 0,
                    key: CONFIGURED_SOURCE_KEY.into(),
                    type_: JSON_TYPE.into(),
                    value: r#"{"name":"usb-b"}"#.into(),
                },
                MetaWrite::Set {
                    subject: 0,
                    key: CONFIGURED_SOURCE_KEY.into(),
                    type_: JSON_TYPE.into(),
                    value: r#"{"name":"unplugged"}"#.into(),
                },
                MetaWrite::Delete {
                    subject: 20,
                    key: TARGET_KEY.into(),
                },
            ]
        );
        g.apply(&writes);
        assert_eq!(g.configured_source().as_deref(), Some("unplugged"));
        // No device at all but ours: only the previous one.
        g.apply(&g.make_default("").unwrap().writes);
        g.remove(11);
        assert_eq!(g.fallback_source("unplugged", None), None);
    }

    fn set_configured(name: &str) -> MetaWrite {
        MetaWrite::Set {
            subject: 0,
            key: CONFIGURED_SOURCE_KEY.into(),
            type_: JSON_TYPE.into(),
            value: json_of_name(name),
        }
    }

    #[test]
    fn restore_pinned_to_the_previous_or_unpinned_writes_another_device_first() {
        let unpin = MetaWrite::Delete {
            subject: 20,
            key: TARGET_KEY.into(),
        };
        // Pinned to the previous one: the other device under it.
        let mut g = graph();
        let MadeDefault {
            previous, writes, ..
        } = g.make_default("").unwrap();
        g.apply(&writes);
        assert_eq!(g.pin().as_deref(), Some("usb-a"));
        assert_eq!(
            g.restore_default(&previous, None),
            [
                set_configured("usb-b"),
                set_configured("usb-a"),
                unpin.clone()
            ]
        );
        // No pin: the same.
        g.apply(std::slice::from_ref(&unpin));
        assert_eq!(g.pin(), None);
        assert_eq!(
            g.restore_default(&previous, None),
            [set_configured("usb-b"), set_configured("usb-a")]
        );
        // Pinned to a device that went away, the previous one the only
        // device left: the previous one alone.
        g.meta_event(20, Some(TARGET_KEY), Some(ID_TYPE), Some("111"));
        g.remove(11);
        assert_eq!(
            g.restore_default(&previous, None)[..1],
            [set_configured("usb-a")]
        );
        assert_eq!(
            g.restore_default(&previous, None)
                .iter()
                .filter(|w| matches!(w, MetaWrite::Set { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn restore_writes_only_the_previous_with_no_other_device() {
        let mut g = graph();
        g.remove(11);
        // Neither a sink, a monitor nor ours counts.
        g.add_node(14, node("x.monitor", "Audio/Source", 114));
        let MadeDefault {
            previous, writes, ..
        } = g.make_default("").unwrap();
        g.apply(&writes);
        assert_eq!(
            g.restore_default(&previous, None),
            [
                set_configured("usb-a"),
                MetaWrite::Delete {
                    subject: 20,
                    key: TARGET_KEY.into(),
                },
            ]
        );
    }

    #[test]
    fn restore_picks_the_highest_priority_device_then_the_lowest_id() {
        let mut g = graph();
        let mut n = node("usb-c", "Audio/Source", 113);
        n.priority = 2000;
        g.add_node(13, n);
        let mut n = node("virt", "Audio/Source/Virtual", 114);
        n.priority = 2000;
        g.add_node(14, n);
        // The previous one is never chosen, whatever its priority.
        let mut n = node("usb-a", "Audio/Source", 110);
        n.priority = 9000;
        g.add_node(10, n);
        let MadeDefault {
            previous, writes, ..
        } = g.make_default("").unwrap();
        g.apply(&writes);
        g.apply(&[MetaWrite::Delete {
            subject: 20,
            key: TARGET_KEY.into(),
        }]);
        assert_eq!(
            g.restore_default(&previous, None),
            [set_configured("usb-c"), set_configured("usb-a")]
        );
        // The pinned device is left out while another is there.
        let mut n = node("usb-b", "Audio/Source", 111);
        n.priority = 5000;
        g.add_node(11, n);
        g.meta_event(20, Some(TARGET_KEY), Some(ID_TYPE), Some("111"));
        assert_eq!(
            g.restore_default(&previous, None)[0],
            set_configured("usb-c")
        );
        g.remove(13);
        assert_eq!(
            g.restore_default(&previous, None)[0],
            set_configured("virt")
        );
        g.remove(14);
        assert_eq!(
            g.restore_default(&previous, None)[0],
            set_configured("usb-b")
        );
    }

    fn pin_to(serial: &str) -> MetaWrite {
        MetaWrite::Set {
            subject: 20,
            key: TARGET_KEY.into(),
            type_: ID_TYPE.into(),
            value: serial.into(),
        }
    }

    #[test]
    fn pin_writes_with_ours_not_the_default_pins_to_the_target_or_unpins() {
        let mut g = graph();
        // A device: pinned to it; the default untouched.
        let writes = g.pin_writes("usb-b", None).unwrap();
        assert_eq!(writes, [pin_to("111")]);
        g.apply(&writes);
        assert_eq!(g.pin().as_deref(), Some("usb-b"));
        assert_eq!(g.configured_source().as_deref(), Some("usb-a"));
        // Again: nothing to write.
        assert!(g.pin_writes("usb-b", None).unwrap().is_empty());
        // Another device: moved.
        assert_eq!(g.pin_writes("usb-a", None).unwrap(), [pin_to("110")]);
        // The default: unpinned, the capture side follows the system's.
        let writes = g.pin_writes("", None).unwrap();
        assert_eq!(
            writes,
            [MetaWrite::Delete {
                subject: 20,
                key: TARGET_KEY.into(),
            }]
        );
        g.apply(&writes);
        assert_eq!(g.pin(), None);
        assert!(g.pin_writes("", None).unwrap().is_empty());
        // No configured default at all: the same.
        g.meta_event(0, Some(CONFIGURED_SOURCE_KEY), None, None);
        assert_eq!(g.pin_writes("usb-b", None).unwrap(), [pin_to("111")]);
        assert!(g.pin_writes("", None).unwrap().is_empty());
    }

    #[test]
    fn pin_writes_with_ours_the_default_pins_as_make_default_never_unpins() {
        let mut g = graph();
        g.apply(&g.make_default("").unwrap().writes);
        assert_eq!(g.pin().as_deref(), Some("usb-a"));
        // A device: the pin moves, ours stays the default.
        let writes = g.pin_writes("usb-b", None).unwrap();
        assert_eq!(writes, [pin_to("111")]);
        g.apply(&writes);
        assert_eq!(g.configured_source().as_deref(), Some(MIC_NODE));
        // The default, nothing recorded: the pin in force is kept (the
        // previous default, with ours the configured one), never deleted.
        assert!(g.pin_writes("", None).unwrap().is_empty());
        assert_eq!(g.pin().as_deref(), Some("usb-b"));
        // Unpinned (by hand), ours the configured default and the one in
        // force: never left so (the capture side would follow ours), and
        // never deleted; WirePlumber's choice (the lowest id of equal
        // priority), as nothing else is known.
        g.meta_event(20, None, None, None);
        g.meta_event(
            0,
            Some(SOURCE_KEY),
            Some(JSON_TYPE),
            Some(&json_of_name(MIC_NODE)),
        );
        assert_eq!(g.pin_writes("", None).unwrap(), [pin_to("110")]);
        // The default in force a connected device: that one.
        g.meta_event(
            0,
            Some(SOURCE_KEY),
            Some(JSON_TYPE),
            Some(r#"{"name":"usb-b"}"#),
        );
        assert_eq!(g.pin_writes("", None).unwrap(), [pin_to("111")]);
        // Ours only in force (WirePlumber's history): the same rules. The
        // pin in force is not kept as such: with no record, the previous
        // default (unplugged here), else WirePlumber's choice; with one,
        // the recorded source.
        g.meta_event(
            0,
            Some(CONFIGURED_SOURCE_KEY),
            Some(JSON_TYPE),
            Some(r#"{"name":"unplugged"}"#),
        );
        g.meta_event(
            0,
            Some(SOURCE_KEY),
            Some(JSON_TYPE),
            Some(&json_of_name(MIC_NODE)),
        );
        g.meta_event(20, Some(TARGET_KEY), Some(ID_TYPE), Some("111"));
        assert_eq!(g.pin_writes("", None).unwrap(), [pin_to("110")]);
        assert!(g.pin_writes("", Some("usb-b")).unwrap().is_empty());
        assert_eq!(g.pin_writes("usb-a", None).unwrap(), [pin_to("110")]);
    }

    #[test]
    fn pin_writes_of_the_default_with_ours_the_default_follows_make_defaults_rule() {
        // Ours in force only, the configured default (the previous one)
        // connected: pinned to it.
        let mut g = graph();
        g.meta_event(
            0,
            Some(SOURCE_KEY),
            Some(JSON_TYPE),
            Some(&json_of_name(MIC_NODE)),
        );
        assert_eq!(g.pin_writes("", None).unwrap(), [pin_to("110")]);
        // Unplugged, the pin in force to it gone with it, the default in
        // force ours: WirePlumber's choice, the highest priority, never
        // ours, whatever its priority.
        g.meta_event(
            0,
            Some(CONFIGURED_SOURCE_KEY),
            Some(JSON_TYPE),
            Some(r#"{"name":"unplugged"}"#),
        );
        let mut n = node("usb-c", "Audio/Source", 113);
        n.priority = 2000;
        g.add_node(13, n);
        let mut ours = node(MIC_NODE, "Audio/Source", 121);
        ours.priority = 9000;
        g.add_node(21, ours);
        assert_eq!(g.pin_writes("", None).unwrap(), [pin_to("113")]);
        // No device at all but ours: nothing to pin to, nothing deleted,
        // no error.
        for id in [10, 11, 13] {
            g.remove(id);
        }
        assert_eq!(g.pin_writes("", None), Ok(Vec::new()));
    }

    #[test]
    fn make_default_reports_the_real_source_in_force_never_ours() {
        let mut g = graph();
        // Nothing in force known: none.
        assert_eq!(g.make_default("").unwrap().in_force, None);
        // The configured default unplugged, WirePlumber using usb-b.
        g.meta_event(
            0,
            Some(CONFIGURED_SOURCE_KEY),
            Some(JSON_TYPE),
            Some(r#"{"name":"unplugged"}"#),
        );
        g.meta_event(
            0,
            Some(SOURCE_KEY),
            Some(JSON_TYPE),
            Some(r#"{"name":"usb-b"}"#),
        );
        let made = g.make_default("usb-a").unwrap();
        assert_eq!(
            (made.previous.as_str(), made.in_force.as_deref()),
            ("unplugged", Some("usb-b"))
        );
        // Again, ours the default in force too: never ours.
        g.apply(&made.writes);
        g.meta_event(
            0,
            Some(SOURCE_KEY),
            Some(JSON_TYPE),
            Some(&json_of_name(MIC_NODE)),
        );
        assert_eq!(g.make_default("").unwrap().in_force, None);
        // Neither a sink nor a source that is not there.
        for in_force in ["speakers", "gone"] {
            g.meta_event(
                0,
                Some(SOURCE_KEY),
                Some(JSON_TYPE),
                Some(&json_of_name(in_force)),
            );
            assert_eq!(g.make_default("").unwrap().in_force, None, "{in_force}");
        }
    }

    #[test]
    fn pin_writes_of_the_default_with_ours_the_default_follows_the_recorded_source() {
        // Made the default with usb-b in force, then pinned to usb-a by
        // `[mic] device`.
        let mut g = graph();
        g.apply(&g.make_default("usb-a").unwrap().writes);
        assert_eq!(g.pin().as_deref(), Some("usb-a"));
        // `[mic] device = default`: the recorded source, not the pin in
        // force.
        assert_eq!(g.pin_writes("", Some("usb-b")).unwrap(), [pin_to("111")]);
        g.apply(&g.pin_writes("", Some("usb-b")).unwrap());
        assert!(g.pin_writes("", Some("usb-b")).unwrap().is_empty());
        // A device chosen wins over it.
        assert_eq!(
            g.pin_writes("usb-a", Some("usb-b")).unwrap(),
            [pin_to("110")]
        );
        // The recorded source unplugged: the rule without one (the
        // previous default, with ours the configured one the pin in
        // force, here usb-b's, gone with it; else the default in force;
        // else WirePlumber's choice).
        g.remove(11);
        g.meta_event(20, Some(TARGET_KEY), Some(ID_TYPE), Some("111"));
        assert_eq!(g.pin_writes("", Some("usb-b")).unwrap(), [pin_to("110")]);
        // Ours not the default: the record is not read, unpinned.
        g.meta_event(
            0,
            Some(CONFIGURED_SOURCE_KEY),
            Some(JSON_TYPE),
            Some(r#"{"name":"usb-a"}"#),
        );
        g.add_node(11, node("usb-b", "Audio/Source", 111));
        assert_eq!(
            g.pin_writes("", Some("usb-b")).unwrap(),
            [MetaWrite::Delete {
                subject: 20,
                key: TARGET_KEY.into(),
            }]
        );
    }

    /// The incident's graph: the configured C-Media unplugged, the laptop's
    /// DMIC in force, a C930e and a K66 of the highest priority.
    fn incident() -> Graph {
        let mut g = Graph::default();
        g.add_node(30, node("dmic", "Audio/Source", 130));
        g.add_node(31, node("c930e", "Audio/Source", 131));
        let mut k66 = node("k66", "Audio/Source", 132);
        k66.priority = 2010;
        g.add_node(32, k66);
        g.add_node(20, node(CAPTURE_NODE, "Stream/Input/Audio", 120));
        g.add_node(21, node(MIC_NODE, "Audio/Source", 121));
        g.meta_event(
            0,
            Some(CONFIGURED_SOURCE_KEY),
            Some(JSON_TYPE),
            Some(r#"{"name":"cmedia"}"#),
        );
        g.meta_event(
            0,
            Some(SOURCE_KEY),
            Some(JSON_TYPE),
            Some(r#"{"name":"dmic"}"#),
        );
        g
    }

    #[test]
    fn restore_writes_the_recorded_source_in_force_before_the_previous_one() {
        let mut g = incident();
        let MadeDefault {
            previous,
            in_force,
            writes,
            ..
        } = g.make_default("c930e").unwrap();
        assert_eq!(
            (previous.as_str(), in_force.as_deref()),
            ("cmedia", Some("dmic"))
        );
        g.apply(&writes);
        assert_eq!(g.pin().as_deref(), Some("c930e"));
        // WirePlumber's history becomes cmedia, dmic, ours: with cmedia
        // unplugged the DMIC is the default again, not the K66.
        assert_eq!(
            g.restore_default(&previous, in_force.as_deref()),
            [
                set_configured("dmic"),
                set_configured("cmedia"),
                MetaWrite::Delete {
                    subject: 20,
                    key: TARGET_KEY.into(),
                },
            ]
        );
        // Recorded as the previous one too (the usual case): as without a
        // record, WirePlumber's choice among the others.
        assert_eq!(
            g.restore_default("dmic", Some("dmic"))[0],
            set_configured("k66")
        );
        assert_eq!(g.restore_default("dmic", None)[0], set_configured("k66"));
        // Never ours, even recorded.
        assert_eq!(
            g.restore_default(&previous, Some(MIC_NODE))[0],
            set_configured("k66")
        );
    }

    /// The incident: made the default twice, then restored. The second
    /// make reports the pin in force and `already`, so the caller keeps the
    /// first record; restored with it, the DMIC goes under the C-Media in
    /// WirePlumber's history (not the C930e the capture side was pinned
    /// to).
    #[test]
    fn make_default_twice_then_restore_writes_the_first_record() {
        let mut g = incident();
        let first = g.make_default("c930e").unwrap();
        assert_eq!(
            (
                first.previous.as_str(),
                first.in_force.as_deref(),
                first.already
            ),
            ("cmedia", Some("dmic"), false)
        );
        g.apply(&first.writes);
        // WirePlumber follows: ours in force.
        g.meta_event(
            0,
            Some(SOURCE_KEY),
            Some(JSON_TYPE),
            Some(&json_of_name(MIC_NODE)),
        );
        let again = g.make_default("c930e").unwrap();
        // The pin in force, ours the configured default already.
        assert_eq!(
            (
                again.previous.as_str(),
                again.in_force.as_deref(),
                again.already
            ),
            ("c930e", None, true)
        );
        g.apply(&again.writes);
        assert_eq!(
            g.restore_default(&first.previous, first.in_force.as_deref()),
            [
                set_configured("dmic"),
                set_configured("cmedia"),
                MetaWrite::Delete {
                    subject: 20,
                    key: TARGET_KEY.into(),
                },
            ]
        );
        // Restored, ours is no longer the configured default: a make is
        // not `already` again.
        g.apply(&g.restore_default(&first.previous, first.in_force.as_deref()));
        assert!(!g.make_default("c930e").unwrap().already);
    }

    #[test]
    fn restore_with_the_recorded_source_gone_falls_back_by_priority() {
        let mut g = incident();
        let made = g.make_default("c930e").unwrap();
        g.apply(&made.writes);
        g.remove(30);
        assert_eq!(
            g.restore_default(&made.previous, made.in_force.as_deref())[..2],
            [set_configured("k66"), set_configured("cmedia")]
        );
        // The pinned device only as a last resort, as without a record.
        g.remove(32);
        assert_eq!(
            g.restore_default(&made.previous, made.in_force.as_deref())[0],
            set_configured("c930e")
        );
    }

    #[test]
    fn the_capture_identity_is_the_capture_nodes_id_and_serial() {
        let mut g = graph();
        assert_eq!(g.capture_identity(), Some((20, Some("120".into()))));
        g.remove(20);
        assert_eq!(g.capture_identity(), None);
        g.add_node(20, node(CAPTURE_NODE, "Stream/Input/Audio", 130));
        assert_eq!(g.capture_identity(), Some((20, Some("130".into()))));
    }

    #[test]
    fn pin_writes_refuses_an_unconnected_target_or_no_mic_host() {
        let mut g = graph();
        for target in ["nope", MIC_NODE, "speakers"] {
            assert!(
                matches!(g.pin_writes(target, None), Err(PipeWireError::Invalid(_))),
                "{target}"
            );
        }
        g.remove(20);
        assert!(matches!(
            g.pin_writes("usb-a", None),
            Err(PipeWireError::Failed(_))
        ));
        assert!(matches!(
            g.pin_writes("", None),
            Err(PipeWireError::Failed(_))
        ));
    }

    #[test]
    fn consumers_by_node_monitors_apart_unknown_counts_as_real() {
        let mut g = graph();
        assert_eq!(g.consumers(), Consumers::default());
        // A stereo recorder: two links, one consumer.
        g.add_node(30, node("rec", "Stream/Input/Audio", 130));
        g.add_link(40, 21, 30);
        g.add_link(41, 21, 30);
        // A level meter.
        g.add_node(31, node("meter", "Stream/Input/Audio", 131));
        g.add_link(42, 21, 31);
        // Into the capture side: not a consumer of ours.
        g.add_link(43, 10, 20);
        assert_eq!(
            g.consumers(),
            Consumers {
                real: 2,
                monitors: 0
            }
        );
        g.set_monitor(31, true);
        g.set_monitor(30, false);
        assert_eq!(
            g.consumers(),
            Consumers {
                real: 1,
                monitors: 1
            }
        );
        // New properties keep the known flag.
        g.add_node(31, node("meter", "Stream/Input/Audio", 131));
        assert_eq!(g.consumers().monitors, 1);
        g.remove(40);
        assert_eq!(g.consumers().real, 1);
        g.remove(30);
        g.remove(41);
        assert_eq!(
            g.consumers(),
            Consumers {
                real: 0,
                monitors: 1
            }
        );
        // The mic host goes: nobody.
        g.remove(21);
        assert_eq!(g.consumers(), Consumers::default());
    }

    #[test]
    fn monitor_properties() {
        assert!(is_monitor(Some("true"), None));
        assert!(is_monitor(None, Some("true")));
        assert!(!is_monitor(Some("false"), None));
        assert!(!is_monitor(None, None));
    }

    #[test]
    fn metadata_events_and_removal() {
        let mut g = graph();
        g.meta_event(20, Some(TARGET_KEY), Some(ID_TYPE), Some("111"));
        assert_eq!(g.pin().as_deref(), Some("usb-b"));
        g.meta_event(20, None, None, None);
        assert_eq!(g.pin(), None);
        g.meta_event(20, Some(TARGET_KEY), Some(ID_TYPE), Some("111"));
        // The capture node goes: its metadata with it.
        g.remove(20);
        g.add_node(20, node(CAPTURE_NODE, "Stream/Input/Audio", 150));
        assert_eq!(g.pin(), None);
        g.clear_meta();
        assert_eq!(g.default_source(), None);
        assert_eq!(name_of_json("{}"), None);
        assert_eq!(name_of_json("x"), None);
    }

    #[test]
    fn the_alsa_side_is_read_from_the_properties() {
        let props = |pairs: &'static [(&'static str, &'static str)]| {
            AlsaProps::of(|k| pairs.iter().find(|(key, _)| *key == k).map(|(_, v)| *v))
        };
        assert_eq!(
            props(&[
                ("api.alsa.pcm.card", "0"),
                ("alsa.card", "7"),
                ("alsa.driver_name", "snd_usb_audio")
            ]),
            AlsaProps {
                card: Some("0".into()),
                usb: true
            }
        );
        assert_eq!(
            props(&[("alsa.card", "3"), ("device.bus", "usb")]),
            AlsaProps {
                card: Some("3".into()),
                usb: true
            }
        );
        // A PCI card, a card that is not a number, no ALSA at all.
        assert_eq!(
            props(&[("api.alsa.pcm.card", "1"), ("device.bus", "pci")]),
            AlsaProps {
                card: Some("1".into()),
                usb: false
            }
        );
        assert_eq!(props(&[("api.alsa.pcm.card", "hw:0")]).card, None);
        assert_eq!(props(&[]), AlsaProps::default());
    }

    #[test]
    fn the_captured_device_is_the_capture_sides_link_source() {
        let mut g = graph();
        g.set_alsa(
            10,
            AlsaProps {
                card: Some("0".into()),
                usb: true,
            },
        );
        // Kept when the node's registry entry is seen again.
        g.add_node(10, node("usb-a", "Audio/Source", 110));
        let s = g.alsa_sources();
        assert_eq!(s.capture, Some(20));
        assert_eq!(s.captured, None, "not linked yet");
        assert_eq!(
            s.sources,
            [AlsaSource {
                id: 10,
                name: "usb-a".into(),
                card: Some("0".into()),
                usb: true
            }],
            "usb-b's properties are not known yet"
        );
        // The mic node feeding a recorder is not what the capture side
        // captures; the links into the capture node are.
        g.add_node(30, node("rec", "Stream/Input/Audio", 130));
        g.add_link(40, 21, 30);
        g.add_link(41, 11, 20);
        g.add_link(42, 11, 20);
        assert_eq!(g.alsa_sources().captured.as_deref(), Some("usb-b"));
        assert!(g.is_device(11) && !g.is_device(20) && !g.is_device(21));
        g.remove(41);
        g.remove(42);
        g.remove(20);
        let s = g.alsa_sources();
        assert_eq!((s.capture, s.captured), (None, None));
    }
}
