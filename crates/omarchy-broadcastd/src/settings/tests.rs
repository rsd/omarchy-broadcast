//! The settings model over fake control sets (no device): the catalogue,
//! the rules and their reasons, effective values open and closed, the
//! inverted toggle, the generic fallback, the caps, validation, write order
//! and the tree diff.

use std::collections::{BTreeMap, BTreeSet};

use obc_pipeline::control_node::{MenuItem, MenuItemValue};
use obc_pipeline::controls::CtrlInfo;
use obc_pipeline::xu::FIELD_OF_VIEW;

use super::*;

fn ctrl(
    id: u32,
    name: &str,
    type_: u32,
    range: (i32, i32, i32),
    default: i32,
    value: i32,
) -> NodeControl {
    NodeControl {
        id,
        name: obc_pipeline::control_node::normalise_control_name(name),
        info: CtrlInfo {
            name: name.into(),
            minimum: range.0,
            maximum: range.1,
            step: range.2,
            default_value: default,
            flags: 0,
            type_,
        },
        value: if type_ == CTRL_TYPE_BUTTON {
            CtrlValue::None
        } else {
            CtrlValue::Value(value)
        },
        menu: Vec::new(),
    }
}

fn int(id: u32, name: &str, max: i32, default: i32, value: i32) -> NodeControl {
    ctrl(id, name, CTRL_TYPE_INTEGER, (0, max, 1), default, value)
}

fn boolean(id: u32, name: &str, default: bool, value: bool) -> NodeControl {
    ctrl(
        id,
        name,
        CTRL_TYPE_BOOLEAN,
        (0, 1, 1),
        default.into(),
        value.into(),
    )
}

fn menu(id: u32, name: &str, items: &[(u32, &str)], default: i32, value: i32) -> NodeControl {
    let max = items.iter().map(|(i, _)| *i).max().unwrap_or(0) as i32;
    let mut c = ctrl(id, name, CTRL_TYPE_MENU, (0, max, 1), default, value);
    c.menu = items
        .iter()
        .map(|(index, label)| MenuItem {
            index: *index,
            value: MenuItemValue::Name((*label).into()),
        })
        .collect();
    c
}

fn flagged(mut c: NodeControl, flags: u32) -> NodeControl {
    c.info.flags |= flags;
    c
}

/// The controls of an MX Brio as it reports them, with the values a real
/// one starts from.
fn brio() -> Vec<NodeControl> {
    vec![
        int(0x0098_0900, "Brightness", 255, 128, 128),
        int(0x0098_0901, "Contrast", 255, 128, 128),
        int(0x0098_0902, "Saturation", 255, 128, 128),
        boolean(0x0098_090c, "White Balance, Automatic", true, true),
        int(0x0098_0913, "Gain", 255, 0, 0),
        menu(
            0x0098_0918,
            "Power Line Frequency",
            &[(0, "Disabled"), (1, "50 Hz"), (2, "60 Hz")],
            2,
            2,
        ),
        flagged(
            ctrl(
                0x0098_091a,
                "White Balance Temperature",
                CTRL_TYPE_INTEGER,
                (2000, 7500, 1),
                5000,
                2975,
            ),
            CTRL_FLAG_INACTIVE,
        ),
        int(0x0098_091b, "Sharpness", 255, 128, 128),
        int(0x0098_091c, "Backlight Compensation", 1, 1, 1),
        menu(
            0x009a_0901,
            "Auto Exposure",
            &[(1, "Manual Mode"), (3, "Aperture Priority Mode")],
            3,
            3,
        ),
        flagged(
            ctrl(
                0x009a_0902,
                "Exposure Time, Absolute",
                CTRL_TYPE_INTEGER,
                (3, 2047, 1),
                156,
                1250,
            ),
            CTRL_FLAG_INACTIVE,
        ),
        boolean(0x009a_0903, "Exposure, Dynamic Framerate", false, true),
        ctrl(
            0x009a_0908,
            "Pan, Absolute",
            CTRL_TYPE_INTEGER,
            (-36000, 36000, 3600),
            0,
            14400,
        ),
        ctrl(
            0x009a_0909,
            "Tilt, Absolute",
            CTRL_TYPE_INTEGER,
            (-36000, 36000, 3600),
            0,
            -14400,
        ),
        flagged(
            ctrl(
                0x009a_090a,
                "Focus, Absolute",
                CTRL_TYPE_INTEGER,
                (0, 255, 5),
                0,
                40,
            ),
            CTRL_FLAG_INACTIVE,
        ),
        boolean(0x009a_090c, "Focus, Automatic Continuous", true, true),
        ctrl(
            0x009a_090d,
            "Zoom, Absolute",
            CTRL_TYPE_INTEGER,
            (100, 400, 1),
            100,
            100,
        ),
        // Never exposed.
        boolean(0x009a_0910, "Privacy", false, false),
        int(0x009a_0904, "Pan, Relative", 1, 0, 0),
        int(0x009a_090f, "Zoom, Continuous", 1, 0, 0),
    ]
}

