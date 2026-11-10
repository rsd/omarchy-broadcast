//! Every control of one opened V4L2 node, by control id.
//!
//! [`ControlNode`] opens a camera node once and serves any number of
//! queries, reads and writes on it:
//!
//! * [`ControlNode::enumerate`] walks every control the node reports
//!   (`VIDIOC_QUERYCTRL` with `V4L2_CTRL_FLAG_NEXT_CTRL`), keeps those of the
//!   user (`0x00980000`) and camera (`0x009a0000`) classes whose type is
//!   integer, boolean, menu, button or integer menu, and reads each one's
//!   value (`VIDIOC_G_CTRL`) and menu items (`VIDIOC_QUERYMENU`). A control
//!   whose value cannot be read is reported with the error, never the whole
//!   list.
//! * [`ControlNode::get`], [`ControlNode::set`] and [`ControlNode::query`]
//!   reach one control by id, and refuse (`InvalidInput`) any id carrying
//!   query flag bits ([`CTRL_ID_FLAG_BITS`]), outside those two classes or
//!   on the [`NEVER_EXPOSED`] list, before any ioctl.
//! * [`ControlNode::capture_format`] reads the capture format back
//!   (`VIDIOC_G_FMT`), to check that a write left it unchanged.
//!
//! [`normalise_control_name`] turns a device's control name into the stable
//! name v4l2-ctl shows (`White Balance, Automatic` ->
//! `white_balance_automatic`). Nothing here starts a stream or changes a
//! format; no camera is special-cased.

use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

use crate::controls::{
    CTRL_FLAG_WRITE_ONLY, CTRL_TYPE_BOOLEAN, CTRL_TYPE_BUTTON, CTRL_TYPE_CTRL_CLASS,
    CTRL_TYPE_INTEGER, CTRL_TYPE_INTEGER_MENU, CTRL_TYPE_MENU, CtrlInfo, VIDIOC_G_CTRL,
    VIDIOC_S_CTRL, ioctl_ctrl_on, open_node, query_ctrl_on, query_next_ctrl_on,
};

/// `V4L2_CTRL_CLASS_USER`: the class of the user controls (brightness,
/// gain, white balance, ...).
pub const CTRL_CLASS_USER: u32 = 0x0098_0000;
/// `V4L2_CTRL_CLASS_CAMERA`: the class of the camera controls (exposure,
/// focus, zoom, pan, tilt, ...).
pub const CTRL_CLASS_CAMERA: u32 = 0x009a_0000;
/// The class bits of a control id (`V4L2_CTRL_ID2WHICH`).
pub const CTRL_CLASS_MASK: u32 = 0x0fff_0000;
/// The bits of a control id the kernel reads as query flags, not as part of
/// the id: everything outside `V4L2_CTRL_ID_MASK` (`0x0fff_ffff`), among
/// them `V4L2_CTRL_FLAG_NEXT_CTRL` (`0x8000_0000`) and
/// `V4L2_CTRL_FLAG_NEXT_COMPOUND` (`0x4000_0000`). The kernel strips them
/// (or, on UVC, answers for the *next* control), so an id carrying any of
/// them could reach a control other than the one named; such an id is
/// never in an exposed class.
pub const CTRL_ID_FLAG_BITS: u32 = 0xf000_0000;

/// The control types reported by [`ControlNode::enumerate`]: integer,
/// boolean, menu, button and integer menu. Every other type is left out.
pub const REPORTED_TYPES: [u32; 5] = [
    CTRL_TYPE_INTEGER,
    CTRL_TYPE_BOOLEAN,
    CTRL_TYPE_MENU,
    CTRL_TYPE_BUTTON,
    CTRL_TYPE_INTEGER_MENU,
];

/// `V4L2_CID_FOCUS_AUTO` (`focus_automatic_continuous`): the one
/// `*_continuous` control that is a mode switch, not a motion control, so
/// the name rule of [`is_never_exposed`] spares it.
pub const CID_FOCUS_AUTO: u32 = 0x009a_090c;

/// The controls never exposed to a user, by v4l2-ctl name and id: the
/// privacy switch and every relative, continuous or speed motion control.
/// [`ControlNode`] refuses to read or write them; [`is_never_exposed`]
/// adds a name rule for controls with other ids.
pub const NEVER_EXPOSED: [(&str, u32); 9] = [
    ("pan_relative", 0x009a_0904),
    ("tilt_relative", 0x009a_0905),
    ("focus_relative", 0x009a_090b),
    ("zoom_relative", 0x009a_090e),
    ("zoom_continuous", 0x009a_090f),
    ("privacy", 0x009a_0910),
    ("iris_relative", 0x009a_0912),
    ("pan_speed", 0x009a_0920),
    ("tilt_speed", 0x009a_0921),
];

