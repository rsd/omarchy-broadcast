//! Vendor settings reached through a UVC Extension Unit, as data rows.
//!
//! Some cameras keep a setting outside the standard V4L2 controls, in a
//! vendor Extension Unit (XU) that `UVCIOC_CTRL_QUERY` reaches by unit id
//! and selector. Each such setting is one [`XuRow`] in [`XU_ROWS`]: its
//! name, the USB ids it applies to, the Extension Unit's GUID, the
//! selector, the value size and the menu of known values. No code is
//! specific to a camera: adding a setting is adding a row.
//!
//! Resolving a row on a camera ([`find_xu_controls`]): the node's USB
//! device is found in sysfs by the node's device number
//! ([`usb_device_of_node`]; the root is injectable), its rows looked up by
//! vendor:product ([`rows_for`]), and the unit id of each row's GUID found
//! in the descriptors of the device's active configuration
//! ([`find_extension_unit`]). The result, an [`XuControl`], is the only way
//! to issue a query, and its queries are held to an allowlist: only
//! `SET_CUR`, `GET_CUR`, `GET_LEN`, `GET_INFO` and `GET_DEF`, only on the
//! (unit, selector) of a data row, and a write only of a value in the row's
//! menu. Any other unit or selector (a camera's LED, say) cannot be
//! reached through this module.
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
    /// The value's size in bytes, which the device's `GET_LEN` must report.
    /// Every row's value is one byte.
    pub size: u16,
    /// The known values, in display order. The first is the fallback
    /// default.
    pub options: &'static [XuOption],
}

impl XuRow {
    /// Whether the row applies to the camera with USB id `usb`.
    pub fn applies_to(&self, usb: UsbId) -> bool {
        self.usb_ids.contains(&(usb.vendor, usb.product))
    }

    /// The option with id `id`.
    pub fn option(&self, id: &str) -> Option<&'static XuOption> {
        self.options.iter().find(|o| o.id == id)
    }

    /// The option the device byte `value` stands for, if it is known.
    pub fn option_for_value(&self, value: u8) -> Option<&'static XuOption> {
        self.options.iter().find(|o| o.value == value)
    }

    /// The GUID's 16 bytes as a descriptor carries them ([`parse_guid`]).
    pub fn guid_bytes(&self) -> Option<[u8; 16]> {
        parse_guid(self.guid)
    }
}

/// The Logitech field of view (Brio, MX Brio): 90°, 78° or 65°, a crop in
/// the camera's image processor.
pub const FIELD_OF_VIEW: XuRow = XuRow {
    name: "field_of_view",
    label: "Field of view",
    usb_ids: &[(0x046d, 0x0944), (0x046d, 0x085e)],
    guid: "49e40215-f434-47fe-b158-0e885023e51b",
    selector: 5,
    size: 1,
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
};

/// Every Extension Unit setting known.
pub const XU_ROWS: &[XuRow] = &[FIELD_OF_VIEW];

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

/// A (unit, selector) a query may reach, with the row's value size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct XuTarget {
    unit: u8,
    selector: u8,
    size: u16,
}

fn refused(why: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, why)
}

