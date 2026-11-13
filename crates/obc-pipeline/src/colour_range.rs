//! The colour range of a raw camera at the ingest: the range quirk rows
//! applied ([`range_fix`]), and a short look at the first frames of a camera
//! with no row ([`watch_range`]).
//!
//! **A quirk row.** A camera listed in [`crate::quirks::RANGE_QUIRKS`]
//! sends full-range frames in a raw format (YUY2, or NV12 for the MX Brio)
//! under a limited-range label. Its USB id comes from sysfs, by the camera
//! node's device number ([`camera_usb_id`], with
//! [`crate::xu::usb_device_of_node`]). The ingest then treats its frames as
//! full range, so they reach the contract's range like any full-range
//! source ([`RangeFix`]):
//!
//! * **CUDA memory**: the frames are relabelled full range right after the
//!   camera ([`full_range_relabel`]). The CPU convert path's `videoconvert`
//!   keeps that label ([`crate::pipeline::pinned_caps`]), the GPU-direct
//!   path uploads it as it is, and `cudaconvertscale` converts full range
//!   to the contract's range with the rest of the label.
//! * **System memory, limited contract**: `videoconvertscale` does not
//!   convert a range-only difference (see
//!   [`crate::pipeline::ingest_adapter`]), so the levels are applied by
//!   the same `videobalance` the MJPEG path uses
//!   ([`crate::pipeline::full_to_limited_levels`]), and the balanced frames
//!   are then relabelled limited range ([`limited_range_relabel`]), the
//!   matrix, transfer and primaries of the camera's label kept. A listed
//!   camera's label says limited range already, but should one ever label
//!   its frames full, a later conversion would otherwise compress the
//!   balanced frames a second time.
//! * **System memory, full contract**: the frames are relabelled full
//!   range, as in CUDA memory. Their data already is, so a label that
//!   differs from the contract's only in range is passed as it is, and a
//!   matrix conversion (should the labels differ in more) reads them as
//!   full range.
//!
//! **No row.** A raw camera labelled limited whose USB id has no row may
//! still send full range. [`watch_range`] samples the luma of a few of its
//! first frames, subsampled, and logs one line suggesting a row when a
//! meaningful share lies outside 16–235 ([`suggests_full_range`]). It only
//! logs: nothing changes while the camera streams, and the probe removes
//! itself after its last sample.

use std::path::Path;
use std::sync::{Arc, Mutex};

use gst::prelude::*;
use gst_video::prelude::*;
use obc_core::config::VideoMemory;
use obc_core::{ColourRange, PixelFormat, UsbId};

use crate::pipeline::{PipelineError, make, names};

/// What the ingest does about a raw camera's colour range ([`range_fix`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeFix {
    /// Nothing: the camera's label is believed.
    None,
    /// Relabel the camera's frames full range right after it
    /// ([`full_range_relabel`], named [`names::RANGE_QUIRK_RELABEL`]).
    RelabelFull,
    /// Apply the full-to-limited levels after the camera's queue
    /// (`videobalance`, named [`names::RANGE_BALANCE`]), then relabel the
    /// frames limited range ([`limited_range_relabel`]).
    LimitLevels,
}

/// The fix for a camera whose raw frames are full range despite a limited
/// label (`full_range_despite_label`, [`crate::quirks`]) in an ingest of
/// canonical frames in `memory` under a contract of `range`; see the module
/// docs. [`RangeFix::None`] for every other camera.
pub fn range_fix(
    full_range_despite_label: bool,
    memory: VideoMemory,
    range: ColourRange,
) -> RangeFix {
    match (full_range_despite_label, memory, range) {
        (false, _, _) => RangeFix::None,
        (true, VideoMemory::System, ColourRange::Limited) => RangeFix::LimitLevels,
        (true, VideoMemory::Cuda, _) | (true, VideoMemory::System, ColourRange::Full) => {
            RangeFix::RelabelFull
        }
    }
}

/// The USB id of the camera node `node`, from sysfs under `sysfs_root`
/// ([`crate::xu::usb_device_of_node`]: by the node's device number).
/// `None` for a node that is not a USB camera, or not there.
pub fn camera_usb_id(sysfs_root: &Path, node: &Path) -> Option<UsbId> {
    crate::xu::usb_device_of_node(sysfs_root, node)
        .ok()
        .map(|d| d.id)
}

