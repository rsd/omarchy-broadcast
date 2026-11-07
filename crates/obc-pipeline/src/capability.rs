//! Camera capability model and deterministic mode selection.
//!
//! A [`Capabilities`] value lists every `(format, size, frame rate)` a V4L2
//! capture node offers in a format the pipeline can ingest, plus the other
//! formats it offers (greyscale/IR, depth, H.264, ...) for diagnostics.
//! Discovery fills it from `VIDIOC_ENUM_FMT` / `VIDIOC_ENUM_FRAMESIZES` /
//! `VIDIOC_ENUM_FRAMEINTERVALS` ([`crate::discovery::enumerate_formats`]); the
//! tests fill it from captured `v4l2-ctl --list-formats-ext` output
//! ([`parse_v4l2_ctl`]), so selection is tested without hardware.
//!
//! # Colour cameras
//!
//! A node is a colour camera when it offers at least one format from
//! [`PixelFormat`]. A node offering only greyscale formats (`GREY`, `Y10`,
//! `Y16`, ... as IR nodes do) or nothing ingestible is not: it is never
//! auto-picked, and selecting it explicitly is an error.
//!
//! # Ingest paths
//!
//! The chosen format decides how frames reach CUDA memory ([`IngestPath`]):
//!
//! * **GPU-direct**: formats `cudaconvert` accepts (NV12, NV21, I420, YV12,
//!   RGB, BGR) are uploaded as they are; every conversion runs on the GPU.
//! * **CPU convert**: packed 4:2:2 (YUY2, UYVY) is not accepted by
//!   `cudaconvert`, so `videoconvert` turns it into NV12 on the CPU first.
//! * **CPU decode**: MJPEG is decoded by `jpegdec` on the CPU into I420. A GPU
//!   (nvJPEG) decode is future work: `nvjpegdec` rejects the 4:2:2 JPEG that
//!   common UVC cameras send.
//!
//! # Selection rule
//!
//! [`choose_mode`] resolves a [`ModeRequest`] against one node; the order of
//! preference is strictly lexicographic (size, then frame rate, then format):
//!
//! 1. **Size.** The requested size if it is offered (in any format, at any
//!    rate). Otherwise the largest size that does not exceed the request in
//!    either dimension, preferring sizes with the request's aspect ratio
//!    (within 1%) over larger sizes with another ratio; ties in area go to the
//!    wider size. Only when no offered size fits inside the request, the
//!    smallest size above it (by area; ties go to the same aspect ratio, then
//!    the narrower size).
//! 2. **Frame rate**, among the modes of that size: the exact requested rate;
//!    else the highest rate not above it (rates within 0.2% count as "not
//!    above", so 30 serves a 30000/1001 request); only when every rate is
//!    above it, the lowest rate.
//! 3. **Format**, among equal size and rate: the requested format, then NV12,
//!    then YUY2, then other raw formats (GPU-direct ones before CPU-convert
//!    ones), then MJPEG. The frame rate always beats the format: a 30 fps
//!    MJPEG mode wins over a 15 fps raw mode of the same size.
//!
//! The result is *exact* when size, rate (as a reduced fraction) and, if one
//! was requested, format all match the request; otherwise it is *degraded*.
//!
//! Across several cameras (no `--camera`), [`crate::device::select_camera`]
//! takes the first node, in `/dev/videoN` order, whose best mode is exact; if
//! none is, the node whose best mode ranks first by the same rule (ties go to
//! the earlier node).

use std::fmt;

use obc_core::{ModeRequest, PixelFormat, VideoMode, reduce_fraction};
use serde::Serialize;

/// How frames of a given format reach CUDA memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum IngestPath {
    /// Uploaded as captured; every conversion runs on the GPU.
    GpuDirect,
    /// `videoconvert` to NV12 on the CPU, then upload.
    CpuConvert,
    /// `jpegdec` to I420 on the CPU, then upload.
    CpuDecode,
}

