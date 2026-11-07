//! Live device discovery: GStreamer `DeviceMonitor` and a sysfs scan.
//!
//! Both routes end in the same [`DeviceInfo`]: every node is enriched from
//! sysfs (`name`, USB `idVendor`/`idProduct`, the `virtual` parent that marks
//! v4l2loopback) and `VIDIOC_QUERYCAP` (driver, capability bits), so selection
//! sees identical data whichever route listed the node.
//!
//! * Cameras: `DeviceMonitor` with the `Video/Source` filter. On this system
//!   the PipeWire provider answers and may hide GStreamer's own
//!   `v4l2deviceprovider`; we read its `api.v4l2.path` (PipeWire) or
//!   `device.path` (v4l2 provider) and always build `v4l2src device=<path>`,
//!   never `pipewiresrc`. If the monitor yields no V4L2 node, we fall back to
//!   the sysfs scan.
//! * The loopback sink: always the sysfs scan. Loopback output nodes are
//!   `Video/Sink`, which the PipeWire provider does not list, and their card
//!   label is readable from sysfs without opening the node (opening it would
//!   count as a consumer).
//! * Capabilities: every non-loopback capture node is enumerated with
//!   `VIDIOC_ENUM_FMT` / `VIDIOC_ENUM_FRAMESIZES` / `VIDIOC_ENUM_FRAMEINTERVALS`
//!   ([`enumerate_formats`]), which needs no streaming and changes nothing. If
//!   that fails on a node the monitor listed, the monitor's caps are used
//!   ([`capabilities_from_gst`]). A stepwise/continuous size or interval
//!   range is listed as its ends and the common values inside it, plus the
//!   requested size and rate when they lie inside the range on its step
//!   grid, so a request the range covers is selectable as exact.
//!
//! Camera lists are sorted by `/dev/videoN` number, so "the first camera" is
//! deterministic whichever route produced the list.

use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use gst::prelude::*;
use obc_core::{ModeRequest, UsbId};
use serde::Serialize;

use crate::capability::{Capabilities, format_from_fourcc, format_from_gst};
use crate::device::{DeviceInfo, DiscoverySource};

const SYSFS_V4L: &str = "/sys/class/video4linux";

/// Which route lists camera candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryRoute {
    /// `DeviceMonitor` first; sysfs scan if it lists no V4L2 node.
    #[default]
    Auto,
    /// `DeviceMonitor` only.
    Monitor,
    /// sysfs scan only.
    Sysfs,
}

impl std::str::FromStr for DiscoveryRoute {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Ok(DiscoveryRoute::Auto),
            "monitor" => Ok(DiscoveryRoute::Monitor),
            "sysfs" => Ok(DiscoveryRoute::Sysfs),
            _ => Err(format!(
                "unknown discovery route {s:?} (auto|monitor|sysfs)"
            )),
        }
    }
}

/// Result of listing camera candidates, with the route that produced it.
#[derive(Debug, Clone)]
pub struct CameraList {
    pub devices: Vec<DeviceInfo>,
    /// The route that actually produced `devices`.
    pub used: DiscoverySource,
    /// Why the fallback was taken, if it was.
    pub note: Option<String>,
    /// The request whose size and rate were added from stepwise/continuous
    /// ranges (also for nodes added later by [`CameraList::include_node`]).
    pub want: Option<ModeRequest>,
}

impl CameraList {
    /// Adds the node at `path` (probed directly) when the route did not list
    /// it, so an explicit `/dev/videoN` works whichever route ran.
    pub fn include_node(&mut self, path: &Path) {
        if self.devices.iter().any(|d| d.path == path) {
            return;
        }
        if let Some(d) = probe_node(path, self.want.as_ref()) {
            self.devices.push(d);
            sort_by_node(&mut self.devices);
        }
    }
}

/// The `N` of `/dev/videoN` (`u32::MAX` for other names).
fn node_number(path: &Path) -> u32 {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("video"))
        .and_then(|n| n.parse().ok())
        .unwrap_or(u32::MAX)
}

fn sort_by_node(devices: &mut [DeviceInfo]) {
    devices.sort_by(|a, b| {
        node_number(&a.path)
            .cmp(&node_number(&b.path))
            .then_with(|| a.path.cmp(&b.path))
    });
}

/// Resolves a camera path given on the command line (`/dev/videoN` or a
/// stable link such as `/dev/v4l/by-id/...`) to the node it points at.
pub fn resolve_node_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Lists camera candidates according to `route`, sorted by `/dev/videoN`.
/// `want`: the mode request, so stepwise/continuous ranges list its size and
/// rate when they cover them (see the module docs). Requires `gst::init()`.
pub fn list_cameras(route: DiscoveryRoute, want: Option<&ModeRequest>) -> CameraList {
    let (devices, used, note) = list_unsorted(route, want);
    let mut list = CameraList {
        devices,
        used,
        note,
        want: want.copied(),
    };
    sort_by_node(&mut list.devices);
    list
}

