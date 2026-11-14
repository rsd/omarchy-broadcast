//! Camera quirks, as data rows keyed by USB vendor and product id.
//!
//! Behaviour that belongs to one camera model is a row here, never a branch
//! in the code: the ingest and the settings model look the rows up by the
//! camera's [`UsbId`] and the rest of the design stays model-independent.
//! Adding a camera is adding a row (a `camera-tune` measurement proposes
//! one). Two kinds of rows exist:
//!
//! * [`RangeQuirk`]: a camera that sends **full-range** frames in a raw
//!   format while its caps label them limited range. The ingest relabels
//!   those frames as full range, so the canonical tail converts them to
//!   the contract's range like any full-range source.
//! * [`ControlQuirk`]: standard controls a camera exposes that do nothing,
//!   or that break the picture when written. The settings model leaves
//!   them out, and nothing stored for them is written to the camera.
//!
//! * [`CameraProfile`]: what a measurement found best for a camera model:
//!   the control values to offer as "recommended settings" (never applied
//!   without the user asking: the controls are the user's, ADR-0020), and
//!   whether its picture is noisy enough to suggest noise removal.
//!
//! The Extension Unit rows of [`crate::xu`] follow the same principle for
//! vendor settings.

use obc_core::{PixelFormat, UsbId};

/// `V4L2_CID_HUE`
pub const CID_HUE: u32 = 0x0098_0903;
/// `V4L2_CID_AUTO_WHITE_BALANCE` (`white_balance_automatic`)
pub const CID_AUTO_WHITE_BALANCE: u32 = 0x0098_090c;
/// `V4L2_CID_WHITE_BALANCE_TEMPERATURE`
pub const CID_WHITE_BALANCE_TEMPERATURE: u32 = 0x0098_091a;

const fn usb(vendor: u16, product: u16) -> UsbId {
    UsbId { vendor, product }
}

/// A camera whose raw frames are full range despite a limited-range label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeQuirk {
    /// The camera model.
    pub usb: UsbId,
    /// The formats it mislabels. MJPEG is never listed: decoded JPEG is
    /// full range and labelled so.
    pub formats: &'static [PixelFormat],
    /// The camera's name, for logs and the documentation.
    pub model: &'static str,
}

impl RangeQuirk {
    /// Whether the row covers `usb` in `format`.
    pub fn applies_to(&self, usb: UsbId, format: PixelFormat) -> bool {
        self.usb == usb && self.formats.contains(&format)
    }
}

/// Measured: luma spans 0–255 (or close to it) on a static, well-lit scene
/// under a limited label (a survey of the cameras at hand, 2026-11-13). The
/// Logitech MX Brio is not listed: re-measured on 2026-11-14, its YUYV and
/// NV12 are limited range, as labelled.
pub const RANGE_QUIRKS: &[RangeQuirk] = &[
    RangeQuirk {
        usb: usb(0x1b3f, 0x2002),
        formats: &[PixelFormat::Yuy2],
        model: "Generalplus GENERAL WEBCAM",
    },
    RangeQuirk {
        usb: usb(0x045e, 0x0766),
        formats: &[PixelFormat::Yuy2],
        model: "Microsoft LifeCam VX-800",
    },
];

/// Whether the camera `usb` sends full-range frames in `format` under a
/// limited-range label ([`RANGE_QUIRKS`]). `None` (not a USB camera) never
/// matches.
pub fn full_range_despite_label(usb: Option<UsbId>, format: PixelFormat) -> bool {
    usb.is_some_and(|u| RANGE_QUIRKS.iter().any(|q| q.applies_to(u, format)))
}

/// Why a [`ControlQuirk`] hides a control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HiddenBecause {
    /// The camera accepts writes and nothing changes (measured).
    NoEffect,
    /// Writing it fails or breaks the picture (measured).
    Broken,
}

/// Standard controls of one camera model that the settings model leaves
/// out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlQuirk {
    /// The camera model.
    pub usb: UsbId,
    /// The V4L2 control ids it hides.
    pub hidden: &'static [u32],
    pub because: HiddenBecause,
    /// Hidden controls written back to their default at every open, so a
    /// camera another program left in a broken state recovers (only
    /// controls whose default write is known to be safe).
    pub reset_at_open: &'static [u32],
    /// The camera's name, for logs and the documentation.
    pub model: &'static str,
}

