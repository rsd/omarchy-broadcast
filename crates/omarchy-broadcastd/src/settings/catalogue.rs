//! The catalogue of the camera settings the daemon knows, as data: the
//! groups in display order and, for each known standard V4L2 control (and
//! the Extension Unit rows of `obc_pipeline::xu`), its group, label, unit,
//! menu option ids and the rule that greys it out. Nothing here is specific
//! to one camera model: a camera exposes the controls it has, with its own
//! ranges, and a control the catalogue does not know goes to `other`.

/// One group of settings: its id (`camera.<group>.<name>`) and label, in
/// the order the groups are published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Group {
    pub id: &'static str,
    pub label: &'static str,
}

/// The groups, in display order. A group with no setting is not published.
/// The first, [`CAPTURE`], holds the daemon's own capture mode setting
/// (`crate::settings::MODE`), no control.
pub const GROUPS: [Group; 6] = [
    Group {
        id: CAPTURE,
        label: "Capture",
    },
    Group {
        id: "image",
        label: "Image",
    },
    Group {
        id: "white_balance",
        label: "White balance",
    },
    Group {
        id: "exposure",
        label: "Exposure",
    },
    Group {
        id: "focus",
        label: "Focus & zoom",
    },
    Group {
        id: OTHER,
        label: "Other",
    },
];

/// The group of the controls the catalogue does not know.
pub const OTHER: &str = "other";

/// The group of the capture mode setting.
pub const CAPTURE: &str = "capture";

/// What a rule's master must be for the setting to be enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operand {
    /// `eq` this bool (the master is a bool setting).
    Is(bool),
    /// `in` these option ids (the master is an enum setting).
    OneOf(&'static [&'static str]),
}

/// The `enabled_when` rule of a known control: one condition on another
/// known control (its `master`, by name), and the text shown while it does
/// not hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule {
    pub master: &'static str,
    pub operand: Operand,
    pub reason: &'static str,
    /// The text shown while the node has the master but the settings leave
    /// it out (its quirk row hides it, or the kernel left a menu without an
    /// item) and the device flags the control inactive: nothing the user
    /// can change makes it active.
    pub no_master: &'static str,
}

/// A known menu item: the device's index, the stable option id and its
/// label.
pub type KnownOption = (u32, &'static str, &'static str);

/// A control (or Extension Unit row) the catalogue knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Known {
    /// The v4l2-ctl name: the storage key, and the setting id's last
    /// segment unless [`Known::id_name`] says otherwise.
    pub name: &'static str,
    /// The V4L2 control id (`V4L2_CID_*`); [`XU`] for an Extension Unit
    /// row, matched by its name.
    pub cid: u32,
    pub group: &'static str,
    pub label: &'static str,
    pub unit: Option<&'static str>,
    /// The setting id's last segment, when it is not the name.
    pub id_name: Option<&'static str>,
    /// A bool setting whose value is the opposite of the control's.
    pub inverted: bool,
    pub rule: Option<Rule>,
    /// The option ids and labels of a menu, by the device's index. An item
    /// the device reports that is not listed gets `m<index>`.
    pub options: &'static [KnownOption],
    /// How a client shows an int's raw value, `(offset, scale)`
    /// (`obc_ipc::settings::IntDisplay`): for a relative control whose raw
    /// middle means no change. The published numbers stay raw.
    pub display: Option<(i64, i64)>,
}

/// The `cid` of a catalogue entry for an Extension Unit row.
pub const XU: u32 = 0;

/// `V4L2_CID_EXPOSURE_AUTO_PRIORITY`'s name: the control the
/// `[camera] fixed_framerate` quirk pins.
pub const DYNAMIC_FRAMERATE: &str = "exposure_dynamic_framerate";

const WB_RULE: Rule = Rule {
    master: "white_balance_automatic",
    operand: Operand::Is(false),
    reason: "Turn off automatic white balance to change this",
    no_master: "The camera does not allow its white balance to be set",
};

const EXPOSURE_RULE: Rule = Rule {
    master: "auto_exposure",
    operand: Operand::OneOf(&["manual", "shutter_priority"]),
    reason: "Set exposure to manual to change this",
    no_master: "The camera does not allow its exposure to be set",
};

const FOCUS_RULE: Rule = Rule {
    master: "focus_automatic_continuous",
    operand: Operand::Is(false),
    reason: "Turn off continuous autofocus to change this",
    no_master: "The camera does not allow its focus to be set",
};

const HUE_RULE: Rule = Rule {
    master: "hue_automatic",
    operand: Operand::Is(false),
    reason: "Turn off automatic hue to change this",
    no_master: "The camera does not allow its hue to be set",
};

/// `auto_exposure`'s items. Item 3 (`aperture_priority`) is labelled
/// "Auto" when the device has no item 0 ([`AUTO_EXPOSURE_ALONE`]): on such
/// a camera it is the automatic mode.
pub const AUTO_EXPOSURE_OPTIONS: &[KnownOption] = &[
    (0, "auto", "Auto"),
    (1, "manual", "Manual"),
    (2, "shutter_priority", "Shutter priority"),
    (3, "aperture_priority", "Aperture priority"),
];

/// The label of `auto_exposure`'s item 3 when the device has no item 0.
pub const AUTO_EXPOSURE_ALONE: &str = "Auto";

