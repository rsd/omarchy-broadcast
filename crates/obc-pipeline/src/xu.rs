//! Vendor settings reached through a UVC Extension Unit, as data rows.
//!
//! Some cameras keep a setting outside the standard V4L2 controls, in a
//! vendor Extension Unit (XU) that `UVCIOC_CTRL_QUERY` reaches by unit id
//! and selector. Each such setting is one [`XuRow`] in [`XU_ROWS`]: its
//! name, the USB ids it applies to, the Extension Unit's GUID, the
//! selector, the control's size, the position of the setting's byte in it
//! and what that byte is ([`XuKind`]: a menu of known values, off or on, or
//! a number in a range). No code is specific to a camera: adding a setting
//! is adding a row.
//!
//! Resolving a row on a camera ([`find_xu_controls`]): the node's USB
//! device is found in sysfs by the node's device number
//! ([`usb_device_of_node`]; the root is injectable), its rows looked up by
//! vendor:product ([`rows_for`]), and the unit id of each row's GUID found
//! in the descriptors of the device's active configuration
//! ([`find_extension_unit`]). The result, an [`XuControl`], is the only way
//! to issue a query, and only for a row that is exactly one of [`XU_ROWS`]
//! (a row made elsewhere, even with a known GUID and selector, resolves to
//! nothing and reaches no query). Its queries are held to an allowlist
//! before they reach the device ([`XuIo`]): only `SET_CUR`, `GET_CUR`,
//! `GET_LEN`, `GET_INFO` and `GET_DEF`, only on the (unit, selector) of a
//! data row with its GUID and size, never on an entry of [`XU_DENIED`], and
//! a write only of a value of the row (a byte outside a range is refused
//! before any query). Any other unit or selector (a camera's LED, say)
//! cannot be reached through this module.
//!
//! Every write checks and reads first: `GET_LEN` and `GET_INFO` must agree
//! with the row again (its size, get and set), then `GET_CUR` of the whole
//! control; nothing is written when any of them fails or disagrees, or the
//! byte is already the one asked for, else the read with only the row's
//! byte changed is written back. The other bytes of a longer control (the
//! MX Brio's RightSight block) are never invented or cached: they are
//! those of that fresh read.
//!
//! A row is offered only when the device agrees ([`XuControl::probe`]):
//! `GET_LEN` equals the row's size and `GET_INFO` says it can be read and
//! written.

use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use obc_core::UsbId;

use crate::control_node::ControlNode;
use crate::discovery::parse_hex_id;

/// One known value of an [`XuRow`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XuOption {
    /// The stable option id (`"90"`).
    pub id: &'static str,
    /// The label shown to a user (`"90°"`).
    pub label: &'static str,
    /// The byte the device takes and reports for it.
    pub value: u8,
}

/// What the value byte of an [`XuRow`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XuKind {
    /// One of a menu of known bytes, `options` in display order; `default`
    /// is the byte of the fallback default (one of the options).
    Menu {
        options: &'static [XuOption],
        default: u8,
    },
    /// Off (byte 0) or on (byte 1): the options `off` and `on`
    /// ([`BOOL_OPTIONS`]); off is the fallback default.
    Bool,
    /// A number, `min..=max`: the options are the bytes in that range, each
    /// with its decimal text as id and label ([`BYTE_OPTIONS`]); `default`
    /// (in the range) is the fallback default. A byte outside the range is
    /// no option: it is never written.
    Range { min: u8, max: u8, default: u8 },
}

/// One vendor setting reached through an Extension Unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XuRow {
    /// The stable name, used as the storage key (`field_of_view`).
    pub name: &'static str,
    /// The label shown to a user.
    pub label: &'static str,
    /// The USB (vendor, product) ids of the cameras it applies to.
    pub usb_ids: &'static [(u16, u16)],
    /// The Extension Unit's GUID, in its usual text form.
    pub guid: &'static str,
    /// The control selector inside the unit.
    pub selector: u8,
    /// The control's size in bytes, which the device's `GET_LEN` must
    /// report: every query reads or writes this many bytes.
    pub size: u16,
    /// The position of the row's value byte in those bytes: 0 for a
    /// one-byte control. A write of a longer control is a read, a change
    /// of this byte alone, and a write of the whole of what was read.
    pub offset: u16,
    /// What the value byte is.
    pub kind: XuKind,
    /// Why the setting is not offered for change while the device reports
    /// a byte that is no option of the row.
    pub unknown_reason: &'static str,
}

/// The options of an [`XuKind::Bool`] row: `off` (0) and `on` (1).
pub static BOOL_OPTIONS: [XuOption; 2] = [
    XuOption {
        id: "off",
        label: "Off",
        value: 0,
    },
    XuOption {
        id: "on",
        label: "On",
        value: 1,
    },
];

/// The decimal text of 0 to 255, one after the other.
const DECIMALS: &[u8] = &decimals();
/// Its length: 10 one-digit, 90 two-digit and 156 three-digit numbers.
const DECIMALS_LEN: usize = 10 + 90 * 2 + 156 * 3;

/// The number of decimal digits of `n` (at most 255).
const fn digits(n: usize) -> usize {
    if n < 10 {
        1
    } else if n < 100 {
        2
    } else {
        3
    }
}

const fn decimals() -> [u8; DECIMALS_LEN] {
    let mut out = [0u8; DECIMALS_LEN];
    let (mut n, mut at) = (0, 0);
    while n < 256 {
        let len = digits(n);
        let mut i = 0;
        let mut rest = n;
        while i < len {
            out[at + len - 1 - i] = b'0' + (rest % 10) as u8;
            rest /= 10;
            i += 1;
        }
        at += len;
        n += 1;
    }
    out
}

const fn byte_options() -> [XuOption; 256] {
    let mut out = [XuOption {
        id: "",
        label: "",
        value: 0,
    }; 256];
    let (mut n, mut at) = (0, 0);
    while n < 256 {
        let len = digits(n);
        let (_, rest) = DECIMALS.split_at(at);
        let (text, _) = rest.split_at(len);
        let text = match std::str::from_utf8(text) {
            Ok(t) => t,
            Err(_) => panic!("decimal digits are UTF-8"),
        };
        out[n] = XuOption {
            id: text,
            label: text,
            value: n as u8,
        };
        at += len;
        n += 1;
    }
    out
}

/// The options of an [`XuKind::Range`] row: byte `n` is option `"n"`
/// (decimal, no sign or leading zero), labelled the same.
pub static BYTE_OPTIONS: [XuOption; 256] = byte_options();

impl XuRow {
    /// Whether the row applies to the camera with USB id `usb`.
    pub fn applies_to(&self, usb: UsbId) -> bool {
        self.usb_ids.contains(&(usb.vendor, usb.product))
    }

    /// Every option, in display order: a menu's, `off` and `on`, or the
    /// bytes of the range.
    pub fn options(&self) -> &'static [XuOption] {
        match self.kind {
            XuKind::Menu { options, .. } => options,
            XuKind::Bool => &BOOL_OPTIONS,
            XuKind::Range { min, max, .. } if min <= max => {
                &BYTE_OPTIONS[usize::from(min)..=usize::from(max)]
            }
            XuKind::Range { .. } => &[],
        }
    }

    /// The option with id `id`: for a range, exactly the decimal text of a
    /// byte in it (`"7"`, not `"07"` or `"+7"`).
    pub fn option(&self, id: &str) -> Option<&'static XuOption> {
        match self.kind {
            XuKind::Range { .. } => id
                .parse::<u8>()
                .ok()
                .and_then(|b| self.option_for_value(b))
                .filter(|o| o.id == id),
            _ => self.options().iter().find(|o| o.id == id),
        }
    }

    /// The option the device byte `value` stands for, if it is known (for
    /// a range: inside it).
    pub fn option_for_value(&self, value: u8) -> Option<&'static XuOption> {
        match self.kind {
            XuKind::Range { min, max, .. } => (min..=max)
                .contains(&value)
                .then(|| &BYTE_OPTIONS[usize::from(value)]),
            _ => self.options().iter().find(|o| o.value == value),
        }
    }

    /// The option taken as the default when the device's `GET_DEF` cannot
    /// be read or is no option: the menu's or range's `default`, `off`.
    pub fn fallback_default(&self) -> &'static XuOption {
        let default = match self.kind {
            XuKind::Menu { default, .. } | XuKind::Range { default, .. } => default,
            XuKind::Bool => 0,
        };
        self.option_for_value(default)
            .or_else(|| self.options().first())
            .unwrap_or(&BOOL_OPTIONS[0])
    }

    /// The GUID's 16 bytes as a descriptor carries them ([`parse_guid`]).
    pub fn guid_bytes(&self) -> Option<[u8; 16]> {
        parse_guid(self.guid)
    }
}

/// The Logitech unit of the field of view, Show Mode, exposure
/// compensation, vibrance and white-balance offset (unit 10 on an MX
/// Brio; found by GUID, never by unit id).
pub const LOGITECH_UNIT_GUID: &str = "49e40215-f434-47fe-b158-0e885023e51b";

/// The Logitech unit of the RightSight auto-framing block (unit 22 on an
/// MX Brio).
pub const RIGHTSIGHT_UNIT_GUID: &str = "29144c1f-658d-4f47-a40f-0ed339428bf0";

/// A Logitech unit an MX Brio carries that no row may reach.
pub const LOGITECH_DENIED_UNIT_GUID: &str = "ffe52d21-8030-4e2c-82d9-f587d00540bd";

/// What no query may reach, whatever the rows say: (GUID, selector), `None`
/// for every selector of the unit. On [`LOGITECH_UNIT_GUID`], selector 0x06
/// mirrors the standard `zoom_absolute`, 0x0b and 0x0c the standard
/// `pan_absolute` and `tilt_absolute` (reached through V4L2, never through
/// the unit), and 0x12 is unknown; [`LOGITECH_DENIED_UNIT_GUID`] is unknown
/// as a whole.
pub const XU_DENIED: &[(&str, Option<u8>)] = &[
    (LOGITECH_UNIT_GUID, Some(0x06)),
    (LOGITECH_UNIT_GUID, Some(0x0b)),
    (LOGITECH_UNIT_GUID, Some(0x0c)),
    (LOGITECH_UNIT_GUID, Some(0x12)),
    (LOGITECH_DENIED_UNIT_GUID, None),
];

/// Whether [`XU_DENIED`] has `selector` of the unit `guid` (GUIDs compared
/// without regard to case).
fn is_denied(guid: &str, selector: u8) -> bool {
    XU_DENIED
        .iter()
        .any(|(g, sel)| g.eq_ignore_ascii_case(guid) && sel.is_none_or(|s| s == selector))
}

/// Whether `row` is exactly one of [`XU_ROWS`] (every field).
fn is_known_row(row: &XuRow) -> bool {
    XU_ROWS.contains(row)
}

/// The Logitech MX Brio (USB 046d:0944).
const MX_BRIO: &[(u16, u16)] = &[(0x046d, 0x0944)];

/// The reason of a row whose current byte is no option, but the field of
/// view's.
const UNKNOWN_VALUE: &str = "The camera reports a value this version does not know";

/// The Logitech field of view (Brio, MX Brio): 90°, 78° or 65°, a crop in
/// the camera's image processor.
pub const FIELD_OF_VIEW: XuRow = XuRow {
    name: "field_of_view",
    label: "Field of view",
    usb_ids: &[(0x046d, 0x0944), (0x046d, 0x085e)],
    guid: LOGITECH_UNIT_GUID,
    selector: 5,
    size: 1,
    offset: 0,
    kind: XuKind::Menu {
        options: &[
            XuOption {
                id: "90",
                label: "90°",
                value: 0x00,
            },
            XuOption {
                id: "78",
                label: "78°",
                value: 0x01,
            },
            XuOption {
                id: "65",
                label: "65°",
                value: 0x02,
            },
        ],
        default: 0x00,
    },
    unknown_reason: "The camera reports a field of view this version does not know",
};