/// Whether a query may be issued: its code is one of [`XU_QUERIES`], its
/// (unit, selector) is in `allow`, the selector and size are those of a row
/// of [`XU_ROWS`], and its buffer is the size the query takes (2 for
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
    if !XU_ROWS
        .iter()
        .any(|r| r.selector == selector && r.size == target.size)
    {
        return Err(refused(format!("selector {selector} is no data row's")));
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

/// Issues one `UVCIOC_CTRL_QUERY`, after [`check_xu_query`].
fn xu_query(
    file: &fs::File,
    allow: &[XuTarget],
    unit: u8,
    selector: u8,
    query: u8,
    data: &mut [u8],
) -> io::Result<()> {
    check_xu_query(allow, unit, selector, query, data.len())?;
    let mut q = UvcXuControlQuery {
        unit,
        selector,
        query,
        // Checked above: 1, 2 or a row's u16 size.
        size: data.len() as u16,
        data: data.as_mut_ptr(),
    };
    // SAFETY: `q` is a valid `struct uvc_xu_control_query` whose `data`
    // points to `size` writable bytes; both outlive the call, and the fd is
    // open for the duration.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), UVCIOC_CTRL_QUERY as _, &mut q) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// What [`XuControl::probe`] read from a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XuState {
    /// `GET_INFO`'s bits ([`XU_INFO_GET`], [`XU_INFO_SET`], ...).
    pub info: u8,
    /// The current byte (`GET_CUR`).
    pub current: u8,
    /// The option it stands for; `None` for a byte the row does not know.
    pub current_option: Option<&'static XuOption>,
    /// The device's default byte (`GET_DEF`), when it could be read.
    pub default_raw: Option<u8>,
    /// The default option: `GET_DEF`'s byte mapped through the options; the
    /// row's first option when that byte is unknown or unreadable.
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
            .unwrap_or(&row.options[0]),
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
    /// the row's GUID); `None` when that configuration has no such unit.
    pub fn resolve(
        row: &'static XuRow,
        descriptors: &[u8],
        configuration: u8,
    ) -> Option<XuControl> {
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

    fn query(&self, node: &ControlNode, query: u8, data: &mut [u8]) -> io::Result<()> {
        let allow = [XuTarget {
            unit: self.unit,
            selector: self.row.selector,
            size: self.row.size,
        }];
        xu_query(
            node.file(),
            &allow,
            self.unit,
            self.row.selector,
            query,
            data,
        )
    }

    /// Reads the setting's state from the device: `Ok(None)` when the
    /// device does not agree with the row (`GET_LEN` is not its size, or
    /// `GET_INFO` lacks the get or set bit), so the setting is not offered.
    /// `Err` when `GET_LEN`, `GET_INFO` or `GET_CUR` fails; a failing
    /// `GET_DEF` falls back to the first option.
    pub fn probe(&self, node: &ControlNode) -> io::Result<Option<XuState>> {
        let mut len = [0u8; 2];
        self.query(node, UVC_GET_LEN, &mut len)?;
        let mut info = [0u8; 1];
        self.query(node, UVC_GET_INFO, &mut info)?;
        if !device_agrees(self.row, u16::from_le_bytes(len), info[0]) {
            return Ok(None);
        }
        let current = self.current(node)?;
        let mut def = [0u8; 1];
        let default_raw = self.query(node, UVC_GET_DEF, &mut def).ok().map(|_| def[0]);
        Ok(Some(map_state(self.row, info[0], current, default_raw)))
    }

    /// The current byte (`GET_CUR`).
    pub fn current(&self, node: &ControlNode) -> io::Result<u8> {
        let mut cur = [0u8; 1];
        self.query(node, UVC_GET_CUR, &mut cur)?;
        Ok(cur[0])
    }

    /// Writes the option `option_id` (`SET_CUR` of its byte). The node must
    /// be open for writing. `InvalidInput` for an id the row does not have,
    /// before any query.
    pub fn set(&self, node: &ControlNode, option_id: &str) -> io::Result<()> {
        let option = self.row.option(option_id).ok_or_else(|| {
            refused(format!(
                "{option_id:?} is not an option of {}",
                self.row.name
            ))
        })?;
        self.query(node, UVC_SET_CUR, &mut [option.value])
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
        assert_eq!(XU_ROWS, &[FIELD_OF_VIEW]);
        let r = &FIELD_OF_VIEW;
        assert_eq!(
            (r.name, r.label, r.selector, r.size),
            ("field_of_view", "Field of view", 5, 1)
        );
        let opts: Vec<(&str, &str, u8)> =
            r.options.iter().map(|o| (o.id, o.label, o.value)).collect();
        assert_eq!(opts, [("90", "90°", 0), ("78", "78°", 1), ("65", "65°", 2)]);
        assert_eq!(r.option("78").map(|o| o.value), Some(1));
        assert_eq!(r.option("45"), None);
        assert_eq!(r.option_for_value(2).map(|o| o.id), Some("65"));
        assert_eq!(r.option_for_value(3), None);
        // Every row: a one-byte value, a valid GUID, distinct option ids and
        // bytes, at least one option.
        for row in XU_ROWS {
            assert_eq!(row.size, 1, "{}", row.name);
            assert!(row.guid_bytes().is_some(), "{}", row.name);
            assert!(!row.options.is_empty());
            for (i, a) in row.options.iter().enumerate() {
                for b in &row.options[i + 1..] {
                    assert!(a.id != b.id && a.value != b.value, "{}", row.name);
                }
            }
        }
    }

    #[test]
    fn rows_are_looked_up_by_usb_id() {
        for product in [0x0944, 0x085e] {
            let usb = UsbId {
                vendor: 0x046d,
                product,
            };
            assert_eq!(rows_for(usb), [&FIELD_OF_VIEW], "{usb}");
        }
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
            selector: 9,
            size: 5,
        }];
        assert!(check_xu_query(&rogue, 11, 9, UVC_GET_CUR, 5).is_err());
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
        let e = xu_query(node.file(), &[], 11, 9, UVC_GET_CUR, &mut [0; 5]).unwrap_err();
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
        let found = find_xu_controls(root.path(), &node).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(
            (found[0].row().name, found[0].unit()),
            ("field_of_view", 10)
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