/// The camera node `node`'s USB id (sysfs under `sysfs_root`) and the fix
/// its frames in `format` get in an ingest of canonical frames in `memory`
/// under a contract of `range` ([`range_fix`] of its quirk row).
pub fn camera_range_fix(
    sysfs_root: &Path,
    node: &Path,
    format: PixelFormat,
    memory: VideoMemory,
    range: ColourRange,
) -> (Option<UsbId>, RangeFix) {
    let usb = camera_usb_id(sysfs_root, node);
    let quirk = crate::quirks::full_range_despite_label(usb, format);
    (usb, range_fix(quirk, memory, range))
}

/// The caps a relabel to `range` sets on frames of the raw `caps`: their
/// effective colorimetry (as [`gst_video::VideoInfo`] reads it, defaults
/// filled in) with the range made `range`, the matrix, transfer and
/// primaries kept. `None` for caps that are not raw video.
pub fn range_caps(caps: &gst::CapsRef, range: ColourRange) -> Option<gst::Caps> {
    if caps.structure(0)?.name() != "video/x-raw" {
        return None;
    }
    let info = gst_video::VideoInfo::from_caps(caps).ok()?;
    let c = info.colorimetry();
    let to = gst_video::VideoColorimetry::new(
        match range {
            ColourRange::Full => gst_video::VideoColorRange::Range0_255,
            ColourRange::Limited => gst_video::VideoColorRange::Range16_235,
        },
        c.matrix(),
        c.transfer(),
        c.primaries(),
    );
    format!("video/x-raw,colorimetry={to}").parse().ok()
}

/// [`range_caps`] made full range.
pub fn full_range_caps(caps: &gst::CapsRef) -> Option<gst::Caps> {
    range_caps(caps, ColourRange::Full)
}

/// A `capssetter` ([`names::RANGE_QUIRK_RELABEL`]) that relabels the
/// frames leaving `from` (the camera's capsfilter) as full range
/// ([`full_range_caps`]); see `relabel_after`.
pub fn full_range_relabel(from: &gst::Element) -> Result<gst::Element, PipelineError> {
    relabel_after(from, names::RANGE_QUIRK_RELABEL, ColourRange::Full)
}

/// A `capssetter` ([`names::RANGE_RELABEL`]) that relabels the frames
/// leaving `from` (the full-to-limited `videobalance` of a quirk camera in
/// system memory) as limited range ([`range_caps`]); see
/// `relabel_after`.
pub fn limited_range_relabel(from: &gst::Element) -> Result<gst::Element, PipelineError> {
    relabel_after(from, names::RANGE_RELABEL, ColourRange::Limited)
}

/// A `capssetter` named `name` that relabels the frames leaving `from` as
/// `range` ([`range_caps`]). The label is only known once the camera has
/// negotiated, so a probe on `from`'s `src` pad sets the relabel's caps
/// from the CAPS event, before the event reaches it. Panic-free (probe
/// context).
fn relabel_after(
    from: &gst::Element,
    name: &str,
    range: ColourRange,
) -> Result<gst::Element, PipelineError> {
    let relabel = make("capssetter", name)?;
    let pad = from
        .static_pad("src")
        .ok_or_else(|| PipelineError::Link(format!("{} has no src pad", from.name())))?;
    let to = relabel.downgrade();
    pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
        let Some(gst::EventView::Caps(ev)) = info.event().map(|e| e.view()) else {
            return gst::PadProbeReturn::Ok;
        };
        let (Some(caps), Some(to)) = (range_caps(ev.caps(), range), to.upgrade()) else {
            return gst::PadProbeReturn::Ok;
        };
        if to.property::<Option<gst::Caps>>("caps").as_ref() != Some(&caps) {
            to.set_property("caps", &caps);
        }
        gst::PadProbeReturn::Ok
    });
    Ok(relabel)
}