impl IngestPath {
    /// The ingest path for frames captured in `format`.
    pub fn of(format: PixelFormat) -> Self {
        match format {
            PixelFormat::Mjpeg => IngestPath::CpuDecode,
            PixelFormat::Yuy2 | PixelFormat::Uyvy => IngestPath::CpuConvert,
            PixelFormat::Nv12
            | PixelFormat::Nv21
            | PixelFormat::I420
            | PixelFormat::Yv12
            | PixelFormat::Rgb
            | PixelFormat::Bgr => IngestPath::GpuDirect,
        }
    }

    /// The raw format handed to `cudaupload` for a camera format.
    pub fn upload_format(self, camera: PixelFormat) -> &'static str {
        match self {
            IngestPath::GpuDirect => camera.gst_name(),
            IngestPath::CpuConvert => "NV12",
            IngestPath::CpuDecode => "I420",
        }
    }

    /// The stages before `cudaupload`, for logs.
    pub fn stages(self) -> &'static str {
        match self {
            IngestPath::GpuDirect => "v4l2src ! cudaupload",
            IngestPath::CpuConvert => "v4l2src ! videoconvert (CPU, to NV12) ! cudaupload",
            IngestPath::CpuDecode => "v4l2src ! jpegdec (CPU, to I420) ! cudaupload",
        }
    }
}

impl fmt::Display for IngestPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            IngestPath::GpuDirect => "gpu-direct",
            IngestPath::CpuConvert => "cpu-convert",
            IngestPath::CpuDecode => "cpu-decode",
        })
    }
}

/// The ingestible format for a V4L2 fourcc (`"NV12"`, `"YUYV"`, `"MJPG"`, ...);
/// `None` for anything else (greyscale, depth, H.264, ...).
pub fn format_from_fourcc(fourcc: &str) -> Option<PixelFormat> {
    match fourcc.trim() {
        "NV12" => Some(PixelFormat::Nv12),
        "NV21" => Some(PixelFormat::Nv21),
        "YU12" => Some(PixelFormat::I420),
        "YV12" => Some(PixelFormat::Yv12),
        "YUYV" => Some(PixelFormat::Yuy2),
        "UYVY" => Some(PixelFormat::Uyvy),
        "RGB3" => Some(PixelFormat::Rgb),
        "BGR3" => Some(PixelFormat::Bgr),
        "MJPG" | "JPEG" => Some(PixelFormat::Mjpeg),
        _ => None,
    }
}

/// The ingestible format for a GStreamer caps structure name and `format`
/// field; `None` for anything else (`GRAY8`, `video/x-h264`, ...).
pub fn format_from_gst(structure_name: &str, format: Option<&str>) -> Option<PixelFormat> {
    match structure_name {
        "image/jpeg" => Some(PixelFormat::Mjpeg),
        "video/x-raw" => match format? {
            "NV12" => Some(PixelFormat::Nv12),
            "NV21" => Some(PixelFormat::Nv21),
            "I420" => Some(PixelFormat::I420),
            "YV12" => Some(PixelFormat::Yv12),
            "YUY2" => Some(PixelFormat::Yuy2),
            "UYVY" => Some(PixelFormat::Uyvy),
            "RGB" => Some(PixelFormat::Rgb),
            "BGR" => Some(PixelFormat::Bgr),
            _ => None,
        },
        _ => None,
    }
}

/// What one capture node offers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    /// Every mode in an ingestible format, in enumeration order, without
    /// duplicates. Frame rates are reduced fractions.
    pub modes: Vec<VideoMode>,
    /// Formats offered that the pipeline does not ingest (V4L2 fourccs or
    /// GStreamer format names), without duplicates.
    pub other_formats: Vec<String>,
}

