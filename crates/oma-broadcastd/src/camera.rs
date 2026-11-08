//! The cameras the daemon can stream from: their stable ids, their order
//! (configured preference first, then discovery order) and how a
//! `SelectCamera` selector picks one.

use std::fs;
use std::path::{Path, PathBuf};

use obc_core::{CameraSelector, UsbId, VideoMode};
use obc_ipc::CameraInfo;

/// Where udev keeps the stable camera links.
pub const V4L_LINKS: &str = "/dev/v4l";

/// One colour camera that is present, with the mode chosen for it from the
/// configured request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Camera {
    /// Stable id ([`stable_id`]); what `SelectCamera` accepts and
    /// `ActiveCamera` reports.
    pub id: String,
    /// V4L2 card name.
    pub name: String,
    pub usb: Option<UsbId>,
    /// The device node (`/dev/videoN`).
    pub path: PathBuf,
    /// The capture mode the ingest opens it in.
    pub mode: VideoMode,
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

/// Whether `selector` names `camera`. A path selector is resolved first, so
/// a `/dev/v4l/by-id/...` link names the node it points at.
pub fn selector_matches(selector: &CameraSelector, camera: &Camera) -> bool {
    match selector {
        CameraSelector::Path(p) => {
            let resolved = fs::canonicalize(p).unwrap_or_else(|_| p.clone());
            resolved == camera.path || p == &camera.path
        }
        other => other.matches(&camera.path, &camera.name, camera.usb),
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
            usb: usb.map(|u| u.parse().unwrap()),
            path: PathBuf::from(node),
            mode: "NV12:1920x1080@30".parse().unwrap(),
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
