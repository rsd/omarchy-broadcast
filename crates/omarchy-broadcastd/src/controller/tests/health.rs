//! `CameraHealth` (revision 20) against the fake backend: the delivered
//! rate and the capture mode's label, the shared-hub and damaged-frames
//! warnings, the suggestions, the recommended settings and
//! `ApplyRecommendedSettings`, the anti-flicker of the time zone, and the
//! privacy shutter (revision 26) with the away image it shows, and the
//! Show Mode read from the camera's HID interface.

use obc_ipc::CameraHealth;
use obc_pipeline::control_node::{MenuItem, MenuItemValue};
use obc_pipeline::controls::{CTRL_TYPE_INTEGER, CTRL_TYPE_MENU};
use obc_pipeline::usb_topology::{SharedHub, UsbPlacement};

use super::*;
use crate::backend::fake::{FakeDevice, node_control};

const SHARP: u32 = obc_pipeline::controls::CID_SHARPNESS;
const HUE: u32 = obc_pipeline::quirks::CID_HUE;
const BRIGHT: u32 = 0x0098_0900;
const POWER: u32 = 0x0098_0918;

fn mode(s: &str) -> obc_core::VideoMode {
    s.parse().unwrap()
}

/// A LifeCam VX-800: YUYV only, 640x480 at most.
fn vx800() -> Camera {
    let mut c = cam("usb-vx", "LifeCam VX-800", Some("045e:0766"), "/dev/video4");
    c.mode = mode("YUY2:640x480@30");
    c.modes = vec![c.mode, mode("YUY2:640x480@15")];
    c
}

/// A Logitech C930e in MJPEG.
fn c930e() -> Camera {
    let mut c = cam("usb-c930e", "C930e", Some("046d:0843"), "/dev/video6");
    c.mode = mode("MJPG:1920x1080@30");
    c.modes = vec![c.mode, mode("MJPG:1280x720@30")];
    c
}

/// The FHD laptop camera, which has a row of recommended values.
fn fhd() -> Camera {
    let mut c = cam("usb-fhd", "FHD Camera", Some("5986:1193"), "/dev/video0");
    c.mode = mode("MJPG:1920x1080@30");
    c.modes = vec![c.mode];
    c
}

/// Its controls: brightness at its default, sharpness 3 (default 2), hue
/// -3 (default 0), anti-flicker at 60 Hz (default 50 Hz).
fn fhd_device() -> FakeDevice {
    let int = |id, name, r, d, v| node_control(id, name, CTRL_TYPE_INTEGER, r, d, v, 0);
    let mut power = node_control(
        POWER,
        "Power Line Frequency",
        CTRL_TYPE_MENU,
        (0, 2, 1),
        1,
        2,
        0,
    );
    power.menu = [(0, "Disabled"), (1, "50 Hz"), (2, "60 Hz")]
        .into_iter()
        .map(|(i, n)| MenuItem {
            index: i,
            value: MenuItemValue::Name(n.into()),
        })
        .collect();
    FakeDevice {
        controls: vec![
            int(BRIGHT, "Brightness", (-64, 64, 1), 0, 0),
            int(SHARP, "Sharpness", (0, 7, 1), 2, 3),
            int(HUE, "Hue", (-40, 40, 1), 0, -3),
            power,
        ],
        ..FakeDevice::default()
    }
}

/// A camera below hub 3-7.2 with a keyboard and a mouse.
fn shared_hub() -> UsbPlacement {
    UsbPlacement {
        usb_path: "3-7.2.1".into(),
        speed_mbps: Some(480),
        shared_hub: Some(SharedHub {
            hub_path: "3-7.2".into(),
            hid_devices: vec!["USB Keyboard".into(), "USB Mouse".into()],
        }),
    }
}

/// A camera on a root port.
fn root_port() -> UsbPlacement {
    UsbPlacement {
        usb_path: "3-1".into(),
        speed_mbps: Some(480),
        shared_hub: None,
    }
}

impl H {
    fn health(&self) -> CameraHealth {
        self.snap()
            .camera_health
            .expect("CameraHealth is published")
    }

    /// The label of the capture mode option `value`.
    fn mode_label(&mut self, value: &str) -> String {
        let s = self.setting(settings::MODE);
        let obc_ipc::SettingKind::Enum { options, .. } = &s.kind else {
            panic!("{s:?}")
        };
        options
            .iter()
            .find(|(v, _)| v == value)
            .unwrap_or_else(|| panic!("no option {value} in {options:?}"))
            .1
            .clone()
    }

    /// The `CameraHealth` changes drained.
    fn health_changes(&mut self) -> Vec<CameraHealth> {
        self.drain()
            .0
            .into_iter()
            .filter_map(|c| match c {
                Change::CameraHealth(h) => Some(h),
                _ => None,
            })
            .collect()
    }

    fn apply_recommended(&mut self, camera: &str, ms: u64) -> Result<(), IpcError> {
        self.call(|r| Command::ApplyRecommendedSettings(camera.into(), r), ms)
    }
}