/// Where the luma of a raw format sits: bytes per luma sample in plane 0
/// and the offset of the first one. `None` for a format without a luma
/// plane (RGB, BGR) or not raw (MJPEG).
pub fn luma_layout(format: PixelFormat) -> Option<(usize, usize)> {
    match format {
        PixelFormat::Nv12 | PixelFormat::Nv21 | PixelFormat::I420 | PixelFormat::Yv12 => {
            Some((1, 0))
        }
        PixelFormat::Yuy2 => Some((2, 0)),
        PixelFormat::Uyvy => Some((2, 1)),
        PixelFormat::Rgb | PixelFormat::Bgr | PixelFormat::Mjpeg => None,
    }
}

/// Adds the luma of every `step`-th pixel of every `step`-th row of a frame
/// to `hist`: `plane` its luma plane (`stride` bytes a row, `width` x
/// `height` pixels), the luma laid out as [`luma_layout`] says (`pixel`
/// bytes a sample from `offset`). Rows or samples past the end of `plane`
/// are skipped.
pub fn sample_luma(
    plane: &[u8],
    stride: usize,
    (width, height): (usize, usize),
    (pixel, offset): (usize, usize),
    step: usize,
    hist: &mut [u64; 256],
) {
    let step = step.max(1);
    for y in (0..height).step_by(step) {
        for x in (0..width).step_by(step) {
            if let Some(&v) = plane.get(y * stride + x * pixel + offset) {
                hist[usize::from(v)] += 1;
            }
        }
    }
}

/// How far luma may lie outside 16–235 before it counts as outside:
/// a correctly labelled camera overshoots the limited range by a level or
/// two (ringing at edges, rounding in its scaler).
pub const RANGE_MARGIN: u8 = 2;

/// The share of luma samples outside 16–235 (beyond [`RANGE_MARGIN`]) from
/// which a camera labelled limited is suspected of full range: 1 %.
///
/// Measured on static, well-lit scenes: the mislabelled cameras put 4.4 %
/// (Microsoft LifeCam VX-800, above 235) and 5.8 % (Generalplus, below 16)
/// of their luma outside the limited range, while a correctly labelled one
/// (Logitech C930e) stays within it or overshoots by a level or two, which
/// the margin absorbs. 1 % sits more than four times below the smaller
/// measured share, and far above what a correct camera produces, so a
/// mislabelled camera is noticed even on a scene with less highlight or
/// shadow than the measurement's. A scene with neither (a grey wall) shows
/// nothing either way: the check only ever suggests.
pub const SUSPECT_SHARE: f64 = 0.01;

/// Fewer samples than this say nothing (a frame too small, a camera that
/// stopped).
pub const MIN_SAMPLES: u64 = 1000;

/// What a luma histogram shows about the range.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RangeEvidence {
    /// The lowest and highest luma seen.
    pub min: u8,
    pub max: u8,
    /// Samples below 16 and above 235, beyond [`RANGE_MARGIN`].
    pub outside: u64,
    pub samples: u64,
}

impl RangeEvidence {
    /// The share of samples outside the limited range (0 to 1).
    pub fn share(&self) -> f64 {
        if self.samples == 0 {
            0.0
        } else {
            self.outside as f64 / self.samples as f64
        }
    }
}

/// The evidence in a luma histogram; `None` when it is empty.
pub fn range_evidence(hist: &[u64; 256]) -> Option<RangeEvidence> {
    let min = hist.iter().position(|&n| n > 0)?;
    let max = hist.iter().rposition(|&n| n > 0)?;
    let low = usize::from(16 - RANGE_MARGIN);
    let high = usize::from(235 + RANGE_MARGIN);
    let outside = hist[..low].iter().sum::<u64>() + hist[high + 1..].iter().sum::<u64>();
    Some(RangeEvidence {
        min: u8::try_from(min).unwrap_or(0),
        max: u8::try_from(max).unwrap_or(255),
        outside,
        samples: hist.iter().sum(),
    })
}

/// Whether a camera labelled limited whose luma histogram is `hist` is
/// suspected of sending full range ([`SUSPECT_SHARE`], [`MIN_SAMPLES`]):
/// the evidence when it is.
pub fn suggests_full_range(hist: &[u64; 256]) -> Option<RangeEvidence> {
    range_evidence(hist).filter(|e| e.samples >= MIN_SAMPLES && e.share() >= SUSPECT_SHARE)
}