/// The MX Brio's field of view as probed: 65° now, 90° by default.
fn fov(current: u8) -> XuReading {
    XuReading {
        row: &FIELD_OF_VIEW,
        state: XuState {
            info: 0x03,
            current,
            current_option: FIELD_OF_VIEW.option_for_value(current),
            default_raw: Some(0),
            default: &FIELD_OF_VIEW.options[0],
        },
    }
}

fn stored(values: &[(&str, i32)]) -> CameraControlState {
    CameraControlState {
        values: values
            .iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect::<BTreeMap<_, _>>(),
        reset_all: false,
        reset_names: BTreeSet::new(),
    }
}

struct Case {
    controls: Vec<NodeControl>,
    xu: Vec<XuReading>,
    stored: Option<CameraControlState>,
    open: bool,
    fixed: bool,
}

impl Case {
    fn brio() -> Case {
        Case {
            controls: brio(),
            xu: vec![fov(0x02)],
            stored: None,
            open: false,
            fixed: false,
        }
    }

    fn of(controls: Vec<NodeControl>) -> Case {
        Case {
            controls,
            xu: Vec::new(),
            stored: None,
            open: false,
            fixed: false,
        }
    }

    fn build(&self) -> CameraSettings {
        build(&Inputs {
            camera: "usb-brio",
            controls: &self.controls,
            xu: &self.xu,
            stored: self.stored.as_ref(),
            open: self.open,
            fixed_framerate: self.fixed,
        })
    }
}

fn setting<'a>(s: &'a CameraSettings, id: &str) -> &'a Setting {
    &s.get(id)
        .unwrap_or_else(|| panic!("no {id} in {:?}", ids(s)))
        .setting
}

fn ids(s: &CameraSettings) -> Vec<&str> {
    s.entries.iter().map(|e| e.setting.id.as_str()).collect()
}

fn value(s: &CameraSettings, id: &str) -> Option<SettingValue> {
    setting(s, id).kind.value()
}

fn greyed(s: &CameraSettings, id: &str) -> Option<String> {
    let st = setting(s, id);
    if st.enabled {
        assert_eq!(st.disabled_reason, "", "{id}");
        None
    } else {
        assert!(!st.disabled_reason.is_empty(), "{id}");
        Some(st.disabled_reason.clone())
    }
}

fn str_value(v: &str) -> Option<SettingValue> {
    Some(SettingValue::Str(v.into()))
}

#[test]
fn the_brio_tree_has_its_groups_in_order_and_leaves_out_what_is_never_exposed() {
    let s = Case::brio().build();
    let groups: Vec<(String, String)> = s.groups().into_iter().map(|g| (g.id, g.label)).collect();
    assert_eq!(
        groups,
        [
            ("image".into(), "Image".into()),
            ("white_balance".into(), "White balance".into()),
            ("exposure".into(), "Exposure".into()),
            ("focus".into(), "Focus & zoom".into()),
        ]
    );
    assert_eq!(
        ids(&s),
        [
            "camera.image.brightness",
            "camera.image.contrast",
            "camera.image.saturation",
            "camera.image.sharpness",
            "camera.image.backlight_compensation",
            "camera.white_balance.white_balance_automatic",
            "camera.white_balance.white_balance_temperature",
            "camera.exposure.auto_exposure",
            "camera.exposure.exposure_time_absolute",
            "camera.exposure.gain",
            "camera.exposure.keep_framerate",
            "camera.exposure.power_line_frequency",
            "camera.focus.focus_automatic_continuous",
            "camera.focus.focus_absolute",
            "camera.focus.zoom_absolute",
            "camera.focus.pan_absolute",
            "camera.focus.tilt_absolute",
            "camera.focus.field_of_view",
        ]
    );
    assert!(s.notes.is_empty(), "{:?}", s.notes);
    // Types, labels, units and hints.
    let sharp = setting(&s, "camera.image.sharpness");
    assert_eq!(sharp.label, "Sharpness");
    assert_eq!(sharp.scope, "camera");
    assert_eq!(sharp.hint.as_deref(), Some("slider"));
    assert_eq!(
        sharp.kind,
        SettingKind::Int {
            min: 0,
            max: 255,
            step: 1,
            default: 128,
            value: 128,
            unit: None
        }
    );
    let temp = setting(&s, "camera.white_balance.white_balance_temperature");
    assert!(matches!(&temp.kind, SettingKind::Int { unit: Some(u), .. } if u == "K"));
    let exp = setting(&s, "camera.exposure.exposure_time_absolute");
    assert!(matches!(&exp.kind, SettingKind::Int { unit: Some(u), .. } if u == "×100 µs"));
    let awb = setting(&s, "camera.white_balance.white_balance_automatic");
    assert_eq!(awb.hint.as_deref(), Some("switch"));
    assert_eq!(
        awb.kind,
        SettingKind::Bool {
            default: true,
            value: true
        }
    );
    // The FoV row: the device's byte through the row's options.
    let f = setting(&s, "camera.focus.field_of_view");
    assert_eq!(f.label, "Field of view");
    assert_eq!(f.hint.as_deref(), Some("segmented"));
    assert_eq!(
        f.kind,
        SettingKind::Enum {
            options: vec![
                ("90".into(), "90°".into()),
                ("78".into(), "78°".into()),
                ("65".into(), "65°".into()),
            ],
            default: "90".into(),
            value: "65".into(),
        }
    );
    assert!(f.enabled);
    let e = s.get("camera.focus.field_of_view").unwrap();
    assert_eq!(e.target, Target::Xu(&FIELD_OF_VIEW));
    assert_eq!(e.name, "field_of_view");
    // The watcher subscribes to the published controls only.
    let watched = s.control_ids();
    assert_eq!(watched.len(), 17);
    assert!(!watched.contains(&0x009a_0910), "privacy");
}

