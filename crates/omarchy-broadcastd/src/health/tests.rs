//! The health rules over fake placements, modes and settings models.

use std::time::{Duration, Instant};

use obc_pipeline::control_node::{MenuItem, MenuItemValue, NodeControl};
use obc_pipeline::controls::{
    CTRL_FLAG_INACTIVE, CTRL_TYPE_BOOLEAN, CTRL_TYPE_BUTTON, CTRL_TYPE_INTEGER, CTRL_TYPE_MENU,
};
use obc_pipeline::quirks::CID_HUE;

use super::*;
use crate::backend::fake::node_control;
use crate::settings::{Inputs, build};

const SHARPNESS: u32 = obc_pipeline::controls::CID_SHARPNESS;
const POWER: u32 = 0x0098_0918;
const WB_TEMP: u32 = 0x0098_091a;
const DYNAMIC_FPS: u32 = 0x009a_0903;

fn mode(s: &str) -> VideoMode {
    s.parse().unwrap()
}

/// A camera on hub 3-7.2 with a keyboard and a mouse (`shared`), or on a
/// root port.
fn placement(shared: bool) -> UsbPlacement {
    UsbPlacement {
        usb_path: "3-7.2.1".into(),
        speed_mbps: Some(480),
        shared_hub: shared.then(|| SharedHub {
            hub_path: "3-7.2".into(),
            hid_devices: vec!["USB Keyboard".into(), "046d:c52b".into()],
        }),
    }
}

#[test]
fn mains_frequency_by_time_zone() {
    for (zone, hz) in [
        ("Europe/Lisbon", Some(50)),
        ("Europe/Berlin", Some(50)),
        ("Africa/Lagos", Some(50)),
        ("Africa/Monrovia", Some(60)),
        ("Asia/Kolkata", Some(50)),
        ("Asia/Taipei", Some(60)),
        ("Asia/Seoul", Some(60)),
        ("Asia/Manila", Some(60)),
        ("Asia/Riyadh", Some(60)),
        ("Australia/Sydney", Some(50)),
        ("America/New_York", Some(60)),
        ("America/Sao_Paulo", Some(60)),
        ("America/Mexico_City", Some(60)),
        ("America/Bogota", Some(60)),
        ("America/Argentina/Buenos_Aires", Some(50)),
        ("America/Buenos_Aires", Some(50)),
        ("America/Santiago", Some(50)),
        ("America/Punta_Arenas", Some(50)),
        // Chile's Aysén region, split from Santiago in tzdata 2025b.
        ("America/Coyhaique", Some(50)),
        ("Chile/Continental", Some(50)),
        ("America/Montevideo", Some(50)),
        ("America/Asuncion", Some(50)),
        ("America/La_Paz", Some(50)),
        ("America/Cayenne", Some(50)),
        ("America/Jamaica", Some(50)),
        ("America/Nuuk", Some(50)),
        ("Atlantic/Stanley", Some(50)),
        ("Atlantic/Bermuda", Some(60)),
        ("US/Pacific", Some(60)),
        ("Canada/Eastern", Some(60)),
        ("Pacific/Honolulu", Some(60)),
        ("Pacific/Guam", Some(60)),
        ("Pacific/Auckland", Some(50)),
        ("Pacific/Easter", Some(50)),
        // Two grids, or the sources disagree: nothing.
        ("Asia/Tokyo", None),
        // Slash-less legacy zones are not listed (zone_name never names
        // one): nothing.
        ("Japan", None),
        ("Jamaica", None),
        ("America/Guyana", None),
        // Nothing to place.
        ("UTC", None),
        ("Etc/UTC", None),
        ("Antarctica/McMurdo", None),
        ("Pacific/Unknown_Atoll", None),
        ("", None),
    ] {
        assert_eq!(mains_hz(zone), hz, "{zone}");
    }
}