#[test]
fn the_delivered_rate_is_published_kept_and_marks_the_mode_in_use() {
    let mut h = H::new("", |w| {
        w.cameras = vec![vx800()];
        w.delivered_fps = Some(19.46);
    });
    let health = h.health();
    assert_eq!(health.camera, "usb-vx");
    assert_eq!(health.delivered_fps, obc_ipc::NOT_MEASURED);
    assert_eq!(health.nominal_fps, 30.0);
    assert_eq!(h.mode_label("640x480@30"), "640x480 \u{b7} 30 fps");
    let serial = h.snap().settings_schema_serial;
    h.activate(0);
    h.drain();
    // The first stats tick measures it.
    h.tick(1300);
    let health = h.health();
    assert_eq!(health.delivered_fps, 19.5);
    assert!(health.delivers_short());
    assert_eq!(h.health_changes().len(), 1);
    assert_eq!(h.mode_label("640x480@30"), "640x480 \u{b7} 19.5 of 30 fps");
    assert_eq!(h.mode_label("640x480@15"), "640x480 \u{b7} 15 fps");
    // A label change is structural: the serial moved.
    assert_ne!(h.snap().settings_schema_serial, serial);
    // The counters move at most every 2 s; a small move keeps the label.
    h.world().delivered_fps = Some(19.84);
    h.tick(2300);
    assert_eq!(h.health().delivered_fps, 19.5, "held back");
    assert!(h.health_changes().is_empty());
    h.tick(3300);
    assert_eq!(h.health().delivered_fps, 19.8);
    assert_eq!(h.health_changes().len(), 1);
    assert_eq!(
        h.mode_label("640x480@30"),
        "640x480 \u{b7} 19.5 of 30 fps",
        "moved by less than 1.5 fps"
    );
    // Idle: the last measurement stays.
    h.consumers(0, 4000);
    h.tick(10_000);
    assert_eq!(h.state(), State::Idle);
    assert_eq!(h.health().delivered_fps, 19.8);
    assert_eq!(h.mode_label("640x480@30"), "640x480 \u{b7} 19.5 of 30 fps");
}

/// Streams the VX-800 for `seconds` stats ticks (one a second, from
/// `from`), delivering `fps(s)` at tick `s`; returns how often the
/// settings tree changed structurally (`SettingsSchemaSerial`).
fn label_changes(h: &mut H, from: u64, seconds: u64, fps: impl Fn(u64) -> f64) -> usize {
    let mut serial = h.snap().settings_schema_serial;
    let mut changes = 0;
    for s in from..from + seconds {
        h.world().delivered_fps = Some(fps(s));
        h.tick(300 + 1000 * s);
        let now = h.snap().settings_schema_serial;
        changes += usize::from(now != serial);
        serial = now;
    }
    changes
}

/// A rate hovering about the 95 % mark puts the label on once and leaves
/// it: it goes only above 97 %.
#[test]
fn the_label_does_not_flicker_about_the_short_mark() {
    let mut h = H::new("", |w| {
        w.cameras = vec![vx800()];
        w.delivered_fps = Some(28.6);
    });
    h.activate(0);
    let n = label_changes(&mut h, 1, 120, |s| {
        if s.is_multiple_of(4) || s % 4 == 1 {
            28.3
        } else {
            28.7
        }
    });
    assert_eq!(n, 1);
    assert_eq!(h.mode_label("640x480@30"), "640x480 \u{b7} 28.3 of 30 fps");
    // Back above 97 %: the label goes.
    assert_eq!(label_changes(&mut h, 121, 4, |_| 29.2), 1);
    assert_eq!(h.mode_label("640x480@30"), "640x480 \u{b7} 30 fps");
}

/// A VX-800 swinging between 17 and 23 fps moves its label at most every
/// 30 s, and only by 1.5 fps or more.
#[test]
fn a_swinging_rate_moves_the_label_rarely() {
    let mut h = H::new("", |w| {
        w.cameras = vec![vx800()];
        w.delivered_fps = Some(20.0);
    });
    h.activate(0);
    // Ten minutes, the rate swinging 17 - 23 - 17 every 12 s.
    let swing = |s: u64| {
        let p = (s % 12) as f64;
        if p < 6.0 { 17.0 + p } else { 29.0 - p }
    };
    let n = label_changes(&mut h, 1, 600, swing);
    assert!(n >= 2, "it follows the rate at all: {n}");
    assert!(n <= 1 + 600 / 30, "{n} label changes in 10 minutes");
    let label = h.mode_label("640x480@30");
    assert!(label.ends_with(" of 30 fps"), "{label}");
    // Steady within the step: no change at all.
    let held = label_changes(&mut h, 601, 120, |s| {
        if s.is_multiple_of(2) { 19.6 } else { 20.4 }
    });
    // At most the one move to the steady rate.
    assert!(held <= 1, "{held}");
    let after = label_changes(&mut h, 721, 120, |s| {
        if s.is_multiple_of(2) { 19.6 } else { 20.4 }
    });
    assert_eq!(after, 0);
}

#[test]
fn a_camera_at_its_rate_has_no_mark() {
    let mut h = H::new("", |w| {
        w.cameras = vec![vx800()];
        w.delivered_fps = Some(29.97);
    });
    h.activate(0);
    h.tick(1300);
    let health = h.health();
    assert_eq!(health.delivered_fps, 30.0);
    assert!(!health.delivers_short());
    assert_eq!(h.mode_label("640x480@30"), "640x480 \u{b7} 30 fps");
}