/// The MX Brio's Show Mode, on or off: Logi Tune's switch for showing the
/// desk with the camera tilted down. The device's default (`GET_DEF`) is
/// 0, off.
pub const SHOW_MODE: XuRow = XuRow {
    name: "show_mode",
    label: "Show Mode",
    usb_ids: MX_BRIO,
    guid: LOGITECH_UNIT_GUID,
    selector: 0x0d,
    size: 1,
    offset: 0,
    kind: XuKind::Bool,
    unknown_reason: UNKNOWN_VALUE,
};

/// The MX Brio's exposure compensation: a relative control centred on
/// 0x80 (128, no compensation), one step a byte. What one step is in EV is
/// not known. The range and default are the device's own (`GET_MIN`
/// 0, `GET_MAX` 255, `GET_DEF` 0x80, read once from an MX Brio).
pub const EXPOSURE_COMPENSATION: XuRow = XuRow {
    name: "exposure_compensation",
    label: "Exposure compensation",
    usb_ids: MX_BRIO,
    guid: LOGITECH_UNIT_GUID,
    selector: 0x13,
    size: 1,
    offset: 0,
    kind: XuKind::Range {
        min: 0,
        max: 255,
        default: 0x80,
    },
    unknown_reason: UNKNOWN_VALUE,
};

/// The MX Brio's vibrance: the whole byte, 128 by default (the device's
/// `GET_MIN` 0, `GET_MAX` 255, `GET_DEF` 128).
pub const VIBRANCE: XuRow = XuRow {
    name: "vibrance",
    label: "Vibrance",
    usb_ids: MX_BRIO,
    guid: LOGITECH_UNIT_GUID,
    selector: 0x14,
    size: 1,
    offset: 0,
    kind: XuKind::Range {
        min: 0,
        max: 255,
        default: 128,
    },
    unknown_reason: UNKNOWN_VALUE,
};

/// The MX Brio's temperature compensation, an offset of its automatic
/// white balance: about 40 K a step, 0x32 (50) is 0 K. The range and
/// default are the device's own (`GET_MIN` 0, `GET_MAX` 100, `GET_DEF`
/// 0x32, read once from an MX Brio).
pub const TEMPERATURE_COMPENSATION: XuRow = XuRow {
    name: "temperature_compensation",
    label: "Temperature compensation",
    usb_ids: MX_BRIO,
    guid: LOGITECH_UNIT_GUID,
    selector: 0x15,
    size: 1,
    offset: 0,
    kind: XuKind::Range {
        min: 0,
        max: 100,
        default: 0x32,
    },
    unknown_reason: UNKNOWN_VALUE,
};

/// The size of the MX Brio's RightSight block (selector 1 of
/// [`RIGHTSIGHT_UNIT_GUID`]): byte 1 is on or off, byte 2 the framing
/// speed, byte 3 the framing distance; the others are copied from a fresh
/// read on every write. The device answers every `GET_*` of it with the
/// same bytes, so it publishes no range: speed and distance stay 0 to 2.
pub const RIGHTSIGHT_BLOCK: u16 = 30;

/// The MX Brio's RightSight auto-framing, on or off.
pub const RIGHTSIGHT: XuRow = XuRow {
    name: "rightsight",
    label: "RightSight",
    usb_ids: MX_BRIO,
    guid: RIGHTSIGHT_UNIT_GUID,
    selector: 0x01,
    size: RIGHTSIGHT_BLOCK,
    offset: 1,
    kind: XuKind::Bool,
    unknown_reason: UNKNOWN_VALUE,
};

/// How fast RightSight follows.
pub const RIGHTSIGHT_SPEED: XuRow = XuRow {
    name: "rightsight_speed",
    label: "Framing speed",
    usb_ids: MX_BRIO,
    guid: RIGHTSIGHT_UNIT_GUID,
    selector: 0x01,
    size: RIGHTSIGHT_BLOCK,
    offset: 2,
    kind: XuKind::Menu {
        options: &[
            XuOption {
                id: "slower",
                label: "Slower",
                value: 0,
            },
            XuOption {
                id: "default",
                label: "Default",
                value: 1,
            },
            XuOption {
                id: "faster",
                label: "Faster",
                value: 2,
            },
        ],
        default: 1,
    },
    unknown_reason: UNKNOWN_VALUE,
};

/// How close RightSight frames.
pub const RIGHTSIGHT_DISTANCE: XuRow = XuRow {
    name: "rightsight_distance",
    label: "Framing distance",
    usb_ids: MX_BRIO,
    guid: RIGHTSIGHT_UNIT_GUID,
    selector: 0x01,
    size: RIGHTSIGHT_BLOCK,
    offset: 3,
    kind: XuKind::Menu {
        options: &[
            XuOption {
                id: "closer",
                label: "Closer",
                value: 0,
            },
            XuOption {
                id: "default",
                label: "Default",
                value: 1,
            },
            XuOption {
                id: "farther",
                label: "Farther",
                value: 2,
            },
        ],
        default: 1,
    },
    unknown_reason: UNKNOWN_VALUE,
};

/// Every Extension Unit setting known.
pub const XU_ROWS: &[XuRow] = &[
    FIELD_OF_VIEW,
    SHOW_MODE,
    EXPOSURE_COMPENSATION,
    VIBRANCE,
    TEMPERATURE_COMPENSATION,
    RIGHTSIGHT,
    RIGHTSIGHT_SPEED,
    RIGHTSIGHT_DISTANCE,
];

/// The rows that apply to the camera with USB id `usb`, in table order.
pub fn rows_for(usb: UsbId) -> Vec<&'static XuRow> {
    XU_ROWS.iter().filter(|r| r.applies_to(usb)).collect()
}

/// A GUID in its text form (`49e40215-f434-47fe-b158-0e885023e51b`) as the
/// 16 bytes a USB descriptor carries: the first three fields little-endian,
/// the last two as written. `None` unless it is 8-4-4-4-12 hex digits.
pub fn parse_guid(text: &str) -> Option<[u8; 16]> {
    let fields: Vec<&str> = text.split('-').collect();
    let lens: Vec<usize> = fields.iter().map(|f| f.len()).collect();
    if lens != [8, 4, 4, 4, 12]
        || !fields
            .iter()
            .all(|f| f.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return None;
    }
    let hex = |s: &str| -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap_or(0))
            .collect()
    };
    let mut out = Vec::with_capacity(16);
    for (i, f) in fields.iter().enumerate() {
        let mut bytes = hex(f);
        if i < 3 {
            bytes.reverse();
        }
        out.extend(bytes);
    }
    out.try_into().ok()
}

/// `USB_DT_CONFIG`
const DT_CONFIG: u8 = 0x02;
/// `USB_DT_INTERFACE`
const DT_INTERFACE: u8 = 0x04;
/// `USB_DT_CS_INTERFACE`
const DT_CS_INTERFACE: u8 = 0x24;
/// `USB_CLASS_VIDEO`
const CLASS_VIDEO: u8 = 0x0e;
/// `UVC_SC_VIDEOCONTROL`
const SUBCLASS_VIDEO_CONTROL: u8 = 0x01;
/// `UVC_VC_EXTENSION_UNIT` (inside a VideoStreaming interface the same
/// subtype is `VS_FORMAT_MJPEG`).
const VC_EXTENSION_UNIT: u8 = 0x06;

/// The unit id of the Extension Unit with GUID `guid` (descriptor byte
/// order, [`parse_guid`]) in the configuration whose `bConfigurationValue`
/// is `configuration` (the active one, sysfs `bConfigurationValue`), in a
/// device's raw descriptors (sysfs `descriptors`: the device descriptor,
/// then every configuration's, active or not). Walks the descriptors by
/// their `bLength`, tracking the current configuration and interface; only
/// inside a VideoControl interface of that configuration is a
/// class-specific descriptor of subtype `0x06` an Extension Unit
/// (`[bLength, 0x24, 0x06, bUnitID, guid[16], ...]`). Stops at a `bLength`
/// below 2 or a descriptor that runs past the end. `None` when no such
/// unit is found.
pub fn find_extension_unit(descriptors: &[u8], configuration: u8, guid: &[u8; 16]) -> Option<u8> {
    let mut at = 0;
    let mut in_active_config = false;
    let mut in_video_control = false;
    while at + 2 <= descriptors.len() {
        let len = usize::from(descriptors[at]);
        if len < 2 || at + len > descriptors.len() {
            return None;
        }
        let d = &descriptors[at..at + len];
        match d[1] {
            DT_CONFIG => {
                in_active_config = len >= 9 && d[5] == configuration;
                in_video_control = false;
            }
            DT_INTERFACE => {
                in_video_control = in_active_config
                    && len >= 9
                    && d[5] == CLASS_VIDEO
                    && d[6] == SUBCLASS_VIDEO_CONTROL;
            }
            DT_CS_INTERFACE
                if in_video_control
                    && len >= 20
                    && d[2] == VC_EXTENSION_UNIT
                    && d[4..20] == guid[..] =>
            {
                return Some(d[3]);
            }
            _ => {}
        }
        at += len;
    }
    None
}

/// The default sysfs root.
pub const SYSFS_ROOT: &str = "/sys";

/// The USB device behind a video node, from sysfs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbDevice {
    pub id: UsbId,
    /// The device's raw descriptors (sysfs `descriptors`: every
    /// configuration's, not only the active one's).
    pub descriptors: Vec<u8>,
    /// The active configuration's `bConfigurationValue` (sysfs
    /// `bConfigurationValue`); `None` while the device is unconfigured.
    pub configuration: Option<u8>,
    /// The USB device's sysfs directory.
    pub dir: PathBuf,
}

fn not_found(why: String) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, why)
}

/// The USB device of the video node `node` (`/dev/videoN`, or a link to
/// it), found by the node's device number, never by its name: `node` must
/// be a character device (`InvalidInput` otherwise); its `major:minor`
/// names `<sysfs_root>/dev/char/<major>:<minor>`, which must resolve to a
/// `video4linux` class device whose `device` link is the USB interface; the
/// interface's parent directory is the USB device, with `idVendor`,
/// `idProduct`, `bConfigurationValue` and `descriptors`. `NotFound` when
/// sysfs knows no video device of that number, or it has no USB device.
pub fn usb_device_of_node(sysfs_root: &Path, node: &Path) -> io::Result<UsbDevice> {
    let meta = fs::metadata(node)?;
    if !meta.file_type().is_char_device() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a character device", node.display()),
        ));
    }
    let number = format!("{}:{}", libc::major(meta.rdev()), libc::minor(meta.rdev()));
    let class_dir = fs::canonicalize(sysfs_root.join("dev/char").join(&number))
        .map_err(|e| not_found(format!("{} ({number}): {e}", node.display())))?;
    if class_dir.parent().and_then(Path::file_name) != Some("video4linux".as_ref()) {
        return Err(not_found(format!(
            "{} ({number}) is not a video4linux device",
            node.display()
        )));
    }
    let interface = fs::canonicalize(class_dir.join("device"))
        .map_err(|e| not_found(format!("{} ({number}) has no device: {e}", node.display())))?;
    let dir = interface
        .parent()
        .filter(|d| d.join("idVendor").is_file())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} is not a USB camera", node.display()),
            )
        })?
        .to_path_buf();
    let hex = |file: &str| -> io::Result<u16> {
        let text = fs::read_to_string(dir.join(file))?;
        parse_hex_id(&text).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: {text:?} is not a USB id", dir.join(file).display()),
            )
        })
    };
    let id = UsbId {
        vendor: hex("idVendor")?,
        product: hex("idProduct")?,
    };
    let configuration = match fs::read_to_string(dir.join("bConfigurationValue"))?.trim() {
        "" => None,
        text => Some(text.parse::<u8>().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{}: {text:?} is not a configuration value",
                    dir.join("bConfigurationValue").display()
                ),
            )
        })?),
    };
    let descriptors = fs::read(dir.join("descriptors"))?;
    Ok(UsbDevice {
        id,
        descriptors,
        configuration,
        dir,
    })
}

