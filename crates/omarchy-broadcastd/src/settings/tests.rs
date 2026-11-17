//! The settings model over fake control sets (no device): the catalogue,
//! the rules and their reasons, effective values open and closed, the
//! inverted toggle, the generic fallback, the caps, validation, write order
//! and the tree diff.

use std::collections::{BTreeMap, BTreeSet};

use obc_pipeline::control_node::{MenuItem, MenuItemValue};
use obc_pipeline::controls::CtrlInfo;
use obc_pipeline::xu::{
    EXPOSURE_COMPENSATION, FIELD_OF_VIEW, SHOW_MODE, TEMPERATURE_COMPENSATION, VIBRANCE,
};

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
            default: FIELD_OF_VIEW.fallback_default(),
        },
    }
}

/// An Extension Unit row as probed, with its `current` and `default`
/// bytes.
fn reading(row: &'static XuRow, current: u8, default: u8) -> XuReading {
    XuReading {
        row,
        state: XuState {
            info: 0x03,
            current,
            current_option: row.option_for_value(current),
            default_raw: Some(default),
            default: row
                .option_for_value(default)
                .unwrap_or_else(|| row.fallback_default()),
        },
    }
}

/// The MX Brio's rows as Logi Tune last left them: field of view 65°, Show
/// Mode on, exposure compensation 0 EV, vibrance 128, white-balance offset
/// +280 K.
fn mx_brio_xu() -> Vec<XuReading> {
    vec![
        fov(0x02),
        reading(&SHOW_MODE, 1, 0),
        reading(&EXPOSURE_COMPENSATION, 0x80, 0x80),
        reading(&VIBRANCE, 0x80, 0x80),
        reading(&TEMPERATURE_COMPENSATION, 0x39, 0x32),
    ]
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
    hidden: Vec<u32>,
    usb: Option<UsbId>,
}

impl Case {
    fn brio() -> Case {
        Case {
            controls: brio(),
            xu: vec![fov(0x02)],
            stored: None,
            open: false,
            fixed: false,
            hidden: Vec::new(),
            usb: None,
        }
    }

    fn of(controls: Vec<NodeControl>) -> Case {
        Case {
            controls,
            xu: Vec::new(),
            stored: None,
            open: false,
            fixed: false,
            hidden: Vec::new(),
            usb: None,
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
            hidden: &self.hidden,
            usb: self.usb,
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
        assert_eq!(st.disabled_kind, "", "{id}");
        None
    } else {
        assert!(!st.disabled_reason.is_empty(), "{id}");
        assert!(disabled::is_known(&st.disabled_kind), "{id}: {st:?}");
        Some(st.disabled_reason.clone())
    }
}

/// The `disabled_kind` of setting `id`.
fn kind_of(s: &CameraSettings, id: &str) -> String {
    setting(s, id).disabled_kind.clone()
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
            ("focus".into(), "Framing & focus".into()),
            ("image".into(), "Image".into()),
            ("white_balance".into(), "White balance".into()),
            ("exposure".into(), "Exposure".into()),
        ]
    );
    // Framing first: the field of view, then zoom.
    assert_eq!(
        ids(&s),
        [
            "camera.focus.field_of_view",
            "camera.focus.zoom_absolute",
            "camera.focus.focus_automatic_continuous",
            "camera.focus.focus_absolute",
            "camera.focus.pan_absolute",
            "camera.focus.tilt_absolute",
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
            unit: None,
            display: None,
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
    // The rule holds, the device says not now.
    assert_eq!(kind_of(&s, id), disabled::BUSY);
    c.controls[10].info.flags = 0;
    let s = c.build();
    assert_eq!(greyed(&s, id), None);
    // Back to the automatic mode on the device: greyed again, by the rule.
    c.controls[9].value = CtrlValue::Value(3);
    let s = c.build();
    assert_eq!(greyed(&s, id).as_deref(), Some(REASON));
    assert_eq!(kind_of(&s, id), disabled::PARENT_OFF);
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
        assert_eq!(kind_of(&s, id), disabled::PARENT_OFF, "{id}");
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
    assert_eq!(kind_of(&s, "camera.focus.focus_absolute"), disabled::BUSY);
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
        // Fixed by the device: `device`; for now: `busy`.
        for (id, kind) in [
            ("camera.image.brightness", disabled::DEVICE),
            ("camera.image.contrast", disabled::DEVICE),
            ("camera.image.saturation", disabled::BUSY),
            ("camera.image.sharpness", disabled::BUSY),
            ("camera.image.gamma", ""),
        ] {
            assert_eq!(kind_of(&s, id), kind, "{id} open {open}");
        }
    }
    // An unknown field of view byte.
    let mut c = Case::brio();
    c.xu = vec![fov(0x07)];
    let s = c.build();
    assert_eq!(
        greyed(&s, "camera.focus.field_of_view").as_deref(),
        Some(REASON_XU_UNKNOWN)
    );
    assert_eq!(kind_of(&s, "camera.focus.field_of_view"), disabled::DEVICE);
    assert_eq!(value(&s, "camera.focus.field_of_view"), str_value("m7"));
}

