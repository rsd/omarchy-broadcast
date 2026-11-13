//! The media gallery of the daemon (revision 15): what `GetMedia` returns,
//! read into a typed model ([`MediaReply`], [`MediaEntry`]) and written
//! back.
//!
//! `GetMedia(s kind) -> (u serial, s bundled_dir, s user_dir, aa{sv}
//! items)`: `kind` is [`KIND_BACKGROUND`] or [`KIND_AWAY`]; `serial` is the
//! `MediaSerial` property's value when the list was made; `bundled_dir` and
//! `user_dir` are the kind's directories in the packaged (read only) and the
//! user's gallery; each item is a dictionary with `path` (absolute), `label`,
//! `bundled` (b), `type` ([`TYPE_IMAGE`] or [`TYPE_VIDEO`]), `width` (u),
//! `height` (u), `duration_ms` (t, 0 for a still) and `poster` (the path of
//! a video's poster image, `""` when there is none); from revision 19 a
//! video also has `cpu_ready` (b: it plays without the NVIDIA GPU as it is)
//! and, when that is false, `convert_command` (s: the shell command that
//! converts it into a CPU-ready WebM in the user's gallery, absent when the
//! daemon cannot name it). `ImportMedia(s kind, s
//! path) -> s path` copies a file into the user's gallery and returns the
//! stored path; `RemoveMedia(s path)` removes an item of the user's gallery.
//! `GetMediaDirs(s kind) -> a{sv}` (revision 19, [`MEDIA_DIRS_REVISION`])
//! names the kind's user directories ([`MediaDirs`]): `user_dir`, as
//! `GetMedia` gives it, and `video_dir`, the user's videos folder for a
//! background, where its videos are stored (`""` for `away`, which has
//! stills only).
//!
//! Reading is tolerant, as for the settings channel: a key missing or of
//! another type reads as its default (a number outside its type as 0), keys
//! this crate does not know are ignored, and an item without a `path` is
//! dropped.

use crate::settings::{VariantDict, entries_of_dict, get_bool, get_int, get_str, put};
use serde::Serialize;
use zbus::Variant;

/// The first [`crate::INTERFACE_REVISION`] with the media gallery:
/// `GetMedia`, `ImportMedia`, `RemoveMedia` and `MediaSerial`.
pub const MEDIA_REVISION: u32 = 15;

/// The first [`crate::INTERFACE_REVISION`] with `GetMediaDirs`
/// ([`MediaDirs`]).
pub const MEDIA_DIRS_REVISION: u32 = crate::CPU_CONVERT_REVISION;

/// The `GetMedia` reply on the wire: `(serial, bundled_dir, user_dir,
/// items)`.
pub type MediaWire = (u32, String, String, Vec<VariantDict>);

/// The kind of the backgrounds of the `replace` effect.
pub const KIND_BACKGROUND: &str = "background";
/// The kind of the images shown while the camera is away.
pub const KIND_AWAY: &str = "away";
/// The kinds `GetMedia` and `ImportMedia` take, in display order.
pub const KINDS: [&str; 2] = [KIND_BACKGROUND, KIND_AWAY];

/// An item's `type`: a still image.
pub const TYPE_IMAGE: &str = "image";
/// An item's `type`: a video.
pub const TYPE_VIDEO: &str = "video";

/// One item of the gallery.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MediaEntry {
    /// The file's absolute path.
    pub path: String,
    /// The name shown for it.
    pub label: String,
    /// Part of the packaged gallery (it cannot be removed).
    pub bundled: bool,
    /// [`TYPE_IMAGE`] or [`TYPE_VIDEO`] (the wire key is `type`); a client
    /// shows a value it does not know as it is.
    pub media_type: String,
    /// The width in pixels; 0 when unknown.
    pub width: u32,
    /// The height in pixels; 0 when unknown.
    pub height: u32,
    /// The duration in milliseconds; 0 for a still or when unknown.
    pub duration_ms: u64,
    /// The poster image of a video; `""` when there is none.
    pub poster: String,
    /// Whether a video plays without the NVIDIA GPU as it is (revision
    /// 19; on the wire for a video only, `false` when absent).
    pub cpu_ready: bool,
    /// For a video that is not CPU-ready, the shell command that converts
    /// it (revision 19); `""` when there is none.
    pub convert_command: String,
}