const POWER_LINE_OPTIONS: &[KnownOption] = &[
    (0, "disabled", "Off"),
    (1, "50hz", "50 Hz"),
    (2, "60hz", "60 Hz"),
    (3, "auto", "Auto"),
];

const fn known(name: &'static str, cid: u32, group: &'static str, label: &'static str) -> Known {
    Known {
        name,
        cid,
        group,
        label,
        unit: None,
        id_name: None,
        inverted: false,
        rule: None,
        options: &[],
        display: None,
    }
}

const fn ruled(mut k: Known, rule: Rule) -> Known {
    k.rule = Some(rule);
    k
}

const fn with_unit(mut k: Known, unit: &'static str) -> Known {
    k.unit = Some(unit);
    k
}

const fn with_display(mut k: Known, offset: i64, scale: i64) -> Known {
    k.display = Some((offset, scale));
    k
}

const fn with_options(mut k: Known, options: &'static [KnownOption]) -> Known {
    k.options = options;
    k
}

/// Every known control, in publishing order within each group.
pub const CATALOGUE: &[Known] = &[
    known("brightness", 0x0098_0900, "image", "Brightness"),
    known("contrast", 0x0098_0901, "image", "Contrast"),
    known("saturation", 0x0098_0902, "image", "Saturation"),
    known("vibrance", XU, "image", "Vibrance"),
    ruled(known("hue", 0x0098_0903, "image", "Hue"), HUE_RULE),
    known("hue_automatic", 0x0098_0919, "image", "Automatic hue"),
    known("gamma", 0x0098_0910, "image", "Gamma"),
    known("sharpness", 0x0098_091b, "image", "Sharpness"),
    known(
        "backlight_compensation",
        0x0098_091c,
        "image",
        "Backlight compensation",
    ),
    known(
        "white_balance_automatic",
        0x0098_090c,
        "white_balance",
        "Automatic white balance",
    ),
    ruled(
        with_unit(
            known(
                "white_balance_temperature",
                0x0098_091a,
                "white_balance",
                "Temperature",
            ),
            "K",
        ),
        WB_RULE,
    ),
    ruled(
        known("red_balance", 0x0098_090e, "white_balance", "Red balance"),
        WB_RULE,
    ),
    ruled(
        known("blue_balance", 0x0098_090f, "white_balance", "Blue balance"),
        WB_RULE,
    ),
    // 0x32 (50) is 0 K, about 40 K a step (`obc_pipeline::xu`).
    with_display(
        with_unit(
            known(
                "temperature_compensation",
                XU,
                "white_balance",
                "Temperature compensation",
            ),
            "K",
        ),
        50,
        40,
    ),
    known(
        "do_white_balance",
        0x0098_090d,
        "white_balance",
        "Balance once",
    ),
    with_options(
        known("auto_exposure", 0x009a_0901, "exposure", "Exposure mode"),
        AUTO_EXPOSURE_OPTIONS,
    ),
    ruled(
        with_unit(
            known(
                "exposure_time_absolute",
                0x009a_0902,
                "exposure",
                "Exposure time",
            ),
            "×100 µs",
        ),
        EXPOSURE_RULE,
    ),
    known("gain", 0x0098_0913, "exposure", "Gain"),
    // 0x80 (128) is no compensation; what a step is in EV is not known.
    with_display(
        known(
            "exposure_compensation",
            XU,
            "exposure",
            "Exposure compensation",
        ),
        128,
        1,
    ),
    Known {
        id_name: Some("keep_framerate"),
        inverted: true,
        ..known(
            DYNAMIC_FRAMERATE,
            0x009a_0903,
            "exposure",
            "Keep 30 fps in low light",
        )
    },
    with_options(
        known(
            "power_line_frequency",
            0x0098_0918,
            "exposure",
            "Anti-flicker",
        ),
        POWER_LINE_OPTIONS,
    ),
    known(
        "focus_automatic_continuous",
        0x009a_090c,
        "focus",
        "Continuous autofocus",
    ),
    ruled(
        known("focus_absolute", 0x009a_090a, "focus", "Focus"),
        FOCUS_RULE,
    ),
    known("zoom_absolute", 0x009a_090d, "focus", "Zoom"),
    known("pan_absolute", 0x009a_0908, "focus", "Pan"),
    known("tilt_absolute", 0x009a_0909, "focus", "Tilt"),
    known("pan_reset", 0x009a_0906, "focus", "Recentre pan"),
    known("tilt_reset", 0x009a_0907, "focus", "Recentre tilt"),
    known("field_of_view", XU, "focus", "Field of view"),
    known("show_mode", XU, "focus", "Show Mode"),
];

/// The catalogue entry of the control `cid` (not an Extension Unit row).
pub fn by_cid(cid: u32) -> Option<(usize, &'static Known)> {
    if cid == XU {
        return None;
    }
    CATALOGUE.iter().enumerate().find(|(_, k)| k.cid == cid)
}

/// The catalogue entry named `name`.
pub fn by_name(name: &str) -> Option<(usize, &'static Known)> {
    CATALOGUE.iter().enumerate().find(|(_, k)| k.name == name)
}

/// The position of group `id` in [`GROUPS`].
pub fn group_index(id: &str) -> usize {
    GROUPS
        .iter()
        .position(|g| g.id == id)
        .unwrap_or(GROUPS.len())
}

/// The label of group `id`.
pub fn group_label(id: &str) -> &'static str {
    GROUPS
        .iter()
        .find(|g| g.id == id)
        .map_or("Other", |g| g.label)
}