#[test]
fn the_zone_is_named_by_tz_or_the_localtime_link() {
    for (value, zone) in [
        ("Europe/Lisbon", Some("Europe/Lisbon")),
        (":Europe/Lisbon", Some("Europe/Lisbon")),
        (
            "/usr/share/zoneinfo/America/Sao_Paulo",
            Some("America/Sao_Paulo"),
        ),
        ("../usr/share/zoneinfo/Asia/Tokyo", Some("Asia/Tokyo")),
        (
            "/usr/share/zoneinfo/posix/Europe/Paris",
            Some("Europe/Paris"),
        ),
        (
            "/usr/share/zoneinfo/America/Argentina/Cordoba",
            Some("America/Argentina/Cordoba"),
        ),
        ("EST5EDT", None),
        ("UTC", None),
        ("/etc/somewhere/else", None),
        ("", None),
    ] {
        assert_eq!(zone_name(value).as_deref(), zone, "{value}");
    }
}

/// The shared-hub rule fires for the VX-800 in YUYV 640x480 at 30 fps on a
/// hub with a keyboard and a mouse, and not for a C930e in MJPEG there, nor
/// for the VX-800 on a root port, nor for a light raw mode.
#[test]
fn the_shared_hub_rule_fires_for_a_heavy_raw_stream_only() {
    let vx800 = mode("YUY2:640x480@30");
    assert_eq!(raw_bytes_per_second(&vx800), Some(18_432_000));
    let text = shared_hub_warning("LifeCam VX-800", Some(&placement(true)), &vx800).unwrap();
    assert_eq!(
        text,
        "Plug LifeCam VX-800 into its own USB port: it shares a hub with USB Keyboard and \
         046d:c52b, and its uncompressed 640x480 at 30 fps stream uses most of the hub's \
         bandwidth, which can stall the keyboard or mouse and damage frames."
    );
    let c930e = mode("MJPG:1920x1080@30");
    assert_eq!(
        shared_hub_warning("C930e", Some(&placement(true)), &c930e),
        None
    );
    assert_eq!(
        shared_hub_warning("VX-800", Some(&placement(false)), &vx800),
        None
    );
    assert_eq!(shared_hub_warning("VX-800", None, &vx800), None);
    // 320x240 YUYV at 30 fps: 4.6 MB/s, light enough.
    let light = mode("YUY2:320x240@30");
    assert_eq!(
        shared_hub_warning("VX-800", Some(&placement(true)), &light),
        None
    );
    // The hub advice of the damaged-frames warning: only when the
    // shared-hub warning does not already give it.
    assert!(!hub_advice_needed(Some(&placement(true)), &vx800));
    assert!(hub_advice_needed(Some(&placement(true)), &c930e));
    assert!(!hub_advice_needed(Some(&placement(false)), &c930e));
}

#[test]
fn the_damaged_frames_warning_gives_the_hub_advice_when_shared() {
    assert_eq!(
        damaged_warning("C930e", Some(&placement(true))),
        "C930e sends damaged frames: at least 1 in 20 of its frames were dropped in the last \
         10 seconds. Plug it into its own USB port: it shares a hub with USB Keyboard and \
         046d:c52b."
    );
    assert_eq!(
        damaged_warning("C930e", None),
        "C930e sends damaged frames: at least 1 in 20 of its frames were dropped in the last \
         10 seconds. Try another USB port or cable."
    );
    let many: Vec<String> = (1..=5).map(|i| format!("Device {i}")).collect();
    assert_eq!(names_list(&many), "Device 1, Device 2, Device 3 and 2 more");
}