impl MediaEntry {
    /// The item dictionary.
    pub fn to_dict(&self) -> VariantDict {
        let mut d = VariantDict::new();
        put(&mut d, "path", Variant::from(self.path.as_str()));
        put(&mut d, "label", Variant::from(self.label.as_str()));
        put(&mut d, "bundled", Variant::from(self.bundled));
        put(&mut d, "type", Variant::from(self.media_type.as_str()));
        put(&mut d, "width", Variant::from(self.width));
        put(&mut d, "height", Variant::from(self.height));
        put(&mut d, "duration_ms", Variant::from(self.duration_ms));
        put(&mut d, "poster", Variant::from(self.poster.as_str()));
        if self.media_type == TYPE_VIDEO {
            put(&mut d, "cpu_ready", Variant::from(self.cpu_ready));
        }
        if !self.convert_command.is_empty() {
            put(
                &mut d,
                "convert_command",
                Variant::from(self.convert_command.as_str()),
            );
        }
        d
    }

    /// The item of a dictionary; `None` without a `path`.
    pub fn from_dict(dict: &VariantDict) -> Option<MediaEntry> {
        let e = entries_of_dict(dict);
        let text = |key: &str| get_str(&e, key).unwrap_or_default().to_string();
        let dimension = |key: &str| {
            get_int(&e, key)
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or_default()
        };
        let path = get_str(&e, "path")?.to_string();
        Some(MediaEntry {
            path,
            label: text("label"),
            bundled: get_bool(&e, "bundled").unwrap_or_default(),
            media_type: text("type"),
            width: dimension("width"),
            height: dimension("height"),
            duration_ms: get_int(&e, "duration_ms")
                .and_then(|v| u64::try_from(v).ok())
                .unwrap_or_default(),
            poster: text("poster"),
            cpu_ready: get_bool(&e, "cpu_ready").unwrap_or_default(),
            convert_command: text("convert_command"),
        })
    }
}

/// The items of one kind with the serial they were listed at and the
/// kind's two directories.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MediaReply {
    pub serial: u32,
    /// The kind's directory in the packaged gallery (read only).
    pub bundled_dir: String,
    /// The kind's directory in the user's gallery.
    pub user_dir: String,
    /// The bundled items first, then the user's.
    pub items: Vec<MediaEntry>,
}

impl MediaReply {
    /// The wire form.
    pub fn to_wire(&self) -> MediaWire {
        (
            self.serial,
            self.bundled_dir.clone(),
            self.user_dir.clone(),
            self.items.iter().map(MediaEntry::to_dict).collect(),
        )
    }

    /// The reply of the wire form; items without a `path` are dropped.
    pub fn from_wire(reply: &MediaWire) -> MediaReply {
        MediaReply {
            serial: reply.0,
            bundled_dir: reply.1.clone(),
            user_dir: reply.2.clone(),
            items: reply.3.iter().filter_map(MediaEntry::from_dict).collect(),
        }
    }

    /// The item at `path`.
    pub fn find(&self, path: &str) -> Option<&MediaEntry> {
        self.items.iter().find(|i| i.path == path)
    }
}

/// The user directories of one kind, `GetMediaDirs`' dictionary
/// (revision 19).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MediaDirs {
    /// The kind's directory in the user's gallery, as `GetMedia` gives it.
    pub user_dir: String,
    /// The user's videos folder of the kind, where its videos are stored;
    /// `""` when it has none (`away`).
    pub video_dir: String,
}

impl MediaDirs {
    /// The dictionary.
    pub fn to_dict(&self) -> VariantDict {
        let mut d = VariantDict::new();
        put(&mut d, "user_dir", Variant::from(self.user_dir.as_str()));
        put(&mut d, "video_dir", Variant::from(self.video_dir.as_str()));
        d
    }

