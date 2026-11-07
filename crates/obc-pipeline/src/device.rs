//! Pure device model and selection logic.
//!
//! Discovery ([`crate::discovery`]) turns the live system into a list of
//! [`DeviceInfo`], each with the [`Capabilities`] its node enumerates; the
//! functions here pick the source camera (and its mode) and the loopback sink
//! from such a list. Keeping selection pure means it is tested over device
//! lists built from captured fixtures, with no hardware.

use std::fmt;
use std::path::PathBuf;

use obc_core::{CameraSelector, ModeRequest, SinkSelector, UsbId};
use serde::Serialize;

use crate::capability::{Capabilities, ModeChoice, choose_mode, selection_key};

/// `V4L2_CAP_VIDEO_CAPTURE`
pub const CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
/// `V4L2_CAP_VIDEO_OUTPUT`
pub const CAP_VIDEO_OUTPUT: u32 = 0x0000_0002;
/// `V4L2_CAP_VIDEO_CAPTURE_MPLANE`
pub const CAP_VIDEO_CAPTURE_MPLANE: u32 = 0x0000_1000;
/// `V4L2_CAP_VIDEO_OUTPUT_MPLANE`
pub const CAP_VIDEO_OUTPUT_MPLANE: u32 = 0x0000_2000;
/// `V4L2_CAP_META_CAPTURE` (UVC metadata nodes carry only this).
pub const CAP_META_CAPTURE: u32 = 0x0080_0000;
/// `V4L2_CAP_DEVICE_CAPS`: `device_caps` is valid.
pub const CAP_DEVICE_CAPS: u32 = 0x8000_0000;

/// Driver name reported by v4l2loopback in `VIDIOC_QUERYCAP`.
pub const LOOPBACK_DRIVER: &str = "v4l2 loopback";

/// Where a [`DeviceInfo`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverySource {
    /// GStreamer `DeviceMonitor` (PipeWire or v4l2 provider), enriched from sysfs.
    DeviceMonitor,
    /// Direct scan of `/sys/class/video4linux` plus `VIDIOC_QUERYCAP`.
    Sysfs,
    /// Hand-built (tests).
    Synthetic,
}

/// One V4L2 device node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceInfo {
    /// Device node, e.g. `/dev/video4`. Resolved at runtime, never configured.
    pub path: PathBuf,
    /// V4L2 card name (for v4l2loopback this is the `card_label`).
    pub card: String,
    /// V4L2 driver name, if known (`uvcvideo`, `v4l2 loopback`, ...).
    pub driver: Option<String>,
    /// USB vendor:product of the parent USB device, if any.
    pub usb: Option<UsbId>,
    /// Effective V4L2 capability bits (`device_caps`), if the node could be queried.
    pub caps: Option<u32>,
    /// Whether this node can capture video (not metadata-only).
    pub capture: bool,
    /// Whether this node is a v4l2loopback device.
    pub loopback: bool,
    pub source: DiscoverySource,
    /// What the node offers; `None` when it was not (or could not be)
    /// enumerated (loopback nodes are never opened to enumerate).
    #[serde(skip)]
    pub capabilities: Option<Capabilities>,
}

impl DeviceInfo {
    /// Derives `capture` from V4L2 capability bits: true when the node offers
    /// single- or multi-planar video capture. Metadata-only nodes are false.
    pub fn capture_from_caps(caps: u32) -> bool {
        caps & (CAP_VIDEO_CAPTURE | CAP_VIDEO_CAPTURE_MPLANE) != 0
    }

    /// The effective capability set: `device_caps` when the driver reports it,
    /// otherwise the legacy `capabilities` field.
    pub fn effective_caps(capabilities: u32, device_caps: u32) -> u32 {
        if capabilities & CAP_DEVICE_CAPS != 0 {
            device_caps
        } else {
            capabilities
        }
    }

    /// Whether a driver name / bus-info pair identifies a v4l2loopback node.
    pub fn is_loopback_driver(driver: Option<&str>, bus_info: Option<&str>) -> bool {
        driver.is_some_and(|d| d.trim() == LOOPBACK_DRIVER)
            || bus_info.is_some_and(|b| b.trim().starts_with("platform:v4l2loopback"))
    }