#[test]
fn the_damage_window_measures_the_last_ten_seconds() {
    let t0 = Instant::now();
    let at = |s: u64| t0 + Duration::from_secs(s);
    let mut w = DamageWindow::default();
    // 30 fps of good frames, 5 damaged each second: 14 %.
    for s in 0..=4 {
        w.record(at(s), 5 * s, 30.0);
    }
    assert_eq!(w.ratio(), None, "4 s: too short");
    w.record(at(5), 25, 30.0);
    let r = w.ratio().unwrap();
    assert!((r - 5.0 / 35.0).abs() < 1e-9, "{r}");
    // Then clean for 12 s: the damage leaves the window.
    for s in 6..=17 {
        w.record(at(s), 25, 30.0);
    }
    assert_eq!(w.ratio(), Some(0.0));
    // No frame at all: nothing to judge.
    let mut idle = DamageWindow::default();
    for s in 0..=6 {
        idle.record(at(s), 0, 0.0);
    }
    assert_eq!(idle.ratio(), None);
    // A clean C930e (0 damaged) never reaches the threshold; the VX-800's
    // 14 % does.
    assert!(r >= DAMAGE_RATIO);
    w.clear();
    assert_eq!(w.ratio(), None);
}

/// The damaged flag comes at 5 % and goes only below 3 %: a share
/// oscillating about 5 % sets it once and keeps it.
#[test]
fn the_damaged_flag_has_hysteresis() {
    assert!(!damaged_now(false, 0.049));
    assert!(damaged_now(false, 0.05));
    assert!(damaged_now(true, 0.049));
    assert!(damaged_now(true, 0.03));
    assert!(!damaged_now(true, 0.0299));
    let mut flag = false;
    let mut flips = 0;
    for i in 0..100 {
        let ratio = if i % 2 == 0 { 0.052 } else { 0.046 };
        let now = damaged_now(flag, ratio);
        flips += usize::from(now != flag);
        flag = now;
    }
    assert_eq!(flips, 1);
    assert!(flag);
    assert!(!damaged_now(flag, 0.02));
}

fn menu(id: u32, name: &str, items: &[(u32, &str)], default: i32, value: i32) -> NodeControl {
    let max = items.iter().map(|(i, _)| *i).max().unwrap_or(0) as i32;
    let mut c = node_control(id, name, CTRL_TYPE_MENU, (0, max, 1), default, value, 0);
    c.menu = items
        .iter()
        .map(|(i, n)| MenuItem {
            index: *i,
            value: MenuItemValue::Name((*n).into()),
        })
        .collect();
    c
}

/// A camera like the FHD laptop camera: sharpness 3 (default 2), hue -3
/// (default 0), anti-flicker at 60 Hz (default 50 Hz), white balance
/// temperature inactive and off its default, and a button.
fn fhd_model(open: bool) -> CameraSettings {
    let controls = vec![
        node_control(
            SHARPNESS,
            "Sharpness",
            CTRL_TYPE_INTEGER,
            (0, 7, 1),
            2,
            3,
            0,
        ),
        node_control(CID_HUE, "Hue", CTRL_TYPE_INTEGER, (-40, 40, 1), 0, -3, 0),
        menu(
            POWER,
            "Power Line Frequency",
            &[(0, "Disabled"), (1, "50 Hz"), (2, "60 Hz")],
            1,
            2,
        ),
        node_control(
            WB_TEMP,
            "White Balance Temperature",
            CTRL_TYPE_INTEGER,
            (2800, 6500, 1),
            4600,
            3000,
            CTRL_FLAG_INACTIVE,
        ),
        node_control(
            0x009a_0906,
            "Pan, Reset",
            CTRL_TYPE_BUTTON,
            (0, 0, 0),
            0,
            0,
            0,
        ),
    ];
    build(&Inputs {
        camera: "fhd",
        controls: &controls,
        xu: &[],
        stored: None,
        open,
        fixed_framerate: false,
        hidden: &[],
        usb: None,
    })
}

const FHD_ROW: &[(u32, i64)] = &[(SHARPNESS, 5), (CID_HUE, 0)];

