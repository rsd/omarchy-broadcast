//! Minimal V4L2 control access (`VIDIOC_QUERYCTRL` / `VIDIOC_G_CTRL` /
//! `VIDIOC_S_CTRL`).
//!
//! Used by the bench to read, and optionally pin then restore, the UVC
//! `exposure_dynamic_framerate` control: when it is on, the camera may lower
//! its frame rate in dim light (a 60 fps mode can drop to 15 fps), which no
//! pipeline can compensate for.
//!
//! The image controls the user may adjust ([`IMAGE_CONTROLS`]: brightness,
//! contrast, saturation, sharpness, backlight compensation) are an
//! allowlist, the same for every camera: [`ImageControls`] opens a node
//! once and reports those it exposes as writable integer or boolean
//! controls (skipping one it cannot read), writes one by name and reads one
//! back; [`check_value`] checks a value against a control's range and
//! step. No camera is special-cased.
//!
//! Every control is optional: cameras differ in what they expose. Callers
//! probe with [`probe_ctrl`] first; an absent or disabled control is skipped,
//! never an error.

use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// `V4L2_CID_EXPOSURE_AUTO_PRIORITY` (`exposure_dynamic_framerate` in v4l2-ctl).
pub const CID_EXPOSURE_AUTO_PRIORITY: u32 = 0x009a_0903;
/// `V4L2_CID_BRIGHTNESS`
pub const CID_BRIGHTNESS: u32 = 0x0098_0900;
/// `V4L2_CID_CONTRAST`
pub const CID_CONTRAST: u32 = 0x0098_0901;
/// `V4L2_CID_SATURATION`
pub const CID_SATURATION: u32 = 0x0098_0902;
/// `V4L2_CID_SHARPNESS`
pub const CID_SHARPNESS: u32 = 0x0098_091b;
/// `V4L2_CID_BACKLIGHT_COMPENSATION`
pub const CID_BACKLIGHT_COMPENSATION: u32 = 0x0098_091c;

/// The image controls a user may adjust, by their v4l2-ctl name, in the
/// order they are reported: the same list for every camera. A camera
/// exposes the subset it has ([`query_image_controls`]).
pub const IMAGE_CONTROLS: [(&str, u32); 5] = [
    ("brightness", CID_BRIGHTNESS),
    ("contrast", CID_CONTRAST),
    ("saturation", CID_SATURATION),
    ("sharpness", CID_SHARPNESS),
    ("backlight_compensation", CID_BACKLIGHT_COMPENSATION),
];

/// The control id of the image control `name` ([`IMAGE_CONTROLS`]).
pub fn image_control_id(name: &str) -> Option<u32> {
    IMAGE_CONTROLS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, id)| *id)
}

#[repr(C)]
struct V4l2Control {
    id: u32,
    value: i32,
}

/// `V4L2_CTRL_FLAG_DISABLED`
pub const CTRL_FLAG_DISABLED: u32 = 0x0001;
/// `V4L2_CTRL_FLAG_READ_ONLY`
pub const CTRL_FLAG_READ_ONLY: u32 = 0x0004;
/// `V4L2_CTRL_FLAG_INACTIVE`: the control's value does not apply now (a
/// manual setting while its automatic mode is on, say).
pub const CTRL_FLAG_INACTIVE: u32 = 0x0010;

/// `V4L2_CTRL_FLAG_GRABBED`: the control cannot be changed now (the device
/// streams, typically).
pub const CTRL_FLAG_GRABBED: u32 = 0x0002;
/// `V4L2_CTRL_FLAG_UPDATE`: changing it may change other controls.
pub const CTRL_FLAG_UPDATE: u32 = 0x0008;
/// `V4L2_CTRL_FLAG_SLIDER`: a hint that a slider suits it.
pub const CTRL_FLAG_SLIDER: u32 = 0x0020;
/// `V4L2_CTRL_FLAG_WRITE_ONLY`: it has no readable value.
pub const CTRL_FLAG_WRITE_ONLY: u32 = 0x0040;
/// `V4L2_CTRL_FLAG_VOLATILE`: the device changes its value by itself.
pub const CTRL_FLAG_VOLATILE: u32 = 0x0080;