    /// Whether the node is a colour camera: a non-loopback capture node that
    /// offers at least one ingestible format (see [`crate::capability`]).
    pub fn is_colour_camera(&self) -> bool {
        self.capture
            && !self.loopback
            && self
                .capabilities
                .as_ref()
                .is_some_and(Capabilities::is_colour)
    }

    fn describe(&self) -> String {
        let mut s = format!("{} {:?}", self.path.display(), self.card);
        if let Some(u) = self.usb {
            s.push_str(&format!(" usb={u}"));
        }
        if self.loopback {
            s.push_str(" [loopback]");
        } else if !self.capture {
            s.push_str(" [no video capture]");
        } else {
            match &self.capabilities {
                None => s.push_str(" [formats unknown]"),
                Some(c) if !c.is_colour() => {
                    s.push_str(&format!(" [not colour: {}]", c.describe_formats()))
                }
                Some(_) => {}
            }
        }
        s
    }
}

impl fmt::Display for DeviceInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}

/// Why a device could not be selected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SelectError {
    #[error("no camera matches {selector}; video-capture devices seen: {}", list_or_none(.candidates))]
    NoCamera {
        selector: String,
        candidates: Vec<String>,
    },
    #[error("camera selector {selector} is ambiguous, it matches: {}; use a more specific name, the USB vid:pid or the /dev/videoN path", .matches.join(", "))]
    AmbiguousCamera {
        selector: String,
        matches: Vec<String>,
    },
    #[error(
        "{device} is not a colour camera: it offers no format the pipeline can ingest (formats: {formats}); greyscale/IR and metadata nodes cannot be used"
    )]
    NotColour { device: String, formats: String },
    #[error("{device} is not a camera: {reason}")]
    NotACamera { device: String, reason: String },
    #[error("no colour camera found; video-capture devices seen: {}", list_or_none(.seen))]
    NoColourCamera { seen: Vec<String> },
    #[error("no v4l2loopback device labelled {label:?}; loopback devices seen: {} (see docs/setup/loopback.md)", list_or_none(.available))]
    NoSink {
        label: String,
        available: Vec<String>,
    },
    #[error("{count} v4l2loopback devices are labelled {label:?}; labels must be unique")]
    AmbiguousSink { label: String, count: usize },
    #[error("{path} is not a known v4l2loopback device (seen: {})", list_or_none(.available))]
    SinkNotLoopback {
        path: String,
        available: Vec<String>,
    },
}

fn list_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "none".to_string()
    } else {
        items.join(", ")
    }
}

/// A selected camera and the mode chosen for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraChoice<'a> {
    pub device: &'a DeviceInfo,
    pub choice: ModeChoice,
    /// No selector was given: the device was picked automatically.
    pub auto: bool,
}

fn not_colour(d: &DeviceInfo) -> SelectError {
    SelectError::NotColour {
        device: d.describe(),
        formats: d.capabilities.as_ref().map_or_else(
            || "could not be enumerated".into(),
            |c| c.describe_formats(),
        ),
    }
}

fn choose_for<'a>(
    d: &'a DeviceInfo,
    request: &ModeRequest,
    auto: bool,
) -> Result<CameraChoice<'a>, SelectError> {
    let choice = d
        .capabilities
        .as_ref()
        .and_then(|c| choose_mode(c, request))
        .ok_or_else(|| not_colour(d))?;
    Ok(CameraChoice {
        device: d,
        choice,
        auto,
    })
}