    /// The directories of a dictionary, read tolerantly (a key missing or
    /// of another type is `""`).
    pub fn from_dict(dict: &VariantDict) -> MediaDirs {
        let e = entries_of_dict(dict);
        let text = |key: &str| get_str(&e, key).unwrap_or_default().to_string();
        MediaDirs {
            user_dir: text("user_dir"),
            video_dir: text("video_dir"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::OwnedVariant;
    use zbus::wire::serialized::Context;
    use zbus::wire::{LE, to_bytes};

    fn sample() -> MediaReply {
        MediaReply {
            serial: 4,
            bundled_dir: "/usr/share/omarchy-broadcast/media/backgrounds".into(),
            user_dir: "/home/u/.local/share/omarchy-broadcast/media/backgrounds".into(),
            items: vec![
                MediaEntry {
                    path: "/usr/share/omarchy-broadcast/media/backgrounds/dusk.webp".into(),
                    label: "Dusk".into(),
                    bundled: true,
                    media_type: TYPE_IMAGE.into(),
                    width: 3840,
                    height: 2160,
                    duration_ms: 0,
                    poster: String::new(),
                    cpu_ready: false,
                    convert_command: String::new(),
                },
                MediaEntry {
                    path: "/home/u/.local/share/omarchy-broadcast/media/backgrounds/loop.webm"
                        .into(),
                    label: "loop".into(),
                    bundled: false,
                    media_type: TYPE_VIDEO.into(),
                    width: 1920,
                    height: 1080,
                    duration_ms: 10_000,
                    poster: "/home/u/.cache/omarchy-broadcast/posters/ab.jpg".into(),
                    cpu_ready: false,
                    convert_command: "ffmpeg -i '/home/u/loop.mp4' -an 'out.webm'".into(),
                },
            ],
        }
    }

    #[test]
    fn the_reply_round_trips_through_the_wire() {
        let reply = sample();
        let wire = reply.to_wire();
        // Through the bus encoding, as a client receives it.
        let ctxt = Context::new(LE, 0);
        let bytes = to_bytes(ctxt, &wire).unwrap();
        let (decoded, _): (MediaWire, _) = bytes.deserialize().unwrap();
        let back = MediaReply::from_wire(&decoded);
        assert_eq!(back, reply);
        assert!(back.find(&reply.items[1].path).is_some_and(|i| !i.bundled));
        assert!(back.find("/nowhere.png").is_none());
        // The keys on the wire: a video has `cpu_ready`, and the command
        // when it is not.
        let mut keys: Vec<&str> = wire.3[1].keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "bundled",
                "convert_command",
                "cpu_ready",
                "duration_ms",
                "height",
                "label",
                "path",
                "poster",
                "type",
                "width"
            ]
        );
        let mut ready = reply.items[1].clone();
        ready.cpu_ready = true;
        ready.convert_command.clear();
        let d = ready.to_dict();
        assert!(d.contains_key("cpu_ready") && !d.contains_key("convert_command"));
        assert_eq!(MediaEntry::from_dict(&d), Some(ready));
        // A still has neither.
        let mut keys: Vec<&str> = wire.3[0].keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "bundled",
                "duration_ms",
                "height",
                "label",
                "path",
                "poster",
                "type",
                "width"
            ]
        );
    }

    #[test]
    fn reading_is_tolerant() {
        let mut d = sample().items[0].to_dict();
        d.insert("bundled".into(), OwnedVariant::from(7u32));
        d.insert("width".into(), OwnedVariant::from(-1i64));
        d.insert("height".into(), OwnedVariant::from(u64::MAX));
        d.insert("duration_ms".into(), OwnedVariant::from(-5i32));
        d.insert("later".into(), OwnedVariant::from(true));
        d.remove("label");
        let i = MediaEntry::from_dict(&d).unwrap();
        assert!(!i.bundled);
        assert_eq!((i.width, i.height, i.duration_ms), (0, 0, 0));
        assert_eq!(i.label, "");
        assert_eq!(i.media_type, TYPE_IMAGE);
        // A narrower integer type reads too.
        d.insert("width".into(), OwnedVariant::from(640u16));
        assert_eq!(MediaEntry::from_dict(&d).unwrap().width, 640);
        d.remove("path");
        assert!(MediaEntry::from_dict(&d).is_none());
        let reply = MediaReply::from_wire(&(1, String::new(), String::new(), vec![d]));
        assert!(reply.items.is_empty());
    }

    #[test]
    fn the_dirs_round_trip_through_the_wire_and_read_tolerantly() {
        let dirs = MediaDirs {
            user_dir: "/home/u/Pictures/omarchy-broadcast".into(),
            video_dir: "/home/u/Videos/omarchy-broadcast".into(),
        };
        let d = dirs.to_dict();
        let ctxt = Context::new(LE, 0);
        let bytes = to_bytes(ctxt, &d).unwrap();
        let (decoded, _): (VariantDict, _) = bytes.deserialize().unwrap();
        assert_eq!(MediaDirs::from_dict(&decoded), dirs);
        let mut keys: Vec<&str> = d.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["user_dir", "video_dir"]);
        // An empty videos folder (away) is still on the wire.
        let away = MediaDirs {
            video_dir: String::new(),
            ..dirs.clone()
        };
        assert!(away.to_dict().contains_key("video_dir"));
        // Tolerant: another type or a missing key reads as "", an unknown
        // key is ignored.
        let mut d = dirs.to_dict();
        d.insert("video_dir".into(), OwnedVariant::from(7u32));
        d.remove("user_dir");
        d.insert("later".into(), OwnedVariant::from(true));
        assert_eq!(MediaDirs::from_dict(&d), MediaDirs::default());
    }
}
