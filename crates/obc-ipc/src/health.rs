//! What the daemon knows about the active camera's health (revision 20):
//! the `CameraHealth` property (`a{sv}`), read into [`CameraHealth`] and
//! written back.
//!
//! The dictionary's keys: `camera` (s, the id of `ActiveCamera`, `""` when
//! there is none, every other key then at its empty value),
//! `delivered_fps` (d, the rate of good frames the camera really
//! delivers, rounded to 0.1; [`NOT_MEASURED`] until measured; the last
//! measurement of a camera and capture mode is kept while the daemon
//! runs), `nominal_fps` (d, the capture mode's rate; [`NOT_MEASURED`] when
//! unknown), `corrupt_frames` (t, the damaged frames dropped since the
//! camera last opened), `warnings` (as, sentences for people),
//! `suggestions` (aa{sv}, [`Suggestion`]: an effect setting to turn on,
//! with why), `has_recommendation` (b, the camera has a row of
//! recommended settings) and `recommended` (aa{sv}, [`Recommended`]: what
//! `ApplyRecommendedSettings` would change, only where it differs from
//! now). `ApplyRecommendedSettings(s camera)` (`""`: the active camera)
//! puts the camera's device settings back to their defaults, then writes
//! the row's values and the power-line frequency of the time zone, stored
//! as the user's.
//!
//! A change is announced at once when the camera changes or a warning, a
//! suggestion or the recommended changes appear or go, and otherwise (the
//! counters) at most every [`CAMERA_HEALTH_PERIOD_MS`] milliseconds.
//!
//! Reading is tolerant, as for the settings channel: a key missing or of
//! another type reads as its empty value, keys this crate does not know
//! are ignored, and a list entry without its `setting` is dropped.

use serde::Serialize;
use zbus::Variant;

use crate::settings::{
    Entries, VariantDict, entries_of, entries_of_dict, get_bool, get_int, get_str, list_of, peel,
    put, str_of, to_owned,
};

/// The first [`crate::INTERFACE_REVISION`] with `CameraHealth` and
/// `ApplyRecommendedSettings`.
pub const CAMERA_HEALTH_REVISION: u32 = 20;

/// `delivered_fps` and `nominal_fps` before they are known.
pub const NOT_MEASURED: f64 = -1.0;

/// The shortest time between two announced changes of `CameraHealth` that
/// only its counters make, in milliseconds.
pub const CAMERA_HEALTH_PERIOD_MS: u64 = 2000;

/// A camera delivers short of its mode when its delivered rate is below
/// this share of the nominal one: the daemon then marks the capture mode's
/// label, and clients say so.
pub const DELIVERS_SHORT_RATIO: f64 = 0.95;

/// An effect setting the daemon suggests turning on for this camera.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Suggestion {
    /// A global setting id (`effect.video.denoise`).
    pub setting: String,
    /// The value to write with `SetSetting`, as the CLI takes it (`on`).
    pub value: String,
    /// Why, as a sentence.
    pub reason: String,
}

/// One change `ApplyRecommendedSettings` would make.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Recommended {
    /// The camera setting's id (`camera.image.sharpness`).
    pub setting: String,
    /// Its label, as the settings tree gives it.
    pub label: String,
    /// The recommended value, as the settings tree displays it.
    pub value: String,
    /// The value now, displayed the same way.
    pub current: String,
}

/// The `CameraHealth` property.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CameraHealth {
    /// The id of `ActiveCamera`; `""` when there is none.
    pub camera: String,
    /// The rate of good frames the camera delivers; [`NOT_MEASURED`]
    /// until measured.
    pub delivered_fps: f64,
    /// The capture mode's rate; [`NOT_MEASURED`] when unknown.
    pub nominal_fps: f64,
    /// Damaged frames dropped since the camera last opened.
    pub corrupt_frames: u64,
    /// Sentences for people; empty when none.
    pub warnings: Vec<String>,
    /// Effect settings worth turning on.
    pub suggestions: Vec<Suggestion>,
    /// The camera has a row of recommended settings.
    pub has_recommendation: bool,
    /// What `ApplyRecommendedSettings` would change.
    pub recommended: Vec<Recommended>,
}

impl Default for CameraHealth {
    fn default() -> Self {
        CameraHealth::none()
    }
}