#[test]
fn enum_options_are_exactly_the_items_the_device_reports() {
    let s = Case::brio().build();
    // The MX Brio's auto_exposure has items 1 and 3 only: item 3 is its
    // automatic mode, labelled Auto.
    assert_eq!(
        setting(&s, "camera.exposure.auto_exposure").kind,
        SettingKind::Enum {
            options: vec![
                ("manual".into(), "Manual".into()),
                ("aperture_priority".into(), "Auto".into()),
            ],
            default: "aperture_priority".into(),
            value: "aperture_priority".into(),
        }
    );
    assert_eq!(
        setting(&s, "camera.exposure.power_line_frequency").kind,
        SettingKind::Enum {
            options: vec![
                ("disabled".into(), "Off".into()),
                ("50hz".into(), "50 Hz".into()),
                ("60hz".into(), "60 Hz".into()),
            ],
            default: "60hz".into(),
            value: "60hz".into(),
        }
    );
    // With item 0, item 3 is aperture priority; an item the catalogue does
    // not know is m<index> with the device's label, sanitised.
    let s = Case::of(vec![menu(
        0x009a_0901,
        "Auto Exposure",
        &[
            (0, "Auto Mode"),
            (1, "Manual Mode"),
            (3, "Aperture Priority Mode"),
            (5, "Night\u{202e} \t Mode"),
        ],
        0,
        5,
    )])
    .build();
    let SettingKind::Enum { options, value, .. } =
        &setting(&s, "camera.exposure.auto_exposure").kind
    else {
        panic!()
    };
    assert_eq!(
        options,
        &[
            ("auto".to_string(), "Auto".to_string()),
            ("manual".into(), "Manual".into()),
            ("aperture_priority".into(), "Aperture priority".into()),
            ("m5".into(), "Night Mode".into()),
        ]
    );
    assert_eq!(value, "m5");
}