fn list_unsorted(
    route: DiscoveryRoute,
    want: Option<&ModeRequest>,
) -> (Vec<DeviceInfo>, DiscoverySource, Option<String>) {
    match route {
        DiscoveryRoute::Sysfs => (scan_sysfs_for(want), DiscoverySource::Sysfs, None),
        DiscoveryRoute::Monitor => (
            monitor_sources(want).unwrap_or_default(),
            DiscoverySource::DeviceMonitor,
            None,
        ),
        DiscoveryRoute::Auto => match monitor_sources(want) {
            Ok(devs) if !devs.is_empty() => (devs, DiscoverySource::DeviceMonitor, None),
            Ok(_) => (
                scan_sysfs_for(want),
                DiscoverySource::Sysfs,
                Some("DeviceMonitor listed no V4L2 Video/Source device".into()),
            ),
            Err(e) => (
                scan_sysfs_for(want),
                DiscoverySource::Sysfs,
                Some(format!("DeviceMonitor failed: {e}")),
            ),
        },
    }
}

/// Lists V4L2 `Video/Source` devices through GStreamer's `DeviceMonitor`.
///
/// The monitor is started, read once, stopped and dropped before returning.
/// `want` as for [`list_cameras`].
pub fn monitor_sources(want: Option<&ModeRequest>) -> Result<Vec<DeviceInfo>, String> {
    let monitor = gst::DeviceMonitor::new();
    monitor.add_filter(Some("Video/Source"), None);
    monitor.start().map_err(|e| e.to_string())?;
    let mut out: Vec<DeviceInfo> = Vec::new();
    for device in monitor.devices() {
        let Some(props) = device.properties() else {
            continue;
        };
        let get = |k: &str| prop_string(&props, k);
        let api = get("device.api");
        if api.as_deref().is_some_and(|a| a != "v4l2") {
            continue; // libcamera, etc.
        }
        let Some(path) = v4l2_path_from_props(&get) else {
            continue;
        };
        if out.iter().any(|d| d.path == path) {
            continue;
        }
        let fallback = DeviceInfo {
            path: path.clone(),
            card: get("api.v4l2.cap.card")
                .or_else(|| get("v4l2.device.card"))
                .unwrap_or_else(|| device.display_name().to_string()),
            driver: get("api.v4l2.cap.driver").or_else(|| get("v4l2.device.driver")),
            usb: usb_from_props(&get),
            caps: None,
            // The monitor only lists Video/Source devices.
            capture: true,
            loopback: DeviceInfo::is_loopback_driver(
                get("api.v4l2.cap.driver")
                    .or_else(|| get("v4l2.device.driver"))
                    .as_deref(),
                get("api.v4l2.cap.bus_info")
                    .or_else(|| get("v4l2.device.bus_info"))
                    .as_deref(),
            ),
            source: DiscoverySource::DeviceMonitor,
            capabilities: device.caps().map(|c| capabilities_from_gst(&c)),
        };
        // Authoritative data comes from sysfs + QUERYCAP + format
        // enumeration; the monitor's properties and caps are only the
        // fallback when the node cannot be probed.
        let info = match probe_node(&path, want) {
            Some(mut d) => {
                d.source = DiscoverySource::DeviceMonitor;
                d.usb = d.usb.or(fallback.usb);
                if d.capabilities.is_none() && !d.loopback {
                    d.capabilities = fallback.capabilities;
                }
                d
            }
            None => fallback,
        };
        out.push(info);
    }
    monitor.stop();
    drop(monitor);
    Ok(out)
}

/// A provider property as a string. Integer-typed values are rendered as
/// `0x`-prefixed hex: the only integers read here are USB ids, and
/// [`parse_hex_id`] reads every id string as hex (as sysfs, udev and
/// PipeWire print them), so a decimal rendering would be misread.
fn prop_string(props: &gst::StructureRef, key: &str) -> Option<String> {
    let v = props.value(key).ok()?;
    if let Ok(s) = v.get::<String>() {
        return Some(s);
    }
    if let Ok(i) = v.get::<i32>() {
        return Some(format!("{i:#x}"));
    }
    if let Ok(u) = v.get::<u32>() {
        return Some(format!("{u:#x}"));
    }
    v.serialize().ok().map(|s| s.to_string())
}

/// Resolves the V4L2 node path from device-provider properties.
///
/// PipeWire: `api.v4l2.path` (and `object.path = v4l2:/dev/videoN`);
/// GStreamer v4l2 provider: `device.path` (newer versions also `api.v4l2.path`).
pub fn v4l2_path_from_props(get: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(p) = get("api.v4l2.path").or_else(|| get("device.path"))
        && p.starts_with("/dev/")
    {
        return Some(PathBuf::from(p));
    }
    get("object.path")
        .and_then(|p| p.strip_prefix("v4l2:").map(str::to_string))
        .filter(|p| p.starts_with("/dev/"))
        .map(PathBuf::from)
}

/// Reads a USB vendor/product pair from provider properties. Ids are always
/// hex, with or without `0x` (`0x1a2b`, `1a2b`); a bare `1133` is 0x1133,
/// never decimal. Integer-typed properties arrive here as `0x` hex via
/// `prop_string`.
pub fn usb_from_props(get: &dyn Fn(&str) -> Option<String>) -> Option<UsbId> {
    let vendor = parse_hex_id(&get("device.vendor.id")?)?;
    let product = parse_hex_id(&get("device.product.id")?)?;
    Some(UsbId { vendor, product })
}

