//! Minimal V4L2 control access (`VIDIOC_QUERYCTRL` / `VIDIOC_G_CTRL` /
//! `VIDIOC_S_CTRL`).
//!
//! Used by the bench to read, and optionally pin then restore, the UVC
//! `exposure_dynamic_framerate` control: when it is on, the camera may lower
//! its frame rate in dim light (a 60 fps mode can drop to 15 fps), which no
//! pipeline can compensate for.
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

#[repr(C)]
struct V4l2Control {
    id: u32,
    value: i32,
}

/// `V4L2_CTRL_FLAG_DISABLED`
pub const CTRL_FLAG_DISABLED: u32 = 0x0001;
/// `V4L2_CTRL_FLAG_READ_ONLY`
pub const CTRL_FLAG_READ_ONLY: u32 = 0x0004;

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
    pub default_value: i32,
    pub flags: u32,
}

impl CtrlInfo {
    pub fn disabled(&self) -> bool {
        self.flags & CTRL_FLAG_DISABLED != 0
    }

    pub fn read_only(&self) -> bool {
        self.flags & CTRL_FLAG_READ_ONLY != 0
    }
}

/// Queries control `id`. `Ok(None)` when the device does not have it
/// (`EINVAL`); `Err` when the node cannot be opened or the query fails
/// otherwise.
pub fn query_ctrl(path: &Path, id: u32) -> std::io::Result<Option<CtrlInfo>> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
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
    Ok(Some(CtrlInfo {
        name: String::from_utf8_lossy(&q.name[..end]).into_owned(),
        minimum: q.minimum,
        maximum: q.maximum,
        default_value: q.default_value,
        flags: q.flags,
    }))
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
const VIDIOC_G_CTRL: libc::c_ulong = (3 << 30) | (8 << 16) | ((b'V' as libc::c_ulong) << 8) | 27;
/// `_IOWR('V', 28, struct v4l2_control)`
const VIDIOC_S_CTRL: libc::c_ulong = (3 << 30) | (8 << 16) | ((b'V' as libc::c_ulong) << 8) | 28;

fn ioctl_ctrl(path: &Path, request: libc::c_ulong, id: u32, value: i32) -> std::io::Result<i32> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
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
            default_value: 0,
            flags: CTRL_FLAG_DISABLED | CTRL_FLAG_READ_ONLY,
        };
        assert!(info.disabled() && info.read_only());
        assert_eq!(CtrlSkip::Absent.to_string(), "not exposed by the device");
    }

    #[test]
    fn guard_is_not_created_when_the_set_fails() {
        let missing = Path::new("/nonexistent/obc-no-such-video-node");
        assert!(ControlGuard::set(missing, CID_EXPOSURE_AUTO_PRIORITY, 1, 0).is_err());
    }
}