/// The controls of a Generalplus GENERAL WEBCAM (1b3f:2002) as it reports
/// them: brightness, contrast and hue that change nothing, an
/// `auto_exposure` the kernel left without an item (it could not query
/// it), and an exposure time the device flags inactive for good.
fn generalplus() -> Vec<NodeControl> {
    vec![
        int(0x0098_0900, "Brightness", 255, 128, 128),
        int(0x0098_0901, "Contrast", 255, 128, 128),
        int(0x0098_0902, "Saturation", 255, 128, 128),
        ctrl(0x0098_0903, "Hue", CTRL_TYPE_INTEGER, (-180, 180, 1), 0, 0),
        int(0x0098_091b, "Sharpness", 15, 7, 7),
        menu(0x009a_0901, "Auto Exposure", &[], 0, 0),
        flagged(
            ctrl(
                0x009a_0902,
                "Exposure Time, Absolute",
                CTRL_TYPE_INTEGER,
                (1, 5000, 1),
                156,
                156,
            ),
            CTRL_FLAG_INACTIVE,
        ),
    ]
}

/// The controls of a Microsoft LifeCam VX-800 (045e:0766) that matter
/// here: its manual white balance breaks the picture.
fn vx800() -> Vec<NodeControl> {
    vec![
        int(0x0098_0900, "Brightness", 255, 128, 128),
        boolean(0x0098_090c, "White Balance, Automatic", true, true),
        flagged(
            ctrl(
                0x0098_091a,
                "White Balance Temperature",
                CTRL_TYPE_INTEGER,
                (2800, 10000, 1),
                4000,
                4000,
            ),
            CTRL_FLAG_INACTIVE,
        ),
    ]
}

fn quirk_hidden(vendor: u16, product: u16) -> Vec<u32> {
    obc_pipeline::quirks::hidden_controls(Some(obc_core::UsbId { vendor, product }))
}

#[test]
fn a_quirk_row_hides_its_controls_and_an_empty_menu_is_absent() {
    const NO_EXPOSURE: &str = "The camera does not allow its exposure to be set";
    let exposure = "camera.exposure.exposure_time_absolute";
    let mut c = Case::of(generalplus());
    c.hidden = quirk_hidden(0x1b3f, 0x2002);
    c.stored = Some(stored(&[
        ("brightness", 200),
        ("exposure_time_absolute", 10),
    ]));
    for open in [false, true] {
        c.open = open;
        let s = c.build();
        // The hidden controls and the empty menu are not published, and
        // nothing is noted for them.
        assert_eq!(
            ids(&s),
            [
                "camera.image.saturation",
                "camera.image.sharpness",
                exposure
            ]
        );
        assert!(s.notes.is_empty(), "{:?}", s.notes);
        assert_eq!(s.hidden, ["brightness", "contrast", "hue"]);
        assert_eq!(s.absent, ["auto_exposure"]);
        assert!(s.by_name("brightness").is_none());
        // The exposure time has no master to wait for: greyed for good,
        // with a reason that says so, and no condition.
        assert_eq!(greyed(&s, exposure).as_deref(), Some(NO_EXPOSURE));
        assert_eq!(kind_of(&s, exposure), disabled::DEVICE);
        assert!(setting(&s, exposure).enabled_when.is_empty());
        assert!(matches!(
            s.check(exposure, Some(&SettingValue::Int(10))),
            Err(Refusal::Disabled(r)) if r == NO_EXPOSURE
        ));
    }
    // Without the row (another camera of the same make, say): the controls
    // are there; the empty menu is still absent, without a note.
    c.hidden = Vec::new();
    let s = c.build();
    assert!(s.by_name("brightness").is_some());
    assert!(s.by_name("hue").is_some());
    assert!(s.hidden.is_empty());
    assert_eq!(s.absent, ["auto_exposure"]);
    assert!(s.notes.is_empty(), "{:?}", s.notes);
    // An active exposure time without its master is enabled.
    c.controls[6].info.flags = 0;
    assert_eq!(greyed(&c.build(), exposure), None);
}