/// The VX-800's raw 640x480 at 30 fps on a hub with a keyboard and a mouse
/// draws the warning; the C930e's MJPEG on the same hub does not, nor the
/// VX-800 on a root port.
#[test]
fn the_shared_hub_warning_fires_for_a_heavy_raw_stream_only() {
    let mut h = H::new("", |w| {
        w.cameras = vec![vx800(), c930e()];
        w.placements.insert("usb-vx".into(), shared_hub());
        w.placements.insert("usb-c930e".into(), shared_hub());
    });
    let health = h.health();
    assert_eq!(health.camera, "usb-vx");
    assert_eq!(
        health.warnings,
        [
            "Plug LifeCam VX-800 into its own USB port: it shares a hub with USB Keyboard and \
          USB Mouse, and its uncompressed 640x480 at 30 fps stream uses most of the hub's \
          bandwidth, which can stall the keyboard or mouse and damage frames."
        ]
    );
    h.call(|r| Command::SelectCamera("usb-c930e".into(), r), 100)
        .unwrap();
    let health = h.health();
    assert_eq!(health.camera, "usb-c930e");
    assert!(health.warnings.is_empty(), "{:?}", health.warnings);

    let h = H::new("", |w| {
        w.cameras = vec![vx800()];
        w.placements.insert("usb-vx".into(), root_port());
    });
    assert!(h.health().warnings.is_empty());
}

/// Feeds `seconds` stats ticks after `activate(0)`, the guard counting
/// `per_second` more damaged frames at each.
fn stream_damaged(h: &mut H, seconds: u64, per_second: u64) {
    for s in 1..=seconds {
        h.world().corrupt.flagged += per_second;
        h.tick(300 + 1000 * s);
    }
}

#[test]
fn damaged_frames_draw_a_warning_and_are_counted() {
    let mut h = H::new("", |w| {
        w.cameras = vec![c930e()];
        w.placements.insert("usb-c930e".into(), root_port());
        w.delivered_fps = Some(30.0);
    });
    h.activate(0);
    // 5 of every 35 frames damaged (14 %): judged once 5 s are measured.
    stream_damaged(&mut h, 4, 5);
    assert!(h.health().warnings.is_empty(), "too soon");
    stream_damaged_from(&mut h, 5, 7, 5);
    let health = h.health();
    assert_eq!(
        health.warnings,
        [
            "C930e sends damaged frames: at least 1 in 20 of its frames were dropped in the last \
          10 seconds. Try another USB port or cable."
        ]
    );
    assert!(health.corrupt_frames >= 25, "{}", health.corrupt_frames);
    // Clean again for more than the window: the warning goes, the count
    // since the open stays.
    let count = h.world().corrupt.total();
    stream_damaged_from(&mut h, 8, 20, 0);
    let health = h.health();
    assert!(health.warnings.is_empty(), "{:?}", health.warnings);
    assert_eq!(health.corrupt_frames, count);
}

/// [`stream_damaged`] for the ticks `from..=to`.
fn stream_damaged_from(h: &mut H, from: u64, to: u64, per_second: u64) {
    for s in from..=to {
        h.world().corrupt.bad_jpeg += per_second;
        h.tick(300 + 1000 * s);
    }
}

#[test]
fn damaged_frames_on_a_shared_hub_give_the_hub_advice_and_a_clean_stream_none() {
    // The C930e in MJPEG on a shared hub: no hub warning of its own, so the
    // damaged-frames warning carries the advice.
    let mut h = H::new("", |w| {
        w.cameras = vec![c930e()];
        w.placements.insert("usb-c930e".into(), shared_hub());
        w.delivered_fps = Some(30.0);
    });
    h.activate(0);
    stream_damaged(&mut h, 7, 6);
    assert_eq!(
        h.health().warnings,
        [
            "C930e sends damaged frames: at least 1 in 20 of its frames were dropped in the last \
          10 seconds. Plug it into its own USB port: it shares a hub with USB Keyboard and USB \
          Mouse."
        ]
    );
    // The VX-800 on that hub: the hub warning, and the damaged-frames one
    // without repeating the advice.
    let mut h = H::new("", |w| {
        w.cameras = vec![vx800()];
        w.placements.insert("usb-vx".into(), shared_hub());
        w.delivered_fps = Some(24.0);
    });
    h.activate(0);
    stream_damaged(&mut h, 7, 6);
    let warnings = h.health().warnings;
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(warnings[0].starts_with("Plug LifeCam VX-800 into its own USB port"));
    assert!(
        warnings[1].ends_with("Try another USB port or cable."),
        "{warnings:?}"
    );
    // A clean C930e (no damaged frame) on the shared hub: nothing.
    let mut h = H::new("", |w| {
        w.cameras = vec![c930e()];
        w.placements.insert("usb-c930e".into(), shared_hub());
        w.delivered_fps = Some(30.0);
    });
    h.activate(0);
    stream_damaged(&mut h, 12, 0);
    let health = h.health();
    assert!(health.warnings.is_empty(), "{:?}", health.warnings);
    assert_eq!(health.corrupt_frames, 0);
}

fn suggested(h: &H) -> Vec<String> {
    h.health()
        .suggestions
        .iter()
        .map(|s| s.setting.clone())
        .collect()
}

