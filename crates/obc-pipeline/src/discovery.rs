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
    Discovery::system().scan(want)
}

/// Builds a [`DeviceInfo`] for one node from sysfs and, for non-loopback
/// nodes, `VIDIOC_QUERYCAP`. Returns `None` if the node has no sysfs entry.
/// `want` as for [`list_cameras`].
pub fn probe_node(path: &Path, want: Option<&ModeRequest>) -> Option<DeviceInfo> {
    Discovery::system().probe(path, want)
}

/// Where the sysfs scan reads the video4linux class and finds device nodes,
/// and what answers the node queries. [`Discovery::system`] is the live
/// system; tests replay captured cameras through a fake tree and
/// [`V4l2Queries`].
pub(crate) struct Discovery<Q> {
    /// The video4linux class directory (`/sys/class/video4linux`).
    pub(crate) sysfs_v4l: PathBuf,
    /// The directory holding the device nodes (`/dev`).
    pub(crate) dev: PathBuf,
    pub(crate) nodes: Q,
}

impl Discovery<SystemNodes> {
    /// The live system: `/sys/class/video4linux`, `/dev` and real ioctls.
    pub(crate) fn system() -> Self {
        Discovery {
            sysfs_v4l: PathBuf::from(SYSFS_V4L),
            dev: PathBuf::from("/dev"),
            nodes: SystemNodes,
        }
    }
}

impl<Q: V4l2Queries> Discovery<Q> {
    /// Probes every `videoN` entry of the class directory, in node order.
    pub(crate) fn scan(&self, want: Option<&ModeRequest>) -> Vec<DeviceInfo> {
        let Ok(entries) = fs::read_dir(&self.sysfs_v4l) else {
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
            .filter_map(|n| self.probe(&self.dev.join(n), want))
            .collect()
    }

    /// [`probe_node`] against this tree and these queries.
    pub(crate) fn probe(&self, path: &Path, want: Option<&ModeRequest>) -> Option<DeviceInfo> {
        let node = path.file_name()?.to_str()?;
        let class_dir = self.sysfs_v4l.join(node);
        let name = fs::read_to_string(class_dir.join("name")).ok()?;
        let real = fs::canonicalize(&class_dir).ok();
        let is_virtual = real
            .as_ref()
            .is_some_and(|r| r.components().any(|c| c.as_os_str() == "virtual"));
        let usb = real.as_deref().and_then(usb_id_from_sysfs);

        // Never open a loopback node to identify it: an open counts as an
        // opener (and, with exclusive_caps, flips what the node advertises).
        let qc = if is_virtual {
            None
        } else {
            self.nodes.query_cap(path)
        };
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
            Some(c) if capture && !loopback => enumerate_with(&self.nodes, path, c, want),
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
}

/// The queries discovery issues on a device node. [`SystemNodes`] issues the
/// real ioctls; every call leaves the node as it was (nothing is set and
/// nothing streams).
pub(crate) trait V4l2Queries {
    /// A node opened for format enumeration (closed on drop).
    type Node: V4l2Enum;
    /// `VIDIOC_QUERYCAP` (open, query, close); `None` if either fails.
    fn query_cap(&self, path: &Path) -> Option<QueryCap>;
    /// Opens the node for [`V4l2Enum`]; `None` if it cannot be opened.
    fn open(&self, path: &Path) -> Option<Self::Node>;
}

/// The enumeration ioctls on one open node. Each returns `None` once the
/// driver rejects the index (`EINVAL` ends every enumeration).
pub(crate) trait V4l2Enum {
    /// `VIDIOC_ENUM_FMT`: the fourcc code at `index` for buffer type `buf_type`.
    fn enum_fmt(&self, buf_type: u32, index: u32) -> Option<u32>;
    /// `VIDIOC_ENUM_FRAMESIZES`: the entry type and its union (discrete:
    /// `width, height`; stepwise/continuous: `min_w, max_w, step_w, min_h,
    /// max_h, step_h`).
    fn enum_frame_sizes(&self, pixel_format: u32, index: u32) -> Option<(u32, [u32; 6])>;
    /// `VIDIOC_ENUM_FRAMEINTERVALS`: the entry type and its union (discrete:
    /// the interval `num, den`; stepwise/continuous: `min, max, step` as
    /// `num, den` pairs).
    fn enum_frame_intervals(
        &self,
        pixel_format: u32,
        width: u32,
        height: u32,
        index: u32,
    ) -> Option<(u32, [u32; 6])>;
}

/// The live system's node queries: real V4L2 ioctls.
pub(crate) struct SystemNodes;

/// A node opened read-only and non-blocking.
pub(crate) struct OpenNode(fs::File);

fn open_node(path: &Path) -> Option<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .ok()
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
    SystemNodes.query_cap(path)
}

impl V4l2Queries for SystemNodes {
    type Node = OpenNode;