/// Picks the camera and its capture mode.
///
/// * `Some(selector)`: the one colour camera the selector matches. A path
///   must name a known capture node; a name or USB id that matches several
///   colour nodes is ambiguous. A matching node that is not a colour camera
///   (greyscale/IR, metadata-only, loopback) is a clear error, never a
///   silent fallback to another device.
/// * `None`: the first colour camera, in list order (discovery sorts by
///   `/dev/videoN`), that offers the request exactly; if none does, the
///   camera whose best mode ranks first by the rule in
///   [`crate::capability`] (ties go to the earlier camera).
///
/// The mode is then chosen by [`choose_mode`]. Loopback devices (including
/// our own output, which becomes a capture node while we stream into it) are
/// never candidates. Duplicate entries for the same path count once.
pub fn select_camera<'a>(
    devices: &'a [DeviceInfo],
    selector: Option<&CameraSelector>,
    request: &ModeRequest,
) -> Result<CameraChoice<'a>, SelectError> {
    let mut unique: Vec<&DeviceInfo> = Vec::new();
    for d in devices {
        if !unique.iter().any(|c| c.path == d.path) {
            unique.push(d);
        }
    }
    let capture: Vec<&DeviceInfo> = unique
        .iter()
        .copied()
        .filter(|d| d.capture && !d.loopback)
        .collect();
    let seen = || capture.iter().map(|d| d.describe()).collect::<Vec<_>>();

    let Some(selector) = selector else {
        let colour: Vec<&DeviceInfo> = capture
            .iter()
            .copied()
            .filter(|d| d.is_colour_camera())
            .collect();
        let choices: Vec<CameraChoice<'a>> = colour
            .iter()
            .filter_map(|d| choose_for(d, request, true).ok())
            .collect();
        if let Some(exact) = choices.iter().find(|c| c.choice.exact) {
            return Ok(exact.clone());
        }
        // min_by_key keeps the first of equal keys: list order breaks ties.
        return choices
            .into_iter()
            .min_by_key(|c| selection_key(&c.choice.mode, request))
            .ok_or_else(|| SelectError::NoColourCamera { seen: seen() });
    };

    if let CameraSelector::Path(path) = selector {
        let Some(d) = unique.iter().copied().find(|d| &d.path == path) else {
            return Err(SelectError::NoCamera {
                selector: selector.to_string(),
                candidates: seen(),
            });
        };
        if d.loopback {
            return Err(SelectError::NotACamera {
                device: d.describe(),
                reason: "it is a v4l2loopback device".into(),
            });
        }
        if !d.capture {
            return Err(SelectError::NotACamera {
                device: d.describe(),
                reason: "it has no video capture (a metadata node?)".into(),
            });
        }
        return choose_for(d, request, false);
    }

    let matches: Vec<&DeviceInfo> = capture
        .iter()
        .copied()
        .filter(|d| selector.matches(&d.path, &d.card, d.usb))
        .collect();
    let colour: Vec<&DeviceInfo> = matches
        .iter()
        .copied()
        .filter(|d| d.is_colour_camera())
        .collect();
    match (colour.as_slice(), matches.as_slice()) {
        ([one], _) => choose_for(one, request, false),
        ([], []) => Err(SelectError::NoCamera {
            selector: selector.to_string(),
            candidates: seen(),
        }),
        ([], [first, ..]) => Err(not_colour(first)),
        (many, _) => Err(SelectError::AmbiguousCamera {
            selector: selector.to_string(),
            matches: many.iter().map(|d| d.describe()).collect(),
        }),
    }
}