#[test]
fn the_recommended_changes_are_the_defaults_with_the_row_and_the_zone() {
    let model = fhd_model(true);
    let rec = recommended(&model, FHD_ROW, Some(50), false);
    let rows: Vec<(&str, &str, &str, &str)> = rec
        .iter()
        .map(|r| {
            (
                r.setting.as_str(),
                r.label.as_str(),
                r.value.as_str(),
                r.current.as_str(),
            )
        })
        .collect();
    let power = model.get("camera.exposure.power_line_frequency").unwrap();
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert!(rows.contains(&("camera.image.sharpness", "Sharpness", "5", "3")));
    assert!(rows.contains(&("camera.image.hue", "Hue", "0", "-3")));
    assert!(rows.contains(&(
        "camera.exposure.power_line_frequency",
        power.setting.label.as_str(),
        "50 Hz",
        "60 Hz"
    )));
    // In a 60 Hz zone the anti-flicker is right already.
    let rec = recommended(&model, FHD_ROW, Some(60), false);
    assert_eq!(rec.len(), 2, "{rec:?}");
    // With no zone it goes back to its default (50 Hz).
    let rec = recommended(&model, FHD_ROW, None, false);
    assert!(rec.iter().any(|r| r.value == "50 Hz"), "{rec:?}");
    // The inactive temperature and the button are never listed.
    assert!(
        !recommended(&model, &[(WB_TEMP, 4000)], None, false)
            .iter()
            .any(|r| r.setting.contains("temperature"))
    );
    // What is written after the reset: the row and the zone only.
    let writes = recommended_writes(&model, FHD_ROW, Some(60), false);
    let names: Vec<(&str, i32)> = writes
        .iter()
        .map(|(i, raw)| (model.entries[*i].name.as_str(), *raw))
        .collect();
    assert_eq!(
        names,
        [("hue", 0), ("sharpness", 5), ("power_line_frequency", 2)]
    );
    // A row value the camera does not take keeps the default.
    let rec = recommended(&model, &[(SHARPNESS, 99)], Some(60), false);
    assert!(
        rec.iter()
            .any(|r| r.setting == "camera.image.sharpness" && r.value == "2")
    );
}

/// With `[camera] fixed_framerate`, the reset puts
/// `exposure_dynamic_framerate` back to the pin's 0, not its default: that
/// is the recommended value, so the list is empty once applied.
#[test]
fn the_frame_rate_pin_is_the_recommended_dynamic_framerate() {
    let dynamic = |default, value| {
        node_control(
            DYNAMIC_FPS,
            "Exposure, Dynamic Framerate",
            CTRL_TYPE_BOOLEAN,
            (0, 1, 1),
            default,
            value,
            0,
        )
    };
    let model = |value| {
        build(&Inputs {
            camera: "fhd",
            controls: &[dynamic(1, value)],
            xu: &[],
            stored: None,
            open: true,
            fixed_framerate: true,
            hidden: &[],
            usb: None,
        })
    };
    // At the pin's 0 after the reset: nothing to change.
    let pinned = model(0);
    assert_eq!(
        recommended_targets(&pinned, &[], None, true),
        [(0, 0)],
        "the pin's 0"
    );
    assert!(recommended(&pinned, &[], None, true).is_empty());
    // Unpinned, the default is the target.
    assert_eq!(recommended_targets(&pinned, &[], None, false), [(0, 1)]);
    assert_eq!(recommended(&pinned, &[], None, false).len(), 1);
    // Changed over the pin: the pin's 0 is recommended, not the default.
    let rec = recommended(&model(1), &[], None, true);
    assert_eq!(rec.len(), 1, "{rec:?}");
    assert_eq!(recommended_targets(&model(1), &[], None, true), [(0, 0)]);
    // The reset alone writes it: it is not among the row's writes.
    assert!(recommended_writes(&model(1), &[], None, true).is_empty());
    // A row that sets it wins over the pin.
    assert_eq!(
        recommended_targets(&model(0), &[(DYNAMIC_FPS, 1)], None, true),
        [(0, 1)]
    );
}