/// Parses a 16-bit id written as hex (`0x1a2b`, `1a2b`, as sysfs prints it).
/// Digits are always hex: `"1133"` is 0x1133, not decimal 1133.
pub fn parse_hex_id(s: &str) -> Option<u16> {
    let t = s.trim();
    let t = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    if t.is_empty() || t.len() > 4 {
        return None;
    }
    u16::from_str_radix(t, 16).ok()
}

/// Scans `/sys/class/video4linux` and probes every node.
pub fn scan_sysfs() -> Vec<DeviceInfo> {
    scan_sysfs_for(None)
}

/// [`scan_sysfs`], with `want` as for [`list_cameras`].
fn scan_sysfs_for(want: Option<&ModeRequest>) -> Vec<DeviceInfo> {
    let Ok(entries) = fs::read_dir(SYSFS_V4L) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("video"))
        .collect();
    names.sort_by_key(|n| {
        n.trim_start_matches("video")
            .parse::<u32>()
            .unwrap_or(u32::MAX)
    });
    names
        .into_iter()
        .filter_map(|n| probe_node(&Path::new("/dev").join(n), want))
        .collect()
}

/// Builds a [`DeviceInfo`] for one node from sysfs and, for non-loopback
/// nodes, `VIDIOC_QUERYCAP`. Returns `None` if the node has no sysfs entry.
/// `want` as for [`list_cameras`].
pub fn probe_node(path: &Path, want: Option<&ModeRequest>) -> Option<DeviceInfo> {
    let node = path.file_name()?.to_str()?;
    let class_dir = Path::new(SYSFS_V4L).join(node);
    let name = fs::read_to_string(class_dir.join("name")).ok()?;
    let real = fs::canonicalize(&class_dir).ok();
    let is_virtual = real
        .as_ref()
        .is_some_and(|r| r.components().any(|c| c.as_os_str() == "virtual"));
    let usb = real.as_deref().and_then(usb_id_from_sysfs);

    // Never open a loopback node to identify it: an open counts as an opener
    // (and, with exclusive_caps, flips what the node advertises).
    let qc = if is_virtual { None } else { query_cap(path) };
    let (driver, caps, loopback) = match &qc {
        Some(q) => (
            Some(q.driver.clone()),
            Some(q.caps),
            DeviceInfo::is_loopback_driver(Some(&q.driver), Some(&q.bus_info)),
        ),
        None => (None, None, is_virtual && looks_like_loopback(&class_dir)),
    };
    let capture = caps.is_some_and(DeviceInfo::capture_from_caps);
    let capabilities = match caps {
        Some(c) if capture && !loopback => enumerate_formats(path, c, want),
        _ => None,
    };
    Some(DeviceInfo {
        path: path.to_path_buf(),
        card: qc
            .as_ref()
            .map_or_else(|| name.trim().to_string(), |q| q.card.clone()),
        driver: driver.or_else(|| loopback.then(|| crate::device::LOOPBACK_DRIVER.to_string())),
        usb,
        caps,
        capture,
        loopback,
        source: DiscoverySource::Sysfs,
        capabilities,
    })
}

/// A virtual video4linux node is v4l2loopback when it carries the
/// driver-specific sysfs attributes `max_openers` and `buffers`.
fn looks_like_loopback(class_dir: &Path) -> bool {
    class_dir.join("max_openers").exists() && class_dir.join("buffers").exists()
}

fn usb_id_from_sysfs(real: &Path) -> Option<UsbId> {
    // .../usbX/2-2/2-2:1.0/video4linux/videoN: walk up to the USB device dir.
    let mut dir = real.parent();
    for _ in 0..5 {
        let d = dir?;
        if let (Ok(v), Ok(p)) = (
            fs::read_to_string(d.join("idVendor")),
            fs::read_to_string(d.join("idProduct")),
        ) {
            return Some(UsbId {
                vendor: parse_hex_id(&v)?,
                product: parse_hex_id(&p)?,
            });
        }
        dir = d.parent();
    }
    None
}

/// Result of `VIDIOC_QUERYCAP`.
#[derive(Debug, Clone)]
pub struct QueryCap {
    pub driver: String,
    pub card: String,
    pub bus_info: String,
    /// Effective capability bits (`device_caps` when valid).
    pub caps: u32,
}

#[repr(C)]
struct V4l2Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

const _: () = assert!(std::mem::size_of::<V4l2Capability>() == 104);

/// `_IOR('V', 0, struct v4l2_capability)`
const VIDIOC_QUERYCAP: libc::c_ulong = (2 << 30) | (104 << 16) | ((b'V' as libc::c_ulong) << 8);

fn cstr(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

/// Opens the node read-only and non-blocking, issues `VIDIOC_QUERYCAP` and
/// closes it. Does not start streaming or change any format.
pub fn query_cap(path: &Path) -> Option<QueryCap> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let mut cap = V4l2Capability {
        driver: [0; 16],
        card: [0; 32],
        bus_info: [0; 32],
        version: 0,
        capabilities: 0,
        device_caps: 0,
        reserved: [0; 3],
    };
    // SAFETY: `cap` is a properly sized and aligned `struct v4l2_capability`
    // that outlives the call, and the fd is open for the duration.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), VIDIOC_QUERYCAP as _, &mut cap) };
    if rc != 0 {
        return None;
    }
    Some(QueryCap {
        driver: cstr(&cap.driver),
        card: cstr(&cap.card),
        bus_info: cstr(&cap.bus_info),
        caps: DeviceInfo::effective_caps(cap.capabilities, cap.device_caps),
    })
}