impl Capabilities {
    /// Adds a mode (frame rate reduced), ignoring duplicates.
    pub fn push_mode(&mut self, format: PixelFormat, width: u32, height: u32, num: u32, den: u32) {
        if width == 0 || height == 0 || num == 0 || den == 0 {
            return;
        }
        let (fps_num, fps_den) = reduce_fraction(num, den);
        let m = VideoMode {
            format,
            width,
            height,
            fps_num,
            fps_den,
        };
        if !self.modes.contains(&m) {
            self.modes.push(m);
        }
    }

    /// Records a format the pipeline does not ingest.
    pub fn push_other(&mut self, name: &str) {
        let name = name.trim().to_string();
        if !name.is_empty() && !self.other_formats.contains(&name) {
            self.other_formats.push(name);
        }
    }

    /// Whether the node is a colour camera: it offers an ingestible format.
    pub fn is_colour(&self) -> bool {
        !self.modes.is_empty()
    }

    /// The distinct ingestible formats, in enumeration order.
    pub fn formats(&self) -> Vec<PixelFormat> {
        let mut out = Vec::new();
        for m in &self.modes {
            if !out.contains(&m.format) {
                out.push(m.format);
            }
        }
        out
    }

    /// The subset of modes `keep` accepts.
    pub fn filtered(&self, keep: impl Fn(&VideoMode) -> bool) -> Capabilities {
        Capabilities {
            modes: self.modes.iter().copied().filter(|m| keep(m)).collect(),
            other_formats: self.other_formats.clone(),
        }
    }

    /// Every format name offered (ingestible first), for messages.
    pub fn describe_formats(&self) -> String {
        let mut names: Vec<String> = self.formats().iter().map(|f| f.to_string()).collect();
        names.extend(self.other_formats.iter().cloned());
        if names.is_empty() {
            "none".into()
        } else {
            names.join(", ")
        }
    }
}

/// A mode chosen for a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModeChoice {
    pub mode: VideoMode,
    /// Size, rate and (if requested) format all match the request.
    pub exact: bool,
    pub ingest: IngestPath,
    /// What differs from the request (empty when exact), e.g. `"fps 60 -> 30"`.
    pub differences: Vec<String>,
}

impl fmt::Display for ModeChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ", self.mode)?;
        if self.exact {
            f.write_str("exact")?;
        } else {
            write!(f, "degraded ({})", self.differences.join(", "))?;
        }
        write!(f, ", ingest {} [{}]", self.ingest, self.ingest.stages())
    }
}

/// Rank of a format in the tie-break among equal size and rate (lower wins).
pub fn format_rank(format: PixelFormat, requested: Option<PixelFormat>) -> u8 {
    if requested == Some(format) {
        return 0;
    }
    match (format, IngestPath::of(format)) {
        (PixelFormat::Nv12, _) => 1,
        (PixelFormat::Yuy2, _) => 2,
        (_, IngestPath::GpuDirect) => 3,
        (_, IngestPath::CpuConvert) => 4,
        (_, IngestPath::CpuDecode) => 5,
    }
}

/// Aspect ratios within 1% count as equal (1600x896 and 848x480 are 16:9).
fn same_aspect(w: u32, h: u32, rw: u32, rh: u32) -> bool {
    let a = f64::from(w) / f64::from(h);
    let r = f64::from(rw) / f64::from(rh);
    ((a - r) / r).abs() <= 0.01
}

/// A rate "not above" the request: at most 0.2% higher (so 30/1 serves a
/// 30000/1001 request, which is 0.1% lower).
fn rate_within(mode: &VideoMode, req: &ModeRequest) -> bool {
    mode.fps() <= req.fps() * 1.002
}

/// Sort key of the selection rule: (size, rate, format rank, format), each
/// "lower is better".
pub type SelectionKey = ((u8, u64, u64), (u8, u64), u8, PixelFormat);