#[test]
fn exposure_time_follows_the_exposure_mode_with_its_reason() {
    const REASON: &str = "Set exposure to manual to change this";
    let id = "camera.exposure.exposure_time_absolute";
    // Closed, auto mode: greyed with the catalogue's reason, and its
    // condition published.
    let s = Case::brio().build();
    assert_eq!(greyed(&s, id).as_deref(), Some(REASON));
    assert_eq!(
        setting(&s, id).enabled_when,
        [Condition {
            id: "camera.exposure.auto_exposure".into(),
            op: ConditionOp::In,
            value: SettingValue::List(vec![
                SettingValue::Str("manual".into()),
                SettingValue::Str("shutter_priority".into())
            ]),
        }]
    );
    assert!(
        s.get("camera.exposure.auto_exposure")
            .unwrap()
            .updates_others
    );
    assert!(!s.get("camera.image.sharpness").unwrap().updates_others);
    // Closed with manual stored: enabled, the device's inactive flag
    // ignored (the stored values decide what the camera will be).
    let mut c = Case::brio();
    c.stored = Some(stored(&[("auto_exposure", 1)]));
    let s = c.build();
    assert_eq!(
        value(&s, "camera.exposure.auto_exposure"),
        str_value("manual")
    );
    assert!(setting(&s, "camera.exposure.auto_exposure").stored);
    assert_eq!(greyed(&s, id), None);
    // Open: the device's values; inactive greys it even with the rule
    // holding (live truth), with the rule's reason.
    c.open = true;
    c.controls[9].value = CtrlValue::Value(1);
    let s = c.build();
    assert_eq!(
        value(&s, "camera.exposure.auto_exposure"),
        str_value("manual")
    );
    assert_eq!(greyed(&s, id).as_deref(), Some(REASON));
    c.controls[10].info.flags = 0;
    let s = c.build();
    assert_eq!(greyed(&s, id), None);
    // Back to the automatic mode on the device: greyed again.
    c.controls[9].value = CtrlValue::Value(3);
    assert_eq!(greyed(&c.build(), id).as_deref(), Some(REASON));
}

#[test]
fn white_balance_and_focus_follow_their_automatic_switches() {
    const WB: &str = "Turn off automatic white balance to change this";
    const FOCUS: &str = "Turn off continuous autofocus to change this";
    let mut c = Case::brio();
    c.controls
        .push(int(0x0098_090e, "Red Balance", 255, 128, 128));
    c.controls
        .push(int(0x0098_090f, "Blue Balance", 255, 128, 128));
    let s = c.build();
    for id in [
        "camera.white_balance.white_balance_temperature",
        "camera.white_balance.red_balance",
        "camera.white_balance.blue_balance",
    ] {
        assert_eq!(greyed(&s, id).as_deref(), Some(WB), "{id}");
        assert_eq!(
            setting(&s, id).enabled_when,
            [Condition {
                id: "camera.white_balance.white_balance_automatic".into(),
                op: ConditionOp::Eq,
                value: SettingValue::Bool(false),
            }]
        );
    }
    assert_eq!(
        greyed(&s, "camera.focus.focus_absolute").as_deref(),
        Some(FOCUS)
    );
    // Automatic white balance stored off: enabled while closed.
    c.stored = Some(stored(&[
        ("white_balance_automatic", 0),
        ("focus_automatic_continuous", 0),
    ]));
    let s = c.build();
    assert_eq!(
        greyed(&s, "camera.white_balance.white_balance_temperature"),
        None
    );
    assert_eq!(greyed(&s, "camera.focus.focus_absolute"), None);
    // Without its master the rule is dropped: the control is a master
    // itself, greyed only by its own flags.
    let s = Case::of(vec![flagged(
        int(0x009a_090a, "Focus, Absolute", 255, 0, 0),
        CTRL_FLAG_INACTIVE,
    )])
    .build();
    let f = setting(&s, "camera.focus.focus_absolute");
    assert!(f.enabled_when.is_empty());
    assert_eq!(
        greyed(&s, "camera.focus.focus_absolute").as_deref(),
        Some(REASON_INACTIVE)
    );
    // A master of another type than the rule's drops the rule too.
    let s = Case::of(vec![
        int(0x009a_0901, "Auto Exposure", 3, 3, 3),
        int(0x009a_0902, "Exposure Time, Absolute", 2047, 156, 156),
    ])
    .build();
    assert!(
        setting(&s, "camera.exposure.exposure_time_absolute")
            .enabled_when
            .is_empty()
    );
}

#[test]
fn flags_grey_a_setting_with_their_reason() {
    let mut c = Case::of(vec![
        flagged(
            int(0x0098_0900, "Brightness", 255, 128, 1),
            CTRL_FLAG_READ_ONLY,
        ),
        flagged(
            int(0x0098_0901, "Contrast", 255, 128, 1),
            CTRL_FLAG_DISABLED,
        ),
        flagged(
            int(0x0098_0902, "Saturation", 255, 128, 1),
            CTRL_FLAG_GRABBED,
        ),
        flagged(
            int(0x0098_091b, "Sharpness", 255, 128, 1),
            CTRL_FLAG_INACTIVE,
        ),
        int(0x0098_0910, "Gamma", 500, 100, 100),
    ]);
    for open in [false, true] {
        c.open = open;
        let s = c.build();
        assert_eq!(
            greyed(&s, "camera.image.brightness").as_deref(),
            Some(REASON_FIXED)
        );
        assert_eq!(
            greyed(&s, "camera.image.contrast").as_deref(),
            Some(REASON_FIXED)
        );
        assert_eq!(
            greyed(&s, "camera.image.saturation").as_deref(),
            Some(REASON_GRABBED)
        );
        // Without a rule, the inactive flag greys it open or closed.
        assert_eq!(
            greyed(&s, "camera.image.sharpness").as_deref(),
            Some(REASON_INACTIVE)
        );
        assert_eq!(greyed(&s, "camera.image.gamma"), None);
    }
    // An unknown field of view byte.
    let mut c = Case::brio();
    c.xu = vec![fov(0x07)];
    let s = c.build();
    assert_eq!(
        greyed(&s, "camera.focus.field_of_view").as_deref(),
        Some(REASON_XU_UNKNOWN)
    );
    assert_eq!(value(&s, "camera.focus.field_of_view"), str_value("m7"));
}