/// `V4L2_BUF_TYPE_VIDEO_CAPTURE`
const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
/// `V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE`
const BUF_TYPE_VIDEO_CAPTURE_MPLANE: u32 = 9;
/// `V4L2_FRMSIZE_TYPE_DISCRETE` / `V4L2_FRMIVAL_TYPE_DISCRETE`
const TYPE_DISCRETE: u32 = 1;
/// `V4L2_FRMSIZE_TYPE_STEPWISE` / `V4L2_FRMIVAL_TYPE_STEPWISE` (the other
/// non-discrete type, `CONTINUOUS`, has no step grid).
const TYPE_STEPWISE: u32 = 3;
/// Upper bound on any enumeration index (drivers end with `EINVAL` long before).
const MAX_ENUM: u32 = 512;
/// Frame rates tried inside a stepwise/continuous interval range.
const COMMON_RATES: [u32; 9] = [60, 50, 30, 25, 24, 20, 15, 10, 5];
/// Sizes tried inside a stepwise/continuous size range.
const COMMON_SIZES: [(u32, u32); 7] = [
    (640, 360),
    (640, 480),
    (1280, 720),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
    (4096, 2160),
];

#[repr(C)]
struct V4l2FmtDesc {
    index: u32,
    type_: u32,
    flags: u32,
    description: [u8; 32],
    pixelformat: u32,
    mbus_code: u32,
    reserved: [u32; 3],
}

#[repr(C)]
struct V4l2FrmSizeEnum {
    index: u32,
    pixel_format: u32,
    type_: u32,
    /// discrete: `width, height`; stepwise: `min_w, max_w, step_w, min_h, max_h, step_h`.
    u: [u32; 6],
    reserved: [u32; 2],
}

#[repr(C)]
struct V4l2FrmIvalEnum {
    index: u32,
    pixel_format: u32,
    width: u32,
    height: u32,
    type_: u32,
    /// discrete: `num, den`; stepwise: `min (num, den), max (num, den), step (num, den)`.
    u: [u32; 6],
    reserved: [u32; 2],
}

const _: () = assert!(std::mem::size_of::<V4l2FmtDesc>() == 64);
const _: () = assert!(std::mem::size_of::<V4l2FrmSizeEnum>() == 44);
const _: () = assert!(std::mem::size_of::<V4l2FrmIvalEnum>() == 52);

/// `_IOWR('V', 2, struct v4l2_fmtdesc)`
const VIDIOC_ENUM_FMT: libc::c_ulong = (3 << 30) | (64 << 16) | ((b'V' as libc::c_ulong) << 8) | 2;
/// `_IOWR('V', 74, struct v4l2_frmsizeenum)`
const VIDIOC_ENUM_FRAMESIZES: libc::c_ulong =
    (3 << 30) | (44 << 16) | ((b'V' as libc::c_ulong) << 8) | 74;
/// `_IOWR('V', 75, struct v4l2_frmivalenum)`
const VIDIOC_ENUM_FRAMEINTERVALS: libc::c_ulong =
    (3 << 30) | (52 << 16) | ((b'V' as libc::c_ulong) << 8) | 75;

/// Issues one `_IOWR` V4L2 ioctl on `fd`; `false` when it fails (`EINVAL`
/// ends every enumeration).
///
/// # Safety
/// `arg` must be the struct type `request` expects.
unsafe fn ioctl_rw<T>(fd: libc::c_int, request: libc::c_ulong, arg: &mut T) -> bool {
    // SAFETY: the caller pairs `request` with its struct; `arg` is a valid,
    // exclusively borrowed instance that outlives the call.
    unsafe { libc::ioctl(fd, request as _, arg as *mut T) == 0 }
}

fn fourcc_string(code: u32) -> String {
    code.to_le_bytes().iter().map(|&b| b as char).collect()
}

/// Lists the capture modes of the node at `path` (`device_caps` from
/// `VIDIOC_QUERYCAP`). Opens the node read-only and non-blocking, enumerates
/// and closes it; no format is set and nothing streams. `want`: a requested
/// size and rate that stepwise/continuous ranges list when they cover them.
/// `None` if the node cannot be opened or `VIDIOC_ENUM_FMT` fails at the
/// first index (so callers can fall back to other caps sources).
pub fn enumerate_formats(
    path: &Path,
    device_caps: u32,
    want: Option<&ModeRequest>,
) -> Option<Capabilities> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let fd = file.as_raw_fd();
    let buf_type = if device_caps & crate::device::CAP_VIDEO_CAPTURE != 0 {
        BUF_TYPE_VIDEO_CAPTURE
    } else {
        BUF_TYPE_VIDEO_CAPTURE_MPLANE
    };
    let want_size = want.map(|r| (r.width, r.height));
    let want_rate = want.map(|r| (r.fps_num, r.fps_den));
    collect_capabilities(
        |index| {
            let mut desc = V4l2FmtDesc {
                index,
                type_: buf_type,
                flags: 0,
                description: [0; 32],
                pixelformat: 0,
                mbus_code: 0,
                reserved: [0; 3],
            };
            // SAFETY: VIDIOC_ENUM_FMT takes a `struct v4l2_fmtdesc`.
            unsafe { ioctl_rw(fd, VIDIOC_ENUM_FMT, &mut desc) }.then_some(desc.pixelformat)
        },
        |pixel_format| {
            let mut out = Vec::new();
            for (w, h) in frame_sizes(fd, pixel_format, want_size) {
                for (num, den) in frame_rates(fd, pixel_format, w, h, want_rate) {
                    out.push((w, h, num, den));
                }
            }
            out
        },
    )
}