/// The sort key implementing the selection rule (lower is better). Usable
/// across cameras: the same key ranks one camera's best mode against another's.
pub fn selection_key(mode: &VideoMode, req: &ModeRequest) -> SelectionKey {
    let area = u64::from(mode.width) * u64::from(mode.height);
    let aspect = same_aspect(mode.width, mode.height, req.width, req.height);
    let fits = mode.width <= req.width && mode.height <= req.height;
    let exact_size = mode.width == req.width && mode.height == req.height;
    // Size: (class, primary, secondary), all "lower is better".
    let size = if exact_size {
        (0u8, 0u64, 0u64)
    } else if fits {
        (
            if aspect { 1 } else { 2 },
            u64::MAX - area,
            u64::from(u32::MAX - mode.width),
        )
    } else {
        (3, area, (u64::from(!aspect) << 32) | u64::from(mode.width))
    };
    let milli_fps = (mode.fps() * 1000.0).round() as u64;
    let exact_rate =
        reduce_fraction(mode.fps_num, mode.fps_den) == reduce_fraction(req.fps_num, req.fps_den);
    let rate = if exact_rate {
        (0u8, 0u64)
    } else if rate_within(mode, req) {
        (1, u64::MAX - milli_fps)
    } else {
        (2, milli_fps)
    };
    (
        size,
        rate,
        format_rank(mode.format, req.format),
        mode.format,
    )
}

/// What differs between a request and a mode.
pub fn differences(req: &ModeRequest, mode: &VideoMode) -> Vec<String> {
    let mut out = Vec::new();
    if (mode.width, mode.height) != (req.width, req.height) {
        out.push(format!(
            "size {}x{} -> {}x{}",
            req.width, req.height, mode.width, mode.height
        ));
    }
    if reduce_fraction(mode.fps_num, mode.fps_den) != reduce_fraction(req.fps_num, req.fps_den) {
        out.push(format!(
            "fps {} -> {}",
            fmt_fps(req.fps()),
            fmt_fps(mode.fps())
        ));
    }
    if let Some(f) = req.format
        && f != mode.format
    {
        out.push(format!("format {f} -> {}", mode.format));
    }
    out
}

fn fmt_fps(fps: f64) -> String {
    if (fps - fps.round()).abs() < 1e-9 {
        format!("{fps:.0}")
    } else {
        format!("{fps:.3}")
    }
}

/// The best mode for `req` among `caps`, by the module's selection rule.
/// `None` when the node offers no ingestible mode (not a colour camera).
pub fn choose_mode(caps: &Capabilities, req: &ModeRequest) -> Option<ModeChoice> {
    let mode = *caps.modes.iter().min_by_key(|m| selection_key(m, req))?;
    Some(ModeChoice {
        mode,
        exact: req.is_exact(&mode),
        ingest: IngestPath::of(mode.format),
        differences: differences(req, &mode),
    })
}

/// Converts a decimal frame rate as `v4l2-ctl` prints it (`30.000`, `7.500`,
/// `29.970`) back to the fraction the driver reports.
fn fraction_from_decimal(fps: f64) -> Option<(u32, u32)> {
    if !(fps.is_finite() && fps > 0.0) {
        return None;
    }
    for den in [1u32, 2, 1001] {
        let num = (fps * f64::from(den)).round();
        if num >= 1.0 && (num / f64::from(den) - fps).abs() < 0.0005 {
            return Some(reduce_fraction(num as u32, den));
        }
    }
    Some(reduce_fraction((fps * 1000.0).round() as u32, 1000))
}