/// Measured in the camera survey of 2026-11-13.
pub const CONTROL_QUIRKS: &[ControlQuirk] = &[
    // Brightness 16, 64 and 200, hue -100 and +100, and contrast change
    // nothing in the picture: the camera's own processing ignores them.
    ControlQuirk {
        usb: usb(0x1b3f, 0x2002),
        hidden: &[
            crate::controls::CID_BRIGHTNESS,
            crate::controls::CID_CONTRAST,
            CID_HUE,
        ],
        because: HiddenBecause::NoEffect,
        reset_at_open: &[],
        model: "Generalplus GENERAL WEBCAM",
    },
    // A manual white balance write fails with EPROTO, and the picture then
    // turns strongly yellow-green. Automatic white balance works, so the
    // switch to manual is hidden with the temperature.
    ControlQuirk {
        usb: usb(0x045e, 0x0766),
        hidden: &[CID_AUTO_WHITE_BALANCE, CID_WHITE_BALANCE_TEMPERATURE],
        because: HiddenBecause::Broken,
        // Back to automatic white balance; the temperature is never
        // written (that write is what fails).
        reset_at_open: &[CID_AUTO_WHITE_BALANCE],
        model: "Microsoft LifeCam VX-800",
    },
];

/// The control ids the camera `usb` hides ([`CONTROL_QUIRKS`]), in table
/// order; empty for `None` or a camera with no row.
pub fn hidden_controls(usb: Option<UsbId>) -> Vec<u32> {
    let Some(u) = usb else {
        return Vec::new();
    };
    CONTROL_QUIRKS
        .iter()
        .filter(|q| q.usb == u)
        .flat_map(|q| q.hidden.iter().copied())
        .collect()
}

/// The hidden controls of the camera `usb` to write back to their default
/// at every open ([`ControlQuirk::reset_at_open`]).
pub fn reset_at_open(usb: Option<UsbId>) -> Vec<u32> {
    let Some(u) = usb else {
        return Vec::new();
    };
    CONTROL_QUIRKS
        .iter()
        .filter(|q| q.usb == u)
        .flat_map(|q| q.reset_at_open.iter().copied())
        .collect()
}

/// Whether the camera `usb` hides the control `cid`.
pub fn is_hidden(usb: Option<UsbId>, cid: u32) -> bool {
    hidden_controls(usb).contains(&cid)
}

/// What a measurement found best for one camera model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CameraProfile {
    /// The camera model.
    pub usb: UsbId,
    /// The recommended settings: every control at the camera's default,
    /// except these `(V4L2 control id, value)` pairs. An empty list means
    /// "the defaults are best". The power-line frequency is never listed:
    /// it follows the region, not the camera.
    pub values: &'static [(u32, i64)],
    /// Whether the picture is noisy at its usual light (temporal noise in
    /// the mid-tones above about 1 code value, or noise removal measured
    /// to clean it visibly): noise removal is suggested.
    pub noisy: bool,
    /// The camera's name, for logs and the documentation.
    pub model: &'static str,
}

/// Measured in the camera survey of 2026-11-13 (daylight, static scene).
pub const CAMERA_PROFILES: &[CameraProfile] = &[
    // Sharpness 5 gives +17 % detail with no halo (7 shows halos); hue 0
    // undoes the -3 that other software leaves. Noise removal: -60 %.
    CameraProfile {
        usb: usb(0x5986, 0x1193),
        values: &[(crate::controls::CID_SHARPNESS, 5), (CID_HUE, 0)],
        noisy: true,
        model: "FHD Camera (laptop)",
    },
    // Defaults are best; noise removal gains about 20 %, not suggested.
    CameraProfile {
        usb: usb(0x046d, 0x0843),
        values: &[],
        noisy: false,
        model: "Logitech Webcam C930e",
    },
    // Defaults are best; noise removal -35 %.
    CameraProfile {
        usb: usb(0x1b3f, 0x2002),
        values: &[],
        noisy: true,
        model: "Generalplus GENERAL WEBCAM",
    },
    // Defaults are best (manual exposure and gain only add noise);
    // mid-tone temporal noise 1.15.
    CameraProfile {
        usb: usb(0x045e, 0x0766),
        values: &[],
        noisy: true,
        model: "Microsoft LifeCam VX-800",
    },
];