/// `V4L2_CTRL_TYPE_INTEGER`
pub const CTRL_TYPE_INTEGER: u32 = 1;
/// `V4L2_CTRL_TYPE_BOOLEAN`
pub const CTRL_TYPE_BOOLEAN: u32 = 2;
/// `V4L2_CTRL_TYPE_MENU`: a value is the index of a named item.
pub const CTRL_TYPE_MENU: u32 = 3;
/// `V4L2_CTRL_TYPE_BUTTON`: an action, without a value.
pub const CTRL_TYPE_BUTTON: u32 = 4;
/// `V4L2_CTRL_TYPE_CTRL_CLASS`: a class header, not a control.
pub const CTRL_TYPE_CTRL_CLASS: u32 = 6;
/// `V4L2_CTRL_TYPE_INTEGER_MENU`: a value is the index of an integer item.
pub const CTRL_TYPE_INTEGER_MENU: u32 = 9;

#[repr(C)]
struct V4l2QueryCtrl {
    id: u32,
    type_: u32,
    name: [u8; 32],
    minimum: i32,
    maximum: i32,
    step: i32,
    default_value: i32,
    flags: u32,
    reserved: [u32; 2],
}

const _: () = assert!(std::mem::size_of::<V4l2QueryCtrl>() == 68);

/// `_IOWR('V', 36, struct v4l2_queryctrl)`
const VIDIOC_QUERYCTRL: libc::c_ulong =
    (3 << 30) | (68 << 16) | ((b'V' as libc::c_ulong) << 8) | 36;

/// What `VIDIOC_QUERYCTRL` reports about a control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtrlInfo {
    pub name: String,
    pub minimum: i32,
    pub maximum: i32,
    /// The distance between two valid values, from `minimum`.
    pub step: i32,
    pub default_value: i32,
    pub flags: u32,
    /// `V4L2_CTRL_TYPE_*` ([`CTRL_TYPE_INTEGER`], [`CTRL_TYPE_BOOLEAN`],
    /// ...).
    pub type_: u32,
}

impl CtrlInfo {
    pub fn disabled(&self) -> bool {
        self.flags & CTRL_FLAG_DISABLED != 0
    }

    pub fn read_only(&self) -> bool {
        self.flags & CTRL_FLAG_READ_ONLY != 0
    }

    pub fn inactive(&self) -> bool {
        self.flags & CTRL_FLAG_INACTIVE != 0
    }

    /// Whether the device refuses changes now ([`CTRL_FLAG_GRABBED`]).
    pub fn grabbed(&self) -> bool {
        self.flags & CTRL_FLAG_GRABBED != 0
    }

    /// Whether a user may adjust it as an image control: neither disabled,
    /// read-only nor inactive, and an integer or a boolean.
    pub fn adjustable(&self) -> bool {
        !self.disabled()
            && !self.read_only()
            && !self.inactive()
            && matches!(self.type_, CTRL_TYPE_INTEGER | CTRL_TYPE_BOOLEAN)
    }
}

pub(crate) fn open_node(path: &Path, write: bool) -> std::io::Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .write(write)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
}

/// Queries control `id`. `Ok(None)` when the device does not have it
/// (`EINVAL`); `Err` when the node cannot be opened or the query fails
/// otherwise.
pub fn query_ctrl(path: &Path, id: u32) -> std::io::Result<Option<CtrlInfo>> {
    query_ctrl_on(&open_node(path, false)?, id)
}

pub(crate) fn query_ctrl_on(file: &fs::File, id: u32) -> std::io::Result<Option<CtrlInfo>> {
    Ok(query_ctrl_raw(file, id)?.map(|(_, info)| info))
}

/// `V4L2_CTRL_FLAG_NEXT_CTRL`: asks [`query_next_ctrl_on`] for the first
/// control after the id it is or'ed with.
const CTRL_FLAG_NEXT_CTRL: u32 = 0x8000_0000;