#[test]
fn values_display_as_the_tree_shows_them() {
    let int = SettingKind::Int {
        min: 0,
        max: 10,
        step: 1,
        default: 0,
        value: 0,
        unit: Some("K".into()),
        display: None,
    };
    assert_eq!(display(&int, &SettingValue::Int(4000)), "4000 K");
    let b = SettingKind::Bool {
        default: false,
        value: false,
    };
    assert_eq!(display(&b, &SettingValue::Bool(true)), "On");
    assert_eq!(display(&b, &SettingValue::Bool(false)), "Off");
    let e = SettingKind::Enum {
        options: vec![("50hz".into(), "50 Hz".into())],
        default: "50hz".into(),
        value: "50hz".into(),
    };
    assert_eq!(display(&e, &SettingValue::Str("50hz".into())), "50 Hz");
    assert_eq!(display(&e, &SettingValue::Str("m7".into())), "m7");
}

fn switch(id: &str, on: bool, enabled: bool) -> GlobalEntry {
    GlobalEntry {
        group: "video".into(),
        group_label: "Video".into(),
        binding: crate::settings::effects::Binding::Plain,
        setting: obc_ipc::Setting {
            id: id.into(),
            label: id.into(),
            kind: SettingKind::Bool {
                default: false,
                value: on,
            },
            scope: "global".into(),
            stored: false,
            enabled,
            disabled_reason: if enabled {
                String::new()
            } else {
                "Not installed".into()
            },
            disabled_kind: String::new(),
            enabled_when: Vec::new(),
            hint: None,
            description: None,
        },
    }
}

#[test]
fn suggestions_skip_what_is_on_or_cannot_run() {
    let both = [switch(DENOISE, false, true), switch(ENHANCE, false, true)];
    let s = suggestions(true, Some(480), &both);
    let ids: Vec<&str> = s.iter().map(|s| s.setting.as_str()).collect();
    assert_eq!(ids, [DENOISE, ENHANCE]);
    assert!(s.iter().all(|s| s.value == "on"));
    assert!(s[1].reason.contains("480 lines"), "{}", s[1].reason);
    // Not noisy, 720 lines or more: nothing.
    assert!(suggestions(false, Some(720), &both).is_empty());
    assert!(suggestions(false, None, &both).is_empty());
    // Already on, greyed, or absent: left out.
    let on = [switch(DENOISE, true, true), switch(ENHANCE, false, false)];
    assert!(suggestions(true, Some(480), &on).is_empty());
    assert!(suggestions(true, Some(480), &[]).is_empty());
}

/// The bound the CLI clips an option label to (`omarchy-broadcast`'s
/// `MAX_OPTION_LABEL_BYTES`).
const MAX_OPTION_LABEL_BYTES: usize = 32;

#[test]
fn the_delivered_rate_replaces_the_rate_of_its_mode_option() {
    let modes = vec![mode("YUY2:640x480@30"), mode("YUY2:640x480@15")];
    let mut s = crate::settings::mode_setting(&modes, None).unwrap();
    mark_delivered(&mut s, "640x480@30", 19.46);
    let SettingKind::Enum { options, .. } = &s.kind else {
        panic!("{s:?}")
    };
    let label = |v: &str| options.iter().find(|(o, _)| o == v).unwrap().1.clone();
    assert_eq!(
        label("640x480@30"),
        "640x480 \u{b7} 19.5 of 30 fps",
        "{options:?}"
    );
    assert_eq!(label("640x480@15"), "640x480 \u{b7} 15 fps");
    assert_eq!(round_fps(19.46), 19.5);
    assert_eq!(round_fps(29.94), 29.9);
}