/// Frames skipped before the first sample: the camera's exposure settles.
pub const WATCH_SKIP: u64 = 30;
/// Frames between two samples.
pub const WATCH_EVERY: u64 = 15;
/// Frames sampled; the probe then removes itself (about 3.5 s at 30 fps).
pub const WATCH_FRAMES: u32 = 6;
/// Pixels between two samples, in both directions (a 1080p frame gives
/// about 32,000 samples).
pub const WATCH_STEP: usize = 8;

/// How a [`watch_range`] ended.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WatchOutcome {
    /// The frames are not labelled limited range: nothing to look at.
    NotLimited,
    /// All [`WATCH_FRAMES`] were sampled: the evidence when the camera is
    /// suspected of full range (and the line logged), `None` otherwise.
    Sampled(Option<RangeEvidence>),
}

#[derive(Debug)]
struct Watch {
    seen: u64,
    sampled: u32,
    hist: [u64; 256],
    outcome: Option<WatchOutcome>,
}

/// A [`watch_range`] in progress or done.
#[derive(Debug)]
pub struct RangeWatch(Mutex<Watch>);

impl RangeWatch {
    /// How it ended; `None` while it is still sampling.
    pub fn outcome(&self) -> Option<WatchOutcome> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).outcome
    }

    /// One camera buffer leaving `pad`: whether the probe stays.
    fn on_buffer(
        &self,
        pad: &gst::Pad,
        buffer: Option<&gst::BufferRef>,
        usb: UsbId,
        format: PixelFormat,
        layout: (usize, usize),
    ) -> bool {
        let mut w = self.0.lock().unwrap_or_else(|e| e.into_inner());
        w.seen += 1;
        if w.seen < WATCH_SKIP || !(w.seen - WATCH_SKIP).is_multiple_of(WATCH_EVERY) {
            return true;
        }
        let Some(info) = pad
            .current_caps()
            .and_then(|c| gst_video::VideoInfo::from_caps(&c).ok())
        else {
            return true;
        };
        if info.colorimetry().range() != gst_video::VideoColorRange::Range16_235 {
            w.outcome = Some(WatchOutcome::NotLimited);
            return false;
        }
        let Some(frame) =
            buffer.and_then(|b| gst_video::VideoFrameRef::from_buffer_ref_readable(b, &info).ok())
        else {
            return true;
        };
        let (Ok(plane), Some(&stride)) = (frame.plane_data(0), frame.plane_stride().first()) else {
            return true;
        };
        let size = (info.width() as usize, info.height() as usize);
        let stride = usize::try_from(stride).unwrap_or(0);
        sample_luma(plane, stride, size, layout, WATCH_STEP, &mut w.hist);
        w.sampled += 1;
        if w.sampled < WATCH_FRAMES {
            return true;
        }
        let suspected = suggests_full_range(&w.hist);
        if let Some(e) = suspected {
            eprintln!(
                "camera {usb} ({format}): luma spans {}-{} with {:.1} % of samples outside \
                 16-235 under a limited-range label: it may send full range. If a dark and a \
                 bright scene confirm it, add a RangeQuirk row for {usb}, {format} \
                 (crates/obc-pipeline/src/quirks.rs)",
                e.min,
                e.max,
                e.share() * 100.0
            );
        }
        w.outcome = Some(WatchOutcome::Sampled(suspected));
        false
    }
}