/// Picks the v4l2loopback sink.
///
/// A label must match one loopback card name exactly (after trimming). An
/// explicit path must be a known loopback device, so a typo can never point
/// the output at a real camera.
pub fn select_sink<'a>(
    devices: &'a [DeviceInfo],
    selector: &SinkSelector,
) -> Result<&'a DeviceInfo, SelectError> {
    let mut loopbacks: Vec<&DeviceInfo> = Vec::new();
    for d in devices.iter().filter(|d| d.loopback) {
        if !loopbacks.iter().any(|c| c.path == d.path) {
            loopbacks.push(d);
        }
    }
    let available = || loopbacks.iter().map(|d| d.describe()).collect::<Vec<_>>();
    match selector {
        SinkSelector::Label(label) => {
            let want = label.trim();
            let matches: Vec<&DeviceInfo> = loopbacks
                .iter()
                .copied()
                .filter(|d| d.card.trim() == want)
                .collect();
            match matches.as_slice() {
                [one] => Ok(one),
                [] => Err(SelectError::NoSink {
                    label: label.clone(),
                    available: available(),
                }),
                many => Err(SelectError::AmbiguousSink {
                    label: label.clone(),
                    count: many.len(),
                }),
            }
        }
        SinkSelector::Path(path) => loopbacks
            .iter()
            .copied()
            .find(|d| &d.path == path)
            .ok_or_else(|| SelectError::SinkNotLoopback {
                path: path.display().to_string(),
                available: available(),
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{IngestPath, parse_v4l2_ctl};
    use obc_core::VideoMode;
    use std::path::Path;

    const BRIO: UsbId = UsbId {
        vendor: 0x046d,
        product: 0x0944,
    };
    const FHD: UsbId = UsbId {
        vendor: 0x5986,
        product: 0x1193,
    };
    const BRIO_CTL: &str = include_str!("../tests/data/mx-brio-046d-0944.v4l2-ctl.txt");
    const FHD_CTL: &str = include_str!("../tests/data/fhd-camera-5986-1193-colour.v4l2-ctl.txt");
    const FHD_IR_CTL: &str = include_str!("../tests/data/fhd-camera-5986-1193-ir.v4l2-ctl.txt");

    fn dev(
        path: &str,
        card: &str,
        usb: Option<UsbId>,
        caps: u32,
        loopback: bool,
        fixture: Option<&str>,
    ) -> DeviceInfo {
        let capture = DeviceInfo::capture_from_caps(caps);
        DeviceInfo {
            path: PathBuf::from(path),
            card: card.to_string(),
            driver: Some(
                if loopback {
                    LOOPBACK_DRIVER
                } else {
                    "uvcvideo"
                }
                .to_string(),
            ),
            usb,
            caps: Some(caps),
            capture,
            loopback,
            source: DiscoverySource::Synthetic,
            capabilities: match fixture {
                Some(text) => Some(parse_v4l2_ctl(text)),
                None if capture && !loopback => Some(Capabilities::default()),
                None => None,
            },
        }
    }

    /// Mirrors a laptop with a built-in FHD camera (colour node, IR node and
    /// their metadata nodes), an external MX Brio with its metadata node, and
    /// two loopback devices, one of which is being fed (so it shows capture
    /// caps). Capabilities come from the captured fixtures.
    fn laptop() -> Vec<DeviceInfo> {
        vec![
            dev(
                "/dev/vidtest0",
                "FHD Camera: FHD Camera",
                Some(FHD),
                0x0420_0001,
                false,
                Some(FHD_CTL),
            ),
            dev(
                "/dev/vidtest1",
                "FHD Camera: FHD Camera",
                Some(FHD),
                0x0480_0000,
                false,
                None,
            ),
            dev(
                "/dev/vidtest2",
                "FHD Camera: FHD IR Camera",
                Some(FHD),
                0x0420_0001,
                false,
                Some(FHD_IR_CTL),
            ),
            dev(
                "/dev/vidtest3",
                "FHD Camera: FHD IR Camera",
                Some(FHD),
                0x0480_0000,
                false,
                None,
            ),
            dev(
                "/dev/vidtest4",
                "MX Brio",
                Some(BRIO),
                0x0420_0001,
                false,
                Some(BRIO_CTL),
            ),
            dev(
                "/dev/vidtest5",
                "MX Brio",
                Some(BRIO),
                0x0480_0000,
                false,
                None,
            ),
            dev(
                "/dev/vidtest10",
                "NVbroadcast",
                None,
                0x0520_0002,
                true,
                None,
            ),
            dev(
                "/dev/vidtest11",
                "Omarchy Broadcast",
                None,
                0x0520_0003,
                true,
                None,
            ),
        ]
    }

    fn sel(s: &str) -> CameraSelector {
        s.parse().unwrap()
    }

    fn req(s: &str) -> ModeRequest {
        s.parse().unwrap()
    }

    fn mode(s: &str) -> VideoMode {
        s.parse().unwrap()
    }

    #[test]
    fn mx_brio_by_usb_id_name_and_path() {
        let devs = laptop();
        for s in ["046d:0944", "brio", "/dev/vidtest4"] {
            let got = select_camera(&devs, Some(&sel(s)), &req("NV12:1920x1080@60")).unwrap();
            assert_eq!(got.device.path, Path::new("/dev/vidtest4"), "{s}");
            assert_eq!(got.choice.mode, mode("NV12:1920x1080@60"));
            assert!(got.choice.exact && !got.auto);
            assert_eq!(got.choice.ingest, IngestPath::GpuDirect);
        }
    }

    #[test]
    fn fhd_by_usb_id_skips_the_ir_node() {
        let devs = laptop();
        // 5986:1193 and "FHD Camera" match the colour and the IR node; only
        // the colour node is a colour camera, so neither is ambiguous.
        for s in ["5986:1193", "FHD Camera"] {
            let got = select_camera(&devs, Some(&sel(s)), &req("1920x1080@60")).unwrap();
            assert_eq!(got.device.path, Path::new("/dev/vidtest0"), "{s}");
            assert_eq!(got.choice.mode, mode("MJPEG:1920x1080@30"));
            assert!(!got.choice.exact);
            assert_eq!(got.choice.ingest, IngestPath::CpuDecode);
        }
    }

    /// A camera that is not on USB (a platform/MIPI or PCI capture device:
    /// sysfs has no idVendor/idProduct) is found by its card name.
    #[test]
    fn camera_without_usb_id_by_name() {
        let mut devs = laptop();
        devs.push(dev(
            "/dev/vidtest20",
            "Integrated Platform Camera",
            None,
            0x0420_0001,
            false,
            Some(FHD_CTL),
        ));
        for s in ["integrated platform", "Platform Camera"] {
            let got = select_camera(&devs, Some(&sel(s)), &req("1280x720@30")).unwrap();
            assert_eq!(got.device.path, Path::new("/dev/vidtest20"), "{s}");
            assert_eq!(got.device.usb, None);
            assert_eq!(got.choice.mode, mode("MJPEG:1280x720@30"));
            assert!(got.choice.exact && !got.auto);
        }
        // No USB id selector can reach it; the FHD id still picks the FHD node.
        let got = select_camera(&devs, Some(&sel("5986:1193")), &req("1280x720@30")).unwrap();
        assert_eq!(got.device.path, Path::new("/dev/vidtest0"));
    }

    #[test]
    fn ir_node_is_never_a_colour_candidate() {
        let devs = laptop();
        let ir = &devs[2];
        assert!(ir.capture && !ir.is_colour_camera());
        // Explicit path: a clear error, no fallback to another node.
        let err =
            select_camera(&devs, Some(&sel("/dev/vidtest2")), &req("640x360@15")).unwrap_err();
        assert!(matches!(err, SelectError::NotColour { .. }), "{err:?}");
        let msg = err.to_string();
        assert!(
            msg.contains("not a colour camera") && msg.contains("GREY"),
            "{msg}"
        );
        // By a name only the IR node matches.
        assert!(matches!(
            select_camera(&devs, Some(&sel("FHD IR")), &req("640x360@15")),
            Err(SelectError::NotColour { .. })
        ));
        // Never auto-picked, even when it is the only capture node left.
        let only_ir: Vec<_> = devs
            .iter()
            .filter(|d| d.path == Path::new("/dev/vidtest2"))
            .cloned()
            .collect();
        assert!(matches!(
            select_camera(&only_ir, None, &req("640x360@15")),
            Err(SelectError::NoColourCamera { .. })
        ));
    }

    #[test]
    fn explicit_paths_to_non_cameras_are_errors() {
        let devs = laptop();
        let r = req("1920x1080@30");
        let meta = select_camera(&devs, Some(&sel("/dev/vidtest1")), &r).unwrap_err();
        assert!(matches!(meta, SelectError::NotACamera { .. }), "{meta:?}");
        let lb = select_camera(&devs, Some(&sel("/dev/vidtest11")), &r).unwrap_err();
        assert!(lb.to_string().contains("v4l2loopback"), "{lb}");
        let missing = select_camera(&devs, Some(&sel("/dev/vidtest99")), &r).unwrap_err();
        assert!(
            matches!(missing, SelectError::NoCamera { .. }),
            "{missing:?}"
        );
    }

    #[test]
    fn auto_pick_takes_the_first_exact_camera() {
        let devs = laptop();
        // 1080p60: the FHD camera (listed first) only degrades; the Brio is exact.
        let got = select_camera(&devs, None, &req("1920x1080@60")).unwrap();
        assert!(got.auto);
        assert_eq!(got.device.path, Path::new("/dev/vidtest4"));
        assert_eq!(got.choice.mode, mode("NV12:1920x1080@60"));
        // 720p30: both are exact; the first in list order wins.
        let got = select_camera(&devs, None, &req("1280x720@30")).unwrap();
        assert_eq!(got.device.path, Path::new("/dev/vidtest0"));
        assert_eq!(got.choice.mode, mode("MJPEG:1280x720@30"));
        assert!(got.choice.exact);
        // A format request is part of exactness: NV12 720p30 is only on the Brio.
        let got = select_camera(&devs, None, &req("NV12:1280x720@30")).unwrap();
        assert_eq!(got.device.path, Path::new("/dev/vidtest4"));
    }

    #[test]
    fn auto_pick_takes_the_best_degraded_camera() {
        let devs = laptop();
        // 4K60 is offered nowhere. The Brio keeps the size (MJPEG 4K30); the
        // FHD camera would drop to 1080p30. Size ranks first.
        let got = select_camera(&devs, None, &req("3840x2160@60")).unwrap();
        assert_eq!(got.device.path, Path::new("/dev/vidtest4"));
        assert_eq!(got.choice.mode, mode("MJPEG:3840x2160@30"));
        assert!(!got.choice.exact);
        // Without the Brio, the FHD camera degrades.
        let no_brio: Vec<_> = devs
            .iter()
            .filter(|d| d.usb != Some(BRIO))
            .cloned()
            .collect();
        let got = select_camera(&no_brio, None, &req("1920x1080@60")).unwrap();
        assert_eq!(got.device.path, Path::new("/dev/vidtest0"));
        assert_eq!(got.choice.mode, mode("MJPEG:1920x1080@30"));
        assert!(!got.choice.exact);
        // Equal degraded matches: the earlier camera wins.
        let mut twins = no_brio.clone();
        let mut twin = twins[0].clone();
        twin.path = PathBuf::from("/dev/vidtest6");
        twins.push(twin);
        let got = select_camera(&twins, None, &req("1920x1080@60")).unwrap();
        assert_eq!(got.device.path, Path::new("/dev/vidtest0"));
    }

    #[test]
    fn identical_cameras_by_usb_id_are_ambiguous() {
        let mut devs = laptop();
        let mut twin = devs[4].clone();
        twin.path = PathBuf::from("/dev/vidtest6");
        devs.push(twin);
        let err = select_camera(&devs, Some(&sel("046d:0944")), &req("1920x1080@60")).unwrap_err();
        match &err {
            SelectError::AmbiguousCamera { matches, .. } => assert_eq!(matches.len(), 2),
            other => panic!("unexpected {other:?}"),
        }
        assert!(err.to_string().contains("/dev/videoN"), "{err}");
        // The path still disambiguates.
        let got = select_camera(&devs, Some(&sel("/dev/vidtest6")), &req("1920x1080@60"));
        assert_eq!(got.unwrap().device.path, Path::new("/dev/vidtest6"));
    }

    #[test]
    fn metadata_nodes_are_never_candidates() {
        // Only the metadata node of the Brio is present.
        let devs: Vec<_> = laptop()
            .into_iter()
            .filter(|d| d.path != Path::new("/dev/vidtest4"))
            .collect();
        let err = select_camera(&devs, Some(&sel("MX Brio")), &req("1920x1080@60")).unwrap_err();
        match err {
            SelectError::NoCamera { candidates, .. } => {
                assert!(candidates.iter().all(|c| !c.contains("MX Brio")));
                assert!(candidates.iter().any(|c| c.contains("FHD Camera")));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn loopback_devices_are_never_cameras() {
        let devs = laptop();
        assert!(matches!(
            select_camera(&devs, Some(&sel("Broadcast")), &req("1920x1080@60")),
            Err(SelectError::NoCamera { .. })
        ));
        let only_loopbacks: Vec<_> = devs.iter().filter(|d| d.loopback).cloned().collect();
        assert!(matches!(
            select_camera(&only_loopbacks, None, &req("1920x1080@60")),
            Err(SelectError::NoColourCamera { .. })
        ));
    }

    #[test]
    fn duplicate_entries_count_once() {
        let mut devs = laptop();
        let mut dup = devs[4].clone();
        dup.source = DiscoverySource::DeviceMonitor;
        devs.push(dup);
        assert!(select_camera(&devs, Some(&sel("046d:0944")), &req("1920x1080@60")).is_ok());
    }

    #[test]
    fn unenumerated_capture_node_is_not_colour() {
        let mut devs = laptop();
        devs[4].capabilities = None;
        let err =
            select_camera(&devs, Some(&sel("/dev/vidtest4")), &req("1920x1080@60")).unwrap_err();
        assert!(err.to_string().contains("could not be enumerated"), "{err}");
    }

    #[test]
    fn no_camera_error_is_clear() {
        let devs: Vec<DeviceInfo> = Vec::new();
        let msg = select_camera(&devs, Some(&sel("MX Brio")), &req("1920x1080@60"))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("no camera matches"), "{msg}");
        assert!(msg.contains("none"), "{msg}");
        let msg = select_camera(&devs, None, &req("1920x1080@60"))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("no colour camera"), "{msg}");
    }

    #[test]
    fn sink_by_label() {
        let devs = laptop();
        let got = select_sink(&devs, &SinkSelector::default()).unwrap();
        assert_eq!(got.path, PathBuf::from("/dev/vidtest11"));
        let got = select_sink(&devs, &SinkSelector::Label(" NVbroadcast ".into())).unwrap();
        assert_eq!(got.path, PathBuf::from("/dev/vidtest10"));
    }

    #[test]
    fn missing_sink_label_lists_loopbacks() {
        let devs: Vec<_> = laptop()
            .into_iter()
            .filter(|d| d.card != "Omarchy Broadcast")
            .collect();
        let err = select_sink(&devs, &SinkSelector::default()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("NVbroadcast"), "{msg}");
        assert!(msg.contains("docs/setup/loopback.md"), "{msg}");
    }

    #[test]
    fn duplicate_sink_label_is_ambiguous() {
        let mut devs = laptop();
        devs.push(dev(
            "/dev/vidtest12",
            "Omarchy Broadcast",
            None,
            0x0520_0002,
            true,
            None,
        ));
        assert!(matches!(
            select_sink(&devs, &SinkSelector::default()),
            Err(SelectError::AmbiguousSink { count: 2, .. })
        ));
    }

    #[test]
    fn sink_path_must_be_loopback() {
        let devs = laptop();
        let ok = select_sink(&devs, &SinkSelector::Path("/dev/vidtest10".into())).unwrap();
        assert!(ok.loopback);
        assert!(matches!(
            select_sink(&devs, &SinkSelector::Path("/dev/vidtest4".into())),
            Err(SelectError::SinkNotLoopback { .. })
        ));
    }

    #[test]
    fn caps_helpers() {
        assert!(DeviceInfo::capture_from_caps(0x0420_0001));
        assert!(DeviceInfo::capture_from_caps(CAP_VIDEO_CAPTURE_MPLANE));
        assert!(!DeviceInfo::capture_from_caps(CAP_META_CAPTURE));
        assert!(!DeviceInfo::capture_from_caps(CAP_VIDEO_OUTPUT));
        // device_caps honoured only when V4L2_CAP_DEVICE_CAPS is set.
        assert_eq!(
            DeviceInfo::effective_caps(0x84a0_0001, 0x0420_0001),
            0x0420_0001
        );
        assert_eq!(
            DeviceInfo::effective_caps(0x0000_0003, 0x0000_0001),
            0x0000_0003
        );
        assert!(DeviceInfo::is_loopback_driver(Some("v4l2 loopback"), None));
        assert!(DeviceInfo::is_loopback_driver(
            None,
            Some("platform:v4l2loopback-010")
        ));
        assert!(!DeviceInfo::is_loopback_driver(
            Some("uvcvideo"),
            Some("usb-0000:80:14.0-4")
        ));
    }
}