#[test]
fn suggestions_follow_the_profile_and_the_settings() {
    // The VX-800's row calls it noisy, and its largest mode has 480 lines.
    let mut h = H::new("", |w| w.cameras = vec![vx800()]);
    assert!(h.setting(crate::health::ENHANCE).enabled);
    assert_eq!(
        suggested(&h),
        [crate::health::DENOISE, crate::health::ENHANCE]
    );
    assert!(h.health().suggestions[1].reason.contains("480 lines"));
    let s = &h.health().suggestions[0];
    assert_eq!(s.value, "on");
    assert!(s.reason.ends_with('.'), "{}", s.reason);
    // Noise removal on: no longer suggested.
    h.set_setting(crate::health::DENOISE, SettingValue::Bool(true), 100)
        .unwrap();
    assert!(!suggested(&h).contains(&crate::health::DENOISE.to_string()));
    // A camera at full resolution, not noisy (C930e): nothing.
    let h = H::new("", |w| w.cameras = vec![c930e()]);
    assert!(suggested(&h).is_empty());
    // Enhancement on: only noise removal left.
    let mut h = H::new("", |w| w.cameras = vec![vx800()]);
    h.set_setting(crate::health::ENHANCE, SettingValue::Bool(true), 100)
        .unwrap();
    assert_eq!(suggested(&h), [crate::health::DENOISE]);
    // Noise removal not installed: its switch is greyed, never suggested.
    let h = H::new("", |w| {
        w.cameras = vec![vx800()];
        w.features = Some(denoise_missing("Not installed"));
    });
    assert!(!suggested(&h).contains(&crate::health::DENOISE.to_string()));
}

#[test]
fn the_recommended_changes_differ_from_now_and_follow_the_zone() {
    let mut h = H::new("", |w| {
        w.cameras = vec![fhd()];
        w.devices.insert("usb-fhd".into(), fhd_device());
        w.time_zone = Some("Europe/Lisbon".into());
    });
    let health = h.health();
    assert!(health.has_recommendation);
    let rec: Vec<(String, String, String)> = health
        .recommended
        .iter()
        .map(|r| (r.setting.clone(), r.value.clone(), r.current.clone()))
        .collect();
    assert_eq!(
        rec,
        [
            ("camera.image.hue".into(), "0".into(), "-3".into()),
            ("camera.image.sharpness".into(), "5".into(), "3".into()),
            (
                "camera.exposure.power_line_frequency".into(),
                "50 Hz".into(),
                "60 Hz".into()
            ),
        ],
        "{health:?}"
    );
    assert_eq!(
        health.recommended[1].label,
        h.setting("camera.image.sharpness").label
    );
    // Changed by hand to the recommended value: no longer listed.
    h.set_setting("camera.image.sharpness", SettingValue::Int(5), 100)
        .unwrap();
    assert_eq!(h.health().recommended.len(), 2);

    // In a 60 Hz zone the anti-flicker is not a change; a camera without a
    // row has none.
    let h = H::new("", |w| {
        w.cameras = vec![fhd(), alpha()];
        w.devices.insert("usb-fhd".into(), fhd_device());
        w.time_zone = Some("America/New_York".into());
    });
    assert_eq!(h.health().recommended.len(), 2);
    let mut h = H::new("", |w| {
        w.cameras = vec![alpha()];
        w.devices.insert("usb-a".into(), fhd_device());
    });
    let health = h.health();
    assert!(!health.has_recommendation);
    assert!(health.recommended.is_empty());
    match h.apply_recommended("", 100) {
        Err(IpcError::InvalidArgument(m)) => assert!(m.contains("no recommended"), "{m}"),
        other => panic!("{other:?}"),
    }
    match h.apply_recommended("nope", 100) {
        Err(IpcError::InvalidArgument(_)) => {}
        other => panic!("{other:?}"),
    }
}

/// The FHD camera with a dynamic frame rate control (default on) at the
/// frame rate pin's 0.
fn fhd_pinned_device() -> FakeDevice {
    let mut d = fhd_device();
    d.controls.push(node_control(
        crate::backend::fake::cid::DYNAMIC_FPS,
        "Exposure, Dynamic Framerate",
        obc_pipeline::controls::CTRL_TYPE_BOOLEAN,
        (0, 1, 1),
        1,
        0,
        0,
    ));
    d
}

/// With `[camera] fixed_framerate`, the reset puts the dynamic frame rate
/// back to the pin's 0: the recommendation expects that, so the list is
/// empty after it is applied, closed or open.
#[test]
fn the_recommendation_expects_the_frame_rate_pin() {
    let config = "[camera]\nfixed_framerate = true\n";
    let listed = |h: &H| -> Vec<String> {
        h.health()
            .recommended
            .iter()
            .map(|r| r.setting.clone())
            .collect()
    };
    let new = || {
        H::new(config, |w| {
            w.cameras = vec![fhd()];
            w.devices.insert("usb-fhd".into(), fhd_pinned_device());
            w.time_zone = Some("Europe/Lisbon".into());
        })
    };
    let mut h = new();
    assert!(
        !listed(&h).iter().any(|s| s.contains("framerate")),
        "{:?}",
        listed(&h)
    );
    assert_eq!(listed(&h).len(), 3, "{:?}", listed(&h));
    h.apply_recommended("", 100).unwrap();
    assert!(listed(&h).is_empty(), "{:?}", listed(&h));

    let mut h = new();
    h.activate(0);
    assert_eq!(h.world().ingest_fixed_framerate, [true]);
    assert!(
        !listed(&h).iter().any(|s| s.contains("framerate")),
        "{:?}",
        listed(&h)
    );
    h.apply_recommended("", 1000).unwrap();
    assert!(listed(&h).is_empty(), "{:?}", listed(&h));
    assert_eq!(
        h.world().devices["usb-fhd"].value(crate::backend::fake::cid::DYNAMIC_FPS),
        0
    );
}