/// The profile of the camera `usb` ([`CAMERA_PROFILES`]); `None` for a
/// camera with no row (no recommendation is offered).
pub fn profile(usb: Option<UsbId>) -> Option<&'static CameraProfile> {
    let u = usb?;
    CAMERA_PROFILES.iter().find(|p| p.usb == u)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VX800: UsbId = usb(0x045e, 0x0766);
    const GENERALPLUS: UsbId = usb(0x1b3f, 0x2002);
    const BRIO: UsbId = usb(0x046d, 0x0944);
    const C930E: UsbId = usb(0x046d, 0x0843);

    #[test]
    fn range_rows_match_by_model_and_format_only() {
        assert!(full_range_despite_label(Some(VX800), PixelFormat::Yuy2));
        assert!(full_range_despite_label(
            Some(GENERALPLUS),
            PixelFormat::Yuy2
        ));
        // A correctly labelled camera (the MX Brio, measured): no row.
        assert!(!full_range_despite_label(Some(BRIO), PixelFormat::Yuy2));
        assert!(!full_range_despite_label(Some(BRIO), PixelFormat::Nv12));
        // Decoded MJPEG is full range and labelled so: never a quirk.
        assert!(!full_range_despite_label(
            Some(GENERALPLUS),
            PixelFormat::Mjpeg
        ));
        // A correctly labelled camera, and a camera with no USB id.
        assert!(!full_range_despite_label(Some(C930E), PixelFormat::Yuy2));
        assert!(!full_range_despite_label(None, PixelFormat::Yuy2));
    }

    #[test]
    fn rows_never_list_mjpeg_or_a_model_twice() {
        for q in RANGE_QUIRKS {
            assert!(!q.formats.contains(&PixelFormat::Mjpeg), "{}", q.model);
        }
        for (i, a) in RANGE_QUIRKS.iter().enumerate() {
            assert!(RANGE_QUIRKS[i + 1..].iter().all(|b| b.usb != a.usb));
        }
        for (i, a) in CONTROL_QUIRKS.iter().enumerate() {
            assert!(CONTROL_QUIRKS[i + 1..].iter().all(|b| b.usb != a.usb));
        }
    }

    #[test]
    fn profiles_by_model() {
        let fhd = profile(Some(usb(0x5986, 0x1193))).unwrap();
        assert!(fhd.values.contains(&(crate::controls::CID_SHARPNESS, 5)));
        assert!(fhd.noisy);
        assert!(profile(Some(VX800)).unwrap().values.is_empty());
        assert!(!profile(Some(C930E)).unwrap().noisy);
        assert!(profile(Some(BRIO)).is_none());
        assert!(profile(None).is_none());
        for (i, a) in CAMERA_PROFILES.iter().enumerate() {
            assert!(CAMERA_PROFILES[i + 1..].iter().all(|b| b.usb != a.usb));
            // Never a hidden control, never the power-line frequency.
            for (cid, _) in a.values {
                assert!(!is_hidden(Some(a.usb), *cid), "{}", a.model);
                assert_ne!(*cid, 0x0098_0918, "{}", a.model);
            }
        }
    }

    #[test]
    fn hidden_controls_by_model() {
        assert_eq!(
            hidden_controls(Some(GENERALPLUS)),
            [
                crate::controls::CID_BRIGHTNESS,
                crate::controls::CID_CONTRAST,
                CID_HUE
            ]
        );
        assert!(is_hidden(Some(VX800), CID_WHITE_BALANCE_TEMPERATURE));
        assert!(is_hidden(Some(VX800), CID_AUTO_WHITE_BALANCE));
        assert!(!is_hidden(Some(VX800), crate::controls::CID_BRIGHTNESS));
        assert!(hidden_controls(Some(C930E)).is_empty());
        assert_eq!(reset_at_open(Some(VX800)), [CID_AUTO_WHITE_BALANCE]);
        assert!(reset_at_open(Some(GENERALPLUS)).is_empty());
        for q in CONTROL_QUIRKS {
            assert!(
                q.reset_at_open.iter().all(|c| q.hidden.contains(c)),
                "{}",
                q.model
            );
        }
        assert!(hidden_controls(None).is_empty());
    }
}