/// The enumeration loop of [`enumerate_formats`], over `format_at(index)`
/// (`VIDIOC_ENUM_FMT`: the fourcc code, `None` once it fails) and
/// `modes_of(code)` (`(width, height, fps_num, fps_den)` of one format).
/// `None` when the very first index fails: the node could not be
/// enumerated, which is not the same as offering no format.
fn collect_capabilities(
    mut format_at: impl FnMut(u32) -> Option<u32>,
    mut modes_of: impl FnMut(u32) -> Vec<(u32, u32, u32, u32)>,
) -> Option<Capabilities> {
    let mut caps = Capabilities::default();
    for index in 0..MAX_ENUM {
        let Some(code) = format_at(index) else {
            if index == 0 {
                return None;
            }
            break;
        };
        let fourcc = fourcc_string(code);
        let Some(format) = format_from_fourcc(&fourcc) else {
            caps.push_other(&fourcc);
            continue;
        };
        for (w, h, num, den) in modes_of(code) {
            caps.push_mode(format, w, h, num, den);
        }
    }
    Some(caps)
}

/// Whether `v` lies in `min..=max` on the grid `min + k * step` (any value
/// in range when `step <= 1`, as for a continuous range).
fn on_size_grid(v: u32, min: u32, max: u32, step: u32) -> bool {
    (min..=max).contains(&v) && (step <= 1 || (v - min).is_multiple_of(step))
}

/// The sizes a stepwise/continuous range (`u` as in `v4l2_frmsizeenum`:
/// `min_w, max_w, step_w, min_h, max_h, step_h`) contributes: its minimum,
/// the common sizes inside it, `want` if inside it, and its maximum.
fn sizes_in_range(u: [u32; 6], want: Option<(u32, u32)>) -> Vec<(u32, u32)> {
    let [min_w, max_w, step_w, min_h, max_h, step_h] = u;
    let inside = |&(w, h): &(u32, u32)| {
        on_size_grid(w, min_w, max_w, step_w) && on_size_grid(h, min_h, max_h, step_h)
    };
    let mut out = vec![(min_w, min_h)];
    for s in COMMON_SIZES.into_iter().chain(want).filter(inside) {
        if !out.contains(&s) {
            out.push(s);
        }
    }
    if !out.contains(&(max_w, max_h)) {
        out.push((max_w, max_h));
    }
    out
}

/// Whether the frame rate `num/den` fps (interval `den/num` s) lies in a
/// stepwise/continuous interval range (`u` as in `v4l2_frmivalenum`: min,
/// max and step intervals as `numerator, denominator` pairs), and, for a
/// stepwise range, on its step grid. Exact fraction arithmetic.
fn rate_in_range(type_: u32, u: [u32; 6], num: u32, den: u32) -> bool {
    let [min_n, min_d, max_n, max_d, step_n, step_d] = u.map(u128::from);
    let (num, den) = (u128::from(num), u128::from(den));
    if num == 0 || min_d == 0 || max_d == 0 {
        return false;
    }
    // min_n/min_d <= den/num <= max_n/max_d
    if den * min_d < min_n * num || den * max_d > max_n * num {
        return false;
    }
    if type_ != TYPE_STEPWISE || step_n == 0 || step_d == 0 {
        return true;
    }
    // (den/num - min_n/min_d) / (step_n/step_d) is a whole number.
    let offset = den * min_d - min_n * num; // >= 0 by the range check
    (offset * step_d).is_multiple_of(num * min_d * step_n)
}

/// The rates (`fps_num, fps_den`) a stepwise/continuous interval range
/// contributes: its fastest rate, the common rates inside it, `want` if
/// inside it, and its slowest rate.
fn rates_in_range(type_: u32, u: [u32; 6], want: Option<(u32, u32)>) -> Vec<(u32, u32)> {
    let [min_n, min_d, max_n, max_d, _, _] = u;
    if min_n == 0 || max_n == 0 {
        return Vec::new();
    }
    let mut out = vec![(min_d, min_n)];
    let candidates = COMMON_RATES.into_iter().map(|r| (r, 1)).chain(want);
    for (num, den) in candidates.filter(|&(n, d)| rate_in_range(type_, u, n, d)) {
        if !out.contains(&(num, den)) {
            out.push((num, den));
        }
    }
    if !out.contains(&(max_d, max_n)) {
        out.push((max_d, max_n));
    }
    out
}