/// Parses `v4l2-ctl --list-formats-ext` output. Discrete sizes and intervals
/// are read; stepwise and continuous entries are skipped (UVC devices report
/// only discrete ones, and live enumeration handles the others).
pub fn parse_v4l2_ctl(text: &str) -> Capabilities {
    let mut caps = Capabilities::default();
    let mut format: Option<PixelFormat> = None;
    let mut size: Option<(u32, u32)> = None;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            // `[0]: 'YUYV' (YUYV 4:2:2)`
            let fourcc = t.split('\'').nth(1).unwrap_or_default();
            format = format_from_fourcc(fourcc);
            if format.is_none() {
                caps.push_other(fourcc);
            }
            size = None;
        } else if let Some(rest) = t.strip_prefix("Size: Discrete ") {
            size = rest
                .split_once('x')
                .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)));
        } else if t.starts_with("Size:") {
            size = None;
        } else if let Some(rest) = t.strip_prefix("Interval: Discrete ") {
            let fps = rest
                .split_once('(')
                .and_then(|(_, r)| r.split_whitespace().next())
                .and_then(|v| v.parse::<f64>().ok())
                .and_then(fraction_from_decimal);
            if let (Some(f), Some((w, h)), Some((num, den))) = (format, size, fps) {
                caps.push_mode(f, w, h, num, den);
            }
        }
    }
    caps
}

#[cfg(test)]
mod tests {
    use super::*;

    const BRIO: &str = include_str!("../tests/data/mx-brio-046d-0944.v4l2-ctl.txt");
    const FHD: &str = include_str!("../tests/data/fhd-camera-5986-1193-colour.v4l2-ctl.txt");
    const FHD_IR: &str = include_str!("../tests/data/fhd-camera-5986-1193-ir.v4l2-ctl.txt");

    fn req(s: &str) -> ModeRequest {
        s.parse().unwrap()
    }

    fn mode(s: &str) -> VideoMode {
        s.parse().unwrap()
    }

    #[test]
    fn parses_mx_brio_fixture() {
        let c = parse_v4l2_ctl(BRIO);
        assert_eq!(
            c.formats(),
            vec![PixelFormat::Yuy2, PixelFormat::Mjpeg, PixelFormat::Nv12]
        );
        assert!(c.other_formats.is_empty());
        assert!(c.is_colour());
        assert!(c.modes.contains(&mode("NV12:1920x1080@60")));
        assert!(c.modes.contains(&mode("MJPEG:3840x2160@30")));
        assert!(c.modes.contains(&mode("YUY2:3840x2160@15")));
        assert!(c.modes.contains(&mode("YUY2:640x480@15/2")));
        assert!(!c.modes.contains(&mode("YUY2:3840x2160@30")));
        assert!(!c.modes.contains(&mode("NV12:2560x1440@60")));
    }

    #[test]
    fn parses_fhd_fixtures() {
        let c = parse_v4l2_ctl(FHD);
        assert_eq!(c.formats(), vec![PixelFormat::Mjpeg, PixelFormat::Yuy2]);
        assert!(c.modes.contains(&mode("MJPEG:1920x1080@30")));
        assert!(c.modes.contains(&mode("YUY2:960x540@15")));
        assert!(c.modes.contains(&mode("YUY2:1920x1080@5")));
        assert_eq!(c.modes.len(), 20);

        let ir = parse_v4l2_ctl(FHD_IR);
        assert!(!ir.is_colour(), "a GREY-only node is not a colour camera");
        assert_eq!(ir.other_formats, vec!["GREY".to_string()]);
        assert_eq!(ir.describe_formats(), "GREY");
        assert_eq!(choose_mode(&ir, &req("640x360@15")), None);
    }

    fn pick(fixture: &str, request: &str) -> ModeChoice {
        choose_mode(&parse_v4l2_ctl(fixture), &req(request)).expect("colour camera")
    }

    #[test]
    fn mx_brio_nv12_1080p60_is_exact_gpu_direct() {
        for r in ["NV12:1920x1080@60", "1920x1080@60"] {
            let c = pick(BRIO, r);
            assert_eq!(c.mode, mode("NV12:1920x1080@60"), "{r}");
            assert!(c.exact, "{r}");
            assert_eq!(c.ingest, IngestPath::GpuDirect);
            assert!(c.differences.is_empty());
        }
    }