    fn query_cap(&self, path: &Path) -> Option<QueryCap> {
        let file = open_node(path)?;
        let mut cap = V4l2Capability {
            driver: [0; 16],
            card: [0; 32],
            bus_info: [0; 32],
            version: 0,
            capabilities: 0,
            device_caps: 0,
            reserved: [0; 3],
        };
        // SAFETY: `cap` is a properly sized and aligned `struct
        // v4l2_capability` that outlives the call, and the fd is open for
        // the duration.
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

    fn open(&self, path: &Path) -> Option<OpenNode> {
        open_node(path).map(OpenNode)
    }
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

impl V4l2Enum for OpenNode {
    fn enum_fmt(&self, buf_type: u32, index: u32) -> Option<u32> {
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
        unsafe { ioctl_rw(self.0.as_raw_fd(), VIDIOC_ENUM_FMT, &mut desc) }
            .then_some(desc.pixelformat)
    }

    fn enum_frame_sizes(&self, pixel_format: u32, index: u32) -> Option<(u32, [u32; 6])> {
        let mut e = V4l2FrmSizeEnum {
            index,
            pixel_format,
            type_: 0,
            u: [0; 6],
            reserved: [0; 2],
        };
        // SAFETY: VIDIOC_ENUM_FRAMESIZES takes a `struct v4l2_frmsizeenum`.
        unsafe { ioctl_rw(self.0.as_raw_fd(), VIDIOC_ENUM_FRAMESIZES, &mut e) }
            .then_some((e.type_, e.u))
    }

    fn enum_frame_intervals(
        &self,
        pixel_format: u32,
        width: u32,
        height: u32,
        index: u32,
    ) -> Option<(u32, [u32; 6])> {
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
        unsafe { ioctl_rw(self.0.as_raw_fd(), VIDIOC_ENUM_FRAMEINTERVALS, &mut e) }
            .then_some((e.type_, e.u))
    }
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
    enumerate_with(&SystemNodes, path, device_caps, want)
}

/// [`enumerate_formats`] through `nodes`.
fn enumerate_with(
    nodes: &impl V4l2Queries,
    path: &Path,
    device_caps: u32,
    want: Option<&ModeRequest>,
) -> Option<Capabilities> {
    let node = nodes.open(path)?;
    let buf_type = if device_caps & crate::device::CAP_VIDEO_CAPTURE != 0 {
        BUF_TYPE_VIDEO_CAPTURE
    } else {
        BUF_TYPE_VIDEO_CAPTURE_MPLANE
    };
    let want_size = want.map(|r| (r.width, r.height));
    let want_rate = want.map(|r| (r.fps_num, r.fps_den));
    collect_capabilities(
        |index| node.enum_fmt(buf_type, index),
        |pixel_format| {
            let mut out = Vec::new();
            for (w, h) in frame_sizes(&node, pixel_format, want_size) {
                for (num, den) in frame_rates(&node, pixel_format, w, h, want_rate) {
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
fn frame_sizes(
    node: &impl V4l2Enum,
    pixel_format: u32,
    want: Option<(u32, u32)>,
) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for index in 0..MAX_ENUM {
        let Some((type_, u)) = node.enum_frame_sizes(pixel_format, index) else {
            break;
        };
        if type_ == TYPE_DISCRETE {
            out.push((u[0], u[1]));
            continue;
        }
        out.extend(sizes_in_range(u, want));
        break; // a range is the only entry
    }
    out
}

/// Discrete rates as listed (fps = 1 / interval); a stepwise/continuous range
/// contributes [`rates_in_range`].
fn frame_rates(
    node: &impl V4l2Enum,
    pixel_format: u32,
    width: u32,
    height: u32,
    want: Option<(u32, u32)>,
) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for index in 0..MAX_ENUM {
        let Some((type_, u)) = node.enum_frame_intervals(pixel_format, width, height, index) else {
            break;
        };
        if type_ == TYPE_DISCRETE {
            out.push((u[1], u[0]));
            continue;
        }
        out.extend(rates_in_range(type_, u, want));
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

    /// Fixture replay of the sysfs scan: a fake video4linux tree under the
    /// temp dir and fake node queries that answer `VIDIOC_QUERYCAP` from
    /// `tests/data/v4l2-querycap.txt` and the enumeration ioctls from
    /// `v4l2-ctl --list-formats-ext` dumps.
    mod replay {
        use super::*;
        use crate::capability::{IngestPath, choose_mode, fraction_from_decimal, parse_v4l2_ctl};
        use crate::device::{LOOPBACK_DRIVER, select_camera, select_sink};
        use obc_core::{CameraSelector, PixelFormat, SinkSelector, VideoMode};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::sync::atomic::{AtomicU32, Ordering};

        const BRIO: UsbId = UsbId {
            vendor: 0x046d,
            product: 0x0944,
        };
        const FHD: UsbId = UsbId {
            vendor: 0x5986,
            product: 0x1193,
        };
        const YUYV_ONLY: UsbId = UsbId {
            vendor: 0x1209,
            product: 0x0001,
        };
        const BRIO_CTL: &str = "mx-brio-046d-0944.v4l2-ctl.txt";
        const FHD_CTL: &str = "fhd-camera-5986-1193-colour.v4l2-ctl.txt";
        const FHD_IR_CTL: &str = "fhd-camera-5986-1193-ir.v4l2-ctl.txt";
        const YUYV_ONLY_CTL: &str = "synthetic-yuyv-only-1209-0001.v4l2-ctl.txt";

        fn data(name: &str) -> String {
            let p = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/data")
                .join(name);
            fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
        }

        /// One `Size:` entry of a dump with the `Interval:` entries under it,
        /// as the ioctls report them (type and union).
        struct SizeEntry {
            width: u32,
            height: u32,
            intervals: Vec<(u32, [u32; 6])>,
        }

        /// One `[N]: 'FOURCC'` entry of a dump.
        struct FormatEntry {
            code: u32,
            sizes: Vec<SizeEntry>,
        }

        fn fourcc_code(s: &str) -> u32 {
            let mut b = [b' '; 4];
            for (d, c) in b.iter_mut().zip(s.bytes()) {
                *d = c;
            }
            u32::from_le_bytes(b)
        }

        /// The enumeration ioctls' answers recorded in a `v4l2-ctl
        /// --list-formats-ext` dump: every format (ingestible or not), its
        /// discrete sizes and their discrete intervals. The interval
        /// fraction is recovered from the printed frame rate. The dumps in
        /// `tests/data` hold only discrete entries; any other entry fails
        /// the test rather than replay something the dump does not state.
        fn ioctl_answers(text: &str) -> Vec<FormatEntry> {
            let mut formats: Vec<FormatEntry> = Vec::new();
            for line in text.lines() {
                let t = line.trim();
                if t.starts_with('[') {
                    let fourcc = t.split('\'').nth(1).expect(t);
                    formats.push(FormatEntry {
                        code: fourcc_code(fourcc),
                        sizes: Vec::new(),
                    });
                } else if let Some(rest) = t.strip_prefix("Size: Discrete ") {
                    let (w, h) = rest.split_once('x').expect(t);
                    formats.last_mut().expect(t).sizes.push(SizeEntry {
                        width: w.trim().parse().expect(t),
                        height: h.trim().parse().expect(t),
                        intervals: Vec::new(),
                    });
                } else if let Some(rest) = t.strip_prefix("Interval: Discrete ") {
                    let fps: f64 = rest
                        .split_once('(')
                        .and_then(|(_, r)| r.split_whitespace().next())
                        .and_then(|v| v.parse().ok())
                        .expect(t);
                    let (num, den) = fraction_from_decimal(fps).expect(t);
                    let size = formats.last_mut().and_then(|f| f.sizes.last_mut());
                    // The interval is the inverse of the rate.
                    size.expect(t)
                        .intervals
                        .push((TYPE_DISCRETE, [den, num, 0, 0, 0, 0]));
                } else {
                    assert!(
                        !t.starts_with("Size:") && !t.starts_with("Interval:"),
                        "the replay has no answer for {t:?}"
                    );
                }
            }
            formats
        }

        /// A node opened by [`FakeNodes`], answering from its dump.
        struct FakeNode(Rc<Vec<FormatEntry>>);

        impl FakeNode {
            fn format(&self, code: u32) -> Option<&FormatEntry> {
                self.0.iter().find(|f| f.code == code)
            }
        }

        impl V4l2Enum for FakeNode {
            fn enum_fmt(&self, buf_type: u32, index: u32) -> Option<u32> {
                assert_eq!(buf_type, BUF_TYPE_VIDEO_CAPTURE);
                self.0.get(index as usize).map(|f| f.code)
            }

            fn enum_frame_sizes(&self, pixel_format: u32, index: u32) -> Option<(u32, [u32; 6])> {
                let s = self.format(pixel_format)?.sizes.get(index as usize)?;
                Some((TYPE_DISCRETE, [s.width, s.height, 0, 0, 0, 0]))
            }

            fn enum_frame_intervals(
                &self,
                pixel_format: u32,
                width: u32,
                height: u32,
                index: u32,
            ) -> Option<(u32, [u32; 6])> {
                self.format(pixel_format)?
                    .sizes
                    .iter()
                    .find(|s| (s.width, s.height) == (width, height))?
                    .intervals
                    .get(index as usize)
                    .copied()
            }
        }

        /// What one fake `/dev/videoN` answers.
        struct Reply {
            qc: QueryCap,
            /// The enumeration answers; `None` for a node without video capture.
            formats: Option<Rc<Vec<FormatEntry>>>,
        }

        /// Node queries answered from fixtures; every query and open is
        /// recorded, so a test can tell which nodes were touched.
        #[derive(Default)]
        struct FakeNodes {
            replies: HashMap<PathBuf, Reply>,
            queried: RefCell<Vec<PathBuf>>,
            opened: RefCell<Vec<PathBuf>>,
        }

        impl V4l2Queries for FakeNodes {
            type Node = FakeNode;

            fn query_cap(&self, path: &Path) -> Option<QueryCap> {
                self.queried.borrow_mut().push(path.to_path_buf());
                self.replies.get(path).map(|r| r.qc.clone())
            }

            fn open(&self, path: &Path) -> Option<FakeNode> {
                self.opened.borrow_mut().push(path.to_path_buf());
                let formats = self.replies.get(path)?.formats.clone()?;
                Some(FakeNode(formats))
            }
        }

        /// One row of `tests/data/v4l2-querycap.txt`.
        struct NodeRow {
            qc: QueryCap,
            formats: Option<String>,
        }

        /// The nodes of the USB camera `usb`, in node order.
        fn querycap_rows(usb: UsbId) -> Vec<NodeRow> {
            let key = usb.to_string();
            let rows: Vec<NodeRow> = data("v4l2-querycap.txt")
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .filter_map(|l| {
                    let f: Vec<&str> = l.split('|').map(str::trim).collect();
                    assert_eq!(f.len(), 7, "{l}");
                    (f[0] == key).then(|| {
                        let hex = |s: &str| u32::from_str_radix(&s[2..], 16).expect(l);
                        NodeRow {
                            qc: QueryCap {
                                driver: f[1].into(),
                                card: f[2].into(),
                                bus_info: f[3].into(),
                                caps: DeviceInfo::effective_caps(hex(f[4]), hex(f[5])),
                            },
                            formats: (f[6] != "-").then(|| f[6].to_string()),
                        }
                    })
                })
                .collect();
            assert!(!rows.is_empty(), "no QUERYCAP rows for {key}");
            rows
        }

        /// A video4linux sysfs tree in a fresh temp dir, laid out as the
        /// kernel does: `class/video4linux/videoN` links to the node's
        /// device directory, below its USB interface and USB device (which
        /// carries `idVendor`/`idProduct`, as does the root hub above it),
        /// or below `devices/virtual` for v4l2loopback. Removed on drop.
        struct FakeSysfs {
            root: PathBuf,
        }

        const HUB: &str = "devices/pci0000:00/0000:00:14.0/usb3";

        impl FakeSysfs {
            fn new() -> Self {
                static NEXT: AtomicU32 = AtomicU32::new(0);
                let root = std::env::temp_dir().join(format!(
                    "obc-sysfs-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                let _ = fs::remove_dir_all(&root);
                let t = FakeSysfs { root };
                fs::create_dir_all(t.class()).unwrap();
                fs::create_dir_all(t.dev()).unwrap();
                t.ids(HUB, 0x1d6b, 0x0003);
                t
            }

            fn class(&self) -> PathBuf {
                self.root.join("sys/class/video4linux")
            }

            fn dev(&self) -> PathBuf {
                self.root.join("dev")
            }

            fn ids(&self, rel: &str, vendor: u16, product: u16) {
                let d = self.root.join("sys").join(rel);
                fs::create_dir_all(&d).unwrap();
                fs::write(d.join("idVendor"), format!("{vendor:04x}\n")).unwrap();
                fs::write(d.join("idProduct"), format!("{product:04x}\n")).unwrap();
            }

            /// A class entry `node` for the device dir `sys/<rel>`.
            fn entry(&self, node: &str, rel: &str, name: &str) -> PathBuf {
                let real = self.root.join("sys").join(rel);
                fs::create_dir_all(&real).unwrap();
                fs::write(real.join("name"), format!("{name}\n")).unwrap();
                std::os::unix::fs::symlink(format!("../../{rel}"), self.class().join(node))
                    .unwrap();
                real
            }

            /// A node of the USB device on hub port `port`.
            fn usb_node(&self, n: u32, port: u32, usb: UsbId, name: &str) {
                let dev = format!("{HUB}/3-{port}");
                self.ids(&dev, usb.vendor, usb.product);
                self.entry(
                    &format!("video{n}"),
                    &format!("{dev}/3-{port}:1.0/video4linux/video{n}"),
                    name,
                );
            }

            fn loopback_node(&self, n: u32, label: &str) {
                let real = self.entry(
                    &format!("video{n}"),
                    &format!("devices/virtual/video4linux/video{n}"),
                    label,
                );
                fs::write(real.join("max_openers"), "10\n").unwrap();
                fs::write(real.join("buffers"), "2\n").unwrap();
            }
        }

        impl Drop for FakeSysfs {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.root);
            }
        }

        /// A USB camera of the QUERYCAP table, on hub port `port`, whose
        /// nodes start at `/dev/video<first>`.
        struct Camera {
            usb: UsbId,
            port: u32,
            first: u32,
        }

        /// Builds the tree and the fake queries for `cameras` and one
        /// loopback node per `(N, label)`.
        fn machine(
            cameras: &[Camera],
            loopbacks: &[(u32, &str)],
        ) -> (FakeSysfs, Discovery<FakeNodes>) {
            let sysfs = FakeSysfs::new();
            let mut nodes = FakeNodes::default();
            for cam in cameras {
                for (i, row) in querycap_rows(cam.usb).into_iter().enumerate() {
                    let n = cam.first + i as u32;
                    sysfs.usb_node(n, cam.port, cam.usb, &row.qc.card);
                    let formats = row.formats.map(|f| Rc::new(ioctl_answers(&data(&f))));
                    nodes.replies.insert(
                        sysfs.dev().join(format!("video{n}")),
                        Reply {
                            qc: row.qc,
                            formats,
                        },
                    );
                }
            }
            for &(n, label) in loopbacks {
                sysfs.loopback_node(n, label);
            }
            let discovery = Discovery {
                sysfs_v4l: sysfs.class(),
                dev: sysfs.dev(),
                nodes,
            };
            (sysfs, discovery)
        }

        /// The laptop of the captured fixtures: the built-in camera (colour,
        /// metadata, IR, metadata nodes) at video0..3, the external camera
        /// (colour, metadata) at video4..5, a loopback at video10, and a
        /// sub-device entry that is not a video node.
        fn laptop() -> (FakeSysfs, Discovery<FakeNodes>) {
            let (sysfs, d) = machine(
                &[
                    Camera {
                        usb: FHD,
                        port: 6,
                        first: 0,
                    },
                    Camera {
                        usb: BRIO,
                        port: 2,
                        first: 4,
                    },
                ],
                &[(10, "Omarchy Broadcast")],
            );
            sysfs.entry(
                "v4l-subdev0",
                "devices/platform/sensor/video4linux/v4l-subdev0",
                "sensor",
            );
            (sysfs, d)
        }

        fn req(s: &str) -> ModeRequest {
            s.parse().unwrap()
        }

        fn mode(s: &str) -> VideoMode {
            s.parse().unwrap()
        }

        fn sel(s: &str) -> CameraSelector {
            s.parse().unwrap()
        }

        /// The sysfs scan over a laptop with a colour+IR built-in camera, an
        /// external camera and a loopback yields exactly the hand-built
        /// device lists of the selection tests: QUERYCAP data for real
        /// nodes, sysfs data for the loopback, which is never queried, and
        /// capabilities equal to the parsed dumps.
        #[test]
        fn sysfs_scan_replays_cameras_and_skips_the_loopback() {
            let (sysfs, d) = laptop();
            let devs = d.scan(None);
            let dev = |n: u32| sysfs.dev().join(format!("video{n}"));
            let paths: Vec<PathBuf> = devs.iter().map(|x| x.path.clone()).collect();
            assert_eq!(paths, [0, 1, 2, 3, 4, 5, 10].map(dev));

            let cards: Vec<&str> = devs.iter().map(|x| x.card.as_str()).collect();
            assert_eq!(
                cards,
                [
                    "FHD Camera: FHD Camera",
                    "FHD Camera: FHD Camera",
                    "FHD Camera: FHD IR Camera",
                    "FHD Camera: FHD IR Camera",
                    "MX Brio",
                    "MX Brio",
                    "Omarchy Broadcast",
                ]
            );
            let usb: Vec<Option<UsbId>> = devs.iter().map(|x| x.usb).collect();
            let f = Some(FHD);
            let b = Some(BRIO);
            assert_eq!(usb, [f, f, f, f, b, b, None]);
            let capture: Vec<bool> = devs.iter().map(|x| x.capture).collect();
            assert_eq!(capture, [true, false, true, false, true, false, false]);
            let loopback: Vec<bool> = devs.iter().map(|x| x.loopback).collect();
            assert_eq!(loopback, [false, false, false, false, false, false, true]);
            for x in &devs {
                assert_eq!(x.source, DiscoverySource::Sysfs);
                let driver = if x.loopback {
                    LOOPBACK_DRIVER
                } else {
                    "uvcvideo"
                };
                assert_eq!(x.driver.as_deref(), Some(driver), "{x}");
            }
            assert_eq!(devs[0].caps, Some(0x0420_0001));
            assert_eq!(devs[1].caps, Some(0x0480_0000));
            assert_eq!(devs[6].caps, None);

            let parsed = |name: &str| Some(parse_v4l2_ctl(&data(name)));
            let caps: Vec<Option<Capabilities>> =
                devs.iter().map(|x| x.capabilities.clone()).collect();
            assert_eq!(
                caps,
                [
                    parsed(FHD_CTL),
                    None,
                    parsed(FHD_IR_CTL),
                    None,
                    parsed(BRIO_CTL),
                    None,
                    None,
                ]
            );

            // QUERYCAP on every real node, never on the loopback (nor on the
            // sub-device); only video-capture nodes are opened to enumerate.
            assert_eq!(*d.nodes.queried.borrow(), [0, 1, 2, 3, 4, 5].map(dev));
            assert_eq!(*d.nodes.opened.borrow(), [0, 2, 4].map(dev));

            // The IR node is a capture node but not a colour camera.
            assert!(devs[2].capture && !devs[2].is_colour_camera());
            assert!(devs[0].is_colour_camera() && devs[4].is_colour_camera());
        }

        /// Selection and mode choice on the replayed list match the tests
        /// over hand-built lists and parsed dumps.
        #[test]
        fn selection_on_replayed_devices_picks_the_colour_nodes() {
            let (sysfs, d) = laptop();
            let devs = d.scan(None);
            let dev = |n: u32| sysfs.dev().join(format!("video{n}"));

            // A vid:pid or a name that also matches the IR node picks the colour node.
            for s in ["5986:1193", "FHD Camera"] {
                let got = select_camera(&devs, Some(&sel(s)), &req("1920x1080@60")).unwrap();
                assert_eq!(got.device.path, dev(0), "{s}");
                assert_eq!(got.choice.mode, mode("MJPEG:1920x1080@30"));
                assert!(!got.choice.exact);
                assert_eq!(got.choice.ingest, IngestPath::CpuDecode);
                assert_eq!(got.choice.differences, vec!["fps 60 -> 30".to_string()]);
            }
            for s in ["046d:0944", "brio"] {
                let got = select_camera(&devs, Some(&sel(s)), &req("NV12:1920x1080@60")).unwrap();
                assert_eq!(got.device.path, dev(4), "{s}");
                assert_eq!(got.choice.mode, mode("NV12:1920x1080@60"));
                assert!(got.choice.exact);
                assert_eq!(got.choice.ingest, IngestPath::GpuDirect);
            }
            let caps = devs[4].capabilities.as_ref().unwrap();
            let got = choose_mode(caps, &req("3840x2160@30")).unwrap();
            assert_eq!(got.mode, mode("MJPEG:3840x2160@30"));
            assert_eq!(got.ingest, IngestPath::CpuDecode);
            let caps = devs[0].capabilities.as_ref().unwrap();
            assert_eq!(
                choose_mode(caps, &req("640x360@30")).unwrap().mode,
                mode("YUY2:640x360@30")
            );

            // Auto pick: the first camera that is exact.
            let got = select_camera(&devs, None, &req("1920x1080@60")).unwrap();
            assert_eq!(got.device.path, dev(4));
            let got = select_camera(&devs, None, &req("1280x720@30")).unwrap();
            assert_eq!(got.device.path, dev(0));

            // The IR node, named explicitly, is an error, not a fallback.
            let err = select_camera(&devs, Some(&sel("FHD IR")), &req("640x360@15")).unwrap_err();
            assert!(err.to_string().contains("GREY"), "{err}");

            // The loopback, read from sysfs alone, is the sink.
            let sink = select_sink(&devs, &SinkSelector::default()).unwrap();
            assert_eq!(sink.path, dev(10));
        }

        /// Probing a node directly (an explicit `/dev/videoN`) follows the
        /// same rules: a loopback is identified without being opened, and a
        /// node without a sysfs entry is unknown.
        #[test]
        fn direct_probe_identifies_a_loopback_without_opening_it() {
            let (sysfs, d) = laptop();
            let lb = d.probe(&sysfs.dev().join("video10"), None).unwrap();
            assert!(lb.loopback && !lb.capture);
            assert_eq!(lb.card, "Omarchy Broadcast");
            assert_eq!(lb.usb, None);
            assert_eq!(lb.capabilities, None);
            assert!(d.probe(&sysfs.dev().join("video99"), None).is_none());
            assert!(d.nodes.queried.borrow().is_empty());
            assert!(d.nodes.opened.borrow().is_empty());

            // A virtual node without the loopback attributes is not a loopback.
            sysfs.entry("video11", "devices/virtual/video4linux/video11", "vivid");
            let other = d.probe(&sysfs.dev().join("video11"), None).unwrap();
            assert!(!other.loopback && !other.capture);
            assert_eq!(other.driver, None);
            assert!(d.nodes.queried.borrow().is_empty());
        }

        /// A camera whose only format is discrete YUYV (no MJPEG, no NV12):
        /// discovered like any other, it keeps the requested size and drops
        /// the rate, and always takes the CPU-convert ingest path.
        #[test]
        fn yuyv_only_camera_keeps_the_size_drops_the_rate_and_converts_on_cpu() {
            let (sysfs, d) = machine(
                &[Camera {
                    usb: YUYV_ONLY,
                    port: 3,
                    first: 0,
                }],
                &[(10, "Omarchy Broadcast")],
            );
            let devs = d.scan(None);
            let dev = |n: u32| sysfs.dev().join(format!("video{n}"));
            assert_eq!(devs.len(), 3);
            let cam = &devs[0];
            assert_eq!(cam.usb, Some(YUYV_ONLY));
            assert!(cam.is_colour_camera());
            assert!(!devs[1].capture);
            let caps = cam.capabilities.as_ref().unwrap();
            assert_eq!(*caps, parse_v4l2_ctl(&data(YUYV_ONLY_CTL)));
            assert_eq!(caps.formats(), vec![PixelFormat::Yuy2]);
            assert!(caps.other_formats.is_empty());
            assert!(caps.modes.contains(&mode("YUY2:640x480@15/2")));
            assert_eq!(*d.nodes.opened.borrow(), [dev(0)]);

            // 1080p60: the size is offered (at 5 fps only); size beats rate.
            let got = select_camera(&devs, None, &req("1920x1080@60")).unwrap();
            assert_eq!(got.device.path, dev(0));
            assert_eq!(got.choice.mode, mode("YUY2:1920x1080@5"));
            assert!(!got.choice.exact);
            assert_eq!(got.choice.differences, vec!["fps 60 -> 5".to_string()]);
            assert_eq!(got.choice.ingest, IngestPath::CpuConvert);
            assert_eq!(
                got.choice.ingest.upload_format(got.choice.mode.format),
                "NV12"
            );

            // A format the camera lacks degrades the format too.
            let got =
                select_camera(&devs, Some(&sel("1209:0001")), &req("NV12:1280x720@30")).unwrap();
            assert_eq!(got.choice.mode, mode("YUY2:1280x720@10"));
            assert_eq!(
                got.choice.differences,
                vec![
                    "fps 30 -> 10".to_string(),
                    "format NV12 -> YUY2".to_string()
                ]
            );

            // Where the camera reaches the rate, the choice is exact.
            let got =
                select_camera(&devs, Some(&sel("Synthetic YUYV")), &req("640x480@30")).unwrap();
            assert_eq!(got.device.path, dev(0));
            assert_eq!(got.choice.mode, mode("YUY2:640x480@30"));
            assert!(got.choice.exact);
            assert_eq!(got.choice.ingest, IngestPath::CpuConvert);
        }
    }
}