#[test]
fn applying_the_recommendation_at_idle_resets_then_stores_the_row() {
    let mut h = H::new("", |w| {
        w.cameras = vec![fhd()];
        w.devices.insert("usb-fhd".into(), fhd_device());
        w.time_zone = Some("Europe/Lisbon".into());
    });
    // A value of the user's elsewhere, and a capture mode chosen.
    h.set_setting("camera.image.brightness", SettingValue::Int(10), 100)
        .unwrap();
    h.drain();
    h.settings_notes.clear();
    h.apply_recommended("", 200).unwrap();
    // One settings signal for the whole operation, published before the
    // reply.
    h.drain();
    assert_eq!(h.settings_notes.len(), 1, "{:?}", h.settings_notes);
    let state = h.persisted();
    let stored = &state.camera_controls["usb-fhd"];
    assert!(stored.reset_all, "the defaults apply at the next open");
    let mut values: Vec<(&str, i32)> = stored
        .values
        .iter()
        .map(|(k, v)| (k.as_str(), *v))
        .collect();
    values.sort();
    assert_eq!(
        values,
        [("hue", 0), ("power_line_frequency", 1), ("sharpness", 5)]
    );
    assert!(h.health().recommended.is_empty(), "{:?}", h.health());
    assert!(h.setting("camera.image.sharpness").stored);
    // Nothing was written to the closed camera.
    assert!(h.world().control_writes.is_empty());
}

#[test]
fn applying_the_recommendation_while_open_writes_the_defaults_then_the_row() {
    let mut h = H::new("", |w| {
        w.cameras = vec![fhd()];
        w.devices.insert("usb-fhd".into(), fhd_device());
        w.time_zone = Some("Asia/Tokyo".into());
    });
    h.set_setting("camera.image.brightness", SettingValue::Int(10), 0)
        .unwrap();
    h.activate(100);
    h.world().control_writes.clear();
    h.apply_recommended("usb-fhd", 1000).unwrap();
    let writes: Vec<(String, i32)> = h
        .world()
        .control_writes
        .iter()
        .map(|(_, n, v)| (n.clone(), *v))
        .collect();
    // The reset first (every setting back to its default), then the row;
    // Japan has two grids: the anti-flicker is left at its default.
    let w = |n: &str, v: i32| (n.to_string(), v);
    assert_eq!(writes.len(), 6, "{writes:?}");
    let mut reset = writes[..4].to_vec();
    reset.sort();
    assert_eq!(
        reset,
        [
            w("brightness", 0),
            w("hue", 0),
            w("power_line_frequency", 1),
            w("sharpness", 2)
        ]
    );
    assert_eq!(writes[4..], [w("hue", 0), w("sharpness", 5)]);
    let d = h.world().devices["usb-fhd"].clone();
    assert_eq!(
        (
            d.value(BRIGHT),
            d.value(SHARP),
            d.value(HUE),
            d.value(POWER)
        ),
        (0, 5, 0, 1)
    );
    let state = h.persisted();
    let stored = &state.camera_controls["usb-fhd"];
    assert!(!stored.reset_all);
    let mut names: Vec<&str> = stored.values.keys().map(String::as_str).collect();
    names.sort();
    assert_eq!(names, ["hue", "sharpness"]);
    // The capture mode is not touched by it.
    assert_eq!(
        h.setting(settings::MODE).kind.value(),
        Some(SettingValue::Str("auto".into()))
    );
}

#[test]
fn the_anti_flicker_follows_the_time_zone_at_an_open_unless_stored() {
    // Nothing stored: the zone's 50 Hz is written, and not stored.
    let mut h = H::new("", |w| {
        w.cameras = vec![fhd()];
        w.devices.insert("usb-fhd".into(), fhd_device());
        w.time_zone = Some("Europe/Lisbon".into());
    });
    h.activate(0);
    assert_eq!(h.world().devices["usb-fhd"].value(POWER), 1);
    assert!(h.world().control_writes.contains(&(
        "usb-fhd".into(),
        "power_line_frequency".into(),
        1
    )));
    let state = h.persisted();
    assert!(
        state
            .camera_controls
            .get("usb-fhd")
            .is_none_or(|s| !s.values.contains_key("power_line_frequency")),
        "{state:?}"
    );
    assert!(!h.setting("camera.exposure.power_line_frequency").stored);

    // A stored value wins: the zone is not looked at.
    let mut h = H::new("", |w| {
        w.cameras = vec![fhd()];
        w.devices.insert("usb-fhd".into(), fhd_device());
        w.time_zone = Some("Europe/Lisbon".into());
    });
    h.set_setting(
        "camera.exposure.power_line_frequency",
        SettingValue::Str("60hz".into()),
        0,
    )
    .unwrap();
    h.activate(100);
    assert_eq!(h.world().devices["usb-fhd"].value(POWER), 2);
    assert!(
        !h.world()
            .control_writes
            .iter()
            .any(|(_, n, v)| n == "power_line_frequency" && *v == 1)
    );

    // Japan (two grids), or no zone: nothing written.
    for zone in [Some("Asia/Tokyo"), None] {
        let mut h = H::new("", |w| {
            w.cameras = vec![fhd()];
            w.devices.insert("usb-fhd".into(), fhd_device());
            w.time_zone = zone.map(str::to_string);
        });
        h.activate(0);
        assert_eq!(h.world().devices["usb-fhd"].value(POWER), 2, "{zone:?}");
        assert!(
            !h.world()
                .control_writes
                .iter()
                .any(|(_, n, _)| n == "power_line_frequency"),
            "{zone:?}"
        );
    }
}

