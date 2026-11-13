//! Where a USB camera sits on the bus: its kernel path, its link speed, and
//! whether it shares a hub with keyboards, mice or other HID devices.
//!
//! A camera on a hub shares the hub's upstream link with everything else
//! below it. A raw (uncompressed) mode needs a fixed bandwidth
//! ([`raw_bytes_per_second`]): YUYV 640x480 at 30 fps is 18.4 MB/s, close
//! to what isochronous transfers get from a High-Speed link in practice,
//! and the periodic traffic of the HID devices on the same hub (polled
//! every few milliseconds) competes with it for the same microframes. A
//! camera that delivers fewer frames than it was asked for
//! ([`crate::delivered_rate`]) on a shared hub is worth moving to a root
//! port; [`placement`] says whether it is on one.
//!
//! Everything comes from sysfs, by the camera node's device number
//! ([`crate::xu::usb_device_of_node`]; the root is injectable): the USB
//! device directory sits under its hubs
//! (`.../usb3/3-7/3-7.2/3-7.2.1`), its directory name is its kernel path,
//! `speed` its link speed in Mbit/s, and an interface directory below a
//! device (`3-7.2.3:1.0`) carries its `bInterfaceClass` (`03`: HID, `0e`:
//! video).

use std::fs;
use std::path::Path;

use obc_core::{PixelFormat, VideoMode};

/// The USB interface class of HID devices (keyboards, mice, receivers).
const CLASS_HID: &str = "03";
/// The USB interface class of video devices (cameras).
const CLASS_VIDEO: &str = "0e";

/// A hub above a camera with HID devices below it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedHub {
    /// The hub's kernel path (`3-7.2`).
    pub hub_path: String,
    /// The HID devices anywhere below the hub, the camera itself and other
    /// cameras excluded (a camera with a button exposes a HID interface,
    /// and is no keyboard or mouse): each by its
    /// `product` string, else as `vid:pid` in lower-case hex, ordered by
    /// kernel path.
    pub hid_devices: Vec<String>,
}

/// Where a camera sits on the USB bus ([`placement_of_node`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbPlacement {
    /// The camera's kernel path (`3-7.2.1`: bus 3, port 7, then ports 2
    /// and 1 of the hubs below it).
    pub usb_path: String,
    /// The link speed in Mbit/s (`480` High-Speed, `5000` SuperSpeed);
    /// `None` when sysfs has no whole number (Low-Speed is `1.5`).
    pub speed_mbps: Option<u32>,
    /// The nearest hub above the camera, other than the root hub, with HID
    /// devices below it; `None` when there is none, so always for a camera
    /// on a root port.
    pub shared_hub: Option<SharedHub>,
}

/// The placement of the camera behind the video node `node`, from sysfs
/// under `sysfs_root`. `None` when the node is no USB camera
/// ([`crate::xu::usb_device_of_node`]) or its directory has no name.
pub fn placement_of_node(sysfs_root: &Path, node: &Path) -> Option<UsbPlacement> {
    let usb = crate::xu::usb_device_of_node(sysfs_root, node).ok()?;
    placement_of_device(&usb.dir)
}

/// [`placement_of_node`] on the real sysfs ([`crate::xu::SYSFS_ROOT`]).
pub fn placement(node: &Path) -> Option<UsbPlacement> {
    placement_of_node(Path::new(crate::xu::SYSFS_ROOT), node)
}

/// The bytes per second a camera mode moves over USB, for a raw format:
/// width x height x bytes per pixel (4:2:2 packed 2, 4:2:0 1.5, RGB 3) x
/// the frame rate. `None` for MJPEG, whose size depends on the picture,
/// and for a mode without a rate.
pub fn raw_bytes_per_second(mode: &VideoMode) -> Option<u64> {
    // Bytes per pixel, doubled to stay whole.
    let twice_bpp: u64 = match mode.format {
        PixelFormat::Nv12 | PixelFormat::Nv21 | PixelFormat::I420 | PixelFormat::Yv12 => 3,
        PixelFormat::Yuy2 | PixelFormat::Uyvy => 4,
        PixelFormat::Rgb | PixelFormat::Bgr => 6,
        PixelFormat::Mjpeg => return None,
    };
    if mode.fps_den == 0 {
        return None;
    }
    let per_frame2 = u64::from(mode.width) * u64::from(mode.height) * twice_bpp;
    Some(per_frame2 * u64::from(mode.fps_num) / (2 * u64::from(mode.fps_den)))
}