/// The name suffixes of the motion controls [`is_never_exposed`] leaves
/// out whatever their id.
const NEVER_EXPOSED_SUFFIXES: [&str; 3] = ["_relative", "_continuous", "_speed"];

/// The longest normalised control name, in bytes.
pub const CONTROL_NAME_MAX: usize = 32;

/// The stable name of a control: the device's name lowercased, every run of
/// characters other than `a-z`/`0-9` turned into one `_`, without a leading
/// or trailing `_`, at most [`CONTROL_NAME_MAX`] bytes. The name v4l2-ctl
/// prints (`Exposure Time, Absolute` -> `exposure_time_absolute`). A name
/// with no letter or digit gives `""`.
pub fn normalise_control_name(device_name: &str) -> String {
    let mut out = String::with_capacity(device_name.len());
    let mut gap = false;
    for c in device_name.chars().map(|c| c.to_ascii_lowercase()) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if gap && !out.is_empty() {
                out.push('_');
            }
            gap = false;
            out.push(c);
        } else {
            gap = true;
        }
    }
    // Only ASCII was pushed, so every byte index is a char boundary.
    out.truncate(CONTROL_NAME_MAX);
    while out.ends_with('_') {
        out.pop();
    }
    out
}

/// Whether control `id` belongs to the user or the camera class and
/// carries none of the [`CTRL_ID_FLAG_BITS`].
pub fn in_exposed_class(id: u32) -> bool {
    id & CTRL_ID_FLAG_BITS == 0
        && matches!(id & CTRL_CLASS_MASK, CTRL_CLASS_USER | CTRL_CLASS_CAMERA)
}

/// Whether `id` is on the [`NEVER_EXPOSED`] list.
pub fn is_never_exposed_id(id: u32) -> bool {
    NEVER_EXPOSED.iter().any(|(_, cid)| *cid == id)
}

/// Whether a control must never reach a user: outside the user and camera
/// classes, on the [`NEVER_EXPOSED`] list, named `privacy`, or a motion
/// control by name (`*_relative`, `*_continuous`, `*_speed`; the
/// `focus_automatic_continuous` switch, [`CID_FOCUS_AUTO`], is not one).
/// `name` is the normalised name ([`normalise_control_name`]).
pub fn is_never_exposed(id: u32, name: &str) -> bool {
    if !in_exposed_class(id) || is_never_exposed_id(id) || name == "privacy" {
        return true;
    }
    id != CID_FOCUS_AUTO && NEVER_EXPOSED_SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// `Ok` when [`ControlNode`] may reach control `id`: free of the
/// [`CTRL_ID_FLAG_BITS`], in the user or camera class and not on the
/// [`NEVER_EXPOSED`] list; otherwise `InvalidInput`. The flag bits are
/// refused first: with them stripped, `0x809a_0910` is `privacy`, and on
/// UVC `0x809a_090f` means "the control after `zoom_continuous`".
pub fn check_reachable(id: u32) -> io::Result<()> {
    if id & CTRL_ID_FLAG_BITS != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("control id {id:#010x} carries query flag bits"),
        ));
    }
    if !in_exposed_class(id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("control {id:#010x} is outside the user and camera classes"),
        ));
    }
    if let Some((name, _)) = NEVER_EXPOSED.iter().find(|(_, cid)| *cid == id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("control {name} ({id:#010x}) is never exposed"),
        ));
    }
    Ok(())
}

/// The value of an enumerated control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CtrlValue {
    /// The control has no value to read: a button, or flagged write-only.
    None,
    /// The value read when the node was enumerated.
    Value(i32),
    /// Reading it failed (`EIO` and `EPIPE` are common on UVC): the OS
    /// error number, when there is one, and the message.
    Unreadable { errno: Option<i32>, message: String },
}

impl CtrlValue {
    /// The value, when one was read.
    pub fn value(&self) -> Option<i32> {
        match self {
            CtrlValue::Value(v) => Some(*v),
            _ => None,
        }
    }

    fn from_read(read: io::Result<i32>) -> CtrlValue {
        match read {
            Ok(v) => CtrlValue::Value(v),
            Err(e) => CtrlValue::Unreadable {
                errno: e.raw_os_error(),
                message: e.to_string(),
            },
        }
    }
}

/// What one menu item is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuItemValue {
    /// An item of a menu: its name as the device reports it (not
    /// sanitised; the device's string is untrusted).
    Name(String),
    /// An item of an integer menu: its integer.
    Integer(i64),
}

/// One menu item a device reports (`VIDIOC_QUERYMENU`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    /// The item's index: the control's value that selects it.
    pub index: u32,
    pub value: MenuItemValue,
}