#[test]
fn effective_values_open_and_closed() {
    let mut c = Case::brio();
    let sharp = "camera.image.sharpness";
    let int_of = |s: &CameraSettings, id| match value(s, id) {
        Some(SettingValue::Int(v)) => v,
        other => panic!("{other:?}"),
    };
    // Nothing stored: the device's value.
    c.controls[7].value = CtrlValue::Value(255);
    assert_eq!(int_of(&c.build(), sharp), 255);
    // Stored: what it will be at the open.
    c.stored = Some(stored(&[("sharpness", 100)]));
    let s = c.build();
    assert_eq!(int_of(&s, sharp), 100);
    assert!(setting(&s, sharp).stored);
    // Open: the device's, still stored.
    c.open = true;
    let s = c.build();
    assert_eq!(int_of(&s, sharp), 255);
    assert!(setting(&s, sharp).stored);
    // A whole reset pending: the defaults, unless stored after it.
    c.open = false;
    let mut st = stored(&[("contrast", 90)]);
    st.reset_all = true;
    c.stored = Some(st);
    let s = c.build();
    assert_eq!(int_of(&s, sharp), 128);
    assert_eq!(int_of(&s, "camera.image.contrast"), 90);
    assert_eq!(value(&s, "camera.focus.field_of_view"), str_value("90"));
    // A reset of some names.
    let mut st = stored(&[]);
    st.reset_names.insert("sharpness".into());
    c.stored = Some(st);
    let s = c.build();
    assert_eq!(int_of(&s, sharp), 128);
    assert!(!setting(&s, sharp).stored);
    assert_eq!(value(&s, "camera.focus.field_of_view"), str_value("65"));
    // The FoV stored.
    c.stored = Some(stored(&[("field_of_view", 1)]));
    assert_eq!(
        value(&c.build(), "camera.focus.field_of_view"),
        str_value("78")
    );
}

#[test]
fn keep_framerate_is_the_inverted_dynamic_framerate() {
    let id = "camera.exposure.keep_framerate";
    let mut c = Case::brio();
    // The device has 1 (dynamic): keep off; its default 0: keep on.
    let s = c.build();
    let k = setting(&s, id);
    assert_eq!(k.label, "Keep 30 fps in low light");
    assert_eq!(k.hint.as_deref(), Some("switch"));
    assert_eq!(
        k.kind,
        SettingKind::Bool {
            default: true,
            value: false
        }
    );
    let e = s.get(id).unwrap();
    assert_eq!(e.name, "exposure_dynamic_framerate");
    // Setting it on writes raw 0, off raw 1.
    assert_eq!(
        s.check(id, Some(&SettingValue::Bool(true))).map(|(_, r)| r),
        Ok(0)
    );
    assert_eq!(
        s.check(id, Some(&SettingValue::Bool(false)))
            .map(|(_, r)| r),
        Ok(1)
    );
    // Stored raw 0 (keep on) and raw 1 (keep off).
    c.stored = Some(stored(&[("exposure_dynamic_framerate", 0)]));
    assert_eq!(value(&c.build(), id), Some(SettingValue::Bool(true)));
    c.stored = Some(stored(&[("exposure_dynamic_framerate", 1)]));
    assert_eq!(value(&c.build(), id), Some(SettingValue::Bool(false)));
    // The config's pin with nothing stored: what the open will make it,
    // also over a pending reset (the open leaves the pin alone).
    c.stored = None;
    c.fixed = true;
    assert_eq!(value(&c.build(), id), Some(SettingValue::Bool(true)));
    let mut st = stored(&[]);
    st.reset_all = true;
    c.stored = Some(st);
    assert_eq!(value(&c.build(), id), Some(SettingValue::Bool(true)));
    c.stored = None;
    // Open: the device's (pinned or not).
    c.open = true;
    assert_eq!(value(&c.build(), id), Some(SettingValue::Bool(false)));
    // A default of 1 (another camera): default off.
    let s = Case::of(vec![boolean(
        0x009a_0903,
        "Exposure, Dynamic Framerate",
        true,
        false,
    )])
    .build();
    assert_eq!(
        setting(&s, id).kind,
        SettingKind::Bool {
            default: false,
            value: true
        }
    );
}

