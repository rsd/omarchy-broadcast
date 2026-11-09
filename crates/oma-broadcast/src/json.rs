//! The JSON lines printed by `status --json`, `cameras --json` and
//! `watch --json`. The contract is documented in `docs/cli-json.md`; this
//! module is its only producer.
//!
//! Every line is one JSON object carrying `"v"` ([`VERSION`]) and `"type"`,
//! and is at most [`MAX_LINE_BYTES`] bytes long: strings are clipped to the
//! bounds below (measured in bytes of their JSON encoding, so escapes count)
//! with a trailing [`ELLIPSIS`] (a path, `background_image`, keeps its end,
//! the file name, behind a leading one), and the camera list is cut at
//! [`MAX_CAMERAS`] entries.

use std::borrow::Cow;

use obc_ipc::{CameraInfo, Snapshot, Stats};
use serde::Serialize;

use crate::model;

/// Schema version carried by every line as `"v"`. Raised only on a breaking
/// change; added fields and line types are not breaking.
pub const VERSION: u32 = 1;
/// No line, without its newline, is longer than this many bytes.
pub const MAX_LINE_BYTES: usize = 4096;
/// At most this many cameras are listed; `cameras_truncated` says whether
/// more were present.
pub const MAX_CAMERAS: usize = 12;
/// Bound of the `error` text.
pub const MAX_ERROR_BYTES: usize = 512;
/// Bound of a camera id (`id`, `active_camera`).
pub const MAX_ID_BYTES: usize = 96;
/// Bound of a camera name (`name`, `active_camera_name`).
pub const MAX_NAME_BYTES: usize = 64;
/// Bound of a USB `vid:pid` (`usb`).
pub const MAX_USB_BYTES: usize = 16;
/// Bound of `output_mode`.
pub const MAX_MODE_BYTES: usize = 64;
/// Bound of `effect`.
pub const MAX_EFFECT_BYTES: usize = 32;
/// Bound of `pipewire_output`.
pub const MAX_PIPEWIRE_OUTPUT_BYTES: usize = 32;
/// Bound of `background_image`. Kept so that a `snapshot` line with every
/// string at its bound and [`MAX_CAMERAS`] cameras stays under
/// [`MAX_LINE_BYTES`]. A longer path is clipped at its start
/// ([`clip_start`]), so the file name survives.
pub const MAX_PATH_BYTES: usize = 256;
/// Appended to a clipped string.
pub const ELLIPSIS: &str = "\u{2026}";

/// `code` of an error line: the daemon is not running, or there is no
/// session bus.
pub const CODE_NOT_RUNNING: &str = "daemon-not-running";
/// `code` of an error line: any other D-Bus failure (an unexpected reply, a
/// timeout, a lost connection).
pub const CODE_DBUS: &str = "dbus-error";

/// Bytes `c` takes inside a JSON string as serde_json writes it.
fn escaped_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\u{08}' | '\u{0c}' | '\n' | '\r' | '\t' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// `s`, clipped so that its JSON encoding (without the quotes) is at most
/// `max` bytes, ending in [`ELLIPSIS`] when it was clipped.
pub fn clip(s: &str, max: usize) -> Cow<'_, str> {
    if s.chars().map(escaped_len).sum::<usize>() <= max {
        return Cow::Borrowed(s);
    }
    let budget = max.saturating_sub(ELLIPSIS.len());
    let mut used = 0;
    let mut end = 0;
    for (i, c) in s.char_indices() {
        let n = escaped_len(c);
        if used + n > budget {
            break;
        }
        used += n;
        end = i + c.len_utf8();
    }
    Cow::Owned(format!("{}{ELLIPSIS}", &s[..end]))
}

