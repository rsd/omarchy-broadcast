//! What the worker knows of the PipeWire graph (nodes, links, the
//! `default` metadata), and the decisions taken on it: the device list, the
//! consumers of the microphone, and the metadata writes of "use as default
//! microphone" and its restore. Pure bookkeeping, fed by the registry and
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

use super::{Consumers, PipeWireError, SourceDevice};

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
        self.nodes.insert(id, info);
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

    /// The device to write under `previous` in WirePlumber's history on a
    /// restore: the one the capture side is pinned to, when it is a
    /// connected device other than `previous`; else the connected device
    /// other than `previous` that WirePlumber itself would choose with no
    /// history (the highest `priority.session`, the lowest node id on a
    /// tie). `None` when there is no such device.
    fn fallback_source(&self, previous: &str) -> Option<String> {
        self.pin()
            .filter(|p| p != previous && self.device_serial(p).is_some())
            .or_else(|| {
                self.nodes
                    .values()
                    .filter(|n| n.is_device() && n.name != previous)
                    // `max_by_key` keeps the last of equal keys: reversed,
                    // the first (lowest id) of the highest priority.
                    .rev()
                    .max_by_key(|n| n.priority)
                    .map(|n| n.name.clone())
            })
    }

    /// "Use as default microphone": the writes that make ours the default
    /// source and pin the capture side, and the previous default to
    /// remember.
    ///
    /// The previous default is the user's configured one (else the one in
    /// force), so that a restore writes back exactly what the user had; when
    /// ours is the default already, it is the pin in force (empty when
    /// none): ours is never returned as the previous one.
    ///
    /// The pin is `target` when given; else the previous default when it is
    /// a connected device; else the default in force (`default.audio.source`)
    /// when it is a connected device and not ours: a configured default that
    /// is unplugged (WirePlumber already fell back from it) does not stop the
    /// microphone from being made the default. Else, with a previous default
    /// to pin to, it fails; with none, nothing is pinned.
    pub fn make_default(&self, target: &str) -> Result<(String, Vec<MetaWrite>), PipeWireError> {
        let (capture, _) = self
            .node_by_name(CAPTURE_NODE)
            .ok_or_else(|| PipeWireError::Failed(REASON_NO_MIC_HOST.into()))?;
        if !target.is_empty() && !self.devices().iter().any(|d| d.name == target) {
            return Err(PipeWireError::Invalid(format!(
                "{target} is not a microphone PipeWire knows now"
            )));
        }
        let current = self.default_source();
        let previous = match current {
            Some(c) if !c.starts_with(MIC_NODE) => c,
            _ => self.pin().unwrap_or_default(),
        };
        let pin = if !target.is_empty() {
            target.to_string()
        } else if previous.is_empty() || self.device_serial(&previous).is_some() {
            previous.clone()
        } else {
            self.meta_value(0, SOURCE_KEY)
                .and_then(name_of_json)
                .filter(|n| !n.starts_with(MIC_NODE) && self.device_serial(n).is_some())
                .unwrap_or_else(|| previous.clone())
        };
        // The pin first: by the time WirePlumber sees ours as the default,
        // the capture side is already pinned away from it (never a loop).
        let mut writes = Vec::new();
        if !pin.is_empty() {
            let serial = self.device_serial(&pin).ok_or_else(|| {
                PipeWireError::Invalid(format!(
                    "{pin} is not connected: the microphone cannot be pinned to it; \
                     choose one with `omarchy-broadcast mic device`"
                ))
            })?;
            writes.push(MetaWrite::Set {
                subject: capture,
                key: TARGET_KEY.into(),
                type_: ID_TYPE.into(),
                value: serial,
            });
        }
        writes.push(MetaWrite::Set {
            subject: 0,
            key: CONFIGURED_SOURCE_KEY.into(),
            type_: JSON_TYPE.into(),
            value: json_of_name(MIC_NODE),
        });
        Ok((previous, writes))
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
    /// [`Graph::fallback_source`]): the history becomes `previous`, that
    /// device, ours. With no other device connected only `previous` is
    /// written, and ours stays second in the history: with `previous`
    /// unplugged and no other source there, ours can still become the
    /// default by itself. The writes are executed in the order returned.
    pub fn restore_default(&self, previous: &str) -> Vec<MetaWrite> {
        let mut writes = Vec::new();
        let ours = self
            .configured_source()
            .is_some_and(|c| c.starts_with(MIC_NODE));
        if ours {
            if previous.is_empty() || previous.starts_with(MIC_NODE) {
                writes.push(MetaWrite::Delete {
                    subject: 0,
                    key: CONFIGURED_SOURCE_KEY.into(),
                });
            } else {
                if let Some(other) = self.fallback_source(previous) {
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
        let (previous, writes) = g.make_default("").unwrap();
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
        let (_, writes) = g.make_default("usb-b").unwrap();
        g.apply(&writes);
        assert_eq!(g.pin().as_deref(), Some("usb-b"));
        let (previous, writes) = g.make_default("").unwrap();
        // The pin in force, not ours.
        assert_eq!(previous, "usb-b");
        g.apply(&writes);
        assert_eq!(g.pin().as_deref(), Some("usb-b"));
        // Ours the default, no pin: nothing to remember, nothing pinned.
        g.apply(&g.restore_default("usb-a"));
        g.meta_event(
            0,
            Some(CONFIGURED_SOURCE_KEY),
            Some(JSON_TYPE),
            Some(r#"{"name":"omarchy-broadcast-mic"}"#),
        );
        let (previous, writes) = g.make_default("").unwrap();
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
        assert_eq!(g.make_default("").unwrap().0, "usb-b");
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
        let (previous, writes) = g.make_default("").unwrap();
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
        let (previous, writes) = g.make_default("").unwrap();
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
        assert_eq!(g.make_default("usb-b").unwrap().0, "unplugged");
        g.remove(20);
        assert!(matches!(g.make_default(""), Err(PipeWireError::Failed(_))));
    }

    #[test]
    fn restore_puts_the_previous_back_only_if_ours_and_unpins() {
        let mut g = graph();
        let (previous, writes) = g.make_default("usb-b").unwrap();
        g.apply(&writes);
        let writes = g.restore_default(&previous);
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
        assert!(g.restore_default(&previous).is_empty());

        // The user chose another default meanwhile: kept; still unpinned.
        let (_, writes) = g.make_default("").unwrap();
        g.apply(&writes);
        g.meta_event(
            0,
            Some(CONFIGURED_SOURCE_KEY),
            Some(JSON_TYPE),
            Some(r#"{"name":"usb-b"}"#),
        );
        assert_eq!(
            g.restore_default("usb-a"),
            [MetaWrite::Delete {
                subject: 20,
                key: TARGET_KEY.into(),
            }]
        );
        // Nothing remembered: WirePlumber picks.
        g.apply(&g.make_default("").unwrap().1);
        assert_eq!(
            g.restore_default("")[0],
            MetaWrite::Delete {
                subject: 0,
                key: CONFIGURED_SOURCE_KEY.into(),
            }
        );
    }

    #[test]
    fn restore_writes_the_pinned_device_before_the_previous_one() {
        // The configured default unplugged: pinned to the one in force.
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
        let (previous, writes) = g.make_default("").unwrap();
        g.apply(&writes);
        let writes = g.restore_default(&previous);
        // In this order: WirePlumber's history becomes unplugged, usb-b,
        // ours; with "unplugged" absent, usb-b wins over ours.
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
        let (previous, writes) = g.make_default("").unwrap();
        g.apply(&writes);
        assert_eq!(g.pin().as_deref(), Some("usb-a"));
        assert_eq!(
            g.restore_default(&previous),
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
            g.restore_default(&previous),
            [set_configured("usb-b"), set_configured("usb-a")]
        );
        // Pinned to a device that went away, the previous one the only
        // device left: the previous one alone.
        g.meta_event(20, Some(TARGET_KEY), Some(ID_TYPE), Some("111"));
        g.remove(11);
        assert_eq!(g.restore_default(&previous)[..1], [set_configured("usb-a")]);
        assert_eq!(
            g.restore_default(&previous)
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
        let (previous, writes) = g.make_default("").unwrap();
        g.apply(&writes);
        assert_eq!(
            g.restore_default(&previous),
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
        let (previous, writes) = g.make_default("").unwrap();
        g.apply(&writes);
        g.apply(&[MetaWrite::Delete {
            subject: 20,
            key: TARGET_KEY.into(),
        }]);
        assert_eq!(
            g.restore_default(&previous),
            [set_configured("usb-c"), set_configured("usb-a")]
        );
        // A pin to another device wins over priority.
        g.meta_event(20, Some(TARGET_KEY), Some(ID_TYPE), Some("111"));
        assert_eq!(g.restore_default(&previous)[0], set_configured("usb-b"));
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
}