#[test]
fn unknown_controls_fall_back_to_other_and_bad_names_are_left_out() {
    let mut weird = int(0x0098_0a00, "LED\u{200b}  Brightness\u{7}", 100, 50, 60);
    weird.name = "led_brightness".into();
    let mut int_menu = ctrl(
        0x009a_0a01,
        "Sensor Mode",
        CTRL_TYPE_INTEGER_MENU,
        (0, 2, 1),
        0,
        1,
    );
    int_menu.menu = vec![
        MenuItem {
            index: 0,
            value: MenuItemValue::Integer(24),
        },
        MenuItem {
            index: 2,
            value: MenuItemValue::Integer(60),
        },
    ];
    let mut digit = int(0x0098_0a02, "3D Noise", 1, 0, 0);
    digit.name = "3d_noise".into();
    let mut clash = int(0x0098_0a03, "Sharpness", 9, 0, 0);
    clash.name = "sharpness".into();
    let s = Case::of(vec![
        int(0x0098_091b, "Sharpness", 255, 128, 128),
        weird,
        menu(
            0x0098_0a04,
            "Colour Effect",
            &[(0, "None"), (1, "\u{2066}Sepia\u{2069}"), (2, "")],
            0,
            1,
        ),
        int_menu,
        digit,
        clash,
        ctrl(
            0x0098_0a05,
            "Do Something",
            CTRL_TYPE_BUTTON,
            (0, 0, 0),
            0,
            0,
        ),
        // Another class: never published.
        int(0x009e_0901, "Vertical Blanking", 10, 0, 0),
        // Focus's automatic switch is no motion control.
        boolean(0x009a_090c, "Focus, Automatic Continuous", true, true),
    ])
    .build();
    assert_eq!(
        ids(&s),
        [
            "camera.image.sharpness",
            "camera.focus.focus_automatic_continuous",
            "camera.other.led_brightness",
            "camera.other.colour_effect",
            "camera.other.sensor_mode",
            "camera.other.do_something",
        ]
    );
    let led = setting(&s, "camera.other.led_brightness");
    assert_eq!(led.label, "LED Brightness");
    assert!(led.enabled_when.is_empty());
    assert_eq!(
        setting(&s, "camera.other.colour_effect").kind,
        SettingKind::Enum {
            options: vec![
                ("m0".into(), "None".into()),
                ("m1".into(), "Sepia".into()),
                ("m2".into(), "m2".into()),
            ],
            default: "m0".into(),
            value: "m1".into(),
        }
    );
    assert_eq!(
        setting(&s, "camera.other.sensor_mode").kind,
        SettingKind::Enum {
            options: vec![("m0".into(), "24".into()), ("m2".into(), "60".into())],
            default: "m0".into(),
            value: "m1".into(),
        }
    );
    let action = setting(&s, "camera.other.do_something");
    assert_eq!(action.kind, SettingKind::Action);
    assert_eq!(action.hint, None);
    assert!(!action.stored);
    let notes = s.notes.join("\n");
    assert!(
        notes.contains("\"3D Noise\" (0x00980a02) has no storable name"),
        "{notes}"
    );
    assert!(
        notes.contains("sharpness (0x00980a03) has the name of a known control but another id"),
        "{notes}"
    );
    assert_eq!(s.notes.len(), 2, "{notes}");
}

