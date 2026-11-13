//! The cameras the daemon can stream from: their stable ids, their display
//! names, their order (configured preference first, then discovery order),
//! how a `SelectCamera` selector picks one and whether a camera found at a
//! known node is still the same device.

use std::fs;
use std::path::{Path, PathBuf};

use obc_core::{CameraSelector, UsbId, VideoMode};
use obc_ipc::CameraInfo;

/// Where udev keeps the stable camera links.
pub const V4L_LINKS: &str = "/dev/v4l";
/// The video4linux class directory in sysfs.
pub const SYSFS_V4L: &str = "/sys/class/video4linux";

/// One colour camera that is present, with the mode chosen for it from the
/// configured request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Camera {
    /// Stable id ([`stable_id`]); what `SelectCamera` accepts and
    /// `ActiveCamera` reports.
    pub id: String,
    /// Display name: the card name made readable and unique among the
    /// cameras present ([`display_names`]).
    pub name: String,
    /// V4L2 card name, as the driver reports it.
    pub card: String,
    pub usb: Option<UsbId>,
    /// Serial number of the parent USB device, if it has one.
    pub serial: Option<String>,
    /// Product string of the parent USB device, if it has one: the display
    /// name when the card name is cut ([`display_name`]).
    pub product: Option<String>,
    /// Kernel path of the parent USB device on its bus (`3-4`, `1-2.3`):
    /// the port it is plugged into.
    pub usb_path: Option<String>,
    /// The device node (`/dev/videoN`).
    pub path: PathBuf,
    /// The capture mode the ingest opens it in: the one chosen for the
    /// config's request, or for the mode the user chose for this camera.
    pub mode: VideoMode,
    /// Every mode its node offers in an ingestible format, in enumeration
    /// order (empty when not known): what a mode is chosen from, and the
    /// choices of its `camera.capture.mode` setting.
    pub modes: Vec<VideoMode>,
}

impl Camera {
    /// The entry of the `Cameras` property.
    pub fn info(&self) -> CameraInfo {
        CameraInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            usb: self.usb.map(|u| u.to_string()).unwrap_or_default(),
        }
    }

    /// `name (id)`, for log lines and messages.
    pub fn label(&self) -> String {
        format!("{} ({})", self.name, self.id)
    }
}

/// The stable id of the camera at `node`: the name of its
/// `/dev/v4l/by-id` link, else of its `/dev/v4l/by-path` link (the first
/// in name order), else the node path itself.
///
/// The by-id name survives a change of USB port, the by-path name a
/// change of `/dev/videoN` numbering. udev gives two interfaces of one
/// device with the same serial the same by-id name and keeps only one of
/// them, so a colour node can lack a by-id link while its IR sibling has
/// it; by-path then still names it stably.
pub fn stable_id(node: &Path) -> String {
    stable_id_in(Path::new(V4L_LINKS), node)
}

/// [`stable_id`] with the link directories under `v4l_dir`.
pub fn stable_id_in(v4l_dir: &Path, node: &Path) -> String {
    let target = fs::canonicalize(node).unwrap_or_else(|_| node.to_path_buf());
    for sub in ["by-id", "by-path"] {
        let Ok(entries) = fs::read_dir(v4l_dir.join(sub)) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter(|e| fs::canonicalize(e.path()).is_ok_and(|t| t == target))
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        names.sort();
        if let Some(first) = names.into_iter().next() {
            return first;
        }
    }
    node.display().to_string()
}

/// What sysfs tells of the parent USB device of a video node
/// ([`usb_origin`]). Every field is `None` for a node that is not on USB.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsbOrigin {
    /// Its serial number ([`Camera::serial`]).
    pub serial: Option<String>,
    /// Its kernel bus path ([`Camera::usb_path`]).
    pub usb_path: Option<String>,
    /// Its product string ([`Camera::product`]).
    pub product: Option<String>,
}

/// The parent USB device of the video node `node`: its serial number, its
/// kernel bus path and its product string.
pub fn usb_origin(node: &Path) -> UsbOrigin {
    usb_origin_in(Path::new(SYSFS_V4L), node)
}