/// `s`, clipped so that its JSON encoding (without the quotes) is at most
/// `max` bytes, starting with [`ELLIPSIS`] when it was clipped: the end of
/// `s` is kept. For paths, whose file name is at the end.
pub fn clip_start(s: &str, max: usize) -> Cow<'_, str> {
    if s.chars().map(escaped_len).sum::<usize>() <= max {
        return Cow::Borrowed(s);
    }
    let budget = max.saturating_sub(ELLIPSIS.len());
    let mut used = 0;
    let mut start = s.len();
    for (i, c) in s.char_indices().rev() {
        let n = escaped_len(c);
        if used + n > budget {
            break;
        }
        used += n;
        start = i;
    }
    Cow::Owned(format!("{ELLIPSIS}{}", &s[start..]))
}

/// `blur_strength` as printed: within 0 to 1 (a non-finite value is 0), so
/// it is always a JSON number.
fn strength(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// A stats value as printed: a finite, non-negative measurement, or `-1`.
fn measurement(value: f64) -> f64 {
    if value.is_finite() && Stats::is_measured(value) {
        value
    } else {
        Stats::NOT_MEASURABLE
    }
}

#[derive(Serialize)]
struct Camera<'a> {
    id: Cow<'a, str>,
    name: Cow<'a, str>,
    usb: Cow<'a, str>,
    active: bool,
}

/// The scalar fields shared by `snapshot` and `state` lines.
#[derive(Serialize)]
struct StateFields<'a> {
    state: &'static str,
    enabled: bool,
    error: Cow<'a, str>,
    active_camera: Cow<'a, str>,
    active_camera_name: Cow<'a, str>,
    consumers: u32,
    loopback_consumers: u32,
    pipewire_consumers: u32,
    pipewire_output: Cow<'a, str>,
    output_mode: Cow<'a, str>,
    effect: Cow<'a, str>,
    blur_strength: f64,
    background_image: Cow<'a, str>,
    interface_revision: u32,
}

impl<'a> StateFields<'a> {
    fn of(s: &'a Snapshot) -> Self {
        StateFields {
            state: s.state.as_str(),
            enabled: s.enabled,
            error: clip(&s.error, MAX_ERROR_BYTES),
            active_camera: clip(&s.active_camera, MAX_ID_BYTES),
            active_camera_name: clip(model::active_camera_name(s).unwrap_or(""), MAX_NAME_BYTES),
            consumers: s.consumers,
            loopback_consumers: s.loopback_consumers,
            pipewire_consumers: s.pipewire_consumers,
            pipewire_output: clip(&s.pipewire_output, MAX_PIPEWIRE_OUTPUT_BYTES),
            output_mode: clip(&s.output_mode, MAX_MODE_BYTES),
            effect: clip(&s.effect, MAX_EFFECT_BYTES),
            blur_strength: strength(s.blur_strength),
            background_image: clip_start(&s.background_image, MAX_PATH_BYTES),
            interface_revision: s.interface_revision,
        }
    }
}

fn cameras_of<'a>(s: &'a Snapshot, limit: usize) -> (Vec<Camera<'a>>, bool) {
    let list = s
        .cameras
        .iter()
        .take(limit)
        .map(|c: &CameraInfo| Camera {
            id: clip(&c.id, MAX_ID_BYTES),
            name: clip(&c.name, MAX_NAME_BYTES),
            usb: clip(&c.usb, MAX_USB_BYTES),
            active: !c.id.is_empty() && c.id == s.active_camera,
        })
        .collect();
    (list, s.cameras.len() > limit)
}

#[derive(Serialize)]
struct SnapshotLine<'a> {
    v: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(flatten)]
    fields: StateFields<'a>,
    cameras: Vec<Camera<'a>>,
    cameras_truncated: bool,
}

#[derive(Serialize)]
struct StateLine<'a> {
    v: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    changed: &'a [&'a str],
    #[serde(flatten)]
    fields: StateFields<'a>,
}

#[derive(Serialize)]
struct CamerasLine<'a> {
    v: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    active_camera: Cow<'a, str>,
    cameras: Vec<Camera<'a>>,
    cameras_truncated: bool,
}