/// The control that follows `after` (0: the first one), with its id, by
/// `VIDIOC_QUERYCTRL` with `V4L2_CTRL_FLAG_NEXT_CTRL`. `Ok(None)` past the
/// last one.
pub(crate) fn query_next_ctrl_on(
    file: &fs::File,
    after: u32,
) -> std::io::Result<Option<(u32, CtrlInfo)>> {
    query_ctrl_raw(file, after | CTRL_FLAG_NEXT_CTRL)
}

/// `VIDIOC_QUERYCTRL` for `id` (which may carry `V4L2_CTRL_FLAG_NEXT_CTRL`):
/// the id the driver answered for and its info. `Ok(None)` on `EINVAL`.
fn query_ctrl_raw(file: &fs::File, id: u32) -> std::io::Result<Option<(u32, CtrlInfo)>> {
    let mut q = V4l2QueryCtrl {
        id,
        type_: 0,
        name: [0; 32],
        minimum: 0,
        maximum: 0,
        step: 0,
        default_value: 0,
        flags: 0,
        reserved: [0; 2],
    };
    // SAFETY: `q` is a valid `struct v4l2_queryctrl` that outlives the call;
    // the fd is open for the duration.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), VIDIOC_QUERYCTRL as _, &mut q) };
    if rc != 0 {
        let e = std::io::Error::last_os_error();
        return if e.raw_os_error() == Some(libc::EINVAL) {
            Ok(None)
        } else {
            Err(e)
        };
    }
    let end = q.name.iter().position(|&b| b == 0).unwrap_or(q.name.len());
    Ok(Some((
        q.id,
        CtrlInfo {
            name: String::from_utf8_lossy(&q.name[..end]).into_owned(),
            minimum: q.minimum,
            maximum: q.maximum,
            step: q.step,
            default_value: q.default_value,
            flags: q.flags,
            type_: q.type_,
        },
    )))
}

/// Why [`probe_ctrl`] skipped a control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CtrlSkip {
    /// The device does not expose the control.
    Absent,
    /// The control exists but is flagged disabled.
    Disabled,
    /// The node could not be queried or the value could not be read.
    Unreadable(String),
}

impl std::fmt::Display for CtrlSkip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CtrlSkip::Absent => f.write_str("not exposed by the device"),
            CtrlSkip::Disabled => f.write_str("disabled by the device"),
            CtrlSkip::Unreadable(e) => write!(f, "unreadable: {e}"),
        }
    }
}

/// Probes control `id` with `VIDIOC_QUERYCTRL`, then reads it: the control's
/// info and current value when present and enabled, otherwise why it is
/// skipped. Never an error: every control is optional.
pub fn probe_ctrl(path: &Path, id: u32) -> Result<(CtrlInfo, i32), CtrlSkip> {
    let info = match query_ctrl(path, id) {
        Ok(Some(info)) => info,
        Ok(None) => return Err(CtrlSkip::Absent),
        Err(e) => return Err(CtrlSkip::Unreadable(e.to_string())),
    };
    if info.disabled() {
        return Err(CtrlSkip::Disabled);
    }
    let value = get_ctrl(path, id).map_err(|e| CtrlSkip::Unreadable(e.to_string()))?;
    Ok((info, value))
}

/// `_IOWR('V', 27, struct v4l2_control)`
pub(crate) const VIDIOC_G_CTRL: libc::c_ulong =
    (3 << 30) | (8 << 16) | ((b'V' as libc::c_ulong) << 8) | 27;
/// `_IOWR('V', 28, struct v4l2_control)`
pub(crate) const VIDIOC_S_CTRL: libc::c_ulong =
    (3 << 30) | (8 << 16) | ((b'V' as libc::c_ulong) << 8) | 28;

fn ioctl_ctrl(path: &Path, request: libc::c_ulong, id: u32, value: i32) -> std::io::Result<i32> {
    ioctl_ctrl_on(&open_node(path, true)?, request, id, value)
}