/// [`usb_origin`] with the video4linux class directory at `sysfs_v4l`.
pub fn usb_origin_in(sysfs_v4l: &Path, node: &Path) -> UsbOrigin {
    let Some(class_dir) = node.file_name().map(|n| sysfs_v4l.join(n)) else {
        return UsbOrigin::default();
    };
    let Ok(real) = fs::canonicalize(class_dir) else {
        return UsbOrigin::default();
    };
    // .../usb3/3-4/3-4:1.0/video4linux/video2: the USB device directory is
    // the nearest ancestor with a vendor id.
    let Some(device) = real
        .ancestors()
        .skip(1)
        .take(5)
        .find(|d| d.join("idVendor").is_file())
    else {
        return UsbOrigin::default();
    };
    let read = |file: &str| {
        fs::read_to_string(device.join(file))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    UsbOrigin {
        serial: read("serial"),
        usb_path: device
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string),
        product: read("product"),
    }
}

/// The longest V4L2 card name, in bytes: the driver's 32-byte field holds
/// 31 and a NUL.
pub const CARD_MAX: usize = 31;

/// The shortest card name taken for a cut `<X>: <start of X>` when the
/// product string is unknown: the card is trimmed after the driver cut it,
/// so it may be one or two bytes under [`CARD_MAX`] (a cut on a space, or
/// in the middle of a multibyte character).
const CUT_MIN: usize = CARD_MAX - 2;

/// The display name of a camera from its V4L2 card name and the product
/// string of its USB device (`product`, if it has one).
///
/// The driver (uvcvideo) builds `<product>: <product>` and cuts it at
/// [`CARD_MAX`] bytes, and discovery trims the card, so a cut name may be
/// shorter than that. When the card is the start of `<product>: <product>`
/// and longer than the product (or, for a product longer than the card, at
/// least [`CUT_MIN`] bytes: the product itself was cut), the card is that
/// cut: the product string is shown. Without such a product string, a card of at least [`CUT_MIN`]
/// bytes of the form `<X>:` or `<X>: <start of X>` is reduced to `X`. Any
/// card name is trimmed, and one built as `<product>: <product>` (the same
/// text on both sides of a colon) reduced to one copy. Any other name is
/// kept as it is.
pub fn display_name(card: &str, product: Option<&str>) -> String {
    let card = card.trim();
    if let Some(p) = product.map(str::trim).filter(|p| !p.is_empty())
        && (card.len() > p.len() || card.len() >= CUT_MIN)
        && format!("{p}: {p}").starts_with(card)
    {
        return p.to_string();
    }
    let cut = card.len() >= CUT_MIN;
    for (i, _) in card.match_indices(':') {
        let (prefix, rest) = (card[..i].trim(), card[i + 1..].trim());
        if !prefix.is_empty() && (prefix == rest || (cut && prefix.starts_with(rest))) {
            return prefix.to_string();
        }
    }
    card.to_string()
}

/// Sets every camera's [`Camera::name`] from its card name and product
/// string ([`display_name`]) and makes the names unique: of cameras with the same
/// name, the first by id (then node; cameras with only the fallback node id
/// after those with a link id) keeps it and the others get ` (2)`,
/// ` (3)`, ... (the lowest number no other camera's name uses). The order
/// of `cameras` is left as it is.
pub fn display_names(cameras: &mut [Camera]) {
    for c in cameras.iter_mut() {
        c.name = display_name(&c.card, c.product.as_deref());
    }
    let mut by_id: Vec<usize> = (0..cameras.len()).collect();
    // A camera still known by its node (the fallback id) goes last: its
    // by-id/by-path links appear a moment after it is plugged in, and it must
    // not take a plain name from a camera that already has one meanwhile.
    let key = |c: &Camera| {
        (
            c.id == c.path.display().to_string(),
            c.id.clone(),
            c.path.clone(),
        )
    };
    by_id.sort_by_key(|&i| key(&cameras[i]));
    let mut taken: Vec<String> = cameras.iter().map(|c| c.name.clone()).collect();
    let mut kept: Vec<String> = Vec::new();
    for i in by_id {
        let name = cameras[i].name.clone();
        if !kept.contains(&name) {
            kept.push(name);
            continue;
        }
        let unique = (2..)
            .map(|k| format!("{name} ({k})"))
            .find(|n| !taken.contains(n))
            .expect("a free number");
        taken.push(unique.clone());
        cameras[i].name = unique;
    }
}