/// One control reported by [`ControlNode::enumerate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeControl {
    /// The control id (`V4L2_CID_*`).
    pub id: u32,
    /// The stable name ([`normalise_control_name`] of `info.name`).
    pub name: String,
    /// What `VIDIOC_QUERYCTRL` reports: the device's name (untrusted, not
    /// sanitised), type, range, step, default and flags.
    pub info: CtrlInfo,
    /// The current value.
    pub value: CtrlValue,
    /// The menu items of a menu or integer menu, in index order: exactly
    /// those `VIDIOC_QUERYMENU` returns (an item it refuses is absent).
    /// Empty for the other types.
    pub menu: Vec<MenuItem>,
}

/// What [`ControlNode::enumerate`] found.
#[derive(Debug, Default)]
pub struct Enumeration {
    /// The controls, in the order the device reports them (ascending id).
    pub controls: Vec<NodeControl>,
    /// Why the walk stopped before the end, when it did: a
    /// `VIDIOC_QUERYCTRL` that failed with another error than `EINVAL`
    /// (the controls found before it are kept).
    pub incomplete: Option<io::Error>,
}

/// The most controls [`ControlNode::enumerate`] walks, against a driver
/// that never ends its list.
const MAX_CONTROLS: usize = 1024;
/// The most menu indices queried per control, against a menu whose range
/// is huge.
pub const MAX_MENU_INDICES: u32 = 64;

/// [`ControlNode::enumerate`] over its three ioctls: `next` is
/// `VIDIOC_QUERYCTRL` with `V4L2_CTRL_FLAG_NEXT_CTRL` after an id, `read` is
/// `VIDIOC_G_CTRL` by id, `menu` is `VIDIOC_QUERYMENU` by (id, index)
/// returning the 32 bytes of the item's union. Stops at the end, at an id
/// that does not increase, after [`MAX_CONTROLS`], or at a failing `next`.
fn collect_node_controls(
    mut next: impl FnMut(u32) -> io::Result<Option<(u32, CtrlInfo)>>,
    mut read: impl FnMut(u32) -> io::Result<i32>,
    mut menu: impl FnMut(u32, u32) -> io::Result<[u8; 32]>,
) -> Enumeration {
    let mut out = Enumeration::default();
    let mut after = 0;
    for _ in 0..MAX_CONTROLS {
        let (id, info) = match next(after) {
            Ok(Some(found)) => found,
            Ok(None) => break,
            Err(e) => {
                out.incomplete = Some(e);
                break;
            }
        };
        if id <= after {
            break;
        }
        after = id;
        if info.type_ == CTRL_TYPE_CTRL_CLASS
            || !in_exposed_class(id)
            || !REPORTED_TYPES.contains(&info.type_)
        {
            continue;
        }
        let value = if info.type_ == CTRL_TYPE_BUTTON || info.flags & CTRL_FLAG_WRITE_ONLY != 0 {
            CtrlValue::None
        } else {
            CtrlValue::from_read(read(id))
        };
        let items = match info.type_ {
            CTRL_TYPE_MENU | CTRL_TYPE_INTEGER_MENU => menu_items(&info, |i| menu(id, i)),
            _ => Vec::new(),
        };
        out.controls.push(NodeControl {
            id,
            name: normalise_control_name(&info.name),
            info,
            value,
            menu: items,
        });
    }
    out
}

/// The items of a menu or integer menu control: every index from its
/// minimum (at least 0) to its maximum, at most [`MAX_MENU_INDICES`] of
/// them, that `query` answers. `query` returns the 32 bytes of the item's
/// union: a NUL-terminated name for a menu, a native-endian `i64` in the
/// first 8 bytes for an integer menu.
fn menu_items(
    info: &CtrlInfo,
    mut query: impl FnMut(u32) -> io::Result<[u8; 32]>,
) -> Vec<MenuItem> {
    let Ok(first) = u32::try_from(info.minimum.max(0)) else {
        return Vec::new();
    };
    let Ok(last) = u32::try_from(info.maximum) else {
        return Vec::new();
    };
    let last = last.min(first.saturating_add(MAX_MENU_INDICES - 1));
    let mut items = Vec::new();
    for index in first..=last {
        let Ok(raw) = query(index) else {
            continue;
        };
        let value = if info.type_ == CTRL_TYPE_INTEGER_MENU {
            let mut v = [0; 8];
            v.copy_from_slice(&raw[..8]);
            MenuItemValue::Integer(i64::from_ne_bytes(v))
        } else {
            let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            MenuItemValue::Name(String::from_utf8_lossy(&raw[..end]).into_owned())
        };
        items.push(MenuItem { index, value });
    }
    items
}

/// `struct v4l2_querymenu` (packed in the header; its natural layout is
/// already 44 bytes). The 32 bytes after `index` are a union of the item's
/// name and its `__s64` value.
#[repr(C)]
struct V4l2QueryMenu {
    id: u32,
    index: u32,
    name: [u8; 32],
    reserved: u32,
}