    #[test]
    fn mx_brio_4k30_is_mjpeg_cpu_decode() {
        let c = pick(BRIO, "3840x2160@30");
        assert_eq!(c.mode, mode("MJPEG:3840x2160@30"));
        assert!(c.exact);
        assert_eq!(c.ingest, IngestPath::CpuDecode);
        // Asking for NV12 at 4K30 still gets MJPEG (rate beats format), degraded.
        let c = pick(BRIO, "NV12:3840x2160@30");
        assert_eq!(c.mode, mode("MJPEG:3840x2160@30"));
        assert!(!c.exact);
        assert_eq!(c.differences, vec!["format NV12 -> MJPEG".to_string()]);
    }

    #[test]
    fn mx_brio_1440p30_is_nv12() {
        let c = pick(BRIO, "2560x1440@30");
        assert_eq!(c.mode, mode("NV12:2560x1440@30"));
        assert!(c.exact);
        assert_eq!(c.ingest, IngestPath::GpuDirect);
    }

    #[test]
    fn mx_brio_requested_format_wins_the_tie() {
        let c = pick(BRIO, "YUY2:1920x1080@60");
        assert_eq!(c.mode, mode("YUY2:1920x1080@60"));
        assert!(c.exact);
        assert_eq!(c.ingest, IngestPath::CpuConvert);
        let c = pick(BRIO, "MJPEG:1920x1080@60");
        assert_eq!(c.mode, mode("MJPEG:1920x1080@60"));
        assert_eq!(c.ingest, IngestPath::CpuDecode);
    }

    #[test]
    fn fhd_1080p60_degrades_to_1080p30_mjpeg() {
        let c = pick(FHD, "1920x1080@60");
        assert_eq!(c.mode, mode("MJPEG:1920x1080@30"));
        assert!(!c.exact);
        assert_eq!(c.ingest, IngestPath::CpuDecode);
        assert_eq!(c.differences, vec!["fps 60 -> 30".to_string()]);
    }

    #[test]
    fn fhd_720p30_is_mjpeg_exact() {
        let c = pick(FHD, "1280x720@30");
        assert_eq!(c.mode, mode("MJPEG:1280x720@30"));
        assert!(c.exact);
        assert_eq!(c.ingest, IngestPath::CpuDecode);
    }

    #[test]
    fn fhd_960x540_rate_beats_format() {
        // YUYV 960x540 runs at 15 fps only; MJPEG has 30.
        let c = pick(FHD, "960x540@30");
        assert_eq!(c.mode, mode("MJPEG:960x540@30"));
        assert!(c.exact);
        // Even when YUYV is requested: the rate wins, the format degrades.
        let c = pick(FHD, "YUYV:960x540@30");
        assert_eq!(c.mode, mode("MJPEG:960x540@30"));
        assert!(!c.exact);
    }

    #[test]
    fn fhd_640x360_raw_wins_the_tie() {
        // Both MJPEG and YUYV offer 640x360@30 (captured data): raw wins.
        let caps = parse_v4l2_ctl(FHD);
        assert!(caps.modes.contains(&mode("MJPEG:640x360@30")));
        assert!(caps.modes.contains(&mode("YUY2:640x360@30")));
        let c = pick(FHD, "640x360@30");
        assert_eq!(c.mode, mode("YUY2:640x360@30"));
        assert!(c.exact);
        assert_eq!(c.ingest, IngestPath::CpuConvert);
    }

    #[test]
    fn size_not_offered_takes_largest_below_same_aspect_first() {
        // 1920x1200 (16:10) is not offered: largest fitting size, none 16:10.
        let c = pick(BRIO, "1920x1200@60");
        assert_eq!(c.mode, mode("NV12:1920x1080@60"));
        assert!(!c.exact);
        // 1440x1080 (4:3): the 4:3 sizes win over the larger 16:9 1280x720.
        let c = pick(BRIO, "1440x1080@30");
        assert_eq!((c.mode.width, c.mode.height), (800, 600));
        // 2000x1125 (16:9) on the FHD camera: 1920x1080 at the best rate.
        let c = pick(FHD, "2000x1125@30");
        assert_eq!(c.mode, mode("MJPEG:1920x1080@30"));
    }