/// Discrete sizes as listed; a stepwise/continuous range contributes
/// [`sizes_in_range`].
fn frame_sizes(fd: libc::c_int, pixel_format: u32, want: Option<(u32, u32)>) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for index in 0..MAX_ENUM {
        let mut e = V4l2FrmSizeEnum {
            index,
            pixel_format,
            type_: 0,
            u: [0; 6],
            reserved: [0; 2],
        };
        // SAFETY: VIDIOC_ENUM_FRAMESIZES takes a `struct v4l2_frmsizeenum`.
        if !unsafe { ioctl_rw(fd, VIDIOC_ENUM_FRAMESIZES, &mut e) } {
            break;
        }
        if e.type_ == TYPE_DISCRETE {
            out.push((e.u[0], e.u[1]));
            continue;
        }
        out.extend(sizes_in_range(e.u, want));
        break; // a range is the only entry
    }
    out
}

/// Discrete rates as listed (fps = 1 / interval); a stepwise/continuous range
/// contributes [`rates_in_range`].
fn frame_rates(
    fd: libc::c_int,
    pixel_format: u32,
    width: u32,
    height: u32,
    want: Option<(u32, u32)>,
) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for index in 0..MAX_ENUM {
        let mut e = V4l2FrmIvalEnum {
            index,
            pixel_format,
            width,
            height,
            type_: 0,
            u: [0; 6],
            reserved: [0; 2],
        };
        // SAFETY: VIDIOC_ENUM_FRAMEINTERVALS takes a `struct v4l2_frmivalenum`.
        if !unsafe { ioctl_rw(fd, VIDIOC_ENUM_FRAMEINTERVALS, &mut e) } {
            break;
        }
        if e.type_ == TYPE_DISCRETE {
            out.push((e.u[1], e.u[0]));
            continue;
        }
        out.extend(rates_in_range(e.type_, e.u, want));
        break;
    }
    out
}