/// Watches the first frames of a raw camera with no range row (`usb`
/// `format`) leaving `camera_caps` (the camera's capsfilter): every
/// [`WATCH_EVERY`]-th frame from the [`WATCH_SKIP`]-th, [`WATCH_FRAMES`] in
/// all, subsampled every [`WATCH_STEP`] pixels. When the frames are
/// labelled limited range and [`suggests_full_range`], it logs one line
/// suggesting a [`crate::quirks::RangeQuirk`] row. It changes nothing in
/// the stream, and the probe removes itself after its last sample (or at
/// its first, for frames labelled other than limited). `None` (no probe)
/// for a format without luma ([`luma_layout`]). Panic-free (probe
/// context).
pub fn watch_range(
    camera_caps: &gst::Element,
    usb: UsbId,
    format: PixelFormat,
) -> Option<Arc<RangeWatch>> {
    let layout = luma_layout(format)?;
    let pad = camera_caps.static_pad("src")?;
    let watch = Arc::new(RangeWatch(Mutex::new(Watch {
        seen: 0,
        sampled: 0,
        hist: [0; 256],
        outcome: None,
    })));
    let probe = Arc::clone(&watch);
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        if probe.on_buffer(pad, info.buffer().map(|b| &**b), usb, format, layout) {
            gst::PadProbeReturn::Ok
        } else {
            gst::PadProbeReturn::Remove
        }
    });
    Some(watch)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    use std::path::PathBuf;

    use super::*;

    const VX800: UsbId = UsbId {
        vendor: 0x045e,
        product: 0x0766,
    };

    /// A fake sysfs where `dev/char/<major:minor of /dev/null>` is a
    /// video4linux class device of the USB camera `vendor:product`, and a
    /// node linking to `/dev/null` (a real character device, which a test
    /// cannot create). As in [`crate::xu`]'s tests.
    fn fake_sysfs(vendor: &str, product: &str) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let usb = root.path().join("devices/pci0000:00/usb1/1-2");
        let class_dev = usb.join("1-2:1.0/video4linux/video7");
        fs::create_dir_all(&class_dev).unwrap();
        std::os::unix::fs::symlink("../../../1-2:1.0", class_dev.join("device")).unwrap();
        fs::write(usb.join("idVendor"), vendor).unwrap();
        fs::write(usb.join("idProduct"), product).unwrap();
        fs::write(usb.join("bConfigurationValue"), "1\n").unwrap();
        fs::write(usb.join("descriptors"), b"").unwrap();
        fs::create_dir_all(root.path().join("dev/char")).unwrap();
        let rdev = fs::metadata("/dev/null").unwrap().rdev();
        std::os::unix::fs::symlink(
            "../../devices/pci0000:00/usb1/1-2/1-2:1.0/video4linux/video7",
            root.path().join("dev/char").join(format!(
                "{}:{}",
                libc::major(rdev),
                libc::minor(rdev)
            )),
        )
        .unwrap();
        let node = root.path().join("video0");
        std::os::unix::fs::symlink("/dev/null", &node).unwrap();
        (root, node)
    }

    #[test]
    fn a_quirk_relabels_in_cuda_memory_and_balances_in_system_memory() {
        use ColourRange::{Full, Limited};
        use VideoMemory::{Cuda, System};
        for (memory, range, want) in [
            (Cuda, Limited, RangeFix::RelabelFull),
            (Cuda, Full, RangeFix::RelabelFull),
            (System, Limited, RangeFix::LimitLevels),
            (System, Full, RangeFix::RelabelFull),
        ] {
            assert_eq!(range_fix(true, memory, range), want, "{memory:?} {range}");
            assert_eq!(range_fix(false, memory, range), RangeFix::None);
        }
    }

    /// The decision over a fake sysfs: the node's USB id selects the row,
    /// and the row covers its formats only.
    #[test]
    fn the_quirk_row_is_found_through_sysfs_by_the_node() {
        let (root, node) = fake_sysfs("045e\n", "0766\n");
        let fix =
            |format, memory, range| camera_range_fix(root.path(), &node, format, memory, range);
        assert_eq!(camera_usb_id(root.path(), &node), Some(VX800));
        assert_eq!(
            fix(PixelFormat::Yuy2, VideoMemory::System, ColourRange::Limited),
            (Some(VX800), RangeFix::LimitLevels)
        );
        assert_eq!(
            fix(PixelFormat::Yuy2, VideoMemory::Cuda, ColourRange::Limited),
            (Some(VX800), RangeFix::RelabelFull)
        );
        // The VX-800's row lists YUY2 only.
        assert_eq!(
            fix(
                PixelFormat::Mjpeg,
                VideoMemory::System,
                ColourRange::Limited
            ),
            (Some(VX800), RangeFix::None)
        );
        assert_eq!(
            fix(PixelFormat::Nv12, VideoMemory::Cuda, ColourRange::Limited).1,
            RangeFix::None
        );
        // A camera with no row (Logitech C930e), and a node sysfs does not
        // know.
        let (root, node) = fake_sysfs("046d\n", "0843\n");
        assert_eq!(
            camera_range_fix(
                root.path(),
                &node,
                PixelFormat::Yuy2,
                VideoMemory::System,
                ColourRange::Limited
            ),
            (
                Some(UsbId {
                    vendor: 0x046d,
                    product: 0x0843
                }),
                RangeFix::None
            )
        );
        assert_eq!(
            camera_range_fix(
                root.path(),
                Path::new("/nonexistent/video0"),
                PixelFormat::Yuy2,
                VideoMemory::System,
                ColourRange::Limited
            ),
            (None, RangeFix::None)
        );
    }

    #[test]
    fn the_full_range_label_keeps_all_but_the_range() {
        gst::init().unwrap();
        let label = |caps: &str| -> String {
            let c = full_range_caps(&caps.parse::<gst::Caps>().unwrap()).unwrap();
            c.structure(0)
                .unwrap()
                .get::<String>("colorimetry")
                .unwrap()
        };
        let same = |a: &str, b: &str| crate::pipeline::same_colorimetry(a, b);
        let yuy2 = "video/x-raw,format=YUY2,width=640,height=480,framerate=30/1";
        assert!(same(
            &label(&format!("{yuy2},colorimetry=2:4:5:4")),
            "1:4:5:4"
        ));
        assert!(same(
            &label(&format!("{yuy2},colorimetry=bt709")),
            "1:3:5:1"
        ));
        // No label: the default for the size, made full range.
        assert!(same(&label(yuy2), "1:4:5:4") || same(&label(yuy2), "1:4:7:4"));
        assert!(full_range_caps(&"image/jpeg".parse::<gst::Caps>().unwrap()).is_none());
    }

    #[test]
    fn luma_is_sampled_where_each_format_keeps_it() {
        // 4x2 YUY2: Y at even bytes, chroma 128 in between.
        let yuy2 = [
            10, 128, 20, 128, 30, 128, 40, 128, 50, 128, 60, 128, 70, 128, 80, 128,
        ];
        let mut h = [0u64; 256];
        sample_luma(
            &yuy2,
            8,
            (4, 2),
            luma_layout(PixelFormat::Yuy2).unwrap(),
            1,
            &mut h,
        );
        let seen: Vec<usize> = (0..256).filter(|&v| h[v] > 0).collect();
        assert_eq!(seen, [10, 20, 30, 40, 50, 60, 70, 80]);
        assert_eq!(h[128], 0, "chroma is never sampled");
        // UYVY: Y at odd bytes; a step of 2 takes every other pixel and row.
        let mut h = [0u64; 256];
        sample_luma(
            &yuy2,
            8,
            (4, 2),
            luma_layout(PixelFormat::Uyvy).unwrap(),
            2,
            &mut h,
        );
        assert_eq!(h[128], 2);
        // A short plane is skipped past its end, never read out of bounds.
        let mut h = [0u64; 256];
        sample_luma(&yuy2[..5], 8, (4, 2), (1, 0), 1, &mut h);
        assert_eq!(h.iter().sum::<u64>(), 4);
        assert!(luma_layout(PixelFormat::Mjpeg).is_none());
        assert!(luma_layout(PixelFormat::Rgb).is_none());
    }

    /// A histogram of `n` samples: `shares` of them (as fractions) at the
    /// given levels, the rest spread over 40..=200.
    fn hist(n: u64, shares: &[(u8, f64)]) -> [u64; 256] {
        let mut h = [0u64; 256];
        let mut placed = 0;
        for &(level, share) in shares {
            let k = (n as f64 * share).round() as u64;
            h[usize::from(level)] += k;
            placed += k;
        }
        let rest = n - placed;
        for i in 0..rest {
            h[40 + (i % 161) as usize] += 1;
        }
        h
    }

    /// The threshold separates the measured cameras: VX-800-like (4.4 %
    /// above 235) and Generalplus-like (5.8 % below 16) are suspected; a
    /// C930e-like camera that stays in range, or overshoots by a level or
    /// two, is not.
    #[test]
    fn the_threshold_separates_mislabelled_from_correct_cameras() {
        let n = 30_000;
        let vx800 = hist(n, &[(255, 0.03), (245, 0.014), (20, 0.05)]);
        let e = suggests_full_range(&vx800).expect("VX-800-like");
        assert_eq!((e.min, e.max), (20, 255));
        assert!((e.share() - 0.044).abs() < 0.001, "{}", e.share());
        let generalplus = hist(n, &[(0, 0.04), (8, 0.018), (250, 0.001)]);
        let e = suggests_full_range(&generalplus).expect("Generalplus-like");
        assert!((e.share() - 0.059).abs() < 0.001, "{}", e.share());

        let c930e = hist(n, &[(16, 0.02), (235, 0.03)]);
        assert!(suggests_full_range(&c930e).is_none());
        let overshoot = hist(n, &[(14, 0.02), (15, 0.01), (236, 0.02), (237, 0.02)]);
        let e = range_evidence(&overshoot).unwrap();
        assert_eq!((e.min, e.max, e.outside), (14, 237, 0));
        assert!(suggests_full_range(&overshoot).is_none());
        // Just under the threshold, and too few samples.
        assert!(suggests_full_range(&hist(n, &[(255, 0.009)])).is_none());
        assert!(suggests_full_range(&hist(500, &[(255, 0.5)])).is_none());
        assert!(range_evidence(&[0; 256]).is_none());
    }

    /// Pushes `frames` YUY2 frames of luma `y` (`colorimetry` their label)
    /// through `appsrc ! capsfilter ! fakesink` with the watch on the
    /// capsfilter: how it ended.
    fn watched(y: impl Fn(usize) -> u8, colorimetry: &str, frames: u64) -> Option<WatchOutcome> {
        crate::test_support::require_elements(&["appsrc", "capsfilter", "fakesink"]);
        let caps = format!(
            "video/x-raw,format=YUY2,width=320,height=240,framerate=30/1,\
             colorimetry={colorimetry}"
        );
        let pipeline = gst::Pipeline::with_name("watch-test");
        let src = gst_app::AppSrc::builder()
            .caps(&caps.parse::<gst::Caps>().unwrap())
            .format(gst::Format::Time)
            .build();
        let filter = crate::pipeline::capsfilter(names::CAMERA_CAPS, &caps).unwrap();
        let sink = make("fakesink", "sink").unwrap();
        let chain = [src.clone().upcast(), filter.clone(), sink];
        crate::pipeline::add_linked(&pipeline, &chain).unwrap();
        let watch = watch_range(&filter, VX800, PixelFormat::Yuy2).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        // One frame: luma `y(x)` across each row.
        let frame: Vec<u8> = (0..240)
            .flat_map(|_| (0..320).flat_map(|x| [y(x), 128]))
            .collect();
        for n in 0..frames {
            let mut b = gst::Buffer::from_slice(frame.clone());
            b.get_mut()
                .unwrap()
                .set_pts(gst::ClockTime::from_mseconds(33 * n));
            src.push_buffer(b).unwrap();
        }
        src.end_of_stream().unwrap();
        let msg = pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("EOS or error");
        assert!(matches!(msg.view(), gst::MessageView::Eos(_)), "{msg:?}");
        pipeline.set_state(gst::State::Null).unwrap();
        watch.outcome()
    }

    /// On a pipeline: a full-range picture labelled limited is suspected
    /// once the sampled frames are in; a limited one is not; too few
    /// frames decide nothing; a full-range label is not looked at. CPU
    /// only.
    #[test]
    fn the_watch_samples_a_few_early_frames_and_ends() {
        let last = WATCH_SKIP + WATCH_EVERY * u64::from(WATCH_FRAMES - 1);
        // A ramp over 0..=255: about 12 % of it outside 14-237.
        let ramp = |x: usize| (x * 255 / 319) as u8;
        let Some(WatchOutcome::Sampled(Some(e))) = watched(ramp, "2:4:5:4", last) else {
            panic!("a full-range ramp is suspected");
        };
        // Every 8th pixel: the last sampled is x=312, luma 249.
        assert_eq!((e.min, e.max), (0, 249));
        assert!(e.samples >= MIN_SAMPLES);
        let limited = |x: usize| 16 + (x * 219 / 319) as u8;
        assert_eq!(
            watched(limited, "2:4:5:4", last + 30),
            Some(WatchOutcome::Sampled(None))
        );
        assert_eq!(watched(ramp, "2:4:5:4", last - 1), None);
        assert_eq!(
            watched(ramp, "1:4:5:4", last),
            Some(WatchOutcome::NotLimited)
        );
    }
}