    #[test]
    fn size_above_only_when_nothing_fits() {
        let c = pick(FHD, "100x100@30");
        // Smallest above by area: 320x180 (57600) over 320x240 / 352x288.
        assert_eq!((c.mode.width, c.mode.height), (320, 180));
        assert_eq!(c.mode.fps_num, 30);
    }

    #[test]
    fn rate_above_only_when_nothing_is_below() {
        // FHD offers 1080p at 30 (MJPEG) and 5 (YUYV): 25 takes 5 (not above).
        let c = pick(FHD, "1920x1080@25");
        assert_eq!(c.mode, mode("YUY2:1920x1080@5"));
        // 3 fps: everything is above; the lowest rate wins.
        let c = pick(FHD, "1920x1080@3");
        assert_eq!(c.mode, mode("YUY2:1920x1080@5"));
        // An NTSC request is served by 30/1 (within 0.2%), not by 24.
        let c = pick(BRIO, "NV12:1920x1080@30000/1001");
        assert_eq!(c.mode, mode("NV12:1920x1080@30"));
        assert!(!c.exact);
    }

    #[test]
    fn format_rank_order() {
        use PixelFormat::*;
        let mut v = vec![Mjpeg, Uyvy, I420, Yuy2, Nv12];
        v.sort_by_key(|f| format_rank(*f, None));
        assert_eq!(v, vec![Nv12, Yuy2, I420, Uyvy, Mjpeg]);
        assert_eq!(format_rank(Mjpeg, Some(Mjpeg)), 0);
    }

    #[test]
    fn ingest_paths() {
        assert_eq!(IngestPath::of(PixelFormat::Nv12), IngestPath::GpuDirect);
        assert_eq!(IngestPath::of(PixelFormat::I420), IngestPath::GpuDirect);
        assert_eq!(IngestPath::of(PixelFormat::Yuy2), IngestPath::CpuConvert);
        assert_eq!(IngestPath::of(PixelFormat::Uyvy), IngestPath::CpuConvert);
        assert_eq!(IngestPath::of(PixelFormat::Mjpeg), IngestPath::CpuDecode);
        assert_eq!(
            IngestPath::CpuConvert.upload_format(PixelFormat::Yuy2),
            "NV12"
        );
        assert_eq!(
            IngestPath::CpuDecode.upload_format(PixelFormat::Mjpeg),
            "I420"
        );
        assert_eq!(
            IngestPath::GpuDirect.upload_format(PixelFormat::Nv12),
            "NV12"
        );
        assert_eq!(IngestPath::CpuDecode.to_string(), "cpu-decode");
    }

    #[test]
    fn decimal_rates() {
        assert_eq!(fraction_from_decimal(30.0), Some((30, 1)));
        assert_eq!(fraction_from_decimal(7.5), Some((15, 2)));
        assert_eq!(fraction_from_decimal(29.970), Some((30000, 1001)));
        assert_eq!(fraction_from_decimal(0.0), None);
    }

    #[test]
    fn fourcc_mapping() {
        assert_eq!(format_from_fourcc("MJPG"), Some(PixelFormat::Mjpeg));
        assert_eq!(format_from_fourcc("YUYV"), Some(PixelFormat::Yuy2));
        for grey in ["GREY", "Y10 ", "Y16 ", "Z16 ", "H264"] {
            assert_eq!(format_from_fourcc(grey), None, "{grey}");
        }
        assert_eq!(format_from_gst("video/x-raw", Some("GRAY8")), None);
        assert_eq!(
            format_from_gst("video/x-raw", Some("YUY2")),
            Some(PixelFormat::Yuy2)
        );
        assert_eq!(
            format_from_gst("image/jpeg", None),
            Some(PixelFormat::Mjpeg)
        );
    }
}