/// Whether `now`, found at the node where `was` was, is the same camera,
/// so that a different id is a rename of `was` rather than another camera
/// that took over its node (one unplugged and another plugged in between
/// two scans). It is when the ids are equal; when `was` was known by its
/// node only (the fallback id of [`stable_id`]) and nothing shows the two
/// to differ; or when both are the same USB model (vendor:product) and
/// either both report the same serial number or, without a serial on
/// both, they sit at the same USB port.
pub fn same_camera(was: &Camera, now: &Camera) -> bool {
    if was.id == now.id {
        return true;
    }
    let same_model = matches!((was.usb, now.usb), (Some(a), Some(b)) if a == b);
    let serials = was.serial.as_ref().zip(now.serial.as_ref());
    if was.id == was.path.display().to_string() {
        let model_differs = matches!((was.usb, now.usb), (Some(a), Some(b)) if a != b);
        let serial_differs = serials.is_some_and(|(a, b)| a != b);
        return !model_differs && !serial_differs;
    }
    if !same_model {
        return false;
    }
    match serials {
        Some((a, b)) => a == b,
        None => matches!((&was.usb_path, &now.usb_path), (Some(a), Some(b)) if a == b),
    }
}

/// Whether `selector` names `camera`. A path selector is resolved first, so
/// a `/dev/v4l/by-id/...` link names the node it points at. A name
/// selector matches the display name or the card name.
pub fn selector_matches(selector: &CameraSelector, camera: &Camera) -> bool {
    match selector {
        CameraSelector::Path(p) => {
            let resolved = fs::canonicalize(p).unwrap_or_else(|_| p.clone());
            resolved == camera.path || p == &camera.path
        }
        other => {
            other.matches(&camera.path, &camera.name, camera.usb)
                || other.matches(&camera.path, &camera.card, camera.usb)
        }
    }
}

/// `cameras` (in discovery order) reordered: the cameras matching the first
/// preference entry, then those matching the second, ..., then every other
/// camera in discovery order.
pub fn order(cameras: Vec<Camera>, preference: &[CameraSelector]) -> Vec<Camera> {
    let mut rest = cameras;
    let mut out = Vec::with_capacity(rest.len());
    for sel in preference {
        let mut i = 0;
        while i < rest.len() {
            if selector_matches(sel, &rest[i]) {
                out.push(rest.remove(i));
            } else {
                i += 1;
            }
        }
    }
    out.extend(rest);
    out
}