#[derive(Serialize)]
struct StatsLine {
    v: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    fps: f64,
    latency_p50_ms: f64,
    cpu_percent: f64,
    gpu_percent: f64,
}

#[derive(Serialize)]
struct ErrorLine<'a> {
    v: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    code: &'a str,
    error: Cow<'a, str>,
}

fn to_line<T: Serialize>(value: &T) -> String {
    // Serialising these plain structs cannot fail: every map key is a string.
    serde_json::to_string(value).expect("JSON line serialisation")
}

/// Serialises a camera-bearing line with the full bound of cameras, and with
/// fewer if the line would exceed [`MAX_LINE_BYTES`] (which the field bounds
/// already prevent; this keeps the cap a guarantee).
fn bounded_cameras_line<'a>(
    s: &'a Snapshot,
    build: impl Fn(Vec<Camera<'a>>, bool) -> String,
) -> String {
    let mut limit = MAX_CAMERAS;
    loop {
        let (cameras, truncated) = cameras_of(s, limit);
        let line = build(cameras, truncated);
        if line.len() <= MAX_LINE_BYTES || limit == 0 {
            return line;
        }
        limit -= 1;
    }
}

/// `{"v":1,"type":"snapshot",...}`: every property.
pub fn snapshot_line(s: &Snapshot) -> String {
    bounded_cameras_line(s, |cameras, cameras_truncated| {
        to_line(&SnapshotLine {
            v: VERSION,
            kind: "snapshot",
            fields: StateFields::of(s),
            cameras,
            cameras_truncated,
        })
    })
}

/// `{"v":1,"type":"state",...}`: every scalar field, after a change of the
/// fields named in `changed`.
pub fn state_line(s: &Snapshot, changed: &[&str]) -> String {
    to_line(&StateLine {
        v: VERSION,
        kind: "state",
        changed,
        fields: StateFields::of(s),
    })
}

/// `{"v":1,"type":"cameras",...}`: the camera list and the active camera.
pub fn cameras_line(s: &Snapshot) -> String {
    bounded_cameras_line(s, |cameras, cameras_truncated| {
        to_line(&CamerasLine {
            v: VERSION,
            kind: "cameras",
            active_camera: clip(&s.active_camera, MAX_ID_BYTES),
            cameras,
            cameras_truncated,
        })
    })
}

/// `{"v":1,"type":"stats",...}`: one `Stats` signal; `-1` is not measurable.
pub fn stats_line(st: &Stats) -> String {
    to_line(&StatsLine {
        v: VERSION,
        kind: "stats",
        fps: measurement(st.fps),
        latency_p50_ms: measurement(st.latency_p50_ms),
        cpu_percent: measurement(st.cpu_percent),
        gpu_percent: measurement(st.gpu_percent),
    })
}