pub(crate) fn ioctl_ctrl_on(
    file: &fs::File,
    request: libc::c_ulong,
    id: u32,
    value: i32,
) -> std::io::Result<i32> {
    let mut ctrl = V4l2Control { id, value };
    // SAFETY: `ctrl` is a valid `struct v4l2_control` that outlives the call;
    // the fd is open for the duration.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), request as _, &mut ctrl) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(ctrl.value)
}

/// Reads a control's current value.
pub fn get_ctrl(path: &Path, id: u32) -> std::io::Result<i32> {
    ioctl_ctrl(path, VIDIOC_G_CTRL, id, 0)
}

/// Sets a control. Does not start streaming or change formats.
pub fn set_ctrl(path: &Path, id: u32, value: i32) -> std::io::Result<()> {
    ioctl_ctrl(path, VIDIOC_S_CTRL, id, value).map(|_| ())
}

/// One image control a device exposes ([`ImageControls::query`]): its
/// range, step, default and current value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceControl {
    /// The [`IMAGE_CONTROLS`] name (`sharpness`, ...).
    pub name: String,
    pub min: i32,
    pub max: i32,
    pub step: i32,
    pub default: i32,
    /// The value on the device when it was queried.
    pub value: i32,
}

/// An image control left out of a query because its `VIDIOC_QUERYCTRL` or
/// its `VIDIOC_G_CTRL` failed (`EIO` and `EPIPE` are common on UVC).
#[derive(Debug)]
pub struct SkippedControl {
    /// The [`IMAGE_CONTROLS`] name.
    pub name: &'static str,
    pub error: std::io::Error,
}

/// What [`ImageControls::query`] found: the controls, and those it left
/// out because they could not be read.
#[derive(Debug, Default)]
pub struct QueriedControls {
    pub controls: Vec<DeviceControl>,
    pub skipped: Vec<SkippedControl>,
}

/// The control id of `name`, or `InvalidInput` for a name outside the
/// allowlist.
fn image_control_id_or_err(name: &str) -> std::io::Result<u32> {
    image_control_id(name).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{name:?} is not an image control"),
        )
    })
}

/// The image controls of one device node, opened once for any number of
/// queries, writes and reads. Opening, querying and reading start no
/// stream, so they may run while any application uses the camera.
#[derive(Debug)]
pub struct ImageControls {
    file: fs::File,
}

impl ImageControls {
    /// Opens the node at `path`, for writing as well when `write`.
    pub fn open(path: &Path, write: bool) -> std::io::Result<ImageControls> {
        Ok(ImageControls {
            file: open_node(path, write)?,
        })
    }

    /// The [`IMAGE_CONTROLS`] the device exposes as adjustable
    /// ([`CtrlInfo::adjustable`]), in that order, each with its current
    /// value. One the device does not have, or that is not adjustable, is
    /// left out; one whose query or read fails is left out and listed in
    /// [`QueriedControls::skipped`]. Only queries and reads: nothing is
    /// written.
    pub fn query(&self) -> QueriedControls {
        collect_image_controls(
            |id| query_ctrl_on(&self.file, id),
            |id| ioctl_ctrl_on(&self.file, VIDIOC_G_CTRL, id, 0),
        )
    }

    /// Writes the image control `name` ([`IMAGE_CONTROLS`]). The value is
    /// the caller's to check ([`check_value`]); the driver may refuse it. A
    /// name outside the allowlist is `InvalidInput`.
    pub fn set(&self, name: &str, value: i32) -> std::io::Result<()> {
        let id = image_control_id_or_err(name)?;
        ioctl_ctrl_on(&self.file, VIDIOC_S_CTRL, id, value).map(|_| ())
    }

    /// Reads the image control `name` ([`IMAGE_CONTROLS`]): one
    /// `VIDIOC_G_CTRL`.
    pub fn get(&self, name: &str) -> std::io::Result<i32> {
        let id = image_control_id_or_err(name)?;
        ioctl_ctrl_on(&self.file, VIDIOC_G_CTRL, id, 0)
    }
}

