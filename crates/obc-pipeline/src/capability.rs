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

/// How frames of a given format reach CUDA memory. In an ingest of
/// system-memory frames the CPU convert path has no adapter of its own: the
/// canonical tail's `videoconvertscale` converts the captured frames
/// ([`crate::pipeline::ingest_adapter`]); [`Self::upload_format`] and
/// [`Self::stages`] describe the CUDA ingest.
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
pub(crate) fn fraction_from_decimal(fps: f64) -> Option<(u32, u32)> {
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

// ---- the mode choices offered to the user ----

/// A size and frame rate without a format: `(width, height, fps_num,
/// fps_den)`, the rate a reduced fraction.
type SizeRate = (u32, u32, u32, u32);

/// The slowest capture rate offered as a camera mode choice.
pub const INPUT_MIN_FPS: u32 = 15;
/// The narrowest capture size offered as a camera mode choice.
pub const INPUT_MIN_WIDTH: u32 = 640;
/// The most camera mode choices besides "auto".
pub const INPUT_MAX_OPTIONS: usize = 15;

/// The output sizes that can be offered (16:9), largest first.
pub const OUTPUT_SIZES: [(u32, u32); 6] = [
    (3840, 2160),
    (2560, 1440),
    (1920, 1080),
    (1280, 720),
    (960, 540),
    (640, 360),
];
/// The slowest output rate offered.
pub const OUTPUT_MIN_FPS: u32 = 24;
/// The fastest generated output rate offered.
pub const OUTPUT_MAX_GENERATED_FPS: u32 = 60;
/// The most output mode choices.
pub const OUTPUT_MAX_OPTIONS: usize = 16;
/// The suffix of the label of an output rate made by frame generation.
pub const GENERATED_SUFFIX: &str = " (generated)";
/// The output modes offered when no camera is selected (besides the
/// current one).
pub const OUTPUT_FALLBACK_MODES: [&str; 2] = ["1920x1080@30", "1280x720@30"];

/// `WxH@FPS` (`N/D` for a fractional rate): the value of a mode choice,
/// which [`ModeRequest`] and [`obc_core::OutputContract`] parse.
pub fn mode_value(width: u32, height: u32, fps_num: u32, fps_den: u32) -> String {
    let (n, d) = reduce_fraction(fps_num, fps_den);
    if d == 1 {
        format!("{width}x{height}@{n}")
    } else {
        format!("{width}x{height}@{n}/{d}")
    }
}

/// `3840x2160 · 30 fps`: the label of a mode choice, a whole rate as an
/// integer and a fractional one with two decimals (30000/1001 shows as
/// 29.97), so that two rates of a size never share a label.
pub fn mode_label(width: u32, height: u32, fps_num: u32, fps_den: u32) -> String {
    let (n, d) = reduce_fraction(fps_num, fps_den);
    if d == 1 {
        format!("{width}x{height} \u{b7} {n} fps")
    } else {
        let fps = f64::from(n) / f64::from(d);
        format!("{width}x{height} \u{b7} {fps:.2} fps")
    }
}

fn fps_of(m: &SizeRate) -> f64 {
    f64::from(m.2) / f64::from(m.3)
}

fn area(m: &SizeRate) -> u64 {
    u64::from(m.0) * u64::from(m.1)
}

/// The distinct sizes and rates of `caps`, formats left out.
fn size_rates(caps: &Capabilities) -> Vec<SizeRate> {
    let mut out: Vec<SizeRate> = Vec::new();
    for m in &caps.modes {
        let (n, d) = reduce_fraction(m.fps_num, m.fps_den);
        let sr = (m.width, m.height, n, d);
        if !out.contains(&sr) {
            out.push(sr);
        }
    }
    out
}

/// Size descending (by area, then width), then rate descending.
fn sort_choices(list: &mut [SizeRate]) {
    list.sort_by(|a, b| {
        area(b)
            .cmp(&area(a))
            .then(b.0.cmp(&a.0))
            .then(fps_of(b).total_cmp(&fps_of(a)))
    });
}

/// The camera mode choices of a capture node, as `(value, label)`, the
/// "auto" choice not included: each distinct size and rate it offers in an
/// ingestible format (the format is left to [`choose_mode`]), at
/// [`INPUT_MIN_FPS`] or faster and [`INPUT_MIN_WIDTH`] or wider. Over
/// `max`, the slowest rates go first (the slowest of all, a size keeping
/// its fastest rate; ties drop the smaller size's), then the smallest
/// sizes. Ordered by size, largest first, then by rate, fastest first.
pub fn input_mode_choices(caps: &Capabilities, max: usize) -> Vec<(String, String)> {
    let mut list: Vec<SizeRate> = size_rates(caps)
        .into_iter()
        .filter(|m| m.0 >= INPUT_MIN_WIDTH && fps_of(m) >= f64::from(INPUT_MIN_FPS) - 1e-9)
        .collect();
    sort_choices(&mut list);
    while list.len() > max {
        // The slowest rate of a size that has another one.
        let shared = |m: &SizeRate| list.iter().filter(|o| (o.0, o.1) == (m.0, m.1)).count() > 1;
        let victim = list
            .iter()
            .enumerate()
            .filter(|(_, m)| shared(m))
            .min_by(|(_, a), (_, b)| fps_of(a).total_cmp(&fps_of(b)).then(area(a).cmp(&area(b))))
            .map(|(i, _)| i);
        match victim {
            Some(i) => {
                list.remove(i);
            }
            // One rate per size: the smallest size goes (the list is
            // sorted, so it is the last).
            None => {
                list.pop();
            }
        }
    }
    list.iter()
        .map(|m| {
            (
                mode_value(m.0, m.1, m.2, m.3),
                mode_label(m.0, m.1, m.2, m.3),
            )
        })
        .collect()
}

/// The output mode choices, as `(value, label)`, for a camera offering
/// `caps` (`None`: no camera selected), with `current` (the configured
/// `output.mode`, always offered) and whether frame generation can double
/// a rate (`generated`):
///
/// - the sizes of [`OUTPUT_SIZES`] the camera offers, at any rate;
/// - at each, the rates of [`OUTPUT_MIN_FPS`] or faster the camera offers
///   at that size or a larger one;
/// - with `generated`, twice each of those rates when that is at most
///   [`OUTPUT_MAX_GENERATED_FPS`] and not one of them, labelled with
///   [`GENERATED_SUFFIX`];
/// - ordered by size, largest first, then by rate, fastest first, at most
///   [`OUTPUT_MAX_OPTIONS`] (the last ones dropped; the current value is
///   kept in their place).
///
/// With no camera: the current value, [`OUTPUT_FALLBACK_MODES`].
pub fn output_mode_choices(
    caps: Option<&Capabilities>,
    current: &str,
    generated: bool,
) -> Vec<(String, String)> {
    let parse = |s: &str| {
        s.parse::<ModeRequest>()
            .ok()
            .filter(|r| r.format.is_none())
            .map(|r| {
                let (n, d) = reduce_fraction(r.fps_num, r.fps_den);
                (r.width, r.height, n, d)
            })
    };
    // (size and rate, generated)
    let mut list: Vec<(SizeRate, bool)> = Vec::new();
    match caps {
        None => {
            for v in OUTPUT_FALLBACK_MODES {
                if let Some(m) = parse(v) {
                    list.push((m, false));
                }
            }
        }
        Some(caps) => {
            let offered = size_rates(caps);
            for (w, h) in OUTPUT_SIZES {
                if !offered.iter().any(|m| (m.0, m.1) == (w, h)) {
                    continue;
                }
                let mut rates: Vec<(u32, u32)> = Vec::new();
                for m in offered.iter().filter(|m| m.0 >= w && m.1 >= h) {
                    if fps_of(m) >= f64::from(OUTPUT_MIN_FPS) - 1e-9 && !rates.contains(&(m.2, m.3))
                    {
                        rates.push((m.2, m.3));
                    }
                }
                for &(n, d) in &rates {
                    list.push(((w, h, n, d), false));
                }
                if generated {
                    for &(n, d) in &rates {
                        let Some(n2) = n.checked_mul(2) else { continue };
                        let double = reduce_fraction(n2, d);
                        let fps = f64::from(double.0) / f64::from(double.1);
                        if fps <= f64::from(OUTPUT_MAX_GENERATED_FPS) + 1e-9
                            && !rates.contains(&double)
                            && !list.iter().any(|(m, _)| *m == (w, h, double.0, double.1))
                        {
                            list.push(((w, h, double.0, double.1), true));
                        }
                    }
                }
            }
        }
    }
    let mut modes: Vec<SizeRate> = list.iter().map(|(m, _)| *m).collect();
    sort_choices(&mut modes);
    modes.truncate(OUTPUT_MAX_OPTIONS);
    let label = |m: &SizeRate| {
        let generated = list.iter().any(|(x, g)| x == m && *g);
        let mut l = mode_label(m.0, m.1, m.2, m.3);
        if generated {
            l.push_str(GENERATED_SUFFIX);
        }
        l
    };
    let mut out: Vec<(String, String)> = modes
        .iter()
        .map(|m| (mode_value(m.0, m.1, m.2, m.3), label(m)))
        .collect();
    let current_known =
        out.iter().any(|(v, _)| v == current) || parse(current).is_some_and(|c| modes.contains(&c));
    if !current_known {
        if out.len() >= OUTPUT_MAX_OPTIONS {
            out.pop();
        }
        let entry = match parse(current) {
            Some(m) => (current.to_string(), mode_label(m.0, m.1, m.2, m.3)),
            None => (current.to_string(), current.to_string()),
        };
        // In its place by size and rate, else last.
        let at = parse(current)
            .and_then(|c| {
                out.iter().position(|(v, _)| {
                    parse(v).is_some_and(|m| {
                        area(&m) < area(&c)
                            || (area(&m) == area(&c) && m.0 < c.0)
                            || ((m.0, m.1) == (c.0, c.1) && fps_of(&m) < fps_of(&c))
                    })
                })
            })
            .unwrap_or(out.len());
        out.insert(at, entry);
    } else if let Some(c) = parse(current)
        && !out.iter().any(|(v, _)| v == current)
    {
        // The current value in its own spelling (`60/2` for `30`), so that
        // it is one of the options as it is.
        if let Some(o) = out.iter_mut().find(|(v, _)| parse(v) == Some(c)) {
            o.0 = current.to_string();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BRIO: &str = include_str!("../tests/data/mx-brio-046d-0944.v4l2-ctl.txt");
    const FHD: &str = include_str!("../tests/data/fhd-camera-5986-1193-colour.v4l2-ctl.txt");
    const FHD_IR: &str = include_str!("../tests/data/fhd-camera-5986-1193-ir.v4l2-ctl.txt");
    const C930E: &str = include_str!("../tests/data/logitech-c930e-046d-0843.v4l2-ctl.txt");

    fn req(s: &str) -> ModeRequest {
        s.parse().unwrap()
    }

    fn mode(s: &str) -> VideoMode {
        s.parse().unwrap()
    }

    fn values(list: &[(String, String)]) -> Vec<&str> {
        list.iter().map(|(v, _)| v.as_str()).collect()
    }

    #[test]
    fn mx_brio_input_choices_keep_each_size_at_its_fastest_rate() {
        let list = input_mode_choices(&parse_v4l2_ctl(BRIO), INPUT_MAX_OPTIONS);
        assert_eq!(
            values(&list),
            vec![
                "3840x2160@30",
                "2560x1440@30",
                "1920x1080@60",
                "1920x1080@30",
                "1600x896@60",
                "1600x896@30",
                "1280x720@60",
                "1280x720@30",
                "1024x576@60",
                "960x540@60",
                "800x600@60",
                "848x480@60",
                "800x448@60",
                "640x480@60",
                "640x360@60",
            ]
        );
        assert_eq!(list[0].1, "3840x2160 \u{b7} 30 fps");
        // The 4K choice is MJPEG once resolved: the format is left to
        // the selection rule.
        let req: ModeRequest = list[0].0.parse().unwrap();
        assert_eq!(req.format, None);
        assert_eq!(pick(BRIO, &list[0].0).mode, mode("MJPEG:3840x2160@30"));
        // 1080p60 resolves to NV12.
        assert_eq!(pick(BRIO, "1920x1080@60").mode, mode("NV12:1920x1080@60"));
    }

    #[test]
    fn fhd_input_choices_have_no_4k_and_no_slow_or_narrow_mode() {
        let list = input_mode_choices(&parse_v4l2_ctl(FHD), INPUT_MAX_OPTIONS);
        assert_eq!(
            values(&list),
            vec![
                "1920x1080@30",
                "1280x720@30",
                "960x540@30",
                "960x540@15",
                "848x480@30",
                "848x480@15",
                "640x480@30",
                "640x360@30",
            ]
        );
    }

    #[test]
    fn input_choices_over_the_limit_drop_slow_rates_then_small_sizes() {
        let c = parse_v4l2_ctl(BRIO);
        // Three: the slowest rates go first, then the smallest sizes.
        let list = input_mode_choices(&c, 3);
        assert_eq!(
            values(&list),
            vec!["3840x2160@30", "2560x1440@30", "1920x1080@60"]
        );
        // A fractional rate is shown with two decimals and kept exact.
        let mut caps = Capabilities::default();
        caps.push_mode(PixelFormat::Nv12, 1280, 720, 30000, 1001);
        caps.push_mode(PixelFormat::Nv12, 1280, 720, 10, 1);
        caps.push_mode(PixelFormat::Nv12, 320, 240, 30, 1);
        let list = input_mode_choices(&caps, INPUT_MAX_OPTIONS);
        assert_eq!(
            list,
            vec![(
                "1280x720@30000/1001".to_string(),
                "1280x720 \u{b7} 29.97 fps".to_string()
            )]
        );
        // Next to 30 at the same size: two labels.
        caps.push_mode(PixelFormat::Nv12, 1280, 720, 30, 1);
        let list = input_mode_choices(&caps, INPUT_MAX_OPTIONS);
        let labels: Vec<&str> = list.iter().map(|(_, l)| l.as_str()).collect();
        assert_eq!(
            labels,
            ["1280x720 \u{b7} 30 fps", "1280x720 \u{b7} 29.97 fps"]
        );
    }

    #[test]
    fn mode_labels_show_a_fractional_rate_with_two_decimals() {
        assert_eq!(mode_label(1920, 1080, 30, 1), "1920x1080 \u{b7} 30 fps");
        assert_eq!(mode_label(1920, 1080, 60, 2), "1920x1080 \u{b7} 30 fps");
        assert_eq!(
            mode_label(1920, 1080, 30000, 1001),
            "1920x1080 \u{b7} 29.97 fps"
        );
        assert_eq!(
            mode_label(1920, 1080, 60000, 1001),
            "1920x1080 \u{b7} 59.94 fps"
        );
        assert_eq!(mode_label(640, 480, 15, 2), "640x480 \u{b7} 7.50 fps");
        // The output choices follow the same rule, generated rates too.
        let mut caps = Capabilities::default();
        caps.push_mode(PixelFormat::Nv12, 1920, 1080, 30000, 1001);
        caps.push_mode(PixelFormat::Nv12, 1920, 1080, 30, 1);
        let list = output_mode_choices(Some(&caps), "1920x1080@30", true);
        let labels: Vec<&str> = list.iter().map(|(_, l)| l.as_str()).collect();
        assert!(labels.contains(&"1920x1080 \u{b7} 29.97 fps"), "{labels:?}");
        assert!(labels.contains(&"1920x1080 \u{b7} 30 fps"), "{labels:?}");
        assert!(
            labels.contains(&"1920x1080 \u{b7} 59.94 fps (generated)"),
            "{labels:?}"
        );
        let mut unique = labels.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), labels.len(), "{labels:?}");
    }

    #[test]
    fn mx_brio_output_choices() {
        let c = parse_v4l2_ctl(BRIO);
        let list = output_mode_choices(Some(&c), "1920x1080@30", false);
        assert_eq!(
            values(&list),
            vec![
                "3840x2160@30",
                "3840x2160@24",
                "2560x1440@30",
                "2560x1440@24",
                "1920x1080@60",
                "1920x1080@30",
                "1920x1080@24",
                "1280x720@60",
                "1280x720@30",
                "1280x720@24",
                "960x540@60",
                "960x540@30",
                "960x540@24",
                "640x360@60",
                "640x360@30",
                "640x360@24",
            ]
        );
        // With frame generation: twice a native rate up to 60, when not
        // native; cut at 16.
        let list = output_mode_choices(Some(&c), "1920x1080@30", true);
        assert_eq!(list.len(), OUTPUT_MAX_OPTIONS);
        assert_eq!(
            list[0],
            (
                "3840x2160@60".to_string(),
                "3840x2160 \u{b7} 60 fps (generated)".to_string()
            )
        );
        assert!(list.contains(&("1920x1080@60".into(), "1920x1080 \u{b7} 60 fps".into())));
        assert!(list.contains(&(
            "1920x1080@48".into(),
            "1920x1080 \u{b7} 48 fps (generated)".into()
        )));
        assert!(!values(&list).contains(&"1920x1080@120"));
    }

    #[test]
    fn fhd_output_choices() {
        let c = parse_v4l2_ctl(FHD);
        let list = output_mode_choices(Some(&c), "1920x1080@30", false);
        assert_eq!(
            values(&list),
            vec!["1920x1080@30", "1280x720@30", "960x540@30", "640x360@30"]
        );
        let list = output_mode_choices(Some(&c), "1920x1080@30", true);
        assert_eq!(
            list[..2],
            [
                (
                    "1920x1080@60".to_string(),
                    "1920x1080 \u{b7} 60 fps (generated)".to_string()
                ),
                (
                    "1920x1080@30".to_string(),
                    "1920x1080 \u{b7} 30 fps".to_string()
                ),
            ]
        );
        assert_eq!(list.len(), 8);
    }

    #[test]
    fn output_choices_always_offer_the_current_value() {
        let c = parse_v4l2_ctl(FHD);
        // Not offered by the camera: in its place by size.
        let list = output_mode_choices(Some(&c), "1600x900@25", false);
        assert_eq!(
            values(&list),
            vec![
                "1920x1080@30",
                "1600x900@25",
                "1280x720@30",
                "960x540@30",
                "640x360@30"
            ]
        );
        // In its own spelling.
        let list = output_mode_choices(Some(&c), "1280x720@60/2", false);
        assert!(values(&list).contains(&"1280x720@60/2"));
        assert!(!values(&list).contains(&"1280x720@30"));
        // A full list keeps it in place of the last.
        let list = output_mode_choices(Some(&parse_v4l2_ctl(BRIO)), "800x600@20", false);
        assert_eq!(list.len(), OUTPUT_MAX_OPTIONS);
        assert!(values(&list).contains(&"800x600@20"));
        // No camera: the current value and two common modes.
        let list = output_mode_choices(None, "3840x2160@30", false);
        assert_eq!(
            values(&list),
            vec!["3840x2160@30", "1920x1080@30", "1280x720@30"]
        );
        let list = output_mode_choices(None, "1920x1080@30", true);
        assert_eq!(values(&list), vec!["1920x1080@30", "1280x720@30"]);
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
    fn parses_c930e_fixture() {
        let c = parse_v4l2_ctl(C930E);
        assert_eq!(c.formats(), vec![PixelFormat::Yuy2, PixelFormat::Mjpeg]);
        assert!(c.other_formats.is_empty());
        assert!(c.is_colour());
        assert_eq!(c.modes.len(), 217);
        assert!(c.modes.contains(&mode("MJPEG:1920x1080@30")));
        assert!(c.modes.contains(&mode("YUY2:1920x1080@5")));
        assert!(c.modes.contains(&mode("YUY2:2560x1472@2")));
        assert!(c.modes.contains(&mode("YUY2:1600x896@15/2")));
        // No 60 fps mode at any size, and no 2560x1472 in MJPEG.
        assert!(!c.modes.iter().any(|m| m.fps_num > 30 * m.fps_den));
        assert!(!c.modes.contains(&mode("MJPEG:2560x1472@2")));
    }

    #[test]
    fn c930e_input_choices_are_mjpeg_sizes_at_30_and_24() {
        let list = input_mode_choices(&parse_v4l2_ctl(C930E), INPUT_MAX_OPTIONS);
        assert_eq!(
            values(&list),
            vec![
                "1920x1080@30",
                "1920x1080@24",
                "1600x896@30",
                "1600x896@24",
                "1280x720@30",
                "1280x720@24",
                "1024x576@30",
                "1024x576@24",
                "960x540@30",
                "960x540@24",
                "800x600@30",
                "848x480@30",
                "800x448@30",
                "640x480@30",
                "640x360@30",
            ]
        );
    }

    #[test]
    fn c930e_1080p30_is_mjpeg_exact() {
        let c = pick(C930E, "1920x1080@30");
        assert_eq!(c.mode, mode("MJPEG:1920x1080@30"));
        assert!(c.exact);
        assert_eq!(c.ingest, IngestPath::CpuDecode);
        assert!(c.differences.is_empty());
        // YUYV asked for: 1080p YUYV runs at 5 fps only, so the rate wins.
        let c = pick(C930E, "YUY2:1920x1080@30");
        assert_eq!(c.mode, mode("MJPEG:1920x1080@30"));
        assert!(!c.exact);
    }

    #[test]
    fn c930e_60_fps_requests_degrade_to_30_mjpeg() {
        // The default `camera.mode` (1080p60) on a camera without 60 fps.
        let c = pick(C930E, "1920x1080@60");
        assert_eq!(c.mode, mode("MJPEG:1920x1080@30"));
        assert!(!c.exact);
        assert_eq!(c.ingest, IngestPath::CpuDecode);
        assert_eq!(c.differences, vec!["fps 60 -> 30".to_string()]);
        // 720p60: YUYV 720p is 10 fps at best, MJPEG 30.
        let c = pick(C930E, "1280x720@60");
        assert_eq!(c.mode, mode("MJPEG:1280x720@30"));
        assert!(!c.exact);
        assert_eq!(c.differences, vec!["fps 60 -> 30".to_string()]);
    }

    #[test]
    fn c930e_small_sizes_take_raw_on_the_tie() {
        let c = pick(C930E, "640x360@30");
        assert_eq!(c.mode, mode("YUY2:640x360@30"));
        assert!(c.exact);
        assert_eq!(c.ingest, IngestPath::CpuConvert);
        // 960x540 YUYV is 15 fps at best: MJPEG.
        let c = pick(C930E, "960x540@30");
        assert_eq!(c.mode, mode("MJPEG:960x540@30"));
        assert!(c.exact);
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