impl CameraHealth {
    /// No camera: every key at its empty value.
    pub fn none() -> CameraHealth {
        CameraHealth {
            camera: String::new(),
            delivered_fps: NOT_MEASURED,
            nominal_fps: NOT_MEASURED,
            corrupt_frames: 0,
            warnings: Vec::new(),
            suggestions: Vec::new(),
            has_recommendation: false,
            recommended: Vec::new(),
        }
    }

    /// The camera delivers below [`DELIVERS_SHORT_RATIO`] of its mode's
    /// rate (both known).
    pub fn delivers_short(&self) -> bool {
        is_rate(self.delivered_fps)
            && is_rate(self.nominal_fps)
            && self.nominal_fps > 0.0
            && self.delivered_fps < DELIVERS_SHORT_RATIO * self.nominal_fps
    }

    /// The dictionary.
    pub fn to_dict(&self) -> VariantDict {
        let mut d = VariantDict::new();
        put(&mut d, "camera", Variant::from(self.camera.as_str()));
        put(&mut d, "delivered_fps", Variant::from(self.delivered_fps));
        put(&mut d, "nominal_fps", Variant::from(self.nominal_fps));
        put(&mut d, "corrupt_frames", Variant::from(self.corrupt_frames));
        d.insert("warnings".into(), to_owned(&self.warnings));
        let suggestions: Vec<VariantDict> = self
            .suggestions
            .iter()
            .map(|s| {
                let mut e = VariantDict::new();
                put(&mut e, "setting", Variant::from(s.setting.as_str()));
                put(&mut e, "value", Variant::from(s.value.as_str()));
                put(&mut e, "reason", Variant::from(s.reason.as_str()));
                e
            })
            .collect();
        d.insert("suggestions".into(), to_owned(&suggestions));
        put(
            &mut d,
            "has_recommendation",
            Variant::from(self.has_recommendation),
        );
        let recommended: Vec<VariantDict> = self
            .recommended
            .iter()
            .map(|r| {
                let mut e = VariantDict::new();
                put(&mut e, "setting", Variant::from(r.setting.as_str()));
                put(&mut e, "label", Variant::from(r.label.as_str()));
                put(&mut e, "value", Variant::from(r.value.as_str()));
                put(&mut e, "current", Variant::from(r.current.as_str()));
                e
            })
            .collect();
        d.insert("recommended".into(), to_owned(&recommended));
        d
    }