/// The camera `selector` names among `cameras`: an exact id first, then the
/// selector forms of [`CameraSelector`] (a `/dev/...` path, a `vid:pid`, a
/// case-insensitive card-name substring). Several matches: the first in
/// order. `Err` carries the message for `NoSuchCamera`.
pub fn find<'a>(cameras: &'a [Camera], selector: &str) -> Result<&'a Camera, String> {
    if let Some(c) = cameras.iter().find(|c| c.id == selector) {
        return Ok(c);
    }
    let parsed: CameraSelector = selector
        .parse()
        .map_err(|e| format!("no camera matches {selector:?}: {e}"))?;
    cameras
        .iter()
        .find(|c| selector_matches(&parsed, c))
        .ok_or_else(|| {
            let present = if cameras.is_empty() {
                "none".to_string()
            } else {
                cameras
                    .iter()
                    .map(Camera::label)
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            format!("no camera matches {selector:?} (cameras present: {present})")
        })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn cam(id: &str, name: &str, usb: Option<&str>, node: &str) -> Camera {
        Camera {
            id: id.into(),
            name: name.into(),
            card: name.into(),
            usb: usb.map(|u| u.parse().unwrap()),
            serial: None,
            product: None,
            usb_path: None,
            path: PathBuf::from(node),
            mode: "NV12:1920x1080@30".parse().unwrap(),
            modes: Vec::new(),
        }
    }

    fn three() -> Vec<Camera> {
        vec![
            cam("usb-a", "Alpha Cam", Some("1111:0001"), "/dev/video0"),
            cam("usb-b", "Beta Cam", Some("2222:0002"), "/dev/video2"),
            cam("/dev/video6", "Gamma", None, "/dev/video6"),
        ]
    }

    fn ids(c: &[Camera]) -> Vec<&str> {
        c.iter().map(|c| c.id.as_str()).collect()
    }

    #[test]
    fn preference_comes_first_then_discovery_order() {
        let pref: Vec<CameraSelector> = ["gamma", "2222:0002", "missing"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(
            ids(&order(three(), &pref)),
            ["/dev/video6", "usb-b", "usb-a"]
        );
        assert_eq!(ids(&order(three(), &[])), ["usb-a", "usb-b", "/dev/video6"]);
        // One entry matching several cameras keeps their discovery order.
        let cam_pref = ["cam".parse().unwrap()];
        assert_eq!(
            ids(&order(three(), &cam_pref)),
            ["usb-a", "usb-b", "/dev/video6"]
        );
    }

    #[test]
    fn find_by_id_name_usb_and_path() {
        let c = three();
        assert_eq!(find(&c, "usb-b").unwrap().id, "usb-b");
        assert_eq!(find(&c, "alpha").unwrap().id, "usb-a");
        assert_eq!(find(&c, "BETA").unwrap().id, "usb-b");
        assert_eq!(find(&c, "2222:0002").unwrap().id, "usb-b");
        assert_eq!(find(&c, "/dev/video6").unwrap().id, "/dev/video6");
        // A name matching several: the first in order.
        assert_eq!(find(&c, "cam").unwrap().id, "usb-a");
        let e = find(&c, "nope").unwrap_err();
        assert!(
            e.contains("\"nope\"") && e.contains("Alpha Cam (usb-a)"),
            "{e}"
        );
        assert!(
            find(&[], "x")
                .unwrap_err()
                .contains("cameras present: none")
        );
        assert!(find(&c, "  ").is_err());
    }

    #[test]
    fn info_carries_the_lower_case_usb_id() {
        let c = cam("x", "X", Some("046D:0944"), "/dev/video0");
        assert_eq!(
            c.info(),
            CameraInfo {
                id: "x".into(),
                name: "X".into(),
                usb: "046d:0944".into()
            }
        );
        assert_eq!(cam("y", "Y", None, "/dev/video1").info().usb, "");
    }

    #[test]
    fn display_name_drops_a_repeated_product_name_only() {
        for (card, want) in [
            // The same text on both sides of the colon: one copy.
            ("Laptop Camera: Laptop Camera", "Laptop Camera"),
            ("  Laptop Camera :   Laptop Camera  ", "Laptop Camera"),
            ("Laptop Camera:Laptop Camera", "Laptop Camera"),
            ("A: B: A: B", "A: B"),
            // Different text: kept as it is (trimmed).
            (
                "Laptop Camera: Laptop IR Camera",
                "Laptop Camera: Laptop IR Camera",
            ),
            ("Studio: Pro", "Studio: Pro"),
            ("Cam: cam", "Cam: cam"),
            ("  Plain Cam  ", "Plain Cam"),
            ("Plain Cam", "Plain Cam"),
            (":", ":"),
            (" : ", ":"),
            ("", ""),
        ] {
            assert_eq!(display_name(card, None), want, "{card:?}");
            // A product string that the card name does not start with
            // changes nothing.
            assert_eq!(display_name(card, Some("Product")), want, "{card:?}");
        }
    }

    #[test]
    fn a_cut_card_name_shows_the_product_string() {
        // uvcvideo's "<product>: <product>" cut at 31 bytes.
        let vx800 = "Microsoft LifeCam VX-800: Micro";
        assert_eq!(vx800.len(), CARD_MAX);
        assert_eq!(
            display_name(vx800, Some(" Microsoft LifeCam VX-800 \n")),
            "Microsoft LifeCam VX-800"
        );
        // A product name longer than the card: the whole product string.
        let long = "Integrated Webcam With Very Long";
        let long = &long[..CARD_MAX];
        assert_eq!(
            display_name(long, Some("Integrated Webcam With Very Long Name")),
            "Integrated Webcam With Very Long Name"
        );
        // Without a product string (or an empty one): "<X>: <start of X>"
        // reduced to X; a cut name of another form kept.
        for product in [None, Some(""), Some("  ")] {
            assert_eq!(display_name(vx800, product), "Microsoft LifeCam VX-800");
            // The driver's cut ended on the space: the card arrives trimmed.
            let trimmed = "Microsoft LifeCam VX-8000 Pro:";
            assert_eq!(trimmed.len(), CARD_MAX - 1);
            assert_eq!(
                display_name(trimmed, product),
                "Microsoft LifeCam VX-8000 Pro"
            );
            assert_eq!(
                display_name("Microsoft LifeCam VX-800: Other", product),
                "Microsoft LifeCam VX-800: Other"
            );
            assert_eq!(display_name(long, product), long);
        }
        // The trimmed cut shows the product too, whatever its length.
        assert_eq!(
            display_name(
                "Microsoft LifeCam VX-80: Micro",
                Some("Microsoft LifeCam VX-80")
            ),
            "Microsoft LifeCam VX-80"
        );
        // A card as long as the product is not a cut of "P: P".
        assert_eq!(display_name("Studio: Pro", Some("Studio")), "Studio: Pro");
        // A card name of the limit's length that the product string does
        // not start: whole names, not cut.
        let ir = "Laptop Camera: Laptop IR Camera";
        assert_eq!(ir.len(), CARD_MAX);
        assert_eq!(display_name(ir, Some("Laptop Camera")), ir);
        assert_eq!(
            display_name(vx800, Some("LifeCam")),
            "Microsoft LifeCam VX-800"
        );
    }

    #[test]
    fn display_names_use_the_product_string_and_stay_unique() {
        let vx800 = |id: &str, node: &str, product: Option<&str>| {
            let mut c = carded(id, "Microsoft LifeCam VX-800: Micro", node);
            c.product = product.map(str::to_string);
            c
        };
        let mut c = vec![
            vx800("usb-b", "/dev/video2", Some("Microsoft LifeCam VX-800")),
            vx800("usb-a", "/dev/video0", None),
            carded("usb-c", "Microsoft LifeCam VX-800", "/dev/video4"),
        ];
        display_names(&mut c);
        assert_eq!(
            names(&c),
            [
                "Microsoft LifeCam VX-800 (2)",
                "Microsoft LifeCam VX-800",
                "Microsoft LifeCam VX-800 (3)",
            ]
        );
        // The ids and the card names stay as they are.
        assert_eq!(ids(&c), ["usb-b", "usb-a", "usb-c"]);
        assert_eq!(c[0].card, "Microsoft LifeCam VX-800: Micro");
    }

    fn names(c: &[Camera]) -> Vec<&str> {
        c.iter().map(|c| c.name.as_str()).collect()
    }

    fn carded(id: &str, card: &str, node: &str) -> Camera {
        let mut c = cam(id, "", Some("1111:0001"), node);
        c.card = card.into();
        c
    }

    #[test]
    fn display_names_are_clean_and_unique_in_id_order() {
        // Already unique: only cleaned.
        let mut c = vec![
            carded("usb-a", "Laptop Camera: Laptop Camera", "/dev/video0"),
            carded("usb-b", "Desk Cam", "/dev/video2"),
        ];
        display_names(&mut c);
        assert_eq!(names(&c), ["Laptop Camera", "Desk Cam"]);
        assert_eq!(c[0].card, "Laptop Camera: Laptop Camera", "the card stays");
        // Two identical cameras: the first by id keeps the name, whatever
        // the list order; the order is kept.
        let mut c = vec![
            carded("usb-z", "Desk Cam", "/dev/video0"),
            carded("usb-y", "Desk Cam", "/dev/video2"),
        ];
        display_names(&mut c);
        assert_eq!(names(&c), ["Desk Cam (2)", "Desk Cam"]);
        assert_eq!(ids(&c), ["usb-z", "usb-y"]);
        // Three, two of them with the repeated name: all clean, all unique.
        let mut c = vec![
            carded("usb-c", "Desk Cam: Desk Cam", "/dev/video4"),
            carded("usb-a", "Desk Cam", "/dev/video0"),
            carded("usb-b", "Desk Cam: Desk Cam", "/dev/video2"),
        ];
        display_names(&mut c);
        assert_eq!(names(&c), ["Desk Cam (3)", "Desk Cam", "Desk Cam (2)"]);
        // The same set scanned in another order: the same names.
        c.reverse();
        display_names(&mut c);
        assert_eq!(names(&c), ["Desk Cam (2)", "Desk Cam", "Desk Cam (3)"]);
        // A suffix never collides with a camera's own name.
        let mut c = vec![
            carded("usb-a", "Desk Cam", "/dev/video0"),
            carded("usb-b", "Desk Cam", "/dev/video2"),
            carded("usb-c", "Desk Cam (2)", "/dev/video4"),
        ];
        display_names(&mut c);
        assert_eq!(names(&c), ["Desk Cam", "Desk Cam (3)", "Desk Cam (2)"]);
        // Equal ids (fallback node ids never are, but still): by node.
        let mut c = vec![
            carded("x", "Desk Cam", "/dev/video2"),
            carded("x", "Desk Cam", "/dev/video0"),
        ];
        display_names(&mut c);
        assert_eq!(names(&c), ["Desk Cam (2)", "Desk Cam"]);
    }

    #[test]
    fn a_newcomer_with_a_fallback_id_does_not_take_the_plain_name() {
        // "/dev/video0" sorts before "usb-..." but the camera with the link
        // id keeps the plain name until the newcomer's links appear.
        let mut c = vec![
            carded("/dev/video0", "Desk Cam", "/dev/video0"),
            carded("usb-1", "Desk Cam", "/dev/video2"),
        ];
        display_names(&mut c);
        assert_eq!(names(&c), ["Desk Cam (2)", "Desk Cam"]);
        // Once it has its link id, which sorts after, the names stay.
        c[0].id = "usb-2".into();
        display_names(&mut c);
        assert_eq!(names(&c), ["Desk Cam (2)", "Desk Cam"]);
    }

    #[test]
    fn a_name_selector_matches_the_display_and_the_card_name() {
        let mut c = vec![
            carded("usb-a", "Laptop Camera: Laptop Camera", "/dev/video0"),
            carded("usb-b", "Desk Cam", "/dev/video2"),
            carded("usb-c", "Desk Cam", "/dev/video4"),
        ];
        display_names(&mut c);
        // As before the clean-up: the card name selects.
        assert_eq!(find(&c, "Laptop Camera: Laptop").unwrap().id, "usb-a");
        assert_eq!(find(&c, "laptop camera").unwrap().id, "usb-a");
        // The first of two identical cameras, or the numbered one.
        assert_eq!(find(&c, "desk cam").unwrap().id, "usb-b");
        assert_eq!(find(&c, "Desk Cam (2)").unwrap().id, "usb-c");
        let pref = ["desk cam (2)".parse().unwrap()];
        assert_eq!(ids(&order(c, &pref)), ["usb-c", "usb-a", "usb-b"]);
    }

    fn device(id: &str, usb: &str, serial: Option<&str>, port: Option<&str>) -> Camera {
        let mut c = cam(id, "Cam", Some(usb), "/dev/video2");
        c.serial = serial.map(str::to_string);
        c.usb_path = port.map(str::to_string);
        c
    }

    #[test]
    fn same_camera_needs_the_same_device_unless_known_by_node_only() {
        let by_path = device("pci-usb-0:4:1.0", "1111:0001", Some("S1"), Some("3-4"));
        let by_id = device("usb-Cam_S1", "1111:0001", Some("S1"), Some("3-4"));
        // Same id; same device under a new id (serial, or port without one).
        assert!(same_camera(&by_path, &by_path.clone()));
        assert!(same_camera(&by_path, &by_id));
        let other_port = device("usb-Cam_S1", "1111:0001", Some("S1"), Some("1-2"));
        assert!(same_camera(&by_path, &other_port), "the serial decides");
        let no_serial = device("a", "1111:0001", None, Some("3-4"));
        let no_serial_b = device("b", "1111:0001", None, Some("3-4"));
        assert!(same_camera(&no_serial, &no_serial_b));
        // Another camera that took the node over.
        let other_model = device("usb-Other_S1", "2222:0002", Some("S1"), Some("3-4"));
        assert!(!same_camera(&by_path, &other_model));
        let other_unit = device("usb-Cam_S2", "1111:0001", Some("S2"), Some("3-4"));
        assert!(!same_camera(&by_path, &other_unit), "a different serial");
        let elsewhere = device("b", "1111:0001", None, Some("1-2"));
        assert!(!same_camera(&no_serial, &elsewhere));
        let unknown = device("b", "1111:0001", None, None);
        assert!(!same_camera(&no_serial, &unknown), "nothing to compare");
        let mut no_usb = by_id.clone();
        no_usb.usb = None;
        assert!(!same_camera(&by_path, &no_usb));
        // Known by its node only: a rename unless shown to differ.
        let fallback = device("/dev/video2", "1111:0001", None, None);
        assert!(same_camera(&fallback, &by_id));
        assert!(same_camera(&fallback, &no_usb));
        assert!(!same_camera(&fallback, &other_model));
        let fallback_s1 = device("/dev/video2", "1111:0001", Some("S1"), None);
        assert!(same_camera(&fallback_s1, &by_id));
        assert!(!same_camera(&fallback_s1, &other_unit));
    }

    #[test]
    fn usb_origin_reads_the_parent_usb_device() {
        let dir = tempfile::tempdir().unwrap();
        let sys = dir.path();
        let usb = sys.join("devices/pci0/usb3/3-4");
        let colour = usb.join("3-4:1.0/video4linux/video2");
        let bare = usb.join("3-4:1.2/video4linux/video4");
        let virt = sys.join("devices/virtual/video4linux/video11");
        for d in [&colour, &bare, &virt] {
            fs::create_dir_all(d).unwrap();
        }
        fs::write(usb.join("idVendor"), "1111\n").unwrap();
        fs::write(usb.join("serial"), " S1 \n").unwrap();
        fs::write(usb.join("product"), "Microsoft LifeCam VX-800\n").unwrap();
        let class = sys.join("class/video4linux");
        fs::create_dir_all(&class).unwrap();
        let link =
            |to: &Path, name: &str| std::os::unix::fs::symlink(to, class.join(name)).unwrap();
        link(&colour, "video2");
        link(&virt, "video11");
        assert_eq!(
            usb_origin_in(&class, Path::new("/dev/video2")),
            UsbOrigin {
                serial: Some("S1".into()),
                usb_path: Some("3-4".into()),
                product: Some("Microsoft LifeCam VX-800".into()),
            }
        );
        // No serial file (or an empty one), no product file: the port only.
        fs::write(usb.join("serial"), "\n").unwrap();
        fs::remove_file(usb.join("product")).unwrap();
        assert_eq!(
            usb_origin_in(&class, Path::new("/dev/video2")),
            UsbOrigin {
                usb_path: Some("3-4".into()),
                ..UsbOrigin::default()
            }
        );
        // Not on USB, or not in sysfs.
        assert_eq!(
            usb_origin_in(&class, Path::new("/dev/video11")),
            UsbOrigin::default()
        );
        assert_eq!(
            usb_origin_in(&class, Path::new("/dev/video9")),
            UsbOrigin::default()
        );
    }

    #[test]
    fn stable_id_prefers_by_id_then_by_path_then_the_node() {
        let dir = tempfile::tempdir().unwrap();
        let dev = dir.path().join("dev");
        fs::create_dir_all(&dev).unwrap();
        for n in ["video0", "video2", "video4"] {
            fs::write(dev.join(n), b"").unwrap();
        }
        let v4l = dev.join("v4l");
        fs::create_dir_all(v4l.join("by-id")).unwrap();
        fs::create_dir_all(v4l.join("by-path")).unwrap();
        let link = |sub: &str, name: &str, to: &str| {
            std::os::unix::fs::symlink(dev.join(to), v4l.join(sub).join(name)).unwrap();
        };
        link("by-id", "usb-Brio-video-index0", "video0");
        link("by-path", "pci-1-usb-0:2:1.0-video-index0", "video0");
        link("by-path", "pci-2-usbv2-0:4:1.0-video-index0", "video2");
        link("by-path", "pci-2-usb-0:4:1.0-video-index0", "video2");
        assert_eq!(
            stable_id_in(&v4l, &dev.join("video0")),
            "usb-Brio-video-index0"
        );
        assert_eq!(
            stable_id_in(&v4l, &dev.join("video2")),
            "pci-2-usb-0:4:1.0-video-index0"
        );
        let bare = dev.join("video4");
        assert_eq!(stable_id_in(&v4l, &bare), bare.display().to_string());
        assert_eq!(
            stable_id_in(&dir.path().join("none"), &bare),
            bare.display().to_string()
        );
    }
}