/// A Logitech MX Brio in NV12: its status row says a closed privacy
/// shutter shows as flat black frames.
fn brio() -> Camera {
    let mut c = cam("usb-brio", "MX Brio", Some("046d:0944"), "/dev/video2");
    c.mode = mode("NV12:1920x1080@30");
    c.modes = vec![c.mode];
    c
}

/// The shutter as the ingest reads it: published at once when it changes
/// (not on the counters' cadence), `Shutter closed` while closed, and with
/// an away image chosen the writer's mask on while closed, so the feed
/// shows the image in place of the camera's black. The session streams on
/// untouched throughout; the camera off forgets the shutter and lifts the
/// mask.
#[test]
fn a_closed_shutter_is_published_at_once_and_shows_the_away_image() {
    use obc_pipeline::ShutterState::{Closed, Open};
    let mut h = H::new(&away_config(""), |w| w.cameras = vec![brio()]);
    let health = h.health();
    assert_eq!((health.shutter.as_str(), health.status.len()), ("", 0));
    h.activate(0);
    h.world().shutter = Some(Open);
    h.tick(1300);
    let health = h.health();
    assert_eq!(health.shutter, obc_ipc::SHUTTER_OPEN);
    assert!(health.status.is_empty());
    assert!(!h.world().mask);
    h.drain();
    let starts = h.world().session_starts;

    // Closed: published at the next stats tick, the counters not due yet
    // (2 s).
    h.world().shutter = Some(Closed);
    h.tick(2300);
    let changes = h.health_changes();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].shutter, obc_ipc::SHUTTER_CLOSED);
    assert_eq!(changes[0].status, ["Shutter closed"]);
    assert!(h.world().mask, "the away image in place of the black");
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.world().session_starts, starts, "no restart");
    assert!(h.world().session.is_some());

    // Open again: cleared at once.
    h.world().shutter = Some(Open);
    h.tick(3300);
    let health = h.health();
    assert_eq!(health.shutter, obc_ipc::SHUTTER_OPEN);
    assert!(health.status.is_empty());
    assert_eq!(h.world().mask_sets, [true, false]);

    // Closed, then the camera off: the shutter is unknown, the mask off.
    h.world().shutter = Some(Closed);
    h.tick(4300);
    assert!(h.world().mask);
    h.call(|r| Command::SetEnabled(false, r), 4500).unwrap();
    assert_eq!(h.state(), State::Off);
    assert!(!h.world().mask);
    assert_eq!(h.health().shutter, "");
    assert!(h.health().status.is_empty());
    assert_eq!(h.world().session_starts, starts);
}

/// With no away image a closed shutter is published, and the camera's
/// black shows as it is: the mask stays off. An away image chosen while
/// it is closed masks the feed; unchosen, the mask goes.
#[test]
fn without_an_away_image_a_closed_shutter_leaves_the_feed_alone() {
    let mut h = H::new("", |w| w.cameras = vec![brio()]);
    h.activate(0);
    h.world().shutter = Some(obc_pipeline::ShutterState::Closed);
    h.tick(1300);
    let health = h.health();
    assert_eq!(health.shutter, obc_ipc::SHUTTER_CLOSED);
    assert_eq!(health.status, ["Shutter closed"]);
    assert!(h.world().mask_sets.is_empty());
    h.reload(&away_config(""), 1500).unwrap();
    h.tick(2300);
    assert!(h.world().mask, "{:?}", h.snap());
    h.reload("", 2500).unwrap();
    h.tick(3300);
    assert!(!h.world().mask);
    assert_eq!(h.health().shutter, obc_ipc::SHUTTER_CLOSED);
}

/// A camera without a status row has no detector: its shutter is unknown.
#[test]
fn a_camera_without_a_status_row_has_no_shutter() {
    let mut h = H::new(&away_config(""), |w| w.cameras = vec![c930e()]);
    h.activate(0);
    h.tick(1300);
    assert_eq!(h.health().shutter, "");
    assert!(!h.world().mask);
}

impl H {
    /// An event of the orientation watcher of `generation` at `ms`.
    fn orient(&mut self, generation: WatchGeneration, event: OrientationEvent, ms: u64) {
        let now = self.at(ms);
        assert!(
            self.ctrl
                .handle(Command::Orientation(generation, event), now)
        );
    }
}