/// The placement of the USB device whose sysfs directory is `dir`.
fn placement_of_device(dir: &Path) -> Option<UsbPlacement> {
    let usb_path = dir.file_name()?.to_str()?.to_string();
    let speed_mbps = read_trimmed(&dir.join("speed")).and_then(|s| s.parse().ok());
    let shared_hub = dir
        .ancestors()
        .skip(1)
        .take_while(|d| is_usb_device(d) && !is_root_hub(d))
        .find_map(|hub| {
            let hid_devices = hid_devices_below(hub, dir);
            (!hid_devices.is_empty()).then(|| SharedHub {
                hub_path: name_of(hub),
                hid_devices,
            })
        });
    Some(UsbPlacement {
        usb_path,
        speed_mbps,
        shared_hub,
    })
}

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn name_of(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// A USB device directory (not an interface: its name has no `:`).
fn is_usb_device(dir: &Path) -> bool {
    !name_of(dir).contains(':') && dir.join("idVendor").is_file()
}

/// A root hub (`usb3`): the bus itself, shared by everything on it.
fn is_root_hub(dir: &Path) -> bool {
    name_of(dir).starts_with("usb")
}

/// The classes of the interfaces of the USB device `dir`.
fn interface_classes(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(':'))
        .filter_map(|e| read_trimmed(&e.path().join("bInterfaceClass")))
        .collect()
}

/// Whether the USB device `dir` is a HID device: it has a HID interface
/// and no video one. A camera's HID interface (its buttons, as on the
/// C930e) does not make it a keyboard or a mouse: the polling that stalls
/// next to a heavy stream is theirs.
fn is_hid_device(dir: &Path) -> bool {
    let classes = interface_classes(dir);
    classes.iter().any(|c| c == CLASS_HID) && !classes.iter().any(|c| c == CLASS_VIDEO)
}

/// A device's label: its `product` string, else `vid:pid`.
fn label_of(dir: &Path) -> String {
    match read_trimmed(&dir.join("product")) {
        Some(p) if !p.is_empty() => p,
        _ => format!(
            "{}:{}",
            read_trimmed(&dir.join("idVendor")).unwrap_or_default(),
            read_trimmed(&dir.join("idProduct")).unwrap_or_default()
        )
        .to_lowercase(),
    }
}

/// The HID devices anywhere below the hub `hub`, `camera` and any other
/// device with a video interface excluded ([`is_hid_device`]), ordered by
/// kernel path.
fn hid_devices_below(hub: &Path, camera: &Path) -> Vec<String> {
    let mut found = Vec::new();
    collect_hid(hub, camera, &mut found);
    found.sort_by_key(|(name, _)| path_key(name));
    found.into_iter().map(|(_, label)| label).collect()
}

/// Walks the child devices of `dir` (real directories only: sysfs links
/// such as a port's `device` are not followed), depth first.
fn collect_hid(dir: &Path, camera: &Path, found: &mut Vec<(String, String)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if !e.file_type().is_ok_and(|t| t.is_dir()) || !is_usb_device(&path) || path == camera {
            continue;
        }
        if is_hid_device(&path) {
            found.push((name_of(&path), label_of(&path)));
        }
        collect_hid(&path, camera, found);
    }
}