/// [`ImageControls::query`] over its two ioctls: `query` is
/// `VIDIOC_QUERYCTRL`, `read` is `VIDIOC_G_CTRL`, each by control id. A
/// control whose query or read fails is skipped, never the whole list.
fn collect_image_controls(
    mut query: impl FnMut(u32) -> std::io::Result<Option<CtrlInfo>>,
    mut read: impl FnMut(u32) -> std::io::Result<i32>,
) -> QueriedControls {
    let mut out = QueriedControls::default();
    for (name, id) in IMAGE_CONTROLS {
        let info = match query(id) {
            Ok(Some(info)) if info.adjustable() => info,
            Ok(_) => continue,
            Err(error) => {
                out.skipped.push(SkippedControl { name, error });
                continue;
            }
        };
        match read(id) {
            Ok(value) => out.controls.push(DeviceControl {
                name: name.to_string(),
                min: info.minimum,
                max: info.maximum,
                step: info.step,
                default: info.default_value,
                value,
            }),
            Err(error) => out.skipped.push(SkippedControl { name, error }),
        }
    }
    out
}

/// The image controls of the device at `path` ([`ImageControls::query`]),
/// without those it could not read. `Err` only when the node cannot be
/// opened.
pub fn query_image_controls(path: &Path) -> std::io::Result<Vec<DeviceControl>> {
    Ok(ImageControls::open(path, false)?.query().controls)
}

/// Writes the image control `name` on the device at `path`
/// ([`ImageControls::set`]). A name outside the allowlist is
/// `InvalidInput`, before the node is opened.
pub fn set_image_control(path: &Path, name: &str, value: i32) -> std::io::Result<()> {
    image_control_id_or_err(name)?;
    ImageControls::open(path, true)?.set(name, value)
}

/// Whether `value` is valid for `control`: within `min..=max` and a whole
/// number of steps from `min` (a step of zero or less counts as 1). The
/// error names the control, the value and the range.
pub fn check_value(control: &DeviceControl, value: i32) -> Result<(), String> {
    let (min, max, v) = (
        i64::from(control.min),
        i64::from(control.max),
        i64::from(value),
    );
    let step = i64::from(control.step).max(1);
    if v < min || v > max {
        return Err(format!(
            "{} {value} is outside its range {}..={}",
            control.name, control.min, control.max
        ));
    }
    if (v - min) % step != 0 {
        return Err(format!(
            "{} {value} is not a whole number of steps of {step} from {} (range {}..={})",
            control.name, control.min, control.min, control.max
        ));
    }
    Ok(())
}

/// Holds a control at a temporary value and puts the original back.
///
/// [`ControlGuard::restore`] restores explicitly and reports the outcome;
/// otherwise `Drop` restores, so early returns and unwinding panics leave the
/// camera as they found it (a failure there is printed to stderr). Only a
/// process that dies without unwinding (SIGKILL, abort) skips the restore.
#[derive(Debug)]
pub struct ControlGuard {
    path: PathBuf,
    id: u32,
    original: i32,
    armed: bool,
}

impl ControlGuard {
    /// Sets control `id` on `path` to `value`, remembering `original` (the
    /// value read before, which the caller already has) for the restore.
    /// On error nothing was changed and no guard exists.
    pub fn set(path: &Path, id: u32, original: i32, value: i32) -> std::io::Result<Self> {
        set_ctrl(path, id, value)?;
        Ok(ControlGuard {
            path: path.to_path_buf(),
            id,
            original,
            armed: true,
        })
    }

    /// The value the control will be restored to.
    pub fn original(&self) -> i32 {
        self.original
    }

    /// Restores the original value now and disarms the guard.
    pub fn restore(mut self) -> std::io::Result<()> {
        self.armed = false;
        set_ctrl(&self.path, self.id, self.original)
    }
}