/// A new ingest of the same camera (a mode change: a reopen) says nothing
/// of the shutter until its first sample: a closed one stays closed and
/// masked until the new frames say it is open. Another camera forgets it.
#[test]
fn a_closed_shutter_stays_masked_across_a_reopen_of_the_same_camera() {
    use obc_pipeline::ShutterState::{Closed, Open};
    let mut b = brio();
    b.modes.push(mode("NV12:1280x720@30"));
    let mut h = H::new(&away_config(""), |w| w.cameras = vec![b, c930e()]);
    h.activate(0);
    h.world().shutter = Some(Closed);
    h.tick(1300);
    assert!(h.world().mask);
    // The mode changed: a new ingest, which has sampled nothing yet.
    h.world().shutter = None;
    h.set_setting(settings::MODE, str_value("1280x720@30"), 1500)
        .unwrap();
    assert_eq!(h.world().ingests.len(), 2, "reopened");
    h.tick(2300);
    h.tick(3300);
    assert_eq!(h.health().shutter, obc_ipc::SHUTTER_CLOSED);
    assert_eq!(h.world().mask_sets, [true], "never lifted");
    // Its frames say open: lifted at once.
    h.world().shutter = Some(Open);
    h.tick(4300);
    assert_eq!(h.health().shutter, obc_ipc::SHUTTER_OPEN);
    assert_eq!(h.world().mask_sets, [true, false]);
    // Closed again, then another camera: forgotten, the mask lifted, and
    // the new camera's unknown reading is not taken for closed.
    h.world().shutter = Some(Closed);
    h.tick(5300);
    assert!(h.world().mask);
    h.world().shutter = None;
    h.call(|r| Command::SelectCamera("usb-c930e".into(), r), 5500)
        .unwrap();
    h.tick(6300);
    assert!(!h.world().mask);
    assert_eq!(h.health().shutter, "");
}

/// A module change restarts the session (its last frame held): a closed
/// shutter stays masked across it until the new session's frames say it
/// is open.
#[test]
fn a_closed_shutter_stays_masked_across_a_module_restart() {
    use obc_pipeline::ShutterState::{Closed, Open};
    let mut h = H::new(&away_config("denoise = true\n"), |w| {
        w.cameras = vec![brio()];
        no_headroom(w);
    });
    h.set_module(MAXINE_MODULE, false, 0);
    h.activate(100);
    h.world().shutter = Some(Closed);
    h.tick(1400);
    assert!(h.world().mask);
    h.world().shutter = None;
    h.set_module(MAXINE_MODULE, true, 1500);
    h.tick(1900);
    assert_eq!(h.world().session_starts, 2, "restarted");
    assert_eq!(h.world().restart_holds, [true]);
    h.tick(3300);
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.health().shutter, obc_ipc::SHUTTER_CLOSED);
    assert_eq!(h.world().mask_sets, [true], "never lifted");
    h.world().shutter = Some(Open);
    h.tick(4300);
    assert_eq!(h.world().mask_sets, [true, false]);
}