/// A kernel path's numbers (`3-7.2.10` -> 3, 7, 2, 10), for a numeric
/// order.
fn path_key(name: &str) -> Vec<u32> {
    name.split(['-', '.'])
        .map(|n| n.parse().unwrap_or(u32::MAX))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;
    use std::path::PathBuf;

    use super::*;

    fn char_number(path: &str) -> String {
        let rdev = fs::metadata(path).unwrap().rdev();
        format!("{}:{}", libc::major(rdev), libc::minor(rdev))
    }

    /// A fake sysfs mirroring a real bus.
    struct Tree {
        root: tempfile::TempDir,
        bus: PathBuf,
    }

    impl Tree {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let bus = root.path().join("devices/pci0000:00/0000:00:14.0/usb3");
            let t = Tree { root, bus };
            t.device(
                "",
                "1d6b",
                "0002",
                Some("xHCI Host Controller"),
                "480",
                &["09"],
            );
            t
        }

        /// A USB device at `path` below the bus (`3-7/3-7.2`), its
        /// interfaces of `classes` (`<name>:1.<n>`, each with a port-like
        /// link as sysfs has, which the walk must not follow).
        fn device(
            &self,
            path: &str,
            vid: &str,
            pid: &str,
            product: Option<&str>,
            speed: &str,
            classes: &[&str],
        ) -> PathBuf {
            let dir = self.bus.join(path);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("idVendor"), format!("{vid}\n")).unwrap();
            fs::write(dir.join("idProduct"), format!("{pid}\n")).unwrap();
            fs::write(dir.join("speed"), format!("{speed}\n")).unwrap();
            fs::write(dir.join("bConfigurationValue"), "1\n").unwrap();
            fs::write(dir.join("descriptors"), b"").unwrap();
            if let Some(p) = product {
                fs::write(dir.join("product"), format!("{p}\n")).unwrap();
            }
            let name = name_of(&dir);
            for (n, class) in classes.iter().enumerate() {
                let iface = dir.join(format!("{name}:1.{n}"));
                fs::create_dir_all(&iface).unwrap();
                fs::write(iface.join("bInterfaceClass"), format!("{class}\n")).unwrap();
                std::os::unix::fs::symlink(&self.bus, iface.join("port")).unwrap();
            }
            dir
        }

        /// A camera at `path` (a video interface `:1.0` with node
        /// `video<n>`), reached by the device number of `dev` (a real
        /// character device; a test cannot create one).
        #[allow(clippy::too_many_arguments)]
        fn camera(
            &self,
            path: &str,
            vid: &str,
            pid: &str,
            product: Option<&str>,
            speed: &str,
            extra: &[&str],
            n: u32,
            dev: &str,
        ) {
            let mut classes = vec!["0e", "0e"];
            classes.extend_from_slice(extra);
            let dir = self.device(path, vid, pid, product, speed, &classes);
            let iface = format!("{}:1.0", name_of(&dir));
            let class_dev = dir.join(&iface).join(format!("video4linux/video{n}"));
            fs::create_dir_all(&class_dev).unwrap();
            std::os::unix::fs::symlink(format!("../../../{iface}"), class_dev.join("device"))
                .unwrap();
            let chars = self.root.path().join("dev/char");
            fs::create_dir_all(&chars).unwrap();
            std::os::unix::fs::symlink(&class_dev, chars.join(char_number(dev))).unwrap();
        }

        fn placement(&self, dev: &str) -> Option<UsbPlacement> {
            placement_of_node(self.root.path(), Path::new(dev))
        }
    }

    /// The bus of the development machine: FHD on root port 4; hub 3-7
    /// (0424:2134) with hub 3-7.2 (USB2744) below it, carrying the C930e
    /// (which has a HID interface of its own), a keyboard and a mouse
    /// without a product string; the Generalplus camera on root port 8;
    /// the VX-800 on root port 1; a receiver (HID) on root port 5. Plus
    /// hub 3-9 with a keyboard, and hub 3-9.1 below it with only a camera.
    fn bench() -> Tree {
        let t = Tree::new();
        t.camera(
            "3-4",
            "1bcf",
            "2284",
            Some("FHD Camera"),
            "480",
            &[],
            0,
            "/dev/zero",
        );
        t.device("3-7", "0424", "2134", None, "480", &["09"]);
        t.device("3-7/3-7.2", "0424", "2744", Some("USB2744"), "480", &["09"]);
        t.camera(
            "3-7/3-7.2/3-7.2.1",
            "046d",
            "0843",
            Some("Logitech Webcam C930e"),
            "480",
            &["01", "01", CLASS_HID],
            2,
            "/dev/null",
        );
        t.device(
            "3-7/3-7.2/3-7.2.3",
            "04d9",
            "0024",
            Some("USB Keyboard"),
            "12",
            &[CLASS_HID, CLASS_HID],
        );
        t.device(
            "3-7/3-7.2/3-7.2.4",
            "046D",
            "C077",
            None,
            "1.5",
            &[CLASS_HID],
        );
        t.camera(
            "3-8",
            "1b3f",
            "1167",
            Some("Generalplus"),
            "480",
            &[],
            4,
            "/dev/full",
        );
        t.camera(
            "3-1",
            "045e",
            "0766",
            None,
            "480",
            &[CLASS_HID],
            6,
            "/dev/random",
        );
        t.device(
            "3-5",
            "046d",
            "c52b",
            Some("USB Receiver"),
            "12",
            &[CLASS_HID],
        );
        t.device("3-9", "05e3", "0610", Some("USB2.0 Hub"), "480", &["09"]);
        t.device(
            "3-9/3-9.2",
            "1234",
            "0001",
            Some("Macro Pad"),
            "12",
            &[CLASS_HID],
        );
        t.device(
            "3-9/3-9.1",
            "05e3",
            "0610",
            Some("USB2.0 Hub"),
            "480",
            &["09"],
        );
        t.camera(
            "3-9/3-9.1/3-9.1.1",
            "046d",
            "0843",
            None,
            "5000",
            &[],
            8,
            "/dev/urandom",
        );
        t
    }

    #[test]
    fn a_camera_on_a_hub_with_a_keyboard_and_mouse_shares_it() {
        let t = bench();
        assert_eq!(
            t.placement("/dev/null"),
            Some(UsbPlacement {
                usb_path: "3-7.2.1".into(),
                speed_mbps: Some(480),
                shared_hub: Some(SharedHub {
                    hub_path: "3-7.2".into(),
                    // The camera's own HID interface is not counted.
                    hid_devices: vec!["USB Keyboard".into(), "046d:c077".into()],
                }),
            })
        );
    }

    #[test]
    fn cameras_on_root_ports_share_no_hub() {
        let t = bench();
        for (dev, path) in [
            ("/dev/zero", "3-4"),
            ("/dev/full", "3-8"),
            // With a HID interface of its own, and a receiver on the bus.
            ("/dev/random", "3-1"),
        ] {
            let p = t.placement(dev).unwrap_or_else(|| panic!("{dev}"));
            assert_eq!(p.usb_path, path);
            assert_eq!(p.speed_mbps, Some(480));
            assert_eq!(p.shared_hub, None, "{path}");
        }
    }

    #[test]
    fn the_nearest_hub_with_hid_devices_counts() {
        let t = bench();
        let p = t.placement("/dev/urandom").unwrap();
        assert_eq!(p.usb_path, "3-9.1.1");
        assert_eq!(p.speed_mbps, Some(5000));
        assert_eq!(
            p.shared_hub,
            Some(SharedHub {
                hub_path: "3-9".into(),
                hid_devices: vec!["Macro Pad".into()],
            })
        );
    }

    /// A VX-800 sharing a hub with only a C930e: both have a HID interface
    /// (their buttons), and neither is a keyboard or a mouse.
    #[test]
    fn a_camera_with_buttons_is_no_hid_device() {
        let t = Tree::new();
        t.device("3-2", "0424", "2744", Some("USB2744"), "480", &["09"]);
        t.camera(
            "3-2/3-2.1",
            "045e",
            "0766",
            None,
            "480",
            &[CLASS_HID],
            0,
            "/dev/zero",
        );
        t.camera(
            "3-2/3-2.2",
            "046d",
            "0843",
            Some("Logitech Webcam C930e"),
            "480",
            &["01", "01", CLASS_HID],
            2,
            "/dev/null",
        );
        for (dev, path) in [("/dev/zero", "3-2.1"), ("/dev/null", "3-2.2")] {
            let p = t.placement(dev).unwrap_or_else(|| panic!("{dev}"));
            assert_eq!(p.usb_path, path);
            assert_eq!(p.shared_hub, None, "{path}");
        }
        // A keyboard joining them is one.
        t.device(
            "3-2/3-2.3",
            "04d9",
            "0024",
            Some("USB Keyboard"),
            "12",
            &[CLASS_HID],
        );
        assert_eq!(
            t.placement("/dev/zero").unwrap().shared_hub,
            Some(SharedHub {
                hub_path: "3-2".into(),
                hid_devices: vec!["USB Keyboard".into()],
            })
        );
    }

    #[test]
    fn a_node_that_is_no_usb_camera_has_no_placement() {
        let t = bench();
        assert_eq!(t.placement("/dev/tty"), None);
        assert_eq!(t.placement("/nonexistent/video9"), None);
    }

    #[test]
    fn kernel_paths_order_numerically() {
        let mut names = vec!["3-7.10", "3-7.2.1", "3-7.9", "3-10"];
        names.sort_by_key(|n| path_key(n));
        assert_eq!(names, ["3-7.2.1", "3-7.9", "3-7.10", "3-10"]);
    }

    #[test]
    fn raw_modes_need_their_bandwidth_and_mjpeg_has_none() {
        let mode = |format, width, height, fps_num, fps_den| VideoMode {
            format,
            width,
            height,
            fps_num,
            fps_den,
        };
        let yuyv = mode(PixelFormat::Yuy2, 640, 480, 30, 1);
        assert_eq!(raw_bytes_per_second(&yuyv), Some(18_432_000));
        assert_eq!(
            raw_bytes_per_second(&mode(PixelFormat::Uyvy, 640, 480, 30, 1)),
            Some(18_432_000)
        );
        assert_eq!(
            raw_bytes_per_second(&mode(PixelFormat::Nv12, 1920, 1080, 30, 1)),
            Some(93_312_000)
        );
        assert_eq!(
            raw_bytes_per_second(&mode(PixelFormat::I420, 640, 480, 15, 2)),
            Some(3_456_000)
        );
        assert_eq!(
            raw_bytes_per_second(&mode(PixelFormat::Rgb, 320, 240, 10, 1)),
            Some(2_304_000)
        );
        assert_eq!(
            raw_bytes_per_second(&mode(PixelFormat::Mjpeg, 1920, 1080, 30, 1)),
            None
        );
        assert_eq!(
            raw_bytes_per_second(&mode(PixelFormat::Yuy2, 640, 480, 30, 0)),
            None
        );
    }
}