    /// The health of a dictionary, read tolerantly (see the module docs).
    pub fn from_dict(dict: &VariantDict) -> CameraHealth {
        let e = entries_of_dict(dict);
        let text = |key: &str| get_str(&e, key).unwrap_or_default().to_string();
        let rate = |key: &str| get_double(&e, key).unwrap_or(NOT_MEASURED);
        let dicts = |key: &str| -> Vec<Entries<'_, '_>> {
            e.get(key)
                .and_then(|v| list_of(v))
                .map(|l| l.iter().filter_map(entries_of).collect())
                .unwrap_or_default()
        };
        let field =
            |d: &Entries<'_, '_>, key: &str| get_str(d, key).unwrap_or_default().to_string();
        CameraHealth {
            camera: text("camera"),
            delivered_fps: rate("delivered_fps"),
            nominal_fps: rate("nominal_fps"),
            corrupt_frames: get_int(&e, "corrupt_frames")
                .and_then(|v| u64::try_from(v).ok())
                .or_else(|| match e.get("corrupt_frames").map(|v| peel(v)) {
                    Some(Variant::U64(x)) => Some(*x),
                    _ => None,
                })
                .unwrap_or_default(),
            warnings: e
                .get("warnings")
                .and_then(|v| list_of(v))
                .map(|l| {
                    l.iter()
                        .filter_map(|v| str_of(v).map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            suggestions: dicts("suggestions")
                .iter()
                .filter_map(|d| {
                    Some(Suggestion {
                        setting: get_str(d, "setting")?.to_string(),
                        value: field(d, "value"),
                        reason: field(d, "reason"),
                    })
                })
                .collect(),
            has_recommendation: get_bool(&e, "has_recommendation").unwrap_or_default(),
            recommended: dicts("recommended")
                .iter()
                .filter_map(|d| {
                    Some(Recommended {
                        setting: get_str(d, "setting")?.to_string(),
                        label: field(d, "label"),
                        value: field(d, "value"),
                        current: field(d, "current"),
                    })
                })
                .collect(),
        }
    }
}

/// A rate that is a measurement: finite and not negative.
fn is_rate(v: f64) -> bool {
    v.is_finite() && v >= 0.0
}

/// A double of a dictionary (an integer reads as its value).
fn get_double(e: &Entries<'_, '_>, key: &str) -> Option<f64> {
    match peel(e.get(key)?) {
        Variant::F64(x) => Some(*x),
        _ => get_int(e, key).map(|v| v as f64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::owned as variant;
    use zbus::wire::serialized::Context;
    use zbus::wire::{LE, to_bytes};

    fn sample() -> CameraHealth {
        CameraHealth {
            camera: "usb-Microsoft_LifeCam_VX-800-video-index0".into(),
            delivered_fps: 19.5,
            nominal_fps: 30.0,
            corrupt_frames: 42,
            warnings: vec!["Plug it into its own USB port.".into()],
            suggestions: vec![Suggestion {
                setting: "effect.video.denoise".into(),
                value: "on".into(),
                reason: "This camera's picture is grainy.".into(),
            }],
            has_recommendation: true,
            recommended: vec![Recommended {
                setting: "camera.image.sharpness".into(),
                label: "Sharpness".into(),
                value: "5".into(),
                current: "3".into(),
            }],
        }
    }

    /// `dict` encoded to D-Bus bytes and decoded back, as a peer sees it.
    fn over_the_wire(dict: &VariantDict) -> VariantDict {
        let bytes = to_bytes(Context::new(LE, 0), dict).unwrap();
        bytes.deserialize::<VariantDict>().unwrap().0
    }

    #[test]
    fn the_health_round_trips_with_its_signatures() {
        let h = sample();
        let d = h.to_dict();
        let sig = |k: &str| d[k].value_signature().to_string();
        assert_eq!(sig("camera"), "s");
        assert_eq!(sig("delivered_fps"), "d");
        assert_eq!(sig("nominal_fps"), "d");
        assert_eq!(sig("corrupt_frames"), "t");
        assert_eq!(sig("warnings"), "as");
        assert_eq!(sig("suggestions"), "aa{sv}");
        assert_eq!(sig("has_recommendation"), "b");
        assert_eq!(sig("recommended"), "aa{sv}");
        assert_eq!(CameraHealth::from_dict(&over_the_wire(&d)), h);
        // Empty lists keep their element signature.
        let none = CameraHealth::none().to_dict();
        assert_eq!(none["suggestions"].value_signature().to_string(), "aa{sv}");
        assert_eq!(none["warnings"].value_signature().to_string(), "as");
        assert_eq!(
            CameraHealth::from_dict(&over_the_wire(&none)),
            CameraHealth::none()
        );
    }

    #[test]
    fn reading_is_tolerant() {
        // Nothing at all: the empty health.
        assert_eq!(
            CameraHealth::from_dict(&VariantDict::new()),
            CameraHealth::none()
        );
        // Keys of another type read as empty; unknown keys are ignored; an
        // entry without its setting is dropped; an integer rate reads.
        let mut d = sample().to_dict();
        d.insert("camera".into(), variant(Variant::from(7u32)));
        d.insert("nominal_fps".into(), variant(Variant::from(30u32)));
        d.insert("warnings".into(), variant(Variant::from("not a list")));
        d.insert("future".into(), variant(Variant::from(1.5f64)));
        let mut bare = VariantDict::new();
        put(&mut bare, "value", Variant::from("on"));
        d.insert("suggestions".into(), to_owned(&vec![bare]));
        let h = CameraHealth::from_dict(&over_the_wire(&d));
        assert_eq!(h.camera, "");
        assert_eq!(h.nominal_fps, 30.0);
        assert!(h.warnings.is_empty());
        assert!(h.suggestions.is_empty());
        assert_eq!(h.recommended, sample().recommended);
    }

    #[test]
    fn delivers_short_below_95_percent_of_known_rates() {
        let mut h = sample();
        assert!(h.delivers_short());
        h.delivered_fps = 28.6;
        assert!(!h.delivers_short(), "95.3 %");
        h.delivered_fps = 28.4;
        assert!(h.delivers_short(), "94.7 %");
        h.delivered_fps = NOT_MEASURED;
        assert!(!h.delivers_short());
        h.delivered_fps = 10.0;
        h.nominal_fps = NOT_MEASURED;
        assert!(!h.delivers_short());
        assert!(!CameraHealth::none().delivers_short());
    }
}