/// The longest labels fit the CLI's bound: a fractional nominal rate (two
/// decimals) at 4K, and a three-digit delivered rate.
#[test]
fn a_delivered_label_fits_the_option_label_bound() {
    for (m, value, fps) in [
        ("YUY2:1920x1080@30", "1920x1080@30", 19.5),
        ("MJPG:1920x1080@30000/1001", "1920x1080@30000/1001", 28.4),
        ("MJPG:3840x2160@60000/1001", "3840x2160@60000/1001", 56.9),
        ("MJPG:4096x2160@120000/1001", "4096x2160@120000/1001", 113.8),
    ] {
        let modes = vec![mode(m)];
        let mut s = crate::settings::mode_setting(&modes, None).unwrap();
        mark_delivered(&mut s, value, fps);
        let SettingKind::Enum { options, .. } = &s.kind else {
            panic!("{s:?}")
        };
        let (_, label) = options.iter().find(|(o, _)| o == value).unwrap();
        assert!(label.contains(" of "), "{value}: {options:?}");
        assert!(
            label.len() <= MAX_OPTION_LABEL_BYTES,
            "{label}: {} bytes",
            label.len()
        );
    }
}

#[test]
fn the_shown_rate_has_hysteresis_and_moves_by_its_step() {
    // On below 95 % of 30 (28.5), off only above 97 % (29.1).
    assert_eq!(shown_rate(None, 28.6, 30.0), None);
    assert_eq!(shown_rate(None, 28.4, 30.0), Some(28.4));
    assert_eq!(shown_rate(Some(28.4), 29.0, 30.0), Some(28.4));
    assert_eq!(shown_rate(Some(28.4), 29.2, 30.0), None);
    // Moved only by 1.5 fps or more.
    assert_eq!(shown_rate(Some(19.5), 20.9, 30.0), Some(19.5));
    assert_eq!(shown_rate(Some(19.5), 21.0, 30.0), Some(21.0));
    assert_eq!(shown_rate(Some(19.5), 18.0, 30.0), Some(18.0));
    // No nominal rate: nothing.
    assert_eq!(shown_rate(None, 10.0, 0.0), None);
    assert_eq!(shown_rate(Some(10.0), f64::NAN, 30.0), None);
    // A rate oscillating about the 95 % mark: the label comes once.
    let mut shown = None;
    let mut changes = 0;
    for i in 0..100 {
        let fps = if i % 2 == 0 { 28.4 } else { 28.9 };
        let next = shown_rate(shown, fps, 30.0);
        changes += usize::from(next != shown);
        shown = next;
    }
    assert_eq!(changes, 1);
    assert_eq!(shown, Some(28.4));
}

#[test]
fn the_shutter_reads_open_closed_or_unknown() {
    assert_eq!(
        shutter_status(Some(ShutterState::Open)),
        ("open".to_string(), Vec::new())
    );
    assert_eq!(
        shutter_status(Some(ShutterState::Closed)),
        ("closed".to_string(), vec!["Shutter closed".to_string()])
    );
    assert_eq!(shutter_status(None), (String::new(), Vec::new()));
}

#[test]
fn the_show_mode_reads_normal_desk_view_or_unknown() {
    use obc_pipeline::hid_orientation::Orientation;
    assert_eq!(
        show_mode_status(Some(Orientation::Normal)),
        ("normal".to_string(), Some("Show Mode: normal".to_string()))
    );
    assert_eq!(
        show_mode_status(Some(Orientation::DeskView)),
        (
            "desk_view".to_string(),
            Some("Show Mode: desk view".to_string())
        )
    );
    assert_eq!(show_mode_status(None), (String::new(), None));
}

const AUTO_EXPOSURE: u32 = 0x009a_0901;
const EXPOSURE_TIME: u32 = 0x009a_0902;
const GAIN: u32 = 0x0098_0913;
const WB_AUTO: u32 = 0x0098_090c;