#[test]
fn the_caps_cut_settings_and_options() {
    let mut controls: Vec<NodeControl> = (0..70)
        .map(|i| int(0x0098_0b00 + i, &format!("Extra {i}"), 10, 0, 0))
        .collect();
    let items: Vec<(u32, String)> = (0..20).map(|i| (i, format!("Item {i}"))).collect();
    let items: Vec<(u32, &str)> = items.iter().map(|(i, s)| (*i, s.as_str())).collect();
    controls.insert(0, menu(0x0098_0a10, "Big Menu", &items, 0, 0));
    let s = Case::of(controls).build();
    assert_eq!(s.entries.len(), MAX_SETTINGS);
    assert_eq!(s.entries[0].setting.id, "camera.other.big_menu");
    let SettingKind::Enum { options, .. } = &s.entries[0].setting.kind else {
        panic!()
    };
    assert_eq!(options.len(), MAX_OPTIONS);
    assert_eq!(options[15].0, "m15");
    let notes = s.notes.join("\n");
    assert!(
        notes.contains("big_menu has 20 options, more than the 16"),
        "{notes}"
    );
    assert!(
        notes.contains("71 settings found, more than the 64"),
        "{notes}"
    );
    // A label longer than the bound is cut on a character boundary.
    let long = "é".repeat(40);
    let s = Case::of(vec![int(0x0098_0b00, &format!("x{long}"), 1, 0, 0)]).build();
    assert!(s.entries[0].setting.label.len() <= LABEL_MAX);
    assert!(s.entries[0].setting.label.starts_with("xé"));
    // A menu item's label, at the CLI's bound of 32 bytes.
    let item = format!("x{long}");
    let s = Case::of(vec![menu(0x0098_0a10, "Big Menu", &[(0, &item)], 0, 0)]).build();
    let SettingKind::Enum { options, .. } = &s.entries[0].setting.kind else {
        panic!()
    };
    assert_eq!(OPTION_LABEL_MAX, 32);
    assert!(options[0].1.len() <= OPTION_LABEL_MAX, "{options:?}");
    assert!(
        options[0].1.len() > OPTION_LABEL_MAX - 2,
        "cut, not dropped"
    );
    assert!(options[0].1.starts_with("xé"));
}

#[test]
fn validation_refuses_in_order_and_names_why() {
    let mut c = Case::brio();
    c.controls.push(ctrl(
        0x009a_0906,
        "Pan, Reset",
        CTRL_TYPE_BUTTON,
        (0, 0, 0),
        0,
        0,
    ));
    let s = c.build();
    let check = |id: &str, v: Option<SettingValue>| s.check(id, v.as_ref());
    // Unknown first, then disabled (whatever the value), then invalid.
    assert!(matches!(
        check("camera.image.nope", Some(SettingValue::Int(1))),
        Err(Refusal::Unknown(m)) if m.contains("camera.image.nope") && m.contains("usb-brio")
    ));
    assert_eq!(
        check("camera.exposure.exposure_time_absolute", None),
        Err(Refusal::Disabled(
            "Set exposure to manual to change this".into()
        ))
    );
    let invalid = |r: Result<(usize, i32), Refusal>| match r {
        Err(Refusal::Invalid(m)) => m,
        other => panic!("expected invalid, got {other:?}"),
    };
    let m = invalid(check(
        "camera.image.sharpness",
        Some(SettingValue::Int(256)),
    ));
    assert_eq!(
        m,
        "camera.image.sharpness: 256 is outside its range 0..=255"
    );
    let m = invalid(check(
        "camera.focus.pan_absolute",
        Some(SettingValue::Int(100)),
    ));
    assert!(
        m.contains("not a whole number of steps of 3600 from -36000"),
        "{m}"
    );
    let m = invalid(check(
        "camera.image.sharpness",
        Some(SettingValue::Bool(true)),
    ));
    assert!(m.contains("expects an integer"), "{m}");
    let m = invalid(check("camera.image.sharpness", None));
    assert!(m.contains("expects an integer"), "{m}");
    let m = invalid(check(
        "camera.white_balance.white_balance_automatic",
        Some(SettingValue::Int(1)),
    ));
    assert!(m.contains("expects a boolean"), "{m}");
    let m = invalid(check(
        "camera.exposure.auto_exposure",
        Some(SettingValue::Str("auto".into())),
    ));
    assert!(
        m.contains("\"auto\" is not one of its options (manual, aperture_priority)"),
        "{m}"
    );
    let m = invalid(check(
        "camera.exposure.auto_exposure",
        Some(SettingValue::Int(1)),
    ));
    assert!(m.contains("expects an option id"), "{m}");
    let m = invalid(check(
        "camera.focus.pan_reset",
        Some(SettingValue::Bool(false)),
    ));
    assert!(m.contains("fired with the boolean true"), "{m}");
    // What is accepted, as the raw value.
    let raw = |id: &str, v: SettingValue| check(id, Some(v)).map(|(_, r)| r);
    assert_eq!(raw("camera.image.sharpness", SettingValue::Int(0)), Ok(0));
    assert_eq!(
        raw(
            "camera.focus.pan_absolute",
            SettingValue::Int(-36000 + 7200)
        ),
        Ok(-28800)
    );
    assert_eq!(
        raw(
            "camera.exposure.auto_exposure",
            SettingValue::Str("manual".into())
        ),
        Ok(1)
    );
    assert_eq!(
        raw(
            "camera.white_balance.white_balance_automatic",
            SettingValue::Bool(false)
        ),
        Ok(0)
    );
    assert_eq!(
        raw("camera.focus.field_of_view", SettingValue::Str("78".into())),
        Ok(1)
    );
    assert_eq!(
        raw("camera.focus.pan_reset", SettingValue::Bool(true)),
        Ok(1)
    );
    // No camera at all: unknown.
    assert!(matches!(
        CameraSettings::none().check("camera.image.sharpness", None),
        Err(Refusal::Unknown(m)) if m.contains("no camera")
    ));
}