impl Drop for ControlGuard {
    fn drop(&mut self) {
        if self.armed
            && let Err(e) = set_ctrl(&self.path, self.id, self.original)
        {
            eprintln!(
                "failed to restore V4L2 control {:#x} on {} to {}: {e}",
                self.id,
                self.path.display(),
                self.original
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers() {
        assert_eq!(VIDIOC_G_CTRL, 0xc008_561b);
        assert_eq!(VIDIOC_S_CTRL, 0xc008_561c);
        assert_eq!(VIDIOC_QUERYCTRL, 0xc044_5624);
        assert_eq!(std::mem::size_of::<V4l2Control>(), 8);
    }

    #[test]
    fn probing_a_missing_node_skips_instead_of_failing() {
        let missing = Path::new("/nonexistent/obc-no-such-video-node");
        assert!(matches!(
            probe_ctrl(missing, CID_EXPOSURE_AUTO_PRIORITY),
            Err(CtrlSkip::Unreadable(_))
        ));
        let info = CtrlInfo {
            name: "x".into(),
            minimum: 0,
            maximum: 1,
            step: 1,
            default_value: 0,
            flags: CTRL_FLAG_DISABLED | CTRL_FLAG_READ_ONLY,
            type_: CTRL_TYPE_BOOLEAN,
        };
        assert!(info.disabled() && info.read_only());
        assert!(!info.inactive() && !info.adjustable());
        assert_eq!(CtrlSkip::Absent.to_string(), "not exposed by the device");
    }

    fn control(min: i32, max: i32, step: i32) -> DeviceControl {
        DeviceControl {
            name: "sharpness".into(),
            min,
            max,
            step,
            default: min,
            value: min,
        }
    }

    #[test]
    fn check_value_takes_the_range_and_the_step() {
        let c = control(0, 255, 1);
        for v in [0, 1, 128, 254, 255] {
            assert_eq!(check_value(&c, v), Ok(()), "{v}");
        }
        for v in [-1, 256, i32::MIN, i32::MAX] {
            let e = check_value(&c, v).unwrap_err();
            assert!(e.contains("sharpness"), "{e}");
            assert!(e.contains(&v.to_string()), "{e}");
            assert!(e.contains("0..=255"), "{e}");
        }
        // Off the step, counted from the minimum.
        let c = control(-10, 10, 5);
        for v in [-10, -5, 0, 5, 10] {
            assert_eq!(check_value(&c, v), Ok(()), "{v}");
        }
        for v in [-9, -1, 1, 4, 9] {
            let e = check_value(&c, v).unwrap_err();
            assert!(e.contains("steps of 5") && e.contains("-10..=10"), "{e}");
        }
        let c = control(1, 9, 4);
        assert_eq!(check_value(&c, 5), Ok(()));
        assert!(check_value(&c, 4).is_err());
        // A step of zero or less counts as 1.
        for step in [0, -3, i32::MIN] {
            let c = control(0, 3, step);
            for v in 0..=3 {
                assert_eq!(check_value(&c, v), Ok(()), "step {step}, {v}");
            }
            assert!(check_value(&c, 4).is_err());
        }
        // The whole i32 range, without overflow.
        let c = control(i32::MIN, i32::MAX, i32::MAX);
        for v in [i32::MIN, -1, i32::MAX - 1] {
            assert_eq!(check_value(&c, v), Ok(()), "{v}");
        }
        assert!(check_value(&c, i32::MAX).is_err());
        assert!(check_value(&c, 0).is_err());
        let c = control(i32::MIN, i32::MAX, 1);
        assert_eq!(check_value(&c, i32::MIN), Ok(()));
        assert_eq!(check_value(&c, i32::MAX), Ok(()));
        // An empty range (min above max) takes nothing.
        assert!(check_value(&control(5, 4, 1), 5).is_err());
    }

    #[test]
    fn the_image_control_allowlist() {
        assert_eq!(
            IMAGE_CONTROLS,
            [
                ("brightness", 0x0098_0900),
                ("contrast", 0x0098_0901),
                ("saturation", 0x0098_0902),
                ("sharpness", 0x0098_091b),
                ("backlight_compensation", 0x0098_091c),
            ]
        );
        // The state file checks the same names.
        let names: Vec<&str> = IMAGE_CONTROLS.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, obc_core::config::CAMERA_CONTROL_NAMES);
        assert_eq!(image_control_id("sharpness"), Some(CID_SHARPNESS));
        assert_eq!(image_control_id("exposure_dynamic_framerate"), None);
        assert_eq!(image_control_id("Sharpness"), None);
        assert_eq!(std::mem::size_of::<V4l2QueryCtrl>(), 68);
    }

    #[test]
    fn adjustable_controls_are_writable_integers_or_booleans() {
        let info = |type_, flags| CtrlInfo {
            name: "x".into(),
            minimum: 0,
            maximum: 255,
            step: 1,
            default_value: 128,
            flags,
            type_,
        };
        assert!(info(CTRL_TYPE_INTEGER, 0).adjustable());
        assert!(info(CTRL_TYPE_BOOLEAN, 0).adjustable());
        // V4L2_CTRL_FLAG_SLIDER (0x20) does not matter.
        assert!(info(CTRL_TYPE_INTEGER, 0x20).adjustable());
        for flags in [CTRL_FLAG_DISABLED, CTRL_FLAG_READ_ONLY, CTRL_FLAG_INACTIVE] {
            assert!(!info(CTRL_TYPE_INTEGER, flags).adjustable(), "{flags:#x}");
        }
        // Menu (3), button (4), integer64 (5).
        for type_ in [3, 4, 5] {
            assert!(!info(type_, 0).adjustable(), "type {type_}");
        }
    }

    #[test]
    fn image_controls_of_a_missing_node_are_an_error() {
        let missing = Path::new("/nonexistent/obc-no-such-video-node");
        assert!(query_image_controls(missing).is_err());
        assert!(set_image_control(missing, "sharpness", 128).is_err());
        let e = set_image_control(missing, "zoom_absolute", 1).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
        assert!(ImageControls::open(missing, false).is_err());
    }

    #[test]
    fn a_control_that_cannot_be_queried_or_read_is_skipped_not_the_list() {
        let errno = |n| std::io::Error::from_raw_os_error(n);
        let adjustable = |flags| CtrlInfo {
            name: "x".into(),
            minimum: 0,
            maximum: 255,
            step: 1,
            default_value: 128,
            flags,
            type_: CTRL_TYPE_INTEGER,
        };
        let mut queried = Vec::new();
        let mut read = Vec::new();
        let got = collect_image_controls(
            |id| {
                queried.push(id);
                match id {
                    CID_BRIGHTNESS => Err(errno(libc::EIO)),
                    CID_CONTRAST => Ok(None),
                    CID_BACKLIGHT_COMPENSATION => Ok(Some(adjustable(CTRL_FLAG_READ_ONLY))),
                    _ => Ok(Some(adjustable(0))),
                }
            },
            |id| {
                read.push(id);
                match id {
                    CID_SATURATION => Err(errno(libc::EPIPE)),
                    _ => Ok(7),
                }
            },
        );
        // Every control is tried; only the adjustable ones are read.
        assert_eq!(queried, IMAGE_CONTROLS.map(|(_, id)| id));
        assert_eq!(read, [CID_SATURATION, CID_SHARPNESS]);
        assert_eq!(
            got.controls,
            [DeviceControl {
                name: "sharpness".into(),
                min: 0,
                max: 255,
                step: 1,
                default: 128,
                value: 7,
            }]
        );
        let skipped: Vec<(&str, Option<i32>)> = got
            .skipped
            .iter()
            .map(|s| (s.name, s.error.raw_os_error()))
            .collect();
        assert_eq!(
            skipped,
            [
                ("brightness", Some(libc::EIO)),
                ("saturation", Some(libc::EPIPE))
            ]
        );
        // Nothing failing: everything, nothing skipped.
        let all = collect_image_controls(|_| Ok(Some(adjustable(0))), |_| Ok(1));
        assert_eq!(all.controls.len(), 5);
        assert!(all.skipped.is_empty());
    }

    #[test]
    fn guard_is_not_created_when_the_set_fails() {
        let missing = Path::new("/nonexistent/obc-no-such-video-node");
        assert!(ControlGuard::set(missing, CID_EXPOSURE_AUTO_PRIORITY, 1, 0).is_err());
    }
}