#[test]
fn the_vx800_row_hides_its_manual_white_balance() {
    let mut c = Case::of(vx800());
    c.hidden = quirk_hidden(0x045e, 0x0766);
    c.stored = Some(stored(&[("white_balance_temperature", 6500)]));
    let s = c.build();
    assert_eq!(ids(&s), ["camera.image.brightness"]);
    assert_eq!(
        s.hidden,
        ["white_balance_automatic", "white_balance_temperature"]
    );
    assert!(s.notes.is_empty(), "{:?}", s.notes);
    // The stored temperature has no entry: nothing writes it.
    assert!(s.by_name("white_balance_temperature").is_none());
    assert!(matches!(
        s.check(
            "camera.white_balance.white_balance_temperature",
            Some(&SettingValue::Int(6500))
        ),
        Err(Refusal::Unknown(_))
    ));
    // A camera without a row: unchanged.
    c.hidden = quirk_hidden(0x046d, 0x0944);
    assert!(c.hidden.is_empty());
    let s = c.build();
    assert_eq!(
        ids(&s),
        [
            "camera.image.brightness",
            "camera.white_balance.white_balance_automatic",
            "camera.white_balance.white_balance_temperature",
        ]
    );
    assert!(s.hidden.is_empty() && s.absent.is_empty());
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
fn the_brio_labels_and_describes_two_standard_controls() {
    let brio_usb = UsbId {
        vendor: 0x046d,
        product: 0x0944,
    };
    let backlight = "camera.image.backlight_compensation";
    let keep = "camera.exposure.keep_framerate";
    let mut c = Case::brio();
    // Without a USB id, or with another one: the stock labels.
    for usb in [
        None,
        Some(UsbId {
            vendor: 0x1b3f,
            product: 0x2002,
        }),
    ] {
        c.usb = usb;
        let s = c.build();
        assert_eq!(setting(&s, backlight).label, "Backlight compensation");
        assert_eq!(setting(&s, backlight).description, None);
        assert_eq!(setting(&s, keep).description, None);
    }
    c.usb = Some(brio_usb);
    let s = c.build();
    let hdr = setting(&s, backlight);
    assert_eq!(hdr.label, "HDR");
    assert_eq!(
        hdr.description.as_deref(),
        Some("HDR limits the camera to 30 fps")
    );
    // Only the label rows' controls carry a description.
    let k = setting(&s, keep);
    assert_eq!(k.label, "Keep 30 fps in low light");
    assert!(
        k.description
            .as_deref()
            .unwrap()
            .contains("Low light compensation")
    );
    assert_eq!(s.get(keep).unwrap().name, "exposure_dynamic_framerate");
    assert_eq!(
        k.kind,
        SettingKind::Bool {
            default: true,
            value: false
        }
    );
    assert!(
        s.groups()
            .iter()
            .flat_map(|g| g.settings.iter())
            .filter(|x| x.description.is_some())
            .count()
            == 2
    );
}

#[test]
fn the_c930e_describes_its_gain_pan_and_tilt() {
    let mut c = Case::brio();
    c.usb = Some(UsbId {
        vendor: 0x046d,
        product: 0x0843,
    });
    let s = c.build();
    for (id, text) in [
        ("camera.focus.pan_absolute", "only while zoomed in"),
        ("camera.focus.tilt_absolute", "only while zoomed in"),
        ("camera.exposure.gain", "set exposure to manual"),
    ] {
        let x = setting(&s, id);
        assert!(
            x.description.as_deref().is_some_and(|d| d.contains(text)),
            "{id}: {:?}",
            x.description
        );
    }
    // The labels and every other setting stay stock.
    assert_eq!(setting(&s, "camera.exposure.gain").label, "Gain");
    let backlight = setting(&s, "camera.image.backlight_compensation");
    assert_eq!(backlight.label, "Backlight compensation");
    assert_eq!(backlight.description, None);
    assert_eq!(
        s.groups()
            .iter()
            .flat_map(|g| g.settings.iter())
            .filter(|x| x.description.is_some())
            .count(),
        3
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
            "camera.focus.focus_automatic_continuous",
            "camera.image.sharpness",
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
            "focus_absolute",
            "exposure_time_absolute"
        ]
    );
    assert_eq!(
        names(s.reset_order(&all)),
        [
            "focus_absolute",
            "exposure_time_absolute",
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
                disabled_kind: String::new(),
            },
            SettingChange {
                id: "camera.exposure.exposure_time_absolute".into(),
                value: Some(SettingValue::Int(1250)),
                stored: false,
                enabled: true,
                disabled_reason: String::new(),
                disabled_kind: String::new(),
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

// ---- the capture mode setting ----

fn brio_modes() -> Vec<VideoMode> {
    obc_pipeline::capability::parse_v4l2_ctl(include_str!(
        "../../../obc-pipeline/tests/data/mx-brio-046d-0944.v4l2-ctl.txt"
    ))
    .modes
}

fn mode_options(s: &Setting) -> Vec<(String, String)> {
    match &s.kind {
        SettingKind::Enum { options, .. } => options.clone(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_capture_mode_setting_is_auto_then_the_camera_modes() {
    assert_eq!(mode_setting(&[], None), None);
    let s = mode_setting(&brio_modes(), None).unwrap();
    assert!(valid_setting_id(&s.id));
    assert_eq!(s.id, MODE);
    let options = mode_options(&s);
    assert_eq!(options.len(), 16);
    assert!(options.len() <= MAX_OPTIONS);
    assert_eq!(options[0], (MODE_AUTO.into(), MODE_AUTO_LABEL.into()));
    assert_eq!(options[1].0, "3840x2160@30");
    assert!(options.iter().all(|(_, l)| l.len() <= LABEL_MAX));
    assert_eq!(s.kind.value(), Some(SettingValue::Str("auto".into())));
    assert!(!s.stored);
    // A stored mode the camera offers is the value; one it does not is an
    // option of its own, in place of the last choice.
    let m: ModeRequest = "1280x720@30".parse().unwrap();
    let s = mode_setting(&brio_modes(), Some(&m)).unwrap();
    assert_eq!(
        s.kind.value(),
        Some(SettingValue::Str("1280x720@30".into()))
    );
    assert!(s.stored);
    let gone: ModeRequest = "4096x2160@30".parse().unwrap();
    let s = mode_setting(&brio_modes(), Some(&gone)).unwrap();
    let options = mode_options(&s);
    assert_eq!(options.len(), 16);
    assert_eq!(
        options[15],
        (
            "4096x2160@30".to_string(),
            "4096x2160 \u{b7} 30 fps (gone)".to_string()
        )
    );
}

#[test]
fn every_capture_mode_label_fits_the_option_label_bound() {
    let fixtures = [
        include_str!("../../../obc-pipeline/tests/data/mx-brio-046d-0944.v4l2-ctl.txt"),
        include_str!("../../../obc-pipeline/tests/data/fhd-camera-5986-1193-colour.v4l2-ctl.txt"),
        include_str!("../../../obc-pipeline/tests/data/fhd-camera-5986-1193-ir.v4l2-ctl.txt"),
        include_str!("../../../obc-pipeline/tests/data/synthetic-yuyv-only-1209-0001.v4l2-ctl.txt"),
    ];
    // Stored modes no camera offers: the widest sizes and the longest
    // rates (a fractional one over 100 fps), marked as gone.
    let gone: Vec<ModeRequest> = [
        "3840x2160@30",
        "7680x4320@120000/1001",
        "15360x8640@120000/1001",
        "99999x99999@240000/1001",
    ]
    .iter()
    .map(|m| m.parse().unwrap())
    .collect();
    let worst =
        obc_pipeline::capability::mode_label(99999, 99999, 240_000, 1001) + MODE_UNAVAILABLE_SUFFIX;
    assert_eq!(worst, "99999x99999 \u{b7} 239.76 fps (gone)");
    for text in fixtures {
        let modes = obc_pipeline::capability::parse_v4l2_ctl(text).modes;
        if modes.is_empty() {
            continue;
        }
        let stored = std::iter::once(None).chain(gone.iter().map(Some));
        for m in stored {
            let s = mode_setting(&modes, m).unwrap();
            for (_, l) in mode_options(&s) {
                assert!(l.len() <= OPTION_LABEL_MAX, "{l:?}: {} bytes", l.len());
            }
        }
    }
}

#[test]
fn a_capture_mode_write_is_checked_against_the_options() {
    let s = mode_setting(&brio_modes(), None).unwrap();
    let v = |x: &str| SettingValue::Str(x.into());
    assert_eq!(check_mode(Some(&s), "c", Some(&v("auto"))), Ok(None));
    assert_eq!(
        check_mode(Some(&s), "c", Some(&v("3840x2160@30"))),
        Ok(Some("3840x2160@30".parse().unwrap()))
    );
    assert!(matches!(
        check_mode(Some(&s), "c", Some(&v("3840x2160@24"))),
        Err(Refusal::Invalid(_))
    ));
    assert!(matches!(
        check_mode(Some(&s), "c", Some(&SettingValue::Int(1))),
        Err(Refusal::Invalid(_))
    ));
    assert!(matches!(
        check_mode(None, "c", Some(&v("auto"))),
        Err(Refusal::Unknown(_))
    ));
}

#[test]
fn the_capture_mode_is_the_first_group_and_its_changes_are_diffed() {
    let with = |stored: Option<&str>| CameraSettings {
        camera: "c".into(),
        mode: mode_setting(&brio_modes(), stored.map(|s| s.parse().unwrap()).as_ref()),
        ..CameraSettings::none()
    };
    let a = with(None);
    let groups = a.groups();
    assert_eq!(groups[0].id, catalogue::CAPTURE);
    assert_eq!(groups[0].settings[0].id, MODE);
    // A new value: a change of it.
    let b = with(Some("1280x720@30"));
    let d = diff(&a, &b).unwrap();
    assert!(!d.structural);
    assert_eq!(d.changes.len(), 1);
    assert_eq!(d.changes[0].id, MODE);
    // A new option (a stored mode not offered): structural.
    let c = with(Some("4096x2160@30"));
    assert!(diff(&a, &c).unwrap().structural);
    // Gone: structural.
    assert!(
        diff(
            &a,
            &CameraSettings {
                camera: "c".into(),
                ..CameraSettings::none()
            }
        )
        .unwrap()
        .structural
    );
    assert_eq!(diff(&a, &with(None)), None);
}

#[test]
fn the_mx_brio_extension_unit_rows_are_typed_settings() {
    let mut c = Case::brio();
    c.xu = mx_brio_xu();
    c.open = true;
    let s = c.build();
    let int = |min: i64, max: i64, default: i64, value: i64| SettingKind::Int {
        min,
        max,
        step: 1,
        default,
        value,
        unit: None,
        display: None,
    };
    // The two compensations: raw numbers, shown from their middle.
    let shown = |kind: SettingKind, unit: Option<&str>, offset: i64, scale: i64| match kind {
        SettingKind::Int {
            min,
            max,
            step,
            default,
            value,
            ..
        } => SettingKind::Int {
            min,
            max,
            step,
            default,
            value,
            unit: unit.map(str::to_string),
            display: Some(IntDisplay { offset, scale }),
        },
        k => k,
    };
    let rows: [(&str, &str, SettingKind, &str); 4] = [
        (
            "camera.image.vibrance",
            "Vibrance",
            int(0, 255, 128, 128),
            "slider",
        ),
        (
            "camera.white_balance.temperature_compensation",
            "Temperature compensation",
            shown(int(0, 100, 0x32, 0x39), Some("K"), 50, 40),
            "slider",
        ),
        (
            "camera.exposure.exposure_compensation",
            "Exposure compensation",
            shown(int(0, 255, 128, 128), None, 128, 1),
            "slider",
        ),
        (
            "camera.focus.show_mode",
            "Show Mode",
            SettingKind::Bool {
                default: false,
                value: true,
            },
            "switch",
        ),
    ];
    for (id, label, kind, hint) in rows {
        let st = setting(&s, id);
        assert_eq!((st.label.as_str(), &st.kind), (label, &kind), "{id}");
        assert_eq!(st.hint.as_deref(), Some(hint), "{id}");
        assert!(st.enabled, "{id}");
        assert!(st.enabled_when.is_empty(), "{id}");
        assert!(matches!(s.get(id).unwrap().target, Target::Xu(_)), "{id}");
    }
    // Their place: after their group's standard controls; the field of
    // view first of its group, Show Mode last.
    let ids = ids(&s);
    let at = |id: &str| ids.iter().position(|i| *i == id).unwrap();
    assert_eq!(
        at("camera.image.vibrance"),
        at("camera.image.saturation") + 1
    );
    assert!(
        at("camera.white_balance.temperature_compensation")
            > at("camera.white_balance.white_balance_temperature")
    );
    assert!(at("camera.exposure.exposure_compensation") > at("camera.exposure.gain"));
    assert_eq!(at("camera.focus.field_of_view"), 0);
    let focus: Vec<&str> = ids
        .iter()
        .copied()
        .filter(|i| i.starts_with("camera.focus."))
        .collect();
    assert_eq!(focus.first(), Some(&"camera.focus.field_of_view"));
    assert_eq!(focus.last(), Some(&"camera.focus.show_mode"));
    // Only the two compensations carry a display transform.
    let shown_ids: Vec<&str> = s
        .entries
        .iter()
        .filter(|e| {
            matches!(
                e.setting.kind,
                SettingKind::Int {
                    display: Some(_),
                    ..
                }
            )
        })
        .map(|e| e.setting.id.as_str())
        .collect();
    assert_eq!(
        shown_ids,
        [
            "camera.white_balance.temperature_compensation",
            "camera.exposure.exposure_compensation",
        ]
    );
    // The field of view as before.
    let f = setting(&s, "camera.focus.field_of_view");
    assert_eq!(f.kind.value(), str_value("65"));
    assert!(f.enabled);

    // A write's raw value is the byte, and the byte is the row's option
    // the backend writes.
    let raw = |id: &str, v: SettingValue| s.check(id, Some(&v)).map(|(_, r)| r);
    let option = |id: &str, raw: i32| {
        let Target::Xu(row) = s.get(id).unwrap().target else {
            panic!("{id}")
        };
        row.option_for_value(u8::try_from(raw).unwrap())
            .map(|o| o.id)
    };
    for (id, v, want_raw, want_option) in [
        (
            "camera.focus.show_mode",
            SettingValue::Bool(false),
            0,
            "off",
        ),
        ("camera.focus.show_mode", SettingValue::Bool(true), 1, "on"),
        (
            "camera.exposure.exposure_compensation",
            SettingValue::Int(0),
            0,
            "0",
        ),
        (
            "camera.exposure.exposure_compensation",
            SettingValue::Int(255),
            255,
            "255",
        ),
        ("camera.image.vibrance", SettingValue::Int(0), 0, "0"),
        ("camera.image.vibrance", SettingValue::Int(255), 255, "255"),
        (
            "camera.white_balance.temperature_compensation",
            SettingValue::Int(0),
            0,
            "0",
        ),
        (
            "camera.white_balance.temperature_compensation",
            SettingValue::Int(100),
            100,
            "100",
        ),
    ] {
        let r = raw(id, v).unwrap();
        assert_eq!(r, want_raw, "{id}");
        assert_eq!(option(id, r), Some(want_option), "{id}");
    }
    // Out of a range, not an option, the wrong type: refused.
    for (id, v) in [
        (
            "camera.exposure.exposure_compensation",
            SettingValue::Int(-1),
        ),
        (
            "camera.exposure.exposure_compensation",
            SettingValue::Int(256),
        ),
        (
            "camera.white_balance.temperature_compensation",
            SettingValue::Int(-1),
        ),
        (
            "camera.white_balance.temperature_compensation",
            SettingValue::Int(101),
        ),
        ("camera.image.vibrance", SettingValue::Int(256)),
        ("camera.image.vibrance", SettingValue::Int(-1)),
        ("camera.focus.show_mode", SettingValue::Str("on".into())),
        ("camera.focus.show_mode", SettingValue::Int(1)),
    ] {
        assert!(
            matches!(raw(id, v.clone()), Err(Refusal::Invalid(_))),
            "{id} {v:?}"
        );
    }
    // A stored value outside the range is not written at an open.
    let e = s.get("camera.exposure.exposure_compensation").unwrap();
    assert!(e.check_raw(256).is_err());
    assert!(e.check_raw(-1).is_err());
    assert!(e.check_raw(130).is_ok());
    let e = s
        .get("camera.white_balance.temperature_compensation")
        .unwrap();
    assert!(e.check_raw(101).is_err());
    assert!(e.check_raw(100).is_ok());
    let e = s.get("camera.focus.show_mode").unwrap();
    assert!(e.check_raw(2).is_err());
}

#[test]
fn an_mx_brio_row_with_an_unknown_byte_is_greyed_with_its_reason() {
    const REASON: &str = "The camera reports a value this version does not know";
    let mut c = Case::brio();
    c.xu = mx_brio_xu();
    // Exposure compensation knows every byte: 0x70 is a value.
    c.xu[2] = reading(&EXPOSURE_COMPENSATION, 0x70, 0x80);
    c.xu[4] = reading(&TEMPERATURE_COMPENSATION, 101, 0x32);
    c.xu[1] = reading(&SHOW_MODE, 2, 0);
    c.open = true;
    let s = c.build();
    for id in [
        "camera.white_balance.temperature_compensation",
        "camera.focus.show_mode",
    ] {
        assert_eq!(greyed(&s, id).as_deref(), Some(REASON), "{id}");
    }
    assert_eq!(greyed(&s, "camera.exposure.exposure_compensation"), None);
    assert_eq!(
        value(&s, "camera.exposure.exposure_compensation"),
        Some(SettingValue::Int(0x70))
    );
    assert_eq!(
        value(&s, "camera.white_balance.temperature_compensation"),
        Some(SettingValue::Int(101))
    );
    // An off/on byte that is neither 0 nor 1 is no option: not on.
    assert_eq!(
        setting(&s, "camera.focus.show_mode").kind,
        SettingKind::Bool {
            default: false,
            value: false,
        }
    );
    // The others stay enabled; the field of view keeps its own reason.
    for id in ["camera.image.vibrance", "camera.focus.field_of_view"] {
        assert_eq!(greyed(&s, id), None, "{id}");
    }
    c.xu[0] = fov(0x07);
    assert_eq!(
        greyed(&c.build(), "camera.focus.field_of_view").as_deref(),
        Some(REASON_XU_UNKNOWN)
    );
    assert_eq!(
        REASON_XU_UNKNOWN,
        "The camera reports a field of view this version does not know"
    );
    // A Brio (046d:085e): its rows are the field of view only.
    let brio = obc_core::UsbId {
        vendor: 0x046d,
        product: 0x085e,
    };
    let mut c = Case::brio();
    c.xu = obc_pipeline::xu::rows_for(brio)
        .into_iter()
        .map(|row| reading(row, 0, 0))
        .collect();
    let s = c.build();
    let xu: Vec<&str> = s
        .entries
        .iter()
        .filter(|e| matches!(e.target, Target::Xu(_)))
        .map(|e| e.setting.id.as_str())
        .collect();
    assert_eq!(xu, ["camera.focus.field_of_view"]);
}

#[test]
fn an_off_on_extension_unit_byte_is_on_for_1_only() {
    for (byte, on, known) in [
        (0, false, true),
        (1, true, true),
        (2, false, false),
        (0xff, false, false),
    ] {
        let mut c = Case::brio();
        c.xu = vec![reading(&SHOW_MODE, byte, 0)];
        c.open = true;
        let s = c.build();
        let id = "camera.focus.show_mode";
        assert_eq!(
            setting(&s, id).kind,
            SettingKind::Bool {
                default: false,
                value: on,
            },
            "{byte}"
        );
        assert_eq!(greyed(&s, id).is_none(), known, "{byte}");
    }
}

#[test]
fn a_row_sharing_its_control_reads_the_others_again_after_a_write() {
    // Two bytes of one control (the same unit GUID and selector), and a
    // row of its own on the same unit.
    const A: XuRow = XuRow {
        name: "block_a",
        size: 30,
        offset: 1,
        selector: 0x01,
        ..SHOW_MODE
    };
    const B: XuRow = XuRow {
        name: "block_b",
        offset: 2,
        ..A
    };
    let rows = [A, B, SHOW_MODE];
    assert!(shares_control_in(&rows, &A));
    assert!(shares_control_in(&rows, &B));
    assert!(!shares_control_in(&rows, &SHOW_MODE));
    assert!(!shares_control_in(&[A], &A));
    // Another unit with the same selector: not the same control.
    let other = XuRow {
        guid: "00000000-0000-4000-8000-000000000000",
        ..B
    };
    assert!(!shares_control_in(&[A, other], &A));
    // Every MX Brio row has a control of its own: nothing else to read.
    let mut c = Case::brio();
    c.xu = mx_brio_xu();
    c.open = true;
    let s = c.build();
    for id in [
        "camera.focus.field_of_view",
        "camera.focus.show_mode",
        "camera.exposure.exposure_compensation",
        "camera.image.vibrance",
        "camera.white_balance.temperature_compensation",
    ] {
        let e = s.get(id).unwrap();
        assert!(!e.updates_others, "{id}");
        assert_eq!(e.flags, 0, "{id}");
    }
}

#[test]
fn a_stored_value_of_a_removed_mx_brio_setting_is_harmless() {
    // An older version stored the RightSight rows (removed in revision
    // 27) by name: closed or open, the tree is the same without them, and
    // nothing of theirs is published.
    let old = [
        ("rightsight", 1),
        ("rightsight_speed", 2),
        ("rightsight_distance", 0),
    ];
    let mut c = Case::brio();
    c.xu = mx_brio_xu();
    for open in [false, true] {
        c.open = open;
        c.stored = None;
        let clean = c.build();
        c.stored = Some(stored(&old));
        let s = c.build();
        assert_eq!(ids(&s), ids(&clean), "open {open}");
        assert!(
            s.entries.iter().all(|e| !e.name.starts_with("rightsight")),
            "open {open}"
        );
        assert!(s.by_name("rightsight").is_none());
        assert_eq!(
            value(&s, "camera.focus.show_mode"),
            Some(SettingValue::Bool(true))
        );
    }
    for (name, _) in old {
        assert!(catalogue::by_name(name).is_none(), "{name}");
    }
}

#[test]
fn the_catalogue_shows_the_two_compensations_from_their_middle() {
    let display = |name: &str| catalogue::by_name(name).unwrap().1.display;
    assert_eq!(display("exposure_compensation"), Some((128, 1)));
    assert_eq!(display("temperature_compensation"), Some((50, 40)));
    assert_eq!(
        catalogue::by_name("temperature_compensation")
            .unwrap()
            .1
            .unit,
        Some("K")
    );
    assert_eq!(
        catalogue::by_name("exposure_compensation").unwrap().1.unit,
        None
    );
    let with: Vec<&str> = CATALOGUE
        .iter()
        .filter(|k| k.display.is_some())
        .map(|k| k.name)
        .collect();
    assert_eq!(with, ["temperature_compensation", "exposure_compensation"]);
    // On the wire: the raw numbers, and the transform's two keys.
    let mut c = Case::brio();
    c.xu = mx_brio_xu();
    c.open = true;
    let tree = c.build().tree(1, 1);
    let mut published = Vec::new();
    for g in &tree.groups {
        for st in &g.settings {
            let d = st.to_dict();
            if d.contains_key("display_offset") {
                published.push((
                    st.id.clone(),
                    i64::try_from(&d["display_offset"]).unwrap(),
                    i64::try_from(&d["display_scale"]).unwrap(),
                    i64::try_from(&d["value"]).unwrap(),
                ));
            } else {
                assert!(!d.contains_key("display_scale"), "{}", st.id);
            }
        }
    }
    assert_eq!(
        published,
        [
            (
                "camera.white_balance.temperature_compensation".to_string(),
                50,
                40,
                0x39
            ),
            (
                "camera.exposure.exposure_compensation".to_string(),
                128,
                1,
                128
            ),
        ]
    );
}