/// `{"v":1,"type":"error","code":...,"error":...}`.
pub fn error_line(code: &str, message: &str) -> String {
    to_line(&ErrorLine {
        v: VERSION,
        kind: "error",
        code,
        error: clip(message, MAX_ERROR_BYTES),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use obc_ipc::State;
    use serde_json::Value;

    fn cam(id: &str, name: &str, usb: &str) -> CameraInfo {
        CameraInfo {
            id: id.into(),
            name: name.into(),
            usb: usb.into(),
        }
    }

    fn sample() -> Snapshot {
        Snapshot {
            state: State::Active,
            error: String::new(),
            enabled: true,
            cameras: vec![
                cam("usb-1234_abcd", "First Camera", "1234:abcd"),
                cam("platform-video2", "Second Camera", ""),
            ],
            active_camera: "platform-video2".into(),
            output_mode: "NV12 1920x1080@30".into(),
            effect: "blur".into(),
            blur_strength: 0.5,
            background_image: "/home/u/bg.png".into(),
            consumers: 2,
            loopback_consumers: 1,
            pipewire_consumers: 1,
            pipewire_output: "streaming".into(),
            interface_revision: 3,
        }
    }

    fn parse(line: &str) -> Value {
        assert!(!line.contains('\n'), "line has a newline: {line}");
        assert!(line.len() <= MAX_LINE_BYTES, "{} bytes", line.len());
        let v: Value = serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}"));
        assert_eq!(v["v"], 1, "{line}");
        assert!(v["type"].is_string(), "{line}");
        v
    }

    #[test]
    fn snapshot_line_has_every_field() {
        let v = parse(&snapshot_line(&sample()));
        assert_eq!(v["type"], "snapshot");
        assert_eq!(v["state"], "active");
        assert_eq!(v["enabled"], true);
        assert_eq!(v["error"], "");
        assert_eq!(v["active_camera"], "platform-video2");
        assert_eq!(v["active_camera_name"], "Second Camera");
        assert_eq!(v["consumers"], 2);
        assert_eq!(v["loopback_consumers"], 1);
        assert_eq!(v["pipewire_consumers"], 1);
        assert_eq!(v["pipewire_output"], "streaming");
        assert_eq!(v["output_mode"], "NV12 1920x1080@30");
        assert_eq!(v["effect"], "blur");
        assert_eq!(v["blur_strength"], 0.5);
        assert_eq!(v["background_image"], "/home/u/bg.png");
        assert_eq!(v["interface_revision"], 3);
        assert_eq!(v["cameras_truncated"], false);
        let cams = v["cameras"].as_array().unwrap();
        assert_eq!(cams.len(), 2);
        assert_eq!(cams[0]["id"], "usb-1234_abcd");
        assert_eq!(cams[0]["name"], "First Camera");
        assert_eq!(cams[0]["usb"], "1234:abcd");
        assert_eq!(cams[0]["active"], false);
        assert_eq!(cams[1]["usb"], "");
        assert_eq!(cams[1]["active"], true);
        // "v" and "type" lead the object.
        assert!(snapshot_line(&sample()).starts_with(r#"{"v":1,"type":"snapshot","#));
    }

    #[test]
    fn state_line_names_the_changes_and_carries_the_scalars() {
        let line = state_line(&sample(), &["state", "consumers"]);
        let v = parse(&line);
        assert_eq!(v["type"], "state");
        assert_eq!(v["changed"], serde_json::json!(["state", "consumers"]));
        assert_eq!(v["state"], "active");
        assert_eq!(v["active_camera_name"], "Second Camera");
        assert_eq!(v["blur_strength"], 0.5);
        assert_eq!(v["background_image"], "/home/u/bg.png");
        assert_eq!(v["loopback_consumers"], 1);
        assert_eq!(v["pipewire_consumers"], 1);
        assert_eq!(v["pipewire_output"], "streaming");
        assert!(v.get("cameras").is_none(), "{line}");
        // Every scalar of the model is in the line.
        for field in model::STATE_FIELDS {
            assert!(v.get(field).is_some(), "{field} missing from {line}");
        }
        // A daemon without the PipeWire output: its fields are there,
        // with the values of one that has none.
        let mut old = Snapshot::empty();
        old.consumers = 2;
        old.loopback_consumers = 2;
        let v = parse(&snapshot_line(&old));
        assert_eq!(
            (
                &v["consumers"],
                &v["loopback_consumers"],
                &v["pipewire_consumers"],
                &v["pipewire_output"]
            ),
            (
                &serde_json::json!(2),
                &serde_json::json!(2),
                &serde_json::json!(0),
                &serde_json::json!("off")
            )
        );
    }

    #[test]
    fn blur_strength_is_always_a_number_from_0_to_1() {
        let mut s = sample();
        for (given, printed) in [
            (0.0, 0.0),
            (1.0, 1.0),
            (0.25, 0.25),
            (1.5, 1.0),
            (-0.5, 0.0),
            (f64::NAN, 0.0),
            (f64::INFINITY, 0.0),
        ] {
            s.blur_strength = given;
            for line in [snapshot_line(&s), state_line(&s, &["blur_strength"])] {
                let v = parse(&line);
                assert!(v["blur_strength"].is_number(), "{given}: {line}");
                assert_eq!(v["blur_strength"].as_f64(), Some(printed), "{given}");
            }
        }
    }

    #[test]
    fn background_image_is_clipped_and_stays_valid() {
        let mut s = sample();
        // Control characters (6-byte escapes), bidi overrides and other
        // multi-byte characters, newlines and quotes.
        let nasty = format!("/home/u/{}", "\u{1}\u{202e}\u{2066}é🎥\n\"\\".repeat(200));
        s.background_image = nasty.clone();
        for line in [snapshot_line(&s), state_line(&s, &model::STATE_FIELDS)] {
            let v = parse(&line);
            let img = v["background_image"].as_str().unwrap();
            assert!(img.starts_with(ELLIPSIS), "{img}");
            assert!(nasty.ends_with(img.trim_start_matches(ELLIPSIS)));
            let encoded = serde_json::to_string(img).unwrap();
            assert!(encoded.len() - 2 <= MAX_PATH_BYTES, "{}", encoded.len());
        }
        // A long path keeps its file name: the start is what is clipped.
        s.background_image = format!("/home/u/{}office.jpg", "deep/".repeat(100));
        let v = parse(&snapshot_line(&s));
        let img = v["background_image"].as_str().unwrap();
        assert!(img.starts_with(ELLIPSIS), "{img}");
        assert!(img.ends_with("/deep/office.jpg"), "{img}");
        assert_eq!(img.len(), MAX_PATH_BYTES, "{img}");
        // A short path is printed as it is.
        s.background_image = "/home/u/\u{202e}gnp.exe".into();
        let v = parse(&snapshot_line(&s));
        assert_eq!(v["background_image"], "/home/u/\u{202e}gnp.exe");
    }

    #[test]
    fn cameras_line_lists_cameras() {
        let v = parse(&cameras_line(&sample()));
        assert_eq!(v["type"], "cameras");
        assert_eq!(v["active_camera"], "platform-video2");
        assert_eq!(v["cameras"].as_array().unwrap().len(), 2);
        assert_eq!(v["cameras_truncated"], false);

        let mut none = sample();
        none.cameras.clear();
        none.active_camera.clear();
        let v = parse(&cameras_line(&none));
        assert_eq!(v["cameras"], serde_json::json!([]));
        assert_eq!(v["active_camera"], "");
        let v = parse(&snapshot_line(&none));
        assert_eq!(v["active_camera_name"], "");
    }

    #[test]
    fn stats_line_uses_minus_one_for_unknown() {
        let v = parse(&stats_line(&Stats {
            fps: 30.0,
            latency_p50_ms: 12.5,
            cpu_percent: f64::NAN,
            gpu_percent: -1.0,
        }));
        assert_eq!(v["type"], "stats");
        assert_eq!(v["fps"], 30.0);
        assert_eq!(v["latency_p50_ms"], 12.5);
        assert_eq!(v["cpu_percent"], -1.0);
        assert_eq!(v["gpu_percent"], -1.0);
        let v = parse(&stats_line(&Stats {
            fps: f64::INFINITY,
            latency_p50_ms: -7.0,
            cpu_percent: 0.0,
            gpu_percent: 250.0,
        }));
        assert_eq!(v["fps"], -1.0);
        assert_eq!(v["latency_p50_ms"], -1.0);
        assert_eq!(v["cpu_percent"], 0.0);
        assert_eq!(v["gpu_percent"], 250.0);
    }

    #[test]
    fn error_line_has_code_and_text() {
        let v = parse(&error_line(
            CODE_NOT_RUNNING,
            "oma-broadcastd is not running",
        ));
        assert_eq!(v["type"], "error");
        assert_eq!(v["code"], "daemon-not-running");
        assert_eq!(v["error"], "oma-broadcastd is not running");
        let long = "x".repeat(10_000);
        let v = parse(&error_line(CODE_DBUS, &long));
        let text = v["error"].as_str().unwrap();
        assert!(text.ends_with(ELLIPSIS));
        assert!(text.len() <= MAX_ERROR_BYTES);
    }

    #[test]
    fn clip_bounds_the_encoded_length() {
        assert_eq!(clip("short", 10), "short");
        assert!(matches!(clip("exactly10!", 10), Cow::Borrowed(_)));
        for s in [
            "a".repeat(100),
            "\"".repeat(100),
            "\u{1}".repeat(100),
            "é".repeat(100),
            "🎥".repeat(100),
            "ab\n\t\u{7}\\\"é🎥".repeat(20),
        ] {
            for max in [4, 5, 10, 33, 64] {
                let c = clip(&s, max);
                let encoded = serde_json::to_string(c.as_ref()).unwrap();
                assert!(encoded.len() - 2 <= max, "{max}: {encoded}");
                assert!(c.ends_with(ELLIPSIS), "{c}");
                assert!(s.starts_with(c.trim_end_matches(ELLIPSIS)));
                let c = clip_start(&s, max);
                let encoded = serde_json::to_string(c.as_ref()).unwrap();
                assert!(encoded.len() - 2 <= max, "{max}: {encoded}");
                assert!(c.starts_with(ELLIPSIS), "{c}");
                assert!(s.ends_with(c.trim_start_matches(ELLIPSIS)));
            }
        }
        assert!(matches!(clip_start("exactly10!", 10), Cow::Borrowed(_)));
        assert_eq!(clip_start("/a/b/name.png", 11), "\u{2026}name.png");
        // The escape lengths agree with serde_json for every ASCII char.
        for b in 0u8..128 {
            let c = b as char;
            let encoded = serde_json::to_string(&c.to_string()).unwrap();
            assert_eq!(escaped_len(c), encoded.len() - 2, "{b:#x}");
        }
    }

    #[test]
    fn worst_case_lines_stay_under_the_cap_and_valid() {
        // Every string at its worst: long and made of 6-byte escapes.
        let nasty = "\u{1}".repeat(2000);
        let mut s = sample();
        s.error = nasty.clone();
        s.output_mode = nasty.clone();
        s.effect = nasty.clone();
        s.background_image = nasty.clone();
        s.pipewire_output = nasty.clone();
        s.loopback_consumers = u32::MAX;
        s.pipewire_consumers = u32::MAX;
        // The longest number serde_json prints for a strength in range.
        s.blur_strength = 2.225_073_858_507_201_4e-308;
        s.consumers = u32::MAX;
        s.interface_revision = u32::MAX;
        s.cameras = (0..100)
            .map(|i| cam(&format!("{i}{nasty}"), &nasty, &nasty))
            .collect();
        s.active_camera = s.cameras[3].id.clone();
        for line in [
            snapshot_line(&s),
            cameras_line(&s),
            state_line(&s, &model::STATE_FIELDS),
        ] {
            let v = parse(&line);
            if let Some(cams) = v.get("cameras") {
                assert_eq!(cams.as_array().unwrap().len(), MAX_CAMERAS, "{line}");
                assert_eq!(v["cameras_truncated"], true);
                assert_eq!(cams[3]["active"], true);
            }
            assert!(v["active_camera"].as_str().unwrap().ends_with(ELLIPSIS));
        }
        let v = parse(&snapshot_line(&s));
        assert!(v["error"].as_str().unwrap().ends_with(ELLIPSIS));
        assert!(v["pipewire_output"].as_str().unwrap().ends_with(ELLIPSIS));
        assert!(
            v["background_image"]
                .as_str()
                .unwrap()
                .starts_with(ELLIPSIS)
        );
        assert!(
            v["active_camera_name"]
                .as_str()
                .unwrap()
                .ends_with(ELLIPSIS)
        );
    }
}