/// Auto exposure (item 1 manual, 3 aperture priority; default
/// `ae_default`, now `ae`), exposure time, gain and white balance
/// (automatic: `wb_default`, `wb`), none flagged inactive, the dependents
/// off their defaults.
fn auto_model(ae_default: i32, ae: i32, wb_default: i32, wb: i32) -> CameraSettings {
    let controls = vec![
        menu(
            AUTO_EXPOSURE,
            "Auto Exposure",
            &[(1, "Manual Mode"), (3, "Aperture Priority Mode")],
            ae_default,
            ae,
        ),
        node_control(
            EXPOSURE_TIME,
            "Exposure Time, Absolute",
            CTRL_TYPE_INTEGER,
            (3, 2047, 1),
            166,
            300,
            0,
        ),
        node_control(GAIN, "Gain", CTRL_TYPE_INTEGER, (0, 255, 1), 0, 130, 0),
        node_control(
            WB_AUTO,
            "White Balance, Automatic",
            CTRL_TYPE_BOOLEAN,
            (0, 1, 1),
            wb_default,
            wb,
            0,
        ),
        node_control(
            WB_TEMP,
            "White Balance Temperature",
            CTRL_TYPE_INTEGER,
            (2800, 6500, 1),
            4600,
            3000,
            0,
        ),
    ];
    build(&Inputs {
        camera: "auto",
        controls: &controls,
        xu: &[],
        stored: None,
        open: true,
        fixed_framerate: false,
        hidden: &[],
        usb: None,
    })
}

fn listed(model: &CameraSettings, name: &str) -> bool {
    let id = &model.by_name(name).unwrap().setting.id;
    recommended(model, &[], None, false)
        .iter()
        .any(|r| &r.setting == id)
}

#[test]
fn gain_is_not_recommended_while_auto_exposure_drives_it() {
    // Aperture priority now and by default: the camera owns the gain.
    let auto = auto_model(3, 3, 0, 0);
    assert!(!listed(&auto, "gain"));
    // Manual now and by default: the gain is the user's.
    let manual = auto_model(1, 1, 0, 0);
    assert!(listed(&manual, "gain"));
}

#[test]
fn a_ruled_setting_is_not_recommended_under_its_masters_automatic_value() {
    let auto = auto_model(3, 3, 1, 1);
    assert!(!listed(&auto, "exposure_time_absolute"));
    assert!(!listed(&auto, "white_balance_temperature"));
    let manual = auto_model(1, 1, 0, 0);
    assert!(listed(&manual, "exposure_time_absolute"));
    assert!(listed(&manual, "white_balance_temperature"));
}

#[test]
fn the_masters_target_decides_not_its_current_value() {
    // Manual now, aperture priority recommended: after the reset the
    // camera drives the dependents; the master's own row stays.
    let model = auto_model(3, 1, 1, 0);
    assert!(listed(&model, "auto_exposure"));
    assert!(listed(&model, "white_balance_automatic"));
    assert!(!listed(&model, "gain"));
    assert!(!listed(&model, "exposure_time_absolute"));
    assert!(!listed(&model, "white_balance_temperature"));
    // The other way: automatic now, manual recommended: they are listed.
    let model = auto_model(1, 3, 0, 1);
    assert!(listed(&model, "gain"));
    assert!(listed(&model, "exposure_time_absolute"));
    assert!(listed(&model, "white_balance_temperature"));
}

#[test]
fn the_apply_lists_keep_the_auto_driven_settings() {
    let model = auto_model(3, 3, 1, 1);
    let names: Vec<&str> = recommended_targets(&model, &[], None, false)
        .iter()
        .map(|(i, _)| model.entries[*i].name.as_str())
        .collect();
    for n in [
        "gain",
        "exposure_time_absolute",
        "white_balance_temperature",
        "auto_exposure",
    ] {
        assert!(names.contains(&n), "{n} in {names:?}");
    }
    // Rows are written whatever the camera drives.
    let writes = recommended_writes(&model, &[(GAIN, 10)], None, false);
    assert_eq!(writes.len(), 1);
    assert_eq!(model.entries[writes[0].0].name, "gain");
}