#[test]
fn masters_are_written_first_and_reset_last() {
    let s = Case::brio().build();
    let idx = |id: &str| s.index(id).unwrap();
    let all = [
        idx("camera.exposure.exposure_time_absolute"),
        idx("camera.image.sharpness"),
        idx("camera.exposure.auto_exposure"),
        idx("camera.focus.focus_absolute"),
    ];
    let names = |order: Vec<usize>| -> Vec<&str> {
        order.iter().map(|&i| s.entries[i].name.as_str()).collect()
    };
    assert_eq!(
        names(s.apply_order(&all)),
        [
            "sharpness",
            "auto_exposure",
            "exposure_time_absolute",
            "focus_absolute"
        ]
    );
    assert_eq!(
        names(s.reset_order(&all)),
        [
            "exposure_time_absolute",
            "focus_absolute",
            "sharpness",
            "auto_exposure"
        ]
    );
}

#[test]
fn a_value_change_is_a_delta_and_a_new_shape_is_structural() {
    let mut c = Case::brio();
    let a = c.build();
    assert_eq!(diff(&a, &a), None);
    // Manual mode: the mode and the exposure time it enables, in one delta.
    c.controls[9].value = CtrlValue::Value(1);
    c.controls[10].info.flags = 0;
    c.open = true;
    let b = c.build();
    let d = diff(&a, &b).expect("a delta");
    assert!(!d.structural);
    assert_eq!(
        d.changes,
        [
            SettingChange {
                id: "camera.exposure.auto_exposure".into(),
                value: str_value("manual"),
                stored: false,
                enabled: true,
                disabled_reason: String::new(),
            },
            SettingChange {
                id: "camera.exposure.exposure_time_absolute".into(),
                value: Some(SettingValue::Int(1250)),
                stored: false,
                enabled: true,
                disabled_reason: String::new(),
            },
        ]
    );
    // A stored flag alone is a delta.
    c.stored = Some(stored(&[("gain", 0)]));
    let d = diff(&b, &c.build()).expect("a delta");
    assert_eq!(d.changes.len(), 1);
    assert!(d.changes[0].stored);
    // A range, a label source, a new control, another camera: structural.
    let mut wider = c.clone_controls();
    wider[0].info.maximum = 511;
    let mut c2 = Case::of(wider);
    c2.xu = vec![fov(2)];
    c2.open = true;
    c2.stored = c.stored.clone();
    let d = diff(&c.build(), &c2.build()).expect("a delta");
    assert_eq!(
        d,
        Delta {
            structural: true,
            changes: Vec::new()
        }
    );
    let mut more = c.clone_controls();
    more.push(int(0x0098_0910, "Gamma", 500, 100, 100));
    let mut c3 = Case::of(more);
    c3.xu = vec![fov(2)];
    c3.open = true;
    c3.stored = c.stored.clone();
    assert!(diff(&c.build(), &c3.build()).unwrap().structural);
    assert!(
        diff(&c.build(), &CameraSettings::none())
            .unwrap()
            .structural
    );
    // The tree carries the revision and serial given.
    let t = c.build().tree(7, 3);
    assert_eq!(
        (t.revision, t.serial, t.camera.as_str()),
        (7, 3, "usb-brio")
    );
    assert_eq!(t.groups.len(), 4);
}

impl Case {
    fn clone_controls(&self) -> Vec<NodeControl> {
        self.controls.clone()
    }
}

#[test]
fn sanitise_strips_what_cannot_be_shown() {
    assert_eq!(sanitise("  a\u{0}b \u{9f}c\n\t d  ", 48), "ab c d");
    assert_eq!(
        sanitise("\u{202a}x\u{202e}\u{2066}y\u{2069}\u{200d}z", 48),
        "xyz"
    );
    assert_eq!(sanitise("abc def", 5), "abc d");
    assert_eq!(sanitise("abcd ef", 5), "abcd");
    assert_eq!(sanitise("ééé", 5), "éé");
    assert_eq!(sanitise("\u{7}\u{1b}", 48), "");
}