/// The MX Brio's Show Mode, read from its HID interface only while the
/// session has it open (streaming): an open `hidraw` node would keep the
/// idle camera from autosuspending. Unknown until the watcher reports,
/// then published at once with its status line, after `Shutter closed`
/// when both show. The session stopping (idle) stops the watcher and
/// clears it; another camera does too; an event of a stopped watcher
/// changes nothing; a watcher that ended by itself (unplugged) leaves it
/// unknown, not started again until the cameras are scanned again.
#[test]
fn the_show_mode_is_read_only_while_the_camera_streams() {
    use obc_pipeline::hid_orientation::Orientation::{DeskView, Normal};
    let mut h = H::new("", |w| w.cameras = vec![brio(), c930e()]);
    // Idle: no watcher, the node stays closed.
    assert_eq!(h.world().orientation_watch, None);
    assert_eq!(h.world().orientation_starts, 0);
    let health = h.health();
    assert_eq!(health.show_mode, "");
    assert!(health.status.is_empty());
    h.tick(5000);
    assert_eq!(h.world().orientation_starts, 0, "not while idle");

    // Streaming: the watcher starts with the session.
    h.activate(6000);
    let (camera, report) = h.world().orientation_watch.clone().expect("watching");
    assert_eq!(camera, "usb-brio");
    assert_eq!(
        (report.report_id, report.byte, report.flipped),
        (0x55, 5, 1)
    );
    let generation = h.world().orientation_generation;
    assert_eq!(h.health().show_mode, "");
    h.drain();

    // Turned down to the desk: published at once, no tick needed.
    h.orient(generation, OrientationEvent::Changed(DeskView), 7000);
    let changes = h.health_changes();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].show_mode, obc_ipc::SHOW_MODE_DESK_VIEW);
    assert_eq!(changes[0].status, ["Show Mode: desk view"]);
    h.orient(generation, OrientationEvent::Changed(Normal), 8000);
    let changes = h.health_changes();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].show_mode, obc_ipc::SHOW_MODE_NORMAL);
    assert_eq!(changes[0].status, ["Show Mode: normal"]);
    // The same value again: nothing to announce.
    h.orient(generation, OrientationEvent::Changed(Normal), 8100);
    assert!(h.health_changes().is_empty());
    // With the shutter closed: both lines, the shutter's first.
    h.world().shutter = Some(obc_pipeline::ShutterState::Closed);
    h.tick(9300);
    assert_eq!(h.health().status, ["Shutter closed", "Show Mode: normal"]);
    assert_eq!(h.world().orientation_starts, 1);
    h.world().shutter = None;

    // Idle: the session stops, the watcher with it, the Show Mode cleared.
    h.consumers(0, 10_000);
    h.tick(30_000);
    assert_eq!(h.state(), State::Idle);
    assert!(h.world().session.is_none());
    assert_eq!(h.world().orientation_watch, None);
    assert_eq!(h.world().orientation_stops, 1);
    let health = h.health();
    assert_eq!(health.show_mode, "");
    assert!(health.status.is_empty());
    // An event of the stopped watcher changes nothing.
    h.orient(generation, OrientationEvent::Changed(DeskView), 30_100);
    assert_eq!(h.health().show_mode, "");

    // Streaming again: a new watcher, unknown until it reports; the old
    // one's events are ignored.
    h.activate(31_000);
    let again = h.world().orientation_generation;
    assert!(again > generation);
    assert_eq!(h.world().orientation_starts, 2);
    assert_eq!(h.health().show_mode, "");
    h.orient(generation, OrientationEvent::Changed(DeskView), 31_400);
    assert_eq!(h.health().show_mode, "");
    h.orient(again, OrientationEvent::Changed(DeskView), 31_500);
    assert_eq!(h.health().show_mode, obc_ipc::SHOW_MODE_DESK_VIEW);

    // Another camera: the watcher stops, the Show Mode is cleared.
    h.call(|r| Command::SelectCamera("usb-c930e".into(), r), 32_000)
        .unwrap();
    assert_eq!(h.world().orientation_watch, None);
    assert_eq!(h.world().orientation_stops, 2);
    let health = h.health();
    assert_eq!(
        (health.camera.as_str(), health.show_mode.as_str()),
        ("usb-c930e", "")
    );
    assert!(health.status.is_empty());
    // Back: a new watcher.
    h.call(|r| Command::SelectCamera("usb-brio".into(), r), 33_000)
        .unwrap();
    let back = h.world().orientation_generation;
    assert!(back > again);
    h.orient(back, OrientationEvent::Changed(Normal), 33_100);
    assert_eq!(h.health().show_mode, obc_ipc::SHOW_MODE_NORMAL);

    // Unplugged: the watcher ends by itself; unknown, not restarted by the
    // steps that follow, restarted by the next scan.
    h.orient(
        back,
        OrientationEvent::Ended("No such device".into()),
        34_000,
    );
    assert_eq!(h.world().orientation_watch, None);
    let health = h.health();
    assert_eq!(health.show_mode, "");
    assert!(health.status.is_empty());
    let starts = h.world().orientation_starts;
    h.tick(35_000);
    h.tick(36_000);
    assert_eq!(h.world().orientation_starts, starts);
    h.rescan(37_000);
    assert_eq!(h.world().orientation_starts, starts + 1);
    assert!(h.world().orientation_watch.is_some());
}

/// A camera whose HID node is missing or cannot be read: the Show Mode is
/// unknown (`""`, no line), one warning names the problem, and the watcher
/// is not retried at every step nor at each new session; a scan tries
/// again without warning again. A camera without an orientation report
/// never gets a watcher.
#[test]
fn an_unreadable_hid_node_leaves_the_show_mode_unknown_and_warns_once() {
    let why = "MX Brio: its Show Mode is not shown: its HID node /dev/hidraw0 cannot be read: \
               Permission denied (os error 13); read access to /dev/hidraw0 is needed (such \
               as a udev uaccess rule gives)";
    crate::log::take_lines();
    let mut h = H::new("", |w| {
        w.cameras = vec![brio()];
        w.fail_orientation = Some(why.into());
    });
    assert_eq!(h.world().orientation_starts, 0, "not while idle");
    h.activate(1000);
    let health = h.health();
    assert_eq!(health.show_mode, "");
    assert!(health.status.is_empty());
    assert_eq!(h.world().orientation_starts, 1);
    h.tick(3000);
    h.tick(5000);
    assert_eq!(h.world().orientation_starts, 1, "no retry at each step");
    // Idle, then streaming again: not retried.
    h.consumers(0, 6000);
    h.tick(30_000);
    assert_eq!(h.state(), State::Idle);
    h.activate(31_000);
    assert_eq!(h.world().orientation_starts, 1, "no retry at each session");
    h.rescan(32_000);
    assert_eq!(h.world().orientation_starts, 2);
    assert_eq!(h.health().show_mode, "");
    let warned: Vec<String> = crate::log::take_lines()
        .into_iter()
        .filter(|l| l.contains("Show Mode"))
        .collect();
    assert_eq!(warned, [format!("warning: {why}")]);

    let mut h = H::new("", |w| w.cameras = vec![c930e()]);
    h.activate(0);
    assert_eq!(h.world().orientation_starts, 0);
    assert_eq!(h.health().show_mode, "");
}