/// Capabilities from GStreamer caps (a `DeviceMonitor` device's caps).
/// Structures with a non-system memory feature (`memory:DMABuf`) are skipped:
/// they duplicate the system-memory ones. Sizes must be fixed; rates may be a
/// fraction, a list of fractions or a range (its ends are used).
pub fn capabilities_from_gst(caps: &gst::CapsRef) -> Capabilities {
    let mut out = Capabilities::default();
    for (s, features) in caps.iter_with_features() {
        if !features.is_any() && !features.is_empty() && !features.contains("memory:SystemMemory") {
            continue;
        }
        let format_field = s.get::<&str>("format").ok();
        let Some(format) = format_from_gst(s.name(), format_field) else {
            out.push_other(format_field.unwrap_or(s.name()));
            continue;
        };
        let (Ok(w), Ok(h)) = (s.get::<i32>("width"), s.get::<i32>("height")) else {
            continue;
        };
        let (Ok(w), Ok(h)) = (u32::try_from(w), u32::try_from(h)) else {
            continue;
        };
        let Ok(rate) = s.value("framerate") else {
            continue;
        };
        let mut rates: Vec<gst::Fraction> = Vec::new();
        if let Ok(f) = rate.get::<gst::Fraction>() {
            rates.push(f);
        } else if let Ok(list) = rate.get::<gst::List>() {
            rates.extend(list.iter().filter_map(|v| v.get::<gst::Fraction>().ok()));
        } else if let Ok(range) = rate.get::<gst::FractionRange>() {
            rates.push(range.max());
            rates.push(range.min());
        }
        for f in rates {
            if let (Ok(n), Ok(d)) = (u32::try_from(f.numer()), u32::try_from(f.denom())) {
                out.push_mode(format, w, h, n, d);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn getter<'a>(m: &'a HashMap<&'a str, &'a str>) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| m.get(k).map(|v| v.to_string())
    }

    #[test]
    fn pipewire_props() {
        let m = HashMap::from([
            ("api.v4l2.path", "/dev/video4"),
            ("device.vendor.id", "0x046d"),
            ("device.product.id", "0x0944"),
        ]);
        let g = getter(&m);
        assert_eq!(v4l2_path_from_props(&g), Some(PathBuf::from("/dev/video4")));
        assert_eq!(
            usb_from_props(&g),
            Some(UsbId {
                vendor: 0x046d,
                product: 0x0944
            })
        );
    }

    #[test]
    fn v4l2_provider_props() {
        let m = HashMap::from([
            ("device.path", "/dev/video7"),
            ("device.vendor.id", "046d"),
            ("device.product.id", "0944"),
        ]);
        let g = getter(&m);
        assert_eq!(v4l2_path_from_props(&g), Some(PathBuf::from("/dev/video7")));
        assert!(usb_from_props(&g).is_some());
    }

    #[test]
    fn object_path_fallback_and_garbage() {
        let m = HashMap::from([("object.path", "v4l2:/dev/video9")]);
        assert_eq!(
            v4l2_path_from_props(&getter(&m)),
            Some(PathBuf::from("/dev/video9"))
        );
        let m = HashMap::from([("object.path", "libcamera:foo"), ("device.path", "nope")]);
        assert_eq!(v4l2_path_from_props(&getter(&m)), None);
        let m = HashMap::from([("device.vendor.id", "0x046d")]);
        assert_eq!(usb_from_props(&getter(&m)), None);
    }

    #[test]
    fn hex_ids() {
        assert_eq!(parse_hex_id("0x046d"), Some(0x046d));
        assert_eq!(parse_hex_id("046d\n"), Some(0x046d));
        assert_eq!(parse_hex_id(""), None);
        assert_eq!(parse_hex_id("0x12345"), None);
        assert_eq!(parse_hex_id("zz"), None);
        // Bare digits are hex, never decimal (1133 decimal would be 0x046d).
        assert_eq!(parse_hex_id("1133"), Some(0x1133));
    }

    #[test]
    fn integer_props_are_read_as_ids() {
        gst::init().unwrap();
        let s = gst::Structure::builder("props")
            .field("device.vendor.id", 0x046di32)
            .field("device.product.id", 0x0944u32)
            .field("device.path", "/dev/video4")
            .build();
        let get = |k: &str| prop_string(&s, k);
        assert_eq!(get("device.vendor.id").as_deref(), Some("0x46d"));
        assert_eq!(
            usb_from_props(&get),
            Some(UsbId {
                vendor: 0x046d,
                product: 0x0944
            })
        );
        assert_eq!(get("device.path").as_deref(), Some("/dev/video4"));
    }

    #[test]
    fn ioctl_numbers() {
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_ENUM_FMT, 0xc040_5602);
        assert_eq!(VIDIOC_ENUM_FRAMESIZES, 0xc02c_564a);
        assert_eq!(VIDIOC_ENUM_FRAMEINTERVALS, 0xc034_564b);
        assert_eq!(fourcc_string(u32::from_le_bytes(*b"MJPG")), "MJPG");
    }

    #[test]
    fn node_order_is_numeric() {
        let mut devs: Vec<DeviceInfo> = ["/dev/video10", "/dev/video4", "/dev/video0", "/dev/x"]
            .iter()
            .map(|p| DeviceInfo {
                path: PathBuf::from(p),
                card: String::new(),
                driver: None,
                usb: None,
                caps: None,
                capture: true,
                loopback: false,
                source: DiscoverySource::Synthetic,
                capabilities: None,
            })
            .collect();
        sort_by_node(&mut devs);
        let order: Vec<_> = devs
            .iter()
            .map(|d| d.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            order,
            ["/dev/video0", "/dev/video4", "/dev/video10", "/dev/x"]
        );
    }

    /// A synthetic node with a stepwise size range and a stepwise interval
    /// range: a request inside both, on both grids, is selectable as exact;
    /// one off a grid is not listed (and degrades).
    #[test]
    fn stepwise_ranges_list_the_requested_mode() {
        use crate::capability::choose_mode;
        let fourcc = u32::from_le_bytes(*b"NV12");
        // 320..=1920 step 16 x 240..=1080 step 8.
        let sizes = [320, 1920, 16, 240, 1080, 8];
        // Intervals 1/60 s ..= 1/5 s in steps of 1/1200 s.
        let ivals = [1, 60, 1, 5, 1, 1200];
        let enumerate = |want: &ModeRequest| {
            collect_capabilities(
                |i| (i == 0).then_some(fourcc),
                |_| {
                    let mut out = Vec::new();
                    for (w, h) in sizes_in_range(sizes, Some((want.width, want.height))) {
                        for (n, d) in
                            rates_in_range(TYPE_STEPWISE, ivals, Some((want.fps_num, want.fps_den)))
                        {
                            out.push((w, h, n, d));
                        }
                    }
                    out
                },
            )
            .unwrap()
        };
        let req = |s: &str| s.parse::<ModeRequest>().unwrap();

        // 1904 = 320 + 99*16, 1072 = 240 + 104*8; 1/48 = 1/60 + 5/1200.
        let r = req("NV12:1904x1072@48");
        let got = choose_mode(&enumerate(&r), &r).unwrap();
        assert!(got.exact, "{got}");
        assert_eq!((got.mode.width, got.mode.height), (1904, 1072));
        assert_eq!((got.mode.fps_num, got.mode.fps_den), (48, 1));

        // 1900 is off the 16-pixel grid: not listed, so not exact.
        let r = req("NV12:1900x1072@48");
        let got = choose_mode(&enumerate(&r), &r).unwrap();
        assert!(!got.exact, "{got}");
        assert_ne!(got.mode.width, 1900);

        // 45 fps (1/45 = 1/60 + 6.67/1200) is off the interval grid.
        let r = req("NV12:1904x1072@45");
        let got = choose_mode(&enumerate(&r), &r).unwrap();
        assert!(!got.exact, "{got}");
        // Outside the range altogether.
        let r = req("NV12:1904x1072@120");
        assert!(!choose_mode(&enumerate(&r), &r).unwrap().exact);
        let r = req("NV12:3840x2160@30");
        assert!(!choose_mode(&enumerate(&r), &r).unwrap().exact);
    }

    #[test]
    fn range_membership_is_exact() {
        // Continuous intervals: anything inside, no grid.
        let cont = [1, 30, 1, 1, 1, 1];
        assert!(rate_in_range(2, cont, 30000, 1001));
        assert!(rate_in_range(2, cont, 7, 3));
        assert!(!rate_in_range(2, cont, 31, 1));
        // Stepwise 1/30..=1/10 step 1/30: intervals 1/30, 2/30, 3/30.
        let step = [1, 30, 1, 10, 1, 30];
        assert!(rate_in_range(TYPE_STEPWISE, step, 30, 1));
        assert!(rate_in_range(TYPE_STEPWISE, step, 15, 1));
        assert!(rate_in_range(TYPE_STEPWISE, step, 10, 1));
        assert!(!rate_in_range(TYPE_STEPWISE, step, 20, 1));
        assert!(!rate_in_range(TYPE_STEPWISE, step, 0, 1));
        // Common rates obey the grid too; the ends are always listed.
        assert_eq!(
            rates_in_range(TYPE_STEPWISE, step, None),
            vec![(30, 1), (15, 1), (10, 1)]
        );
        assert!(on_size_grid(1280, 16, 4096, 16));
        assert!(!on_size_grid(1281, 16, 4096, 16));
        assert!(on_size_grid(1281, 16, 4096, 1));
        assert!(!on_size_grid(8, 16, 4096, 1));
    }

    /// `VIDIOC_ENUM_FMT` failing at index 0 means "not enumerated" (`None`,
    /// so the monitor's caps are used), not "offers nothing".
    #[test]
    fn enum_fmt_failure_at_first_index_is_none() {
        assert_eq!(collect_capabilities(|_| None, |_| Vec::new()), None);
        let yuyv = u32::from_le_bytes(*b"YUYV");
        let grey = u32::from_le_bytes(*b"GREY");
        let caps = collect_capabilities(
            |i| [yuyv, grey].get(i as usize).copied(),
            |code| {
                assert_eq!(code, yuyv, "modes are only listed for ingestible formats");
                vec![(640, 480, 30, 1)]
            },
        )
        .expect("enumerated");
        assert_eq!(caps.modes.len(), 1);
        assert_eq!(caps.other_formats, vec!["GREY".to_string()]);
        // A node whose only format is not ingestible: enumerated, not colour.
        let ir = collect_capabilities(|i| (i == 0).then_some(grey), |_| Vec::new()).unwrap();
        assert!(!ir.is_colour());
    }

    /// A `/dev/v4l/by-id/...`-style link resolves to the node it points at;
    /// a path that does not exist is kept as given.
    #[test]
    fn stable_links_resolve_to_the_node() {
        let dir = std::env::temp_dir().join(format!("obc-by-id-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("v4l/by-id")).unwrap();
        let node = dir.join("video4");
        fs::write(&node, b"").unwrap();
        let link = dir.join("v4l/by-id/usb-Logitech_MX_Brio-video-index0");
        std::os::unix::fs::symlink("../../video4", &link).unwrap();
        let resolved = resolve_node_path(&link);
        let missing = dir.join("v4l/by-id/usb-gone-video-index0");
        let kept = resolve_node_path(&missing);
        let canonical_node = fs::canonicalize(&node).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(resolved, canonical_node);
        assert_eq!(resolved.file_name().unwrap(), "video4");
        assert_eq!(kept, missing);
    }

    /// The caps lines of a captured `gst-device-monitor-1.0` listing.
    fn monitor_caps(text: &str) -> gst::Caps {
        let mut caps = gst::Caps::new_empty();
        let mut in_caps = false;
        for line in text.lines() {
            let t = line.trim();
            if let Some(first) = t.strip_prefix("caps  :") {
                in_caps = true;
                caps.merge(first.trim().parse::<gst::Caps>().expect(first));
            } else if t.starts_with("properties:") {
                in_caps = false;
            } else if in_caps && !t.is_empty() {
                caps.merge(t.parse::<gst::Caps>().expect(t));
            }
        }
        caps
    }

    /// Both captured views of each node (V4L2 enumeration via v4l2-ctl, and
    /// GStreamer's PipeWire provider) yield the same capability model.
    #[test]
    fn monitor_caps_and_v4l2_enumeration_agree() {
        use crate::capability::parse_v4l2_ctl;
        gst::init().unwrap();
        for (ctl, mon) in [
            (
                include_str!("../tests/data/mx-brio-046d-0944.v4l2-ctl.txt"),
                include_str!("../tests/data/mx-brio-046d-0944.gst-device-monitor.txt"),
            ),
            (
                include_str!("../tests/data/fhd-camera-5986-1193-colour.v4l2-ctl.txt"),
                include_str!("../tests/data/fhd-camera-5986-1193-colour.gst-device-monitor.txt"),
            ),
            (
                include_str!("../tests/data/fhd-camera-5986-1193-ir.v4l2-ctl.txt"),
                include_str!("../tests/data/fhd-camera-5986-1193-ir.gst-device-monitor.txt"),
            ),
        ] {
            let a = parse_v4l2_ctl(ctl);
            let b = capabilities_from_gst(&monitor_caps(mon));
            let sorted = |c: &Capabilities| {
                let mut m: Vec<String> = c.modes.iter().map(|m| m.to_string()).collect();
                m.sort();
                m
            };
            assert_eq!(sorted(&a), sorted(&b));
            assert_eq!(a.is_colour(), b.is_colour());
        }
    }
}