/// `UVC_SET_CUR`
pub const UVC_SET_CUR: u8 = 0x01;
/// `UVC_GET_CUR`
pub const UVC_GET_CUR: u8 = 0x81;
/// `UVC_GET_LEN`
pub const UVC_GET_LEN: u8 = 0x85;
/// `UVC_GET_INFO`
pub const UVC_GET_INFO: u8 = 0x86;
/// `UVC_GET_DEF`
pub const UVC_GET_DEF: u8 = 0x87;

/// The only query codes this module issues.
pub const XU_QUERIES: [u8; 5] = [
    UVC_SET_CUR,
    UVC_GET_CUR,
    UVC_GET_LEN,
    UVC_GET_INFO,
    UVC_GET_DEF,
];

/// `GET_INFO` bit 0: the control can be read.
pub const XU_INFO_GET: u8 = 0x01;
/// `GET_INFO` bit 1: the control can be written.
pub const XU_INFO_SET: u8 = 0x02;

/// `struct uvc_xu_control_query`
#[repr(C)]
struct UvcXuControlQuery {
    unit: u8,
    selector: u8,
    query: u8,
    size: u16,
    data: *mut u8,
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<UvcXuControlQuery>() == 16);

/// `_IOWR('u', 0x21, struct uvc_xu_control_query)`
const UVCIOC_CTRL_QUERY: libc::c_ulong = (3 << 30)
    | ((std::mem::size_of::<UvcXuControlQuery>() as libc::c_ulong) << 16)
    | ((b'u' as libc::c_ulong) << 8)
    | 0x21;

/// A (unit, selector) a query may reach, with the row's unit GUID and
/// value size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct XuTarget {
    unit: u8,
    guid: &'static str,
    selector: u8,
    size: u16,
}

fn refused(why: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, why)
}

/// Whether a query may be issued: its code is one of [`XU_QUERIES`], its
/// (unit, selector) is in `allow`, that entry's (GUID, selector) is not in
/// [`XU_DENIED`], its GUID, selector and size are those of a row of
/// [`XU_ROWS`], and its buffer is the size the query takes (2 for
/// `GET_LEN`, 1 for `GET_INFO`, the row's size otherwise). `InvalidInput`
/// otherwise.
fn check_xu_query(
    allow: &[XuTarget],
    unit: u8,
    selector: u8,
    query: u8,
    len: usize,
) -> io::Result<()> {
    if !XU_QUERIES.contains(&query) {
        return Err(refused(format!("UVC query {query:#04x} is not allowed")));
    }
    let Some(target) = allow
        .iter()
        .find(|t| t.unit == unit && t.selector == selector)
    else {
        return Err(refused(format!(
            "unit {unit} selector {selector} is not a known extension-unit setting"
        )));
    };
    if is_denied(target.guid, selector) {
        return Err(refused(format!(
            "selector {selector:#04x} of unit {} is never reached",
            target.guid
        )));
    }
    if !XU_ROWS
        .iter()
        .any(|r| r.guid == target.guid && r.selector == selector && r.size == target.size)
    {
        return Err(refused(format!(
            "selector {selector:#04x} of unit {} is no data row's",
            target.guid
        )));
    }
    let want = match query {
        UVC_GET_LEN => 2,
        UVC_GET_INFO => 1,
        _ => usize::from(target.size),
    };
    if len != want {
        return Err(refused(format!(
            "UVC query {query:#04x} takes {want} bytes, not {len}"
        )));
    }
    Ok(())
}

/// Where an [`XuControl`]'s queries go once [`check_xu_query`] allowed
/// them: the camera node's `UVCIOC_CTRL_QUERY`, or a test's fake. It sees
/// only allowed queries, each with a buffer of the size the query takes.
pub trait XuIo {
    /// Issues `query` on `unit`'s `selector`, reading into or writing from
    /// `data`.
    fn query(&mut self, unit: u8, selector: u8, query: u8, data: &mut [u8]) -> io::Result<()>;
}

/// The camera node's `UVCIOC_CTRL_QUERY`.
struct NodeIo<'a>(&'a ControlNode);

impl XuIo for NodeIo<'_> {
    fn query(&mut self, unit: u8, selector: u8, query: u8, data: &mut [u8]) -> io::Result<()> {
        let size = u16::try_from(data.len())
            .map_err(|_| refused(format!("a {}-byte query is too long", data.len())))?;
        let mut q = UvcXuControlQuery {
            unit,
            selector,
            query,
            size,
            data: data.as_mut_ptr(),
        };
        // SAFETY: `q` is a valid `struct uvc_xu_control_query` whose `data`
        // points to `size` writable bytes; both outlive the call, and the
        // fd is open for the duration.
        let rc = unsafe { libc::ioctl(self.0.file().as_raw_fd(), UVCIOC_CTRL_QUERY as _, &mut q) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Issues one query through `io`, after [`check_xu_query`].
fn xu_query(
    io: &mut dyn XuIo,
    allow: &[XuTarget],
    unit: u8,
    selector: u8,
    query: u8,
    data: &mut [u8],
) -> io::Result<()> {
    check_xu_query(allow, unit, selector, query, data.len())?;
    io.query(unit, selector, query, data)
}

/// What [`XuControl::probe`] read from a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XuState {
    /// `GET_INFO`'s bits ([`XU_INFO_GET`], [`XU_INFO_SET`], ...).
    pub info: u8,
    /// The current byte (the row's byte of `GET_CUR`).
    pub current: u8,
    /// The option it stands for; `None` for a byte the row does not know.
    pub current_option: Option<&'static XuOption>,
    /// The device's default byte (the row's byte of `GET_DEF`), when it
    /// could be read.
    pub default_raw: Option<u8>,
    /// The default option: `GET_DEF`'s byte mapped through the options; the
    /// row's fallback default ([`XuRow::fallback_default`]) when that byte
    /// is unknown or unreadable.
    pub default: &'static XuOption,
}

/// The value of `row` for the device's `current` byte and `default_raw`
/// byte, mapped through its options.
fn map_state(row: &'static XuRow, info: u8, current: u8, default_raw: Option<u8>) -> XuState {
    XuState {
        info,
        current,
        current_option: row.option_for_value(current),
        default_raw,
        default: default_raw
            .and_then(|d| row.option_for_value(d))
            .unwrap_or_else(|| row.fallback_default()),
    }
}

/// Whether the device agrees with the row: `GET_LEN` is the row's size,
/// and `GET_INFO` says it can be read and written.
fn device_agrees(row: &XuRow, len: u16, info: u8) -> bool {
    len == row.size && info & (XU_INFO_GET | XU_INFO_SET) == (XU_INFO_GET | XU_INFO_SET)
}

/// A data row resolved on one camera: the row and the unit id its GUID has
/// there. The only handle to issue Extension Unit queries with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XuControl {
    row: &'static XuRow,
    unit: u8,
}

impl XuControl {
    /// The row resolved on a device with these raw `descriptors` whose
    /// active configuration is `configuration` ([`find_extension_unit`] of
    /// the row's GUID); `None` when that configuration has no such unit, or
    /// the row is not exactly one of [`XU_ROWS`].
    pub fn resolve(
        row: &'static XuRow,
        descriptors: &[u8],
        configuration: u8,
    ) -> Option<XuControl> {
        if !is_known_row(row) {
            return None;
        }
        let guid = row.guid_bytes()?;
        Some(XuControl {
            row,
            unit: find_extension_unit(descriptors, configuration, &guid)?,
        })
    }

    pub fn row(&self) -> &'static XuRow {
        self.row
    }

    /// The unit id on this device.
    pub fn unit(&self) -> u8 {
        self.unit
    }

    /// Issues `query` after [`check_xu_query`]; `InvalidInput`, before any
    /// query, for a row that is not exactly one of [`XU_ROWS`].
    fn query(&self, io: &mut dyn XuIo, query: u8, data: &mut [u8]) -> io::Result<()> {
        if !is_known_row(self.row) {
            return Err(refused(format!(
                "{} (unit {} selector {:#04x}) is not a known extension-unit row",
                self.row.name, self.row.guid, self.row.selector
            )));
        }
        let allow = [XuTarget {
            unit: self.unit,
            guid: self.row.guid,
            selector: self.row.selector,
            size: self.row.size,
        }];
        xu_query(io, &allow, self.unit, self.row.selector, query, data)
    }

    /// The whole control as `query` (`GET_CUR` or `GET_DEF`) reads it.
    fn read(&self, io: &mut dyn XuIo, query: u8) -> io::Result<Vec<u8>> {
        let mut data = vec![0u8; usize::from(self.row.size)];
        self.query(io, query, &mut data)?;
        Ok(data)
    }

    /// The row's byte in `data`, the whole control.
    fn byte_of(&self, data: &[u8]) -> io::Result<u8> {
        data.get(usize::from(self.row.offset))
            .copied()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{}: byte {} is past its {} bytes",
                        self.row.name, self.row.offset, self.row.size
                    ),
                )
            })
    }

    /// Reads the setting's state from the device: `Ok(None)` when the
    /// device does not agree with the row (`GET_LEN` is not its size, or
    /// `GET_INFO` lacks the get or set bit), so the setting is not offered.
    /// `Err` when `GET_LEN`, `GET_INFO` or `GET_CUR` fails; a failing
    /// `GET_DEF` falls back to the row's fallback default.
    pub fn probe(&self, node: &ControlNode) -> io::Result<Option<XuState>> {
        self.probe_with(&mut NodeIo(node))
    }

    /// [`Self::probe`] through `io`.
    pub fn probe_with(&self, io: &mut dyn XuIo) -> io::Result<Option<XuState>> {
        let (len, info) = self.len_and_info(io)?;
        if !device_agrees(self.row, len, info) {
            return Ok(None);
        }
        let current = self.current_with(io)?;
        let default_raw = self
            .read(io, UVC_GET_DEF)
            .ok()
            .and_then(|d| self.byte_of(&d).ok());
        Ok(Some(map_state(self.row, info, current, default_raw)))
    }

    /// The device's `GET_LEN` and `GET_INFO` of the control.
    fn len_and_info(&self, io: &mut dyn XuIo) -> io::Result<(u16, u8)> {
        let mut len = [0u8; 2];
        self.query(io, UVC_GET_LEN, &mut len)?;
        let mut info = [0u8; 1];
        self.query(io, UVC_GET_INFO, &mut info)?;
        Ok((u16::from_le_bytes(len), info[0]))
    }

    /// The current byte (the row's byte of `GET_CUR`).
    pub fn current(&self, node: &ControlNode) -> io::Result<u8> {
        self.current_with(&mut NodeIo(node))
    }

    /// [`Self::current`] through `io`.
    pub fn current_with(&self, io: &mut dyn XuIo) -> io::Result<u8> {
        let data = self.read(io, UVC_GET_CUR)?;
        self.byte_of(&data)
    }

    /// The whole control as `GET_CUR` reads it: the row's size in bytes,
    /// the row's byte among them (a block's other bytes too).
    pub fn current_bytes(&self, node: &ControlNode) -> io::Result<Vec<u8>> {
        self.read(&mut NodeIo(node), UVC_GET_CUR)
    }

    /// Writes the option `option_id`. The node must be open for writing.
    /// `InvalidInput` for an id that is no option of the row (for a range:
    /// a byte outside it), before any query. The device must agree with the
    /// row again (`GET_LEN` and `GET_INFO`, as [`Self::probe`]): when it
    /// does not, nothing more is queried and the write fails
    /// (`Unsupported`). The control is then read (`GET_CUR`): nothing is
    /// written when that fails or the row's byte is already the option's;
    /// else what was read, with the row's byte changed and every other byte
    /// as read, is written (`SET_CUR`).
    pub fn set(&self, node: &ControlNode, option_id: &str) -> io::Result<()> {
        self.set_with(&mut NodeIo(node), option_id)
    }

    /// [`Self::set`] through `io`.
    pub fn set_with(&self, io: &mut dyn XuIo, option_id: &str) -> io::Result<()> {
        let option = self.row.option(option_id).ok_or_else(|| {
            refused(format!(
                "{option_id:?} is not an option of {}",
                self.row.name
            ))
        })?;
        let (len, info) = self.len_and_info(io)?;
        if !device_agrees(self.row, len, info) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "{}: the camera does not agree with the row (GET_LEN {len}, GET_INFO \
                     {info:#04x}): not written",
                    self.row.name
                ),
            ));
        }
        let mut data = self.read(io, UVC_GET_CUR)?;
        if self.byte_of(&data)? == option.value {
            return Ok(());
        }
        data[usize::from(self.row.offset)] = option.value;
        self.query(io, UVC_SET_CUR, &mut data)
    }
}