const _: () = assert!(std::mem::size_of::<V4l2QueryMenu>() == 44);

/// `_IOWR('V', 37, struct v4l2_querymenu)`
const VIDIOC_QUERYMENU: libc::c_ulong = (3 << 30)
    | ((std::mem::size_of::<V4l2QueryMenu>() as libc::c_ulong) << 16)
    | ((b'V' as libc::c_ulong) << 8)
    | 37;

/// `VIDIOC_QUERYMENU` for item `index` of control `id`: the 32 bytes of
/// the item's union.
fn query_menu_on(file: &fs::File, id: u32, index: u32) -> io::Result<[u8; 32]> {
    let mut q = V4l2QueryMenu {
        id,
        index,
        name: [0; 32],
        reserved: 0,
    };
    // SAFETY: `q` is a valid `struct v4l2_querymenu` (same size and field
    // offsets) that outlives the call; the fd is open for the duration.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), VIDIOC_QUERYMENU as _, &mut q) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(q.name)
}

/// `V4L2_BUF_TYPE_VIDEO_CAPTURE`
const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;

/// `struct v4l2_format`: the buffer type, then a 200-byte union whose
/// members include pointers (`struct v4l2_window`), so it is
/// pointer-aligned: 208 bytes on 64-bit.
#[repr(C)]
struct V4l2Format {
    type_: u32,
    fmt: [libc::c_ulong; 200 / std::mem::size_of::<libc::c_ulong>()],
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<V4l2Format>() == 208);

/// `_IOWR('V', 4, struct v4l2_format)`
const VIDIOC_G_FMT: libc::c_ulong = (3 << 30)
    | ((std::mem::size_of::<V4l2Format>() as libc::c_ulong) << 16)
    | ((b'V' as libc::c_ulong) << 8)
    | 4;

/// The capture format of a node ([`ControlNode::capture_format`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureFormat {
    pub width: u32,
    pub height: u32,
    /// The V4L2 fourcc (`NV12`, `MJPG`, ... as little-endian bytes).
    pub pixelformat: u32,
}

impl std::fmt::Display for CaptureFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let fourcc: String = self
            .pixelformat
            .to_le_bytes()
            .iter()
            .map(|&b| if b.is_ascii_graphic() { b as char } else { '?' })
            .collect();
        write!(f, "{}x{} {fourcc}", self.width, self.height)
    }
}

/// The width, height and pixel format of a `struct v4l2_pix_format`: its
/// first three `__u32`, from the format union's bytes.
fn pix_format(raw: &[u8]) -> CaptureFormat {
    let word = |i: usize| u32::from_ne_bytes([raw[i], raw[i + 1], raw[i + 2], raw[i + 3]]);
    CaptureFormat {
        width: word(0),
        height: word(4),
        pixelformat: word(8),
    }
}

/// One V4L2 node opened for its controls, for any number of enumerations,
/// queries, reads and writes. Nothing it does starts a stream or changes a
/// format, so it may run while any application streams from the camera.
#[derive(Debug)]
pub struct ControlNode {
    file: fs::File,
}

impl ControlNode {
    /// Opens the node at `path` (non-blocking), for writing as well when
    /// `write`.
    pub fn open(path: &Path, write: bool) -> io::Result<ControlNode> {
        Ok(ControlNode {
            file: open_node(path, write)?,
        })
    }

    pub(crate) fn file(&self) -> &fs::File {
        &self.file
    }

    /// Every user- and camera-class control of a [`REPORTED_TYPES`] type
    /// the node reports, with its value and menu items. Class headers and
    /// other types are left out; [`NEVER_EXPOSED`] ones are included (the
    /// caller filters with [`is_never_exposed`]). Only queries and reads.
    pub fn enumerate(&self) -> Enumeration {
        collect_node_controls(
            |after| query_next_ctrl_on(&self.file, after),
            |id| ioctl_ctrl_on(&self.file, VIDIOC_G_CTRL, id, 0),
            |id, index| query_menu_on(&self.file, id, index),
        )
    }

    /// `VIDIOC_QUERYCTRL` for control `id`: its current flags (inactive,
    /// grabbed, ...) and range. `Ok(None)` when the device does not have
    /// it; `InvalidInput` for an id [`check_reachable`] refuses.
    pub fn query(&self, id: u32) -> io::Result<Option<CtrlInfo>> {
        check_reachable(id)?;
        query_ctrl_on(&self.file, id)
    }

    /// Reads control `id` (`VIDIOC_G_CTRL`); `InvalidInput` for an id
    /// [`check_reachable`] refuses.
    pub fn get(&self, id: u32) -> io::Result<i32> {
        check_reachable(id)?;
        ioctl_ctrl_on(&self.file, VIDIOC_G_CTRL, id, 0)
    }