/// The data rows that apply to the camera at `node`, resolved on it: its
/// USB device from sysfs under `sysfs_root` ([`usb_device_of_node`]), its
/// rows ([`rows_for`]), each one whose GUID the descriptors of its active
/// configuration carry. Empty for a camera without rows, or an
/// unconfigured device. `Err` when its sysfs entries cannot be read
/// (`NotFound` for a node without a USB device).
pub fn find_xu_controls(sysfs_root: &Path, node: &Path) -> io::Result<Vec<XuControl>> {
    let usb = usb_device_of_node(sysfs_root, node)?;
    let Some(configuration) = usb.configuration else {
        return Ok(Vec::new());
    };
    Ok(rows_for(usb.id)
        .into_iter()
        .filter_map(|row| XuControl::resolve(row, &usb.descriptors, configuration))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The FoV GUID in descriptor byte order, as verified on an MX Brio.
    const FOV_GUID: [u8; 16] = [
        0x15, 0x02, 0xe4, 0x49, 0x34, 0xf4, 0xfe, 0x47, 0xb1, 0x58, 0x0e, 0x88, 0x50, 0x23, 0xe5,
        0x1b,
    ];

    #[test]
    fn ioctl_number_and_size() {
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(std::mem::size_of::<UvcXuControlQuery>(), 16);
            assert_eq!(UVCIOC_CTRL_QUERY, 0xc010_7521);
        }
    }

    #[test]
    fn the_field_of_view_row() {
        let r = &FIELD_OF_VIEW;
        assert_eq!(
            (r.name, r.label, r.guid, r.selector, r.size, r.offset),
            (
                "field_of_view",
                "Field of view",
                LOGITECH_UNIT_GUID,
                5,
                1,
                0
            )
        );
        let opts: Vec<(&str, &str, u8)> = r
            .options()
            .iter()
            .map(|o| (o.id, o.label, o.value))
            .collect();
        assert_eq!(opts, [("90", "90°", 0), ("78", "78°", 1), ("65", "65°", 2)]);
        assert_eq!(r.option("78").map(|o| o.value), Some(1));
        assert_eq!(r.option("45"), None);
        assert_eq!(r.option_for_value(2).map(|o| o.id), Some("65"));
        assert_eq!(r.option_for_value(3), None);
        assert_eq!(r.fallback_default().id, "90");
        assert_eq!(
            r.unknown_reason,
            "The camera reports a field of view this version does not know"
        );
    }

    #[test]
    fn the_mx_brio_rows() {
        let rows: Vec<(&str, &str, &str, u8, u16, u16)> = XU_ROWS
            .iter()
            .map(|r| (r.name, r.label, r.guid, r.selector, r.size, r.offset))
            .collect();
        let (a, b) = (LOGITECH_UNIT_GUID, RIGHTSIGHT_UNIT_GUID);
        assert_eq!(
            rows,
            [
                ("field_of_view", "Field of view", a, 0x05, 1, 0),
                ("show_mode", "Show Mode", a, 0x0d, 1, 0),
                (
                    "exposure_compensation",
                    "Exposure compensation",
                    a,
                    0x13,
                    1,
                    0
                ),
                ("vibrance", "Vibrance", a, 0x14, 1, 0),
                (
                    "temperature_compensation",
                    "Temperature compensation",
                    a,
                    0x15,
                    1,
                    0
                ),
                ("rightsight", "RightSight", b, 0x01, 30, 1),
                ("rightsight_speed", "Framing speed", b, 0x01, 30, 2),
                ("rightsight_distance", "Framing distance", b, 0x01, 30, 3),
            ]
        );
        assert_eq!(SHOW_MODE.kind, XuKind::Bool);
        assert_eq!(RIGHTSIGHT.kind, XuKind::Bool);
        assert_eq!(
            EXPOSURE_COMPENSATION.kind,
            XuKind::Range {
                min: 0,
                max: 255,
                default: 0x80
            }
        );
        assert_eq!(
            VIBRANCE.kind,
            XuKind::Range {
                min: 0,
                max: 255,
                default: 128
            }
        );
        assert_eq!(
            TEMPERATURE_COMPENSATION.kind,
            XuKind::Range {
                min: 0,
                max: 100,
                default: 0x32
            }
        );
        let ids = |r: &XuRow| -> Vec<(&str, u8)> {
            r.options().iter().map(|o| (o.id, o.value)).collect()
        };
        assert_eq!(
            ids(&RIGHTSIGHT_SPEED),
            [("slower", 0), ("default", 1), ("faster", 2)]
        );
        assert_eq!(
            ids(&RIGHTSIGHT_DISTANCE),
            [("closer", 0), ("default", 1), ("farther", 2)]
        );
        assert_eq!(ids(&SHOW_MODE), [("off", 0), ("on", 1)]);
        for (r, def) in [
            (&SHOW_MODE, "off"),
            (&RIGHTSIGHT, "off"),
            (&RIGHTSIGHT_SPEED, "default"),
            (&RIGHTSIGHT_DISTANCE, "default"),
            (&EXPOSURE_COMPENSATION, "128"),
            (&VIBRANCE, "128"),
            (&TEMPERATURE_COMPENSATION, "50"),
        ] {
            assert_eq!(r.fallback_default().id, def, "{}", r.name);
            assert_eq!(
                r.unknown_reason,
                "The camera reports a value this version does not know"
            );
            assert_eq!(r.usb_ids, [(0x046d, 0x0944)], "{}", r.name);
        }
        // Every row: a valid GUID, its byte inside its size, at least one
        // option, distinct option ids and bytes, a fallback default that is
        // one of its options, a distinct name.
        for (i, row) in XU_ROWS.iter().enumerate() {
            assert!(row.guid_bytes().is_some(), "{}", row.name);
            assert!(row.offset < row.size, "{}", row.name);
            let options = row.options();
            assert!(!options.is_empty(), "{}", row.name);
            for (j, o) in options.iter().enumerate() {
                for other in &options[j + 1..] {
                    assert!(o.id != other.id && o.value != other.value, "{}", row.name);
                }
                assert_eq!(row.option(o.id), Some(o), "{}", row.name);
                assert_eq!(row.option_for_value(o.value), Some(o), "{}", row.name);
            }
            let default = match row.kind {
                XuKind::Menu { default, .. } | XuKind::Range { default, .. } => default,
                XuKind::Bool => 0,
            };
            assert_eq!(row.fallback_default().value, default, "{}", row.name);
            assert!(options.contains(row.fallback_default()), "{}", row.name);
            assert!(XU_ROWS[i + 1..].iter().all(|r| r.name != row.name));
        }
    }

    #[test]
    fn range_and_bool_options() {
        // The byte table: each byte its decimal text.
        for (n, o) in BYTE_OPTIONS.iter().enumerate() {
            assert_eq!(
                (o.id, o.label, usize::from(o.value)),
                (n.to_string().as_str(), o.id, n)
            );
        }
        // Exposure compensation: the whole byte, every one known.
        let r = &EXPOSURE_COMPENSATION;
        assert_eq!(r.options().len(), 256);
        assert_eq!(r.options().first().map(|o| o.id), Some("0"));
        assert_eq!(r.options().last().map(|o| o.id), Some("255"));
        assert_eq!(r.option("0").map(|o| o.value), Some(0));
        assert_eq!(r.option("255").map(|o| o.value), Some(255));
        for bad in ["256", "-1", "+130", "0130", "00", " 130", "130 ", "", "x"] {
            assert_eq!(r.option(bad), None, "{bad:?}");
        }
        assert!((0..=255).all(|b| r.option_for_value(b).is_some()));
        assert_eq!(r.option_for_value(0x80).map(|o| o.id), Some("128"));
        assert_eq!(VIBRANCE.option("0").map(|o| o.value), Some(0));
        assert_eq!(VIBRANCE.option("255").map(|o| o.value), Some(255));
        assert_eq!(VIBRANCE.options().len(), 256);
        // Temperature compensation: 0 to 100.
        let t = &TEMPERATURE_COMPENSATION;
        assert_eq!(t.options().len(), 101);
        assert_eq!(t.option("0").map(|o| o.value), Some(0));
        assert_eq!(t.option("100").map(|o| o.value), Some(100));
        for bad in ["101", "255", "-1", "050"] {
            assert_eq!(t.option(bad), None, "{bad:?}");
        }
        assert_eq!(t.option_for_value(100).map(|o| o.id), Some("100"));
        assert_eq!(t.option_for_value(101), None);
        assert_eq!(t.option_for_value(255), None);
        // Off and on, nothing else.
        assert_eq!(SHOW_MODE.option("on").map(|o| o.value), Some(1));
        assert_eq!(SHOW_MODE.option("off").map(|o| o.value), Some(0));
        for bad in ["1", "0", "true", "On", ""] {
            assert_eq!(SHOW_MODE.option(bad), None, "{bad:?}");
        }
        assert_eq!(SHOW_MODE.option_for_value(2), None);
        assert_eq!(RIGHTSIGHT_SPEED.option("1"), None);
        assert_eq!(RIGHTSIGHT_SPEED.option_for_value(3), None);
    }

    #[test]
    fn rows_are_looked_up_by_usb_id() {
        let mx_brio = UsbId {
            vendor: 0x046d,
            product: 0x0944,
        };
        let names: Vec<&str> = rows_for(mx_brio).iter().map(|r| r.name).collect();
        assert_eq!(
            names,
            [
                "field_of_view",
                "show_mode",
                "exposure_compensation",
                "vibrance",
                "temperature_compensation",
                "rightsight",
                "rightsight_speed",
                "rightsight_distance",
            ]
        );
        // The Brio: the field of view only.
        let brio = UsbId {
            vendor: 0x046d,
            product: 0x085e,
        };
        assert_eq!(rows_for(brio), [&FIELD_OF_VIEW]);
        for (vendor, product) in [(0x046d, 0x0943), (0x1234, 0x0944), (0, 0)] {
            assert!(rows_for(UsbId { vendor, product }).is_empty());
        }
    }

    #[test]
    fn guids_are_in_descriptor_byte_order() {
        assert_eq!(
            parse_guid("49e40215-f434-47fe-b158-0e885023e51b"),
            Some(FOV_GUID)
        );
        assert_eq!(
            parse_guid("49E40215-F434-47FE-B158-0E885023E51B"),
            Some(FOV_GUID)
        );
        assert_eq!(FIELD_OF_VIEW.guid_bytes(), Some(FOV_GUID));
        for bad in [
            "",
            "49e40215f43447feb1580e885023e51b",
            "49e40215-f434-47fe-b158-0e885023e51",
            "49e40215-f434-47fe-b158-0e885023e51bb",
            "49e4021g-f434-47fe-b158-0e885023e51b",
            "49e40215-f434-47fe-b158-0e88-5023e51b",
            "+9e40215-f434-47fe-b158-0e885023e51b",
        ] {
            assert_eq!(parse_guid(bad), None, "{bad:?}");
        }
    }

    fn interface(number: u8, class: u8, subclass: u8) -> Vec<u8> {
        vec![9, DT_INTERFACE, number, 0, 1, class, subclass, 0, 0]
    }

    fn extension_unit(unit: u8, guid: &[u8; 16]) -> Vec<u8> {
        let mut d = vec![28, DT_CS_INTERFACE, VC_EXTENSION_UNIT, unit];
        d.extend_from_slice(guid);
        d.extend_from_slice(&[14, 1, 3, 3, 0xff, 0xff, 0x00, 0]);
        assert_eq!(d.len(), 28);
        d
    }

    /// A synthetic camera: device, configuration, association, a
    /// VideoControl interface (header, camera terminal, an XU of another
    /// GUID, then `vc_tail`), an endpoint, then a VideoStreaming interface
    /// carrying `vs_tail`.
    fn camera(vc_tail: &[u8], vs_tail: &[u8]) -> Vec<u8> {
        let mut d = vec![
            18, 1, 0x00, 0x02, 0xef, 0x02, 0x01, 0x40, 0x6d, 0x04, 0x44, 0x09,
        ];
        d.extend_from_slice(&[0, 0, 1, 2, 3, 1]);
        d.extend_from_slice(&[9, DT_CONFIG, 0, 0, 2, 1, 0, 0x80, 0xfa]);
        d.extend_from_slice(&[8, 0x0b, 0, 2, CLASS_VIDEO, 0x03, 0, 0]);
        d.extend(interface(0, CLASS_VIDEO, SUBCLASS_VIDEO_CONTROL));
        d.extend_from_slice(&[13, DT_CS_INTERFACE, 0x01, 0, 1, 0, 0, 0, 0, 0, 0, 1, 1]);
        d.extend_from_slice(&[
            18,
            DT_CS_INTERFACE,
            0x02,
            1,
            1,
            2,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            3,
            0,
            0,
        ]);
        d.extend(extension_unit(2, &[0x6a; 16]));
        d.extend_from_slice(vc_tail);
        d.extend_from_slice(&[7, 0x05, 0x83, 0x03, 0x40, 0x00, 0x08]);
        d.extend(interface(1, CLASS_VIDEO, 0x02));
        d.extend_from_slice(vs_tail);
        d
    }

    /// A VideoStreaming `VS_FORMAT_MJPEG` (subtype 0x06 too), long enough
    /// and with the GUID's bytes where an XU's GUID would be.
    fn mjpeg_format_with(guid: &[u8; 16]) -> Vec<u8> {
        let mut d = vec![28, DT_CS_INTERFACE, 0x06, 10];
        d.extend_from_slice(guid);
        d.extend_from_slice(&[0; 8]);
        d
    }

    /// A configuration descriptor of value `value` with one VideoControl
    /// interface carrying `vc_tail`.
    fn configuration(value: u8, vc_tail: &[u8]) -> Vec<u8> {
        let mut d = vec![9, DT_CONFIG, 0, 0, 1, value, 0, 0x80, 0xfa];
        d.extend(interface(0, CLASS_VIDEO, SUBCLASS_VIDEO_CONTROL));
        d.extend_from_slice(vc_tail);
        d
    }

    #[test]
    fn only_the_given_configuration_is_walked() {
        // `camera` is configuration 1, without the unit; configuration 2
        // carries it.
        let mut two = camera(&[], &[]);
        two.extend(configuration(2, &extension_unit(10, &FOV_GUID)));
        assert_eq!(find_extension_unit(&two, 1, &FOV_GUID), None);
        assert_eq!(find_extension_unit(&two, 2, &FOV_GUID), Some(10));
        assert_eq!(find_extension_unit(&two, 3, &FOV_GUID), None);
        // The unit in both under different ids: the given one's id, in
        // either order.
        let mut both = camera(&extension_unit(7, &FOV_GUID), &[]);
        both.extend(configuration(2, &extension_unit(10, &FOV_GUID)));
        assert_eq!(find_extension_unit(&both, 1, &FOV_GUID), Some(7));
        assert_eq!(find_extension_unit(&both, 2, &FOV_GUID), Some(10));
        let mut reversed = camera(&[], &[])[..18].to_vec();
        reversed.extend(configuration(2, &extension_unit(10, &FOV_GUID)));
        reversed.extend(configuration(1, &extension_unit(7, &FOV_GUID)));
        assert_eq!(find_extension_unit(&reversed, 1, &FOV_GUID), Some(7));
        assert_eq!(find_extension_unit(&reversed, 2, &FOV_GUID), Some(10));
        // A VideoControl interface before any configuration descriptor, or
        // under one too short to carry its value, is no configuration's.
        let mut orphan = interface(0, CLASS_VIDEO, SUBCLASS_VIDEO_CONTROL);
        orphan.extend(extension_unit(10, &FOV_GUID));
        let mut short_config = vec![5, DT_CONFIG, 0, 0, 1];
        short_config.extend(orphan.clone());
        for value in [0, 1] {
            assert_eq!(find_extension_unit(&orphan, value, &FOV_GUID), None);
            assert_eq!(find_extension_unit(&short_config, value, &FOV_GUID), None);
        }
        // The resolved control takes the same unit.
        let control = XuControl::resolve(&FIELD_OF_VIEW, &both, 2).unwrap();
        assert_eq!(control.unit(), 10);
        assert!(XuControl::resolve(&FIELD_OF_VIEW, &two, 1).is_none());
    }

    #[test]
    fn the_extension_unit_is_found_by_guid_in_video_control_only() {
        let found = camera(&extension_unit(10, &FOV_GUID), &[]);
        assert_eq!(find_extension_unit(&found, 1, &FOV_GUID), Some(10));
        // The other unit's GUID finds the other unit.
        assert_eq!(find_extension_unit(&found, 1, &[0x6a; 16]), Some(2));
        // A device without the GUID.
        let without = camera(&[], &[11, DT_CS_INTERFACE, 0x06, 1, 1, 0, 1, 0, 0, 0, 0]);
        assert_eq!(find_extension_unit(&without, 1, &FOV_GUID), None);
        // The GUID's bytes in a VS_FORMAT_MJPEG must not match.
        let in_streaming = camera(&[], &mjpeg_format_with(&FOV_GUID));
        assert_eq!(find_extension_unit(&in_streaming, 1, &FOV_GUID), None);
        // A second configuration leaves the VideoControl interface.
        let mut after_config = camera(&[], &[]);
        after_config.extend_from_slice(&[9, DT_CONFIG, 0, 0, 1, 2, 0, 0x80, 0xfa]);
        after_config.extend(mjpeg_format_with(&FOV_GUID));
        assert_eq!(find_extension_unit(&after_config, 1, &FOV_GUID), None);
        // An interface descriptor too short to name its class leaves it too.
        let mut short_iface = camera(&[], &[]);
        short_iface.extend_from_slice(&[5, DT_INTERFACE, 2, 0, 0]);
        short_iface.extend(extension_unit(10, &FOV_GUID));
        assert_eq!(find_extension_unit(&short_iface, 1, &FOV_GUID), None);
        // A unit descriptor too short for a GUID is skipped, and the walk
        // goes on past it.
        let mut short = vec![19, DT_CS_INTERFACE, 0x06, 10];
        short.extend_from_slice(&FOV_GUID[..15]);
        assert_eq!(
            find_extension_unit(&camera(&short, &[]), 1, &FOV_GUID),
            None
        );
        short.extend(extension_unit(12, &FOV_GUID));
        assert_eq!(
            find_extension_unit(&camera(&short, &[]), 1, &FOV_GUID),
            Some(12)
        );
    }

    #[test]
    fn malformed_descriptors_end_the_walk() {
        // Truncated: the unit's bLength runs past the end.
        let mut truncated = camera(&[], &[]);
        let unit = extension_unit(10, &FOV_GUID);
        truncated.extend_from_slice(&interface(2, CLASS_VIDEO, SUBCLASS_VIDEO_CONTROL));
        truncated.extend_from_slice(&unit[..15]);
        assert_eq!(find_extension_unit(&truncated, 1, &FOV_GUID), None);
        // A zero bLength before the unit, inside VideoControl: the walk
        // stops there, it does not spin or skip ahead.
        for bad in [0u8, 1] {
            let mut tail = vec![bad, DT_CS_INTERFACE, 0, 0];
            tail.extend(extension_unit(10, &FOV_GUID));
            assert_eq!(
                find_extension_unit(&camera(&tail, &[]), 1, &FOV_GUID),
                None,
                "{bad}"
            );
        }
        // Empty, single byte, a lone header.
        for d in [&[][..], &[9][..], &[0, 0][..], &[2, DT_CS_INTERFACE][..]] {
            assert_eq!(find_extension_unit(d, 1, &FOV_GUID), None, "{d:?}");
        }
        // The unit as the very last descriptor, exactly to the end.
        let exact = camera(&extension_unit(7, &FOV_GUID), &[]);
        let mut cut = exact.clone();
        let at = cut.len() - 7 - 9;
        cut.truncate(at);
        assert_eq!(find_extension_unit(&cut, 1, &FOV_GUID), Some(7));
    }

    #[test]
    fn the_allowlist_admits_only_a_rows_unit_and_selector() {
        let allow = [XuTarget {
            unit: 10,
            guid: LOGITECH_UNIT_GUID,
            selector: 5,
            size: 1,
        }];
        for q in XU_QUERIES {
            let len = match q {
                UVC_GET_LEN => 2,
                _ => 1,
            };
            assert!(check_xu_query(&allow, 10, 5, q, len).is_ok(), "{q:#x}");
        }
        let refusals = [
            // The LED: unit 11, selector 9 (and either alone).
            (11, 9, UVC_GET_CUR, 5),
            (11, 9, UVC_SET_CUR, 5),
            (11, 5, UVC_GET_CUR, 1),
            (10, 9, UVC_GET_CUR, 1),
            (10, 9, UVC_SET_CUR, 1),
            (10, 4, UVC_GET_LEN, 2),
            // Other codes: GET_MIN, GET_MAX, GET_RES, RC_UNDEFINED.
            (10, 5, 0x82, 1),
            (10, 5, 0x83, 1),
            (10, 5, 0x84, 1),
            (10, 5, 0x00, 1),
            // Wrong buffer sizes.
            (10, 5, UVC_GET_LEN, 1),
            (10, 5, UVC_GET_INFO, 2),
            (10, 5, UVC_SET_CUR, 2),
            (10, 5, UVC_GET_CUR, 0),
        ];
        for (unit, sel, q, len) in refusals {
            let e = check_xu_query(&allow, unit, sel, q, len).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{unit} {sel} {q:#x}");
        }
        // An allowlist entry that is no data row's selector is refused too.
        let rogue = [XuTarget {
            unit: 11,
            guid: LOGITECH_UNIT_GUID,
            selector: 9,
            size: 5,
        }];
        assert!(check_xu_query(&rogue, 11, 9, UVC_GET_CUR, 5).is_err());
        // A row's selector and size under another unit's GUID: refused.
        for (guid, sel, size) in [
            (RIGHTSIGHT_UNIT_GUID, 5, 1),
            (LOGITECH_UNIT_GUID, 1, 30),
            ("2c49d16a-32b8-4485-3ea8-643a152362f2", 5, 1),
        ] {
            let other = [XuTarget {
                unit: 10,
                guid,
                selector: sel,
                size,
            }];
            let e = check_xu_query(&other, 10, sel, UVC_GET_CUR, usize::from(size)).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{guid} {sel}");
        }
        // Nothing allowed: nothing passes.
        assert!(check_xu_query(&[], 10, 5, UVC_GET_CUR, 1).is_err());
    }

    #[test]
    fn queries_are_refused_before_the_ioctl() {
        // /dev/null would answer ENOTTY: a refusal must come first.
        let node = ControlNode::open(Path::new("/dev/null"), true).unwrap();
        let fov = XuControl {
            row: &FIELD_OF_VIEW,
            unit: 10,
        };
        let e = fov.set(&node, "45").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        let e = xu_query(&mut NodeIo(&node), &[], 11, 9, UVC_GET_CUR, &mut [0; 5]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        // An allowed query reaches the ioctl.
        let e = fov.current(&node).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::ENOTTY));
        assert!(fov.probe(&node).is_err());
        assert_eq!(
            fov.set(&node, "78").unwrap_err().raw_os_error(),
            Some(libc::ENOTTY)
        );
    }

    #[test]
    fn the_state_is_mapped_through_the_options() {
        let r = &FIELD_OF_VIEW;
        // As measured on an MX Brio: INFO 0x03, CUR 0x02, DEF 0x00.
        let s = map_state(r, 0x03, 0x02, Some(0x00));
        assert_eq!(s.current_option.map(|o| o.id), Some("65"));
        assert_eq!(s.default.id, "90");
        // An unknown current byte; an unknown or unreadable default.
        let s = map_state(r, 0x03, 0x07, Some(0x09));
        assert_eq!((s.current, s.current_option), (0x07, None));
        assert_eq!(s.default.id, "90");
        assert_eq!(map_state(r, 0x03, 0x01, None).default.id, "90");
        assert_eq!(map_state(r, 0x03, 0x01, Some(0x01)).default.id, "78");
        // The device must agree: GET_LEN the row's size, get and set bits.
        assert!(device_agrees(r, 1, 0x03));
        assert!(device_agrees(r, 1, 0x0f));
        assert!(!device_agrees(r, 2, 0x03));
        assert!(!device_agrees(r, 0, 0x03));
        assert!(!device_agrees(r, 1, 0x01));
        assert!(!device_agrees(r, 1, 0x02));
        assert!(!device_agrees(r, 1, 0x00));
    }

    /// The RightSight unit's GUID in descriptor byte order.
    const RIGHTSIGHT_GUID: [u8; 16] = [
        0x1f, 0x4c, 0x14, 0x29, 0x8d, 0x65, 0x47, 0x4f, 0xa4, 0x0f, 0x0e, 0xd3, 0x39, 0x42, 0x8b,
        0xf0,
    ];

    /// One control of a [`FakeIo`].
    #[derive(Debug, Clone)]
    struct FakeCtl {
        unit: u8,
        selector: u8,
        len: u16,
        info: u8,
        cur: Vec<u8>,
        def: Vec<u8>,
    }

    /// A fake device behind [`XuIo`]: its controls, every query it saw
    /// (unit, selector, code, the bytes written or read) and the codes it
    /// fails.
    #[derive(Debug, Default)]
    struct FakeIo {
        ctls: Vec<FakeCtl>,
        log: Vec<(u8, u8, u8, Vec<u8>)>,
        fail: Vec<u8>,
    }

    impl FakeIo {
        fn with(ctls: &[FakeCtl]) -> FakeIo {
            FakeIo {
                ctls: ctls.to_vec(),
                ..FakeIo::default()
            }
        }

        fn ctl(&mut self, unit: u8, selector: u8) -> &mut FakeCtl {
            self.ctls
                .iter_mut()
                .find(|c| c.unit == unit && c.selector == selector)
                .unwrap()
        }

        /// The codes it saw, in order.
        fn codes(&self) -> Vec<u8> {
            self.log.iter().map(|(_, _, q, _)| *q).collect()
        }

        /// The bytes of each `SET_CUR`.
        fn writes(&self) -> Vec<Vec<u8>> {
            self.log
                .iter()
                .filter(|(_, _, q, _)| *q == UVC_SET_CUR)
                .map(|(_, _, _, d)| d.clone())
                .collect()
        }

        /// Every query it saw is one a row allows: an allowed code on a
        /// row's selector with a buffer of the row's size, never the
        /// Logitech unit's selectors 0x06 or 0x12.
        fn assert_only_allowed(&self) {
            for (unit, sel, q, data) in &self.log {
                assert!(XU_QUERIES.contains(q), "{q:#x}");
                assert!(*sel != 0x12, "unit {unit} selector 0x12");
                assert!(*sel != 0x06, "unit {unit} selector 0x06");
                let sizes: Vec<usize> = XU_ROWS
                    .iter()
                    .filter(|r| r.selector == *sel)
                    .map(|r| usize::from(r.size))
                    .collect();
                let allowed = match *q {
                    UVC_GET_LEN => data.len() == 2,
                    UVC_GET_INFO => data.len() == 1,
                    _ => sizes.contains(&data.len()),
                };
                assert!(
                    !sizes.is_empty() && allowed,
                    "unit {unit} selector {sel} {q:#x} {} bytes",
                    data.len()
                );
            }
        }
    }

    impl XuIo for FakeIo {
        fn query(&mut self, unit: u8, selector: u8, query: u8, data: &mut [u8]) -> io::Result<()> {
            if self.fail.contains(&query) {
                self.log.push((unit, selector, query, data.to_vec()));
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
            let Some(c) = self
                .ctls
                .iter_mut()
                .find(|c| c.unit == unit && c.selector == selector)
            else {
                self.log.push((unit, selector, query, data.to_vec()));
                return Err(io::Error::from_raw_os_error(libc::ENOENT));
            };
            match query {
                UVC_GET_LEN => data.copy_from_slice(&c.len.to_le_bytes()),
                UVC_GET_INFO => data[0] = c.info,
                UVC_GET_CUR => data.copy_from_slice(&c.cur),
                UVC_GET_DEF => data.copy_from_slice(&c.def),
                UVC_SET_CUR => c.cur = data.to_vec(),
                _ => panic!("the fake got {query:#x}"),
            }
            self.log.push((unit, selector, query, data.to_vec()));
            Ok(())
        }
    }

    fn one_byte(unit: u8, selector: u8, cur: u8, def: u8) -> FakeCtl {
        FakeCtl {
            unit,
            selector,
            len: 1,
            info: 0x03,
            cur: vec![cur],
            def: vec![def],
        }
    }

    /// The RightSight block as an MX Brio reads: byte 0 0x01, off, speed
    /// and distance default, byte 4 `b4`, byte 5 0x01, then a non-zero
    /// pattern (a write that zeroed or invented them would show).
    fn block(b4: u8) -> Vec<u8> {
        let mut d = vec![0x01, 0x00, 0x01, 0x01, b4, 0x01];
        d.extend((6..30).map(|i| 0xa0 | i as u8));
        assert_eq!(d.len(), 30);
        d
    }

    fn rightsight_ctl(unit: u8, cur: Vec<u8>) -> FakeCtl {
        FakeCtl {
            unit,
            selector: 0x01,
            len: 30,
            info: 0x03,
            def: block(0x5a),
            cur,
        }
    }

    fn control(row: &'static XuRow, unit: u8) -> XuControl {
        XuControl { row, unit }
    }

    #[test]
    fn the_new_rows_resolve_by_guid_not_by_unit_id() {
        let both = [
            extension_unit(10, &FOV_GUID),
            extension_unit(22, &RIGHTSIGHT_GUID),
        ]
        .concat();
        let elsewhere = [
            extension_unit(22, &FOV_GUID),
            extension_unit(4, &RIGHTSIGHT_GUID),
        ]
        .concat();
        for (tail, a, b) in [(both, 10, 22), (elsewhere, 22, 4)] {
            let desc = camera(&tail, &[]);
            for row in XU_ROWS {
                let unit = XuControl::resolve(row, &desc, 1).map(|x| x.unit());
                let want = if row.guid == RIGHTSIGHT_UNIT_GUID {
                    b
                } else {
                    a
                };
                assert_eq!(unit, Some(want), "{}", row.name);
            }
            // Through sysfs: every MX Brio row, the Brio's field of view.
            let (root, node) = fake_sysfs("046d\n", "0944\n", &desc);
            let found: Vec<(&str, u8)> = find_xu_controls(root.path(), &node)
                .unwrap()
                .iter()
                .map(|x| (x.row().name, x.unit()))
                .collect();
            assert_eq!(found.len(), XU_ROWS.len());
            assert_eq!(found[0], ("field_of_view", a));
            assert_eq!(found[5], ("rightsight", b));
            assert_eq!(found[7], ("rightsight_distance", b));
            let (root, node) = fake_sysfs("046d\n", "085e\n", &desc);
            let found = find_xu_controls(root.path(), &node).unwrap();
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].row().name, "field_of_view");
        }
        // The Logitech unit alone: no RightSight rows.
        let desc = camera(&extension_unit(10, &FOV_GUID), &[]);
        assert!(XuControl::resolve(&RIGHTSIGHT, &desc, 1).is_none());
        assert_eq!(
            XuControl::resolve(&SHOW_MODE, &desc, 1).map(|x| x.unit()),
            Some(10)
        );
        assert_eq!(RIGHTSIGHT.guid_bytes(), Some(RIGHTSIGHT_GUID));
    }

    #[test]
    fn a_value_outside_the_row_is_refused_before_any_query() {
        let mut io = FakeIo::with(&[
            one_byte(10, 0x13, 0x80, 0x80),
            one_byte(10, 0x14, 0x80, 0x80),
            one_byte(10, 0x15, 0x39, 0x32),
            one_byte(10, 0x0d, 0x01, 0x00),
            rightsight_ctl(22, block(0x5a)),
        ]);
        let cases: [(&'static XuRow, u8, &[&str]); 7] = [
            (
                &EXPOSURE_COMPENSATION,
                10,
                &["256", "-1", "+130", "0130", "00"],
            ),
            (&TEMPERATURE_COMPENSATION, 10, &["101", "255", "-1", "050"]),
            (&VIBRANCE, 10, &["256", "-1", "07", ""]),
            (&SHOW_MODE, 10, &["1", "2", "true", "On"]),
            (&RIGHTSIGHT, 22, &["1", "true"]),
            (&RIGHTSIGHT_SPEED, 22, &["2", "fast", "Faster"]),
            (&RIGHTSIGHT_DISTANCE, 22, &["0", "far"]),
        ];
        for (row, unit, bad) in cases {
            for id in bad {
                let e = control(row, unit).set_with(&mut io, id).unwrap_err();
                assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{} {id:?}", row.name);
            }
        }
        assert!(io.log.is_empty(), "{:?}", io.log);
    }

    #[test]
    fn a_one_byte_write_reads_first_and_skips_an_equal_byte() {
        let mut io = FakeIo::with(&[one_byte(10, 0x13, 0x80, 0x80)]);
        let x = control(&EXPOSURE_COMPENSATION, 10);
        x.set_with(&mut io, "130").unwrap();
        assert_eq!(
            io.codes(),
            [UVC_GET_LEN, UVC_GET_INFO, UVC_GET_CUR, UVC_SET_CUR]
        );
        assert_eq!(io.writes(), [vec![130]]);
        assert_eq!(x.current_with(&mut io).unwrap(), 130);
        // Already 130: checked and read, nothing written.
        io.log.clear();
        x.set_with(&mut io, "130").unwrap();
        assert_eq!(io.codes(), [UVC_GET_LEN, UVC_GET_INFO, UVC_GET_CUR]);
        // The bounds themselves are written.
        x.set_with(&mut io, "0").unwrap();
        x.set_with(&mut io, "255").unwrap();
        assert_eq!(io.writes(), [vec![0], vec![255]]);
        // A bool row: on is 1, off is 0.
        let mut io = FakeIo::with(&[one_byte(10, 0x0d, 0x00, 0x00)]);
        let show = control(&SHOW_MODE, 10);
        show.set_with(&mut io, "on").unwrap();
        show.set_with(&mut io, "on").unwrap();
        show.set_with(&mut io, "off").unwrap();
        assert_eq!(io.writes(), [vec![1], vec![0]]);
        io.assert_only_allowed();
    }

    #[test]
    fn a_failed_read_writes_nothing() {
        for (row, ctl) in [
            (&VIBRANCE, one_byte(10, 0x14, 0x80, 0x80)),
            (&RIGHTSIGHT, rightsight_ctl(22, block(0x5a))),
        ] {
            let mut io = FakeIo::with(std::slice::from_ref(&ctl));
            io.fail = vec![UVC_GET_CUR];
            let id = if row.name == "rightsight" {
                "on"
            } else {
                "200"
            };
            let x = control(row, ctl.unit);
            let e = x.set_with(&mut io, id).unwrap_err();
            assert_eq!(e.raw_os_error(), Some(libc::EIO), "{}", row.name);
            assert_eq!(
                io.codes(),
                [UVC_GET_LEN, UVC_GET_INFO, UVC_GET_CUR],
                "{}",
                row.name
            );
            assert_eq!(io.ctl(ctl.unit, ctl.selector).cur, ctl.cur);
            assert!(x.probe_with(&mut io).is_err());
            assert!(io.writes().is_empty());
        }
    }

    #[test]
    fn the_rightsight_block_is_read_modified_and_written_whole() {
        let mut io = FakeIo::with(&[rightsight_ctl(22, block(0x5a))]);
        let on = control(&RIGHTSIGHT, 22);
        let speed = control(&RIGHTSIGHT_SPEED, 22);
        let distance = control(&RIGHTSIGHT_DISTANCE, 22);
        let steps: [(XuControl, &str, usize, u8, u8); 3] = [
            (on, "on", 1, 1, 0x5a),
            (speed, "faster", 2, 2, 0x50),
            (distance, "closer", 3, 0, 0x5a),
        ];
        for (i, (x, id, byte, value, b4)) in steps.into_iter().enumerate() {
            // The camera changes byte 4 (and a tail byte) between writes:
            // each write must carry what it reads now, not an earlier read.
            let ctl = io.ctl(22, 1);
            ctl.cur[4] = b4;
            ctl.cur[29] = 0xa0 + i as u8;
            let before = ctl.cur.clone();
            io.log.clear();
            x.set_with(&mut io, id).unwrap();
            assert_eq!(
                io.codes(),
                [UVC_GET_LEN, UVC_GET_INFO, UVC_GET_CUR, UVC_SET_CUR],
                "{id}"
            );
            let written = io.writes().remove(0);
            assert_eq!(written.len(), 30);
            for (n, (w, b)) in written.iter().zip(&before).enumerate() {
                if n == byte {
                    assert_eq!(*w, value, "{id}: byte {n}");
                } else {
                    assert_eq!(w, b, "{id}: byte {n} is not the fresh read's");
                }
            }
            assert_eq!(x.current_with(&mut io).unwrap(), value);
        }
        // All three changed, every other byte as the camera last had it.
        let mut want = block(0x5a);
        want[1] = 1;
        want[2] = 2;
        want[3] = 0;
        want[29] = 0xa2;
        assert_eq!(io.ctl(22, 1).cur, want);
        // A byte already right: checked and read, nothing written.
        io.log.clear();
        speed.set_with(&mut io, "faster").unwrap();
        assert_eq!(io.codes(), [UVC_GET_LEN, UVC_GET_INFO, UVC_GET_CUR]);
        io.assert_only_allowed();
    }

    #[test]
    fn a_block_row_is_probed_on_its_byte() {
        let mut cur = block(0x5a);
        cur[1] = 1;
        cur[3] = 2;
        let mut io = FakeIo::with(&[rightsight_ctl(22, cur)]);
        io.ctl(22, 1).def[2] = 2;
        let s = control(&RIGHTSIGHT, 22)
            .probe_with(&mut io)
            .unwrap()
            .unwrap();
        assert_eq!((s.current, s.current_option.map(|o| o.id)), (1, Some("on")));
        assert_eq!((s.default_raw, s.default.id), (Some(0), "off"));
        let s = control(&RIGHTSIGHT_SPEED, 22)
            .probe_with(&mut io)
            .unwrap()
            .unwrap();
        assert_eq!(
            (s.current, s.default_raw, s.default.id),
            (1, Some(2), "faster")
        );
        let s = control(&RIGHTSIGHT_DISTANCE, 22)
            .probe_with(&mut io)
            .unwrap()
            .unwrap();
        assert_eq!(s.current_option.map(|o| o.id), Some("farther"));
        assert_eq!(s.default.id, "default");
        // Every query read the whole block.
        assert!(io.log.iter().all(|(_, _, q, d)| match *q {
            UVC_GET_CUR | UVC_GET_DEF => d.len() == 30,
            _ => true,
        }));
        // An unknown byte; a failing GET_DEF: the row's fallback default.
        io.ctl(22, 1).cur[2] = 7;
        io.fail = vec![UVC_GET_DEF];
        let s = control(&RIGHTSIGHT_SPEED, 22)
            .probe_with(&mut io)
            .unwrap()
            .unwrap();
        assert_eq!((s.current, s.current_option), (7, None));
        assert_eq!((s.default_raw, s.default.id), (None, "default"));
        // GET_LEN other than 30: not offered, and nothing more read.
        for len in [1, 29, 31, 0] {
            io.ctl(22, 1).len = len;
            io.log.clear();
            assert_eq!(control(&RIGHTSIGHT, 22).probe_with(&mut io).unwrap(), None);
            assert_eq!(io.codes(), [UVC_GET_LEN, UVC_GET_INFO], "{len}");
        }
        io.assert_only_allowed();
    }

    #[test]
    fn a_range_row_is_probed_against_its_range() {
        let mut io = FakeIo::with(&[one_byte(10, 0x15, 0x39, 0x32)]);
        let x = control(&TEMPERATURE_COMPENSATION, 10);
        let s = x.probe_with(&mut io).unwrap().unwrap();
        assert_eq!(s.current_option.map(|o| o.id), Some("57"));
        assert_eq!(s.default.id, "50");
        // Outside the range: no option; a default outside: the row's.
        io.ctl(10, 0x15).cur = vec![101];
        io.ctl(10, 0x15).def = vec![200];
        let s = x.probe_with(&mut io).unwrap().unwrap();
        assert_eq!((s.current, s.current_option), (101, None));
        assert_eq!((s.default_raw, s.default.id), (Some(200), "50"));
        // The bounds are inside.
        for b in [0, 100] {
            io.ctl(10, 0x15).cur = vec![b];
            let s = x.probe_with(&mut io).unwrap().unwrap();
            assert_eq!(s.current_option.map(|o| o.value), Some(b));
        }
        io.assert_only_allowed();
    }

    #[test]
    fn no_row_reaches_the_logitech_units_selector_0x12() {
        assert!(
            !XU_ROWS
                .iter()
                .any(|r| r.guid == LOGITECH_UNIT_GUID && r.selector == 0x12)
        );
        assert!(
            !XU_ROWS
                .iter()
                .any(|r| r.selector == 0x12 || r.selector == 0x06)
        );
        assert!(
            !XU_ROWS
                .iter()
                .any(|r| r.guid.starts_with("ffe52d21") || r.guid.starts_with("2c49d16a"))
        );
        // Not even an allowlist entry for it lets a query through.
        for size in [1, 2, 30] {
            let allow = [XuTarget {
                unit: 10,
                guid: LOGITECH_UNIT_GUID,
                selector: 0x12,
                size,
            }];
            for q in XU_QUERIES {
                let e = check_xu_query(&allow, 10, 0x12, q, usize::from(size)).unwrap_err();
                assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
            }
            let mut io = FakeIo::with(&[one_byte(10, 0x12, 0x32, 0x32)]);
            let e = xu_query(&mut io, &allow, 10, 0x12, UVC_GET_CUR, &mut [0]).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
            assert!(io.log.is_empty());
        }
        // The RightSight block: its size only.
        let allow = [XuTarget {
            unit: 22,
            guid: RIGHTSIGHT_UNIT_GUID,
            selector: 1,
            size: 30,
        }];
        assert!(check_xu_query(&allow, 22, 1, UVC_GET_CUR, 30).is_ok());
        assert!(check_xu_query(&allow, 22, 1, UVC_SET_CUR, 30).is_ok());
        assert!(check_xu_query(&allow, 22, 1, UVC_GET_LEN, 2).is_ok());
        assert!(check_xu_query(&allow, 22, 1, UVC_GET_CUR, 1).is_err());
        assert!(check_xu_query(&allow, 22, 1, UVC_SET_CUR, 29).is_err());
        assert!(check_xu_query(&allow, 22, 2, UVC_GET_CUR, 30).is_err());
    }

    #[test]
    fn the_deny_list_is_refused_whatever_the_allowlist_says() {
        let denied: [(&'static str, u8, u16); 6] = [
            (LOGITECH_UNIT_GUID, 0x06, 1),
            (LOGITECH_UNIT_GUID, 0x0b, 1),
            (LOGITECH_UNIT_GUID, 0x0c, 1),
            (LOGITECH_UNIT_GUID, 0x12, 1),
            (LOGITECH_DENIED_UNIT_GUID, 5, 1),
            (LOGITECH_DENIED_UNIT_GUID, 1, 30),
        ];
        for (guid, sel, size) in denied {
            assert!(is_denied(guid, sel), "{guid} {sel:#x}");
            assert!(is_denied(&guid.to_uppercase(), sel), "{guid} {sel:#x}");
            let allow = [XuTarget {
                unit: 10,
                guid,
                selector: sel,
                size,
            }];
            for q in XU_QUERIES {
                let len = match q {
                    UVC_GET_LEN => 2,
                    UVC_GET_INFO => 1,
                    _ => usize::from(size),
                };
                let mut io = FakeIo::with(&[one_byte(10, sel, 0, 0)]);
                let e = xu_query(&mut io, &allow, 10, sel, q, &mut vec![0; len]).unwrap_err();
                assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{guid} {sel:#x}");
                assert!(io.log.is_empty());
            }
        }
        // Every selector of the denied unit; no row is on the list.
        assert!((0..=255).all(|s| is_denied(LOGITECH_DENIED_UNIT_GUID, s)));
        assert!(!XU_ROWS.iter().any(|r| is_denied(r.guid, r.selector)));
        assert!(!is_denied(LOGITECH_UNIT_GUID, 0x05));
        assert!(!is_denied(RIGHTSIGHT_UNIT_GUID, 0x06));
    }

    /// A row made outside [`XU_ROWS`] on the denied unit, with the field
    /// of view's selector and size.
    static ROGUE_UNIT: XuRow = XuRow {
        guid: LOGITECH_DENIED_UNIT_GUID,
        ..FIELD_OF_VIEW
    };

    /// A row made outside [`XU_ROWS`]: the RightSight block's selector and
    /// size, on the Logitech unit.
    static ROGUE_BLOCK: XuRow = XuRow {
        guid: LOGITECH_UNIT_GUID,
        ..RIGHTSIGHT
    };

    /// A row equal to a known one but for its byte.
    static ROGUE_OFFSET: XuRow = XuRow {
        offset: 4,
        ..RIGHTSIGHT
    };

    #[test]
    fn a_row_outside_the_table_resolves_to_nothing_and_reaches_no_query() {
        let rogue_guid = parse_guid(LOGITECH_DENIED_UNIT_GUID).unwrap();
        let desc = camera(
            &[
                extension_unit(10, &FOV_GUID),
                extension_unit(22, &RIGHTSIGHT_GUID),
                extension_unit(9, &rogue_guid),
            ]
            .concat(),
            &[],
        );
        for (row, unit) in [(&ROGUE_UNIT, 9), (&ROGUE_BLOCK, 10), (&ROGUE_OFFSET, 22)] {
            assert!(!is_known_row(row), "{}", row.name);
            assert!(XuControl::resolve(row, &desc, 1).is_none(), "{}", row.name);
            // Made by hand anyway: every query is refused before the fake.
            let mut io = FakeIo::with(&[
                one_byte(9, 5, 0, 0),
                one_byte(10, 1, 0, 0),
                rightsight_ctl(22, block(0x5a)),
            ]);
            io.ctl(10, 1).len = 30;
            io.ctl(10, 1).cur = block(0x5a);
            io.ctl(10, 1).def = block(0x5a);
            let x = control(row, unit);
            let on = if row.kind == XuKind::Bool { "on" } else { "78" };
            for e in [
                x.set_with(&mut io, on).unwrap_err(),
                x.probe_with(&mut io).unwrap_err(),
                x.current_with(&mut io).unwrap_err(),
            ] {
                assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{}: {e}", row.name);
            }
            assert!(io.log.is_empty(), "{}: {:?}", row.name, io.log);
        }
        // The known rows still resolve on the same descriptors.
        assert_eq!(
            XuControl::resolve(&FIELD_OF_VIEW, &desc, 1).map(|x| x.unit()),
            Some(10)
        );
        assert_eq!(
            XuControl::resolve(&RIGHTSIGHT, &desc, 1).map(|x| x.unit()),
            Some(22)
        );
    }

    #[test]
    fn a_control_that_no_longer_agrees_is_never_written() {
        // A one-byte row whose GET_LEN is now 2, or whose GET_INFO lacks
        // the set bit; the block with a wrong length.
        for (row, mut ctl, id) in [
            (&VIBRANCE, one_byte(10, 0x14, 0x80, 0x80), "200"),
            (&SHOW_MODE, one_byte(10, 0x0d, 0, 0), "on"),
            (&RIGHTSIGHT, rightsight_ctl(22, block(0x5a)), "on"),
        ] {
            for (len, info) in [(ctl.len + 1, 0x03), (0, 0x03), (ctl.len, 0x01)] {
                ctl.len = len;
                ctl.info = info;
                let before = ctl.cur.clone();
                let mut io = FakeIo::with(std::slice::from_ref(&ctl));
                let e = control(row, ctl.unit).set_with(&mut io, id).unwrap_err();
                assert_eq!(e.kind(), io::ErrorKind::Unsupported, "{}: {e}", row.name);
                assert_eq!(io.codes(), [UVC_GET_LEN, UVC_GET_INFO], "{}", row.name);
                assert!(io.writes().is_empty(), "{}", row.name);
                assert_eq!(io.ctl(ctl.unit, ctl.selector).cur, before);
                ctl.len = if row.size == 1 { 1 } else { 30 };
            }
        }
        // A failing GET_LEN or GET_INFO: no write either.
        for code in [UVC_GET_LEN, UVC_GET_INFO] {
            let mut io = FakeIo::with(&[one_byte(10, 0x14, 0x80, 0x80)]);
            io.fail = vec![code];
            let e = control(&VIBRANCE, 10).set_with(&mut io, "1").unwrap_err();
            assert_eq!(e.raw_os_error(), Some(libc::EIO));
            assert!(io.writes().is_empty());
            assert!(!io.codes().contains(&UVC_GET_CUR));
        }
    }

    /// The `major:minor` of a character device of this machine.
    fn char_number(path: &str) -> String {
        let rdev = fs::metadata(path).unwrap().rdev();
        format!("{}:{}", libc::major(rdev), libc::minor(rdev))
    }

    /// A fake sysfs: `dev/char/<major:minor of /dev/null>` links to the
    /// class device `video7` of the interface `1-2:1.0` of the USB device
    /// `1-2` (active configuration `configuration`). A decoy USB device
    /// `1-3` (`1234:abcd`, the FoV unit as 99) hangs under
    /// `class/video4linux/null`, the name lookup the module must not use.
    /// The node is `v4l/by-id/usb-cam-video-index0`, a link to `/dev/null`:
    /// a real character device, which a test cannot create.
    fn fake_sysfs_with(
        vendor: &str,
        product: &str,
        configuration: &str,
        descriptors: &[u8],
    ) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let usb = root.path().join("devices/pci0000:00/usb1/1-2");
        let class_dev = usb.join("1-2:1.0/video4linux/video7");
        fs::create_dir_all(&class_dev).unwrap();
        std::os::unix::fs::symlink("../../../1-2:1.0", class_dev.join("device")).unwrap();
        fs::write(usb.join("idVendor"), vendor).unwrap();
        fs::write(usb.join("idProduct"), product).unwrap();
        fs::write(usb.join("bConfigurationValue"), configuration).unwrap();
        fs::write(usb.join("descriptors"), descriptors).unwrap();
        fs::create_dir_all(root.path().join("dev/char")).unwrap();
        std::os::unix::fs::symlink(
            "../../devices/pci0000:00/usb1/1-2/1-2:1.0/video4linux/video7",
            root.path().join("dev/char").join(char_number("/dev/null")),
        )
        .unwrap();
        let decoy = root.path().join("devices/pci0000:00/usb1/1-3");
        fs::create_dir_all(decoy.join("1-3:1.0")).unwrap();
        fs::write(decoy.join("idVendor"), "1234\n").unwrap();
        fs::write(decoy.join("idProduct"), "abcd\n").unwrap();
        fs::write(decoy.join("bConfigurationValue"), "1\n").unwrap();
        fs::write(
            decoy.join("descriptors"),
            camera(&extension_unit(99, &FOV_GUID), &[]),
        )
        .unwrap();
        let class = root.path().join("class/video4linux/null");
        fs::create_dir_all(&class).unwrap();
        std::os::unix::fs::symlink(decoy.join("1-3:1.0"), class.join("device")).unwrap();
        fs::create_dir_all(root.path().join("v4l/by-id")).unwrap();
        let node = root.path().join("v4l/by-id/usb-cam-video-index0");
        std::os::unix::fs::symlink("/dev/null", &node).unwrap();
        (root, node)
    }

    fn fake_sysfs(vendor: &str, product: &str, descriptors: &[u8]) -> (tempfile::TempDir, PathBuf) {
        fake_sysfs_with(vendor, product, "1\n", descriptors)
    }

    #[test]
    fn the_usb_device_of_a_node_comes_from_sysfs() {
        let desc = camera(&extension_unit(10, &FOV_GUID), &[]);
        let (root, node) = fake_sysfs("046d\n", "0944\n", &desc);
        let usb = usb_device_of_node(root.path(), &node).unwrap();
        assert_eq!(
            usb.id,
            UsbId {
                vendor: 0x046d,
                product: 0x0944
            }
        );
        assert_eq!(usb.descriptors, desc);
        assert_eq!(usb.configuration, Some(1));
        assert!(usb.dir.ends_with("usb1/1-2"), "{}", usb.dir.display());
        // The Logitech unit's rows; not the RightSight ones (no such unit).
        let found: Vec<(&str, u8)> = find_xu_controls(root.path(), &node)
            .unwrap()
            .iter()
            .map(|x| (x.row().name, x.unit()))
            .collect();
        assert_eq!(
            found,
            [
                ("field_of_view", 10),
                ("show_mode", 10),
                ("exposure_compensation", 10),
                ("vibrance", 10),
                ("temperature_compensation", 10),
            ]
        );

        // Known camera without the unit; unknown camera with it.
        let (root, node) = fake_sysfs("046d\n", "0944\n", &camera(&[], &[]));
        assert!(find_xu_controls(root.path(), &node).unwrap().is_empty());
        let (root, node) = fake_sysfs("1234\n", "abcd\n", &desc);
        assert!(find_xu_controls(root.path(), &node).unwrap().is_empty());

        // A garbled id, a missing node.
        let (root, node) = fake_sysfs("zz\n", "0944\n", &desc);
        let e = usb_device_of_node(root.path(), &node).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert!(usb_device_of_node(root.path(), Path::new("/nonexistent/video7")).is_err());
    }

    #[test]
    fn the_node_is_found_by_its_device_number_not_its_name() {
        let desc = camera(&extension_unit(10, &FOV_GUID), &[]);
        let (root, _) = fake_sysfs("046d\n", "0944\n", &desc);
        // /dev/null itself canonicalises to `null`, the decoy's class name:
        // its number still finds 1-2, never the decoy 1-3.
        let usb = usb_device_of_node(root.path(), Path::new("/dev/null")).unwrap();
        assert!(usb.dir.ends_with("usb1/1-2"), "{}", usb.dir.display());
        let found = find_xu_controls(root.path(), Path::new("/dev/null")).unwrap();
        assert_eq!(found[0].unit(), 10);
        // A link named like another node: still the number's device.
        let renamed = root.path().join("video9");
        std::os::unix::fs::symlink("/dev/null", &renamed).unwrap();
        let usb = usb_device_of_node(root.path(), &renamed).unwrap();
        assert!(usb.dir.ends_with("usb1/1-2"), "{}", usb.dir.display());
        // A regular file named `video7` is no device node at all.
        let file = root.path().join("video7");
        fs::write(&file, b"").unwrap();
        let e = usb_device_of_node(root.path(), &file).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{e}");
        assert!(find_xu_controls(root.path(), &file).is_err());
        // Nor is a directory.
        let e = usb_device_of_node(root.path(), root.path()).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{e}");
        // A character device sysfs has no entry for.
        let e = usb_device_of_node(root.path(), Path::new("/dev/zero")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}");
    }

    #[test]
    fn a_number_that_is_no_video_device_is_not_found() {
        let root = tempfile::tempdir().unwrap();
        let mem = root.path().join("devices/virtual/mem/null");
        fs::create_dir_all(&mem).unwrap();
        fs::create_dir_all(root.path().join("dev/char")).unwrap();
        std::os::unix::fs::symlink(
            &mem,
            root.path().join("dev/char").join(char_number("/dev/null")),
        )
        .unwrap();
        let e = usb_device_of_node(root.path(), Path::new("/dev/null")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}");
        assert!(e.to_string().contains("video4linux"), "{e}");
    }

    #[test]
    fn a_node_without_a_usb_parent_is_not_found() {
        let root = tempfile::tempdir().unwrap();
        let virt = root.path().join("devices/virtual/video4linux/video7");
        fs::create_dir_all(&virt).unwrap();
        fs::create_dir_all(root.path().join("dev/char")).unwrap();
        let link = root.path().join("dev/char").join(char_number("/dev/null"));
        std::os::unix::fs::symlink(&virt, &link).unwrap();
        // A virtual node (v4l2loopback) has no `device` link.
        let e = usb_device_of_node(root.path(), Path::new("/dev/null")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}");
        // A `device` that is not a USB interface (no idVendor above it).
        let platform = root.path().join("devices/platform/cam0");
        fs::create_dir_all(&platform).unwrap();
        std::os::unix::fs::symlink(&platform, virt.join("device")).unwrap();
        let e = usb_device_of_node(root.path(), Path::new("/dev/null")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}");
    }

    #[test]
    fn the_units_come_from_the_active_configuration() {
        // Configuration 1 without the unit, configuration 2 with it.
        let mut two = camera(&[], &[]);
        two.extend(configuration(2, &extension_unit(10, &FOV_GUID)));
        for (active, unit) in [("1\n", None), ("2\n", Some(10)), ("3\n", None)] {
            let (root, node) = fake_sysfs_with("046d\n", "0944\n", active, &two);
            let found = find_xu_controls(root.path(), &node).unwrap();
            assert_eq!(found.first().map(XuControl::unit), unit, "{active:?}");
        }
        // Unconfigured: nothing, whatever the descriptors carry.
        let desc = camera(&extension_unit(10, &FOV_GUID), &[]);
        let (root, node) = fake_sysfs_with("046d\n", "0944\n", "\n", &desc);
        assert_eq!(
            usb_device_of_node(root.path(), &node)
                .unwrap()
                .configuration,
            None
        );
        assert!(find_xu_controls(root.path(), &node).unwrap().is_empty());
        // A garbled configuration value.
        let (root, node) = fake_sysfs_with("046d\n", "0944\n", "x\n", &desc);
        let e = usb_device_of_node(root.path(), &node).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
    }
}