    /// Writes control `id` (`VIDIOC_S_CTRL`; a button is pressed whatever
    /// the value). The node must be open for writing. The value is the
    /// caller's to check; the driver may refuse it. `InvalidInput` for an
    /// id [`check_reachable`] refuses, before any ioctl.
    pub fn set(&self, id: u32, value: i32) -> io::Result<()> {
        check_reachable(id)?;
        ioctl_ctrl_on(&self.file, VIDIOC_S_CTRL, id, value).map(|_| ())
    }

    /// The current capture format (`VIDIOC_G_FMT` for
    /// `V4L2_BUF_TYPE_VIDEO_CAPTURE`): width, height and pixel format.
    pub fn capture_format(&self) -> io::Result<CaptureFormat> {
        let mut f = V4l2Format {
            type_: BUF_TYPE_VIDEO_CAPTURE,
            fmt: [0; 200 / std::mem::size_of::<libc::c_ulong>()],
        };
        // SAFETY: `f` is a valid `struct v4l2_format` (same size, the union
        // pointer-aligned) that outlives the call; the fd is open for the
        // duration.
        let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), VIDIOC_G_FMT as _, &mut f) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        let raw: Vec<u8> = f.fmt.iter().flat_map(|w| w.to_ne_bytes()).collect();
        Ok(pix_format(&raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controls::{CTRL_FLAG_INACTIVE, CTRL_FLAG_READ_ONLY};

    #[test]
    fn ioctl_numbers_and_sizes() {
        assert_eq!(VIDIOC_QUERYMENU, 0xc02c_5625);
        assert_eq!(std::mem::size_of::<V4l2QueryMenu>(), 44);
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(VIDIOC_G_FMT, 0xc0d0_5604);
            assert_eq!(std::mem::size_of::<V4l2Format>(), 208);
        }
    }

    #[test]
    fn names_are_normalised_like_v4l2_ctl() {
        for (device, name) in [
            ("Brightness", "brightness"),
            ("White Balance, Automatic", "white_balance_automatic"),
            ("White Balance Temperature", "white_balance_temperature"),
            ("Exposure Time, Absolute", "exposure_time_absolute"),
            ("Exposure, Dynamic Framerate", "exposure_dynamic_framerate"),
            ("Auto Exposure", "auto_exposure"),
            ("Focus, Automatic Continuous", "focus_automatic_continuous"),
            ("Power Line Frequency", "power_line_frequency"),
            ("  --Leading and trailing!! ", "leading_and_trailing"),
            ("Mixed__runs -- of; junk", "mixed_runs_of_junk"),
            ("LED1 Mode", "led1_mode"),
            ("Caf\u{e9} 2", "caf_2"),
            ("\u{202e}evil\u{0}name", "evil_name"),
            ("", ""),
            (", ; --", ""),
        ] {
            assert_eq!(normalise_control_name(device), name, "{device:?}");
        }
        // At most 32 bytes, never ending on the separator.
        let long = normalise_control_name("Abcdefghij Abcdefghij Abcdefghi Xyz");
        assert_eq!(long, "abcdefghij_abcdefghij_abcdefghi");
        assert!(long.len() <= CONTROL_NAME_MAX);
        let cut = normalise_control_name("Abcdefghij Abcdefghij Abcdefghij Xyz");
        assert_eq!(cut, "abcdefghij_abcdefghij_abcdefghij");
        assert_eq!(cut.len(), 32);
        let edge = normalise_control_name(&format!("{} b", "a".repeat(31)));
        assert_eq!(edge, "a".repeat(31));
    }

    #[test]
    fn the_never_exposed_list_and_rule() {
        let ids: Vec<u32> = NEVER_EXPOSED.iter().map(|(_, id)| *id).collect();
        assert_eq!(
            ids,
            [
                0x009a_0904,
                0x009a_0905,
                0x009a_090b,
                0x009a_090e,
                0x009a_090f,
                0x009a_0910,
                0x009a_0912,
                0x009a_0920,
                0x009a_0921
            ]
        );
        for (name, id) in NEVER_EXPOSED {
            assert!(is_never_exposed(id, name), "{name}");
            assert!(is_never_exposed_id(id));
            let e = check_reachable(id).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
            assert!(e.to_string().contains(name), "{e}");
        }
        // The name rule catches the same kinds under other ids.
        assert!(is_never_exposed(0x0098_1001, "roll_relative"));
        assert!(is_never_exposed(0x0098_1002, "roll_speed"));
        assert!(is_never_exposed(0x0098_1003, "iris_continuous"));
        assert!(is_never_exposed(0x0098_1004, "privacy"));
        // Not the continuous-autofocus switch, nor ordinary controls.
        assert!(!is_never_exposed(
            CID_FOCUS_AUTO,
            "focus_automatic_continuous"
        ));
        assert!(check_reachable(CID_FOCUS_AUTO).is_ok());
        for (name, id) in [
            ("brightness", 0x0098_0900),
            ("zoom_absolute", 0x009a_090d),
            ("pan_reset", 0x009a_0906),
            ("exposure_dynamic_framerate", 0x009a_0903),
        ] {
            assert!(!is_never_exposed(id, name), "{name}");
            assert!(check_reachable(id).is_ok(), "{name}");
        }
        // Other classes: never, by id alone.
        for id in [
            0x009b_0900,
            0x009e_0900,
            0x0099_0900,
            0x0000_0001,
            0x00a2_0000,
        ] {
            assert!(!in_exposed_class(id), "{id:#x}");
            assert!(is_never_exposed(id, "brightness"), "{id:#x}");
            let e = check_reachable(id).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn ids_with_query_flag_bits_are_refused() {
        // NEXT_CTRL | privacy, NEXT_COMPOUND | privacy, NEXT_CTRL |
        // zoom_continuous (UVC: "the one after it" = privacy), NEXT_CTRL |
        // brightness, and every high bit alone on an ordinary control.
        let mut ids = vec![0x809a_0910, 0x409a_0910, 0x809a_090f, 0x8098_0900];
        ids.extend((28..32).map(|bit| (1u32 << bit) | 0x0098_0900));
        ids.push(0xc09a_090d);
        for id in ids {
            assert!(!in_exposed_class(id), "{id:#x}");
            assert!(is_never_exposed(id, "brightness"), "{id:#x}");
            let e = check_reachable(id).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{id:#x}");
            assert!(e.to_string().contains("flag bits"), "{e}");
        }
        // The same ids without the bits: the class and list checks decide.
        assert!(check_reachable(0x0098_0900).is_ok());
        assert!(check_reachable(0x009a_090d).is_ok());
        assert!(check_reachable(0x009a_0910).is_err());
    }

    #[test]
    fn get_set_and_query_refuse_before_any_ioctl() {
        // /dev/null is no V4L2 node: a refused id must fail with
        // InvalidInput, not with the ioctl's ENOTTY.
        let node = ControlNode::open(Path::new("/dev/null"), true).unwrap();
        for id in [
            0x009a_0910,
            0x009a_090f,
            0x009b_0901,
            0x809a_090f,
            0x409a_0910,
        ] {
            assert_eq!(
                node.get(id).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            assert_eq!(
                node.set(id, 1).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            assert_eq!(
                node.query(id).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
        // An allowed id reaches the ioctl, which /dev/null refuses.
        let e = node.get(0x0098_0900).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::ENOTTY), "{e}");
        assert!(node.capture_format().is_err());
        assert!(ControlNode::open(Path::new("/nonexistent/obc-no-such-node"), false).is_err());
    }

    fn info(name: &str, type_: u32, min: i32, max: i32, flags: u32) -> CtrlInfo {
        CtrlInfo {
            name: name.into(),
            minimum: min,
            maximum: max,
            step: 1,
            default_value: min,
            flags,
            type_,
        }
    }

    fn name_bytes(s: &str) -> [u8; 32] {
        let mut b = [0; 32];
        b[..s.len()].copy_from_slice(s.as_bytes());
        b
    }

    /// A device's control list, walked as `VIDIOC_QUERYCTRL` with
    /// `NEXT_CTRL` would: the first entry above `after`.
    fn walk(list: &[(u32, CtrlInfo)]) -> impl FnMut(u32) -> io::Result<Option<(u32, CtrlInfo)>> {
        let list = list.to_vec();
        move |after| Ok(list.iter().find(|(id, _)| *id > after).cloned())
    }

    #[test]
    fn enumeration_keeps_the_exposed_classes_and_types() {
        let list = vec![
            (
                0x0098_0001,
                info("User Controls", CTRL_TYPE_CTRL_CLASS, 0, 0, 0x44),
            ),
            (
                0x0098_0900,
                info("Brightness", CTRL_TYPE_INTEGER, 0, 255, 0),
            ),
            (
                0x0098_090c,
                info("White Balance, Automatic", CTRL_TYPE_BOOLEAN, 0, 1, 0),
            ),
            (
                0x0098_0918,
                info("Power Line Frequency", CTRL_TYPE_MENU, 0, 2, 0),
            ),
            (
                0x0098_091a,
                info(
                    "White Balance Temperature",
                    CTRL_TYPE_INTEGER,
                    2800,
                    7500,
                    CTRL_FLAG_INACTIVE,
                ),
            ),
            // An integer64 (5) and a string (7): left out.
            (0x0098_0950, info("Big", 5, 0, 10, 0)),
            (0x0098_0951, info("Text", 7, 0, 10, 0)),
            (
                0x009a_0001,
                info("Camera Controls", CTRL_TYPE_CTRL_CLASS, 0, 0, 0x44),
            ),
            (0x009a_0901, info("Auto Exposure", CTRL_TYPE_MENU, 0, 3, 0)),
            (0x009a_0906, info("Pan, Reset", CTRL_TYPE_BUTTON, 0, 0, 0)),
            (
                0x009a_0910,
                info("Privacy", CTRL_TYPE_BOOLEAN, 0, 1, CTRL_FLAG_READ_ONLY),
            ),
            (0x009a_0930, info("Rates", CTRL_TYPE_INTEGER_MENU, 0, 2, 0)),
            (
                0x009a_0931,
                info("Secret", CTRL_TYPE_INTEGER, 0, 9, CTRL_FLAG_WRITE_ONLY),
            ),
            // Another class: left out.
            (0x009b_0901, info("Flash", CTRL_TYPE_MENU, 0, 2, 0)),
        ];
        let mut reads = Vec::new();
        let mut menus = Vec::new();
        let got = collect_node_controls(
            walk(&list),
            |id| {
                reads.push(id);
                match id {
                    0x0098_090c => Err(io::Error::from_raw_os_error(libc::EIO)),
                    0x009a_0901 => Ok(3),
                    _ => Ok(7),
                }
            },
            |id, index| {
                menus.push((id, index));
                match (id, index) {
                    (0x0098_0918, 0) => Ok(name_bytes("Disabled")),
                    (0x0098_0918, 1) => Ok(name_bytes("50 Hz")),
                    (0x0098_0918, 2) => Ok(name_bytes("60 Hz")),
                    (0x009a_0901, 1) => Ok(name_bytes("Manual Mode")),
                    (0x009a_0901, 3) => Ok(name_bytes("Aperture Priority Mode")),
                    (0x009a_0930, 0) => Ok(raw_int(15)),
                    (0x009a_0930, 2) => Ok(raw_int(-60_000_000_000)),
                    (0x009a_0930, 1) => Err(io::Error::from_raw_os_error(libc::EIO)),
                    _ => Err(io::Error::from_raw_os_error(libc::EINVAL)),
                }
            },
        );
        assert!(got.incomplete.is_none());
        let names: Vec<&str> = got.controls.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "brightness",
                "white_balance_automatic",
                "power_line_frequency",
                "white_balance_temperature",
                "auto_exposure",
                "pan_reset",
                "privacy",
                "rates",
                "secret"
            ]
        );
        // No value is read for the button or the write-only control.
        assert_eq!(
            reads,
            [
                0x0098_0900,
                0x0098_090c,
                0x0098_0918,
                0x0098_091a,
                0x009a_0901,
                0x009a_0910,
                0x009a_0930
            ]
        );
        let by = |n: &str| got.controls.iter().find(|c| c.name == n).unwrap();
        assert_eq!(by("brightness").value, CtrlValue::Value(7));
        assert_eq!(by("brightness").info.name, "Brightness");
        assert_eq!(by("brightness").id, 0x0098_0900);
        match &by("white_balance_automatic").value {
            CtrlValue::Unreadable { errno, .. } => assert_eq!(*errno, Some(libc::EIO)),
            v => panic!("{v:?}"),
        }
        assert_eq!(by("white_balance_automatic").value.value(), None);
        assert!(by("white_balance_temperature").info.inactive());
        assert_eq!(by("pan_reset").value, CtrlValue::None);
        assert_eq!(by("secret").value, CtrlValue::None);
        assert!(by("privacy").info.read_only());
        let name = |i, s: &str| MenuItem {
            index: i,
            value: MenuItemValue::Name(s.into()),
        };
        assert_eq!(
            by("power_line_frequency").menu,
            [name(0, "Disabled"), name(1, "50 Hz"), name(2, "60 Hz")]
        );
        // Exactly the items the device answers: 0 and 2 refused.
        assert_eq!(
            by("auto_exposure").menu,
            [name(1, "Manual Mode"), name(3, "Aperture Priority Mode")]
        );
        assert_eq!(by("auto_exposure").value, CtrlValue::Value(3));
        assert_eq!(
            by("rates").menu,
            [
                MenuItem {
                    index: 0,
                    value: MenuItemValue::Integer(15)
                },
                MenuItem {
                    index: 2,
                    value: MenuItemValue::Integer(-60_000_000_000)
                }
            ]
        );
        assert!(by("brightness").menu.is_empty() && by("pan_reset").menu.is_empty());
        // Menu items are queried only for the menus, index by index.
        assert!(
            menus
                .iter()
                .all(|(id, _)| [0x0098_0918, 0x009a_0901, 0x009a_0930].contains(id))
        );
        assert_eq!(menus.iter().filter(|(id, _)| *id == 0x009a_0901).count(), 4);
    }

    fn raw_int(v: i64) -> [u8; 32] {
        let mut b = [0xee; 32];
        b[..8].copy_from_slice(&v.to_ne_bytes());
        b
    }

    #[test]
    fn a_failing_walk_keeps_what_it_found() {
        let mut calls = 0;
        let got = collect_node_controls(
            |after| {
                calls += 1;
                match after {
                    0 => Ok(Some((
                        0x0098_0900,
                        info("Brightness", CTRL_TYPE_INTEGER, 0, 9, 0),
                    ))),
                    _ => Err(io::Error::from_raw_os_error(libc::EIO)),
                }
            },
            |_| Ok(1),
            |_, _| Err(io::Error::from_raw_os_error(libc::EINVAL)),
        );
        assert_eq!(calls, 2);
        assert_eq!(got.controls.len(), 1);
        assert_eq!(got.incomplete.unwrap().raw_os_error(), Some(libc::EIO));
    }

    #[test]
    fn a_walk_that_does_not_advance_ends() {
        // A driver answering the same id forever, or a lower one.
        for stuck in [0x0098_0900, 0x0098_0800] {
            let mut calls = 0;
            let got = collect_node_controls(
                |after| {
                    calls += 1;
                    let id = if after == 0 { 0x0098_0900 } else { stuck };
                    Ok(Some((id, info("Brightness", CTRL_TYPE_INTEGER, 0, 9, 0))))
                },
                |_| Ok(1),
                |_, _| Ok([0; 32]),
            );
            assert_eq!(calls, 2, "{stuck:#x}");
            assert_eq!(got.controls.len(), 1);
            assert!(got.incomplete.is_none());
        }
        // A driver that never ends: bounded.
        let mut next = 0x0098_0900;
        let got = collect_node_controls(
            |_| {
                next += 1;
                Ok(Some((next, info("X", CTRL_TYPE_INTEGER, 0, 9, 0))))
            },
            |_| Ok(1),
            |_, _| Ok([0; 32]),
        );
        assert_eq!(got.controls.len(), MAX_CONTROLS);
    }

    #[test]
    fn menu_ranges_are_bounded() {
        let mut asked = Vec::new();
        let items = menu_items(&info("Huge", CTRL_TYPE_MENU, -5, i32::MAX, 0), |i| {
            asked.push(i);
            Ok(name_bytes("x"))
        });
        assert_eq!(items.len(), MAX_MENU_INDICES as usize);
        assert_eq!(asked.first(), Some(&0));
        assert_eq!(asked.last(), Some(&(MAX_MENU_INDICES - 1)));
        // An empty or negative range asks nothing.
        for (min, max) in [(3, 2), (-3, -1)] {
            let none = menu_items(&info("E", CTRL_TYPE_MENU, min, max, 0), |_| {
                panic!("queried {min}..{max}")
            });
            assert!(none.is_empty());
        }
        // A name filling all 32 bytes, without a NUL, and invalid UTF-8.
        let full = menu_items(&info("F", CTRL_TYPE_MENU, 0, 1, 0), |i| {
            Ok(if i == 0 {
                [b'a'; 32]
            } else {
                name_bytes("\u{fffd}")
            })
        });
        assert_eq!(full[0].value, MenuItemValue::Name("a".repeat(32)));
        let bad = menu_items(&info("B", CTRL_TYPE_MENU, 0, 0, 0), |_| {
            let mut b = [0; 32];
            b[0] = 0xff;
            b[1] = b'x';
            Ok(b)
        });
        assert_eq!(bad[0].value, MenuItemValue::Name("\u{fffd}x".into()));
    }

    #[test]
    fn the_capture_format_is_the_first_three_words() {
        let mut raw = vec![0u8; 200];
        raw[0..4].copy_from_slice(&1920u32.to_ne_bytes());
        raw[4..8].copy_from_slice(&1080u32.to_ne_bytes());
        raw[8..12].copy_from_slice(&u32::from_le_bytes(*b"NV12").to_ne_bytes());
        raw[12..16].copy_from_slice(&1u32.to_ne_bytes());
        let f = pix_format(&raw);
        assert_eq!(
            f,
            CaptureFormat {
                width: 1920,
                height: 1080,
                pixelformat: u32::from_le_bytes(*b"NV12")
            }
        );
        assert_eq!(f.to_string(), "1920x1080 NV12");
        let odd = CaptureFormat {
            width: 1,
            height: 2,
            pixelformat: 0x0001_4d4a,
        };
        assert_eq!(odd.to_string(), "1x2 JM??");
    }
}
