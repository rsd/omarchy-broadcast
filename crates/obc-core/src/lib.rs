//! Shared pure types for oma-broadcast.
//!
//! Nothing in this crate touches GStreamer, CUDA or the filesystem: it holds the
//! value types every other crate agrees on (video modes, mode requests, device
//! selectors, output formats) and their parsing, so they can be unit-tested in
//! isolation. Nothing here knows about a particular camera model.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Serialize;

/// Card label of the dedicated virtual camera (see
/// `docs/adr/0004-v4l2loopback-first.md`).
pub const DEFAULT_SINK_LABEL: &str = "Omarchy Broadcast";

/// Errors produced while parsing or validating the shared types.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CoreError {
    #[error(
        "invalid video mode {input:?}: {reason} (expected [FORMAT:]WxH@FPS, e.g. 1920x1080@60 or NV12:1920x1080@60)"
    )]
    InvalidVideoMode { input: String, reason: String },
    #[error(
        "unsupported pixel format {0:?} (supported: NV12, NV21, I420, YV12, YUY2/YUYV, UYVY, RGB, BGR, MJPEG)"
    )]
    InvalidPixelFormat(String),
    #[error("unsupported output format {0:?} (supported: nv12, i420)")]
    InvalidOutputFormat(String),
    #[error("invalid USB id {0:?} (expected vid:pid in hex, e.g. 1234:abcd)")]
    InvalidUsbId(String),
    #[error(
        "empty camera selector (expected a card-name substring, a USB vid:pid such as 1234:abcd, or a /dev/videoN path)"
    )]
    EmptyCameraSelector,
}

/// Pixel formats a capture source can deliver and the pipeline can ingest.
///
/// Raw formats are named as GStreamer names them; [`PixelFormat::Mjpeg`] is
/// the compressed `image/jpeg` stream UVC cameras offer for large sizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub enum PixelFormat {
    /// 4:2:0, Y plane + interleaved UV plane (V4L2 `NV12`).
    Nv12,
    /// 4:2:0, Y plane + interleaved VU plane (V4L2 `NV21`).
    Nv21,
    /// 4:2:0, three planes (V4L2 `YU12`).
    I420,
    /// 4:2:0, three planes, V before U (V4L2 `YV12`).
    Yv12,
    /// 4:2:2 packed Y-U-Y-V (V4L2 `YUYV`).
    Yuy2,
    /// 4:2:2 packed U-Y-V-Y (V4L2 `UYVY`).
    Uyvy,
    /// Packed 24-bit RGB (V4L2 `RGB3`).
    Rgb,
    /// Packed 24-bit BGR (V4L2 `BGR3`).
    Bgr,
    /// Motion-JPEG (V4L2 `MJPG`/`JPEG`), GStreamer `image/jpeg`.
    Mjpeg,
}

impl PixelFormat {
    /// Every format, in declaration order.
    pub const ALL: [PixelFormat; 9] = [
        PixelFormat::Nv12,
        PixelFormat::Nv21,
        PixelFormat::I420,
        PixelFormat::Yv12,
        PixelFormat::Yuy2,
        PixelFormat::Uyvy,
        PixelFormat::Rgb,
        PixelFormat::Bgr,
        PixelFormat::Mjpeg,
    ];

    /// The GStreamer `format=` field value for raw formats; `"MJPEG"` for
    /// the compressed format (which has no `format=` field, see
    /// [`PixelFormat::is_compressed`]).
    pub fn gst_name(self) -> &'static str {
        match self {
            PixelFormat::Nv12 => "NV12",
            PixelFormat::Nv21 => "NV21",
            PixelFormat::I420 => "I420",
            PixelFormat::Yv12 => "YV12",
            PixelFormat::Yuy2 => "YUY2",
            PixelFormat::Uyvy => "UYVY",
            PixelFormat::Rgb => "RGB",
            PixelFormat::Bgr => "BGR",
            PixelFormat::Mjpeg => "MJPEG",
        }
    }

    /// Whether frames arrive compressed (`image/jpeg`), not as `video/x-raw`.
    pub fn is_compressed(self) -> bool {
        self == PixelFormat::Mjpeg
    }

    /// Whether the format subsamples chroma 2x2 (needs even width and height).
    pub fn is_420(self) -> bool {
        matches!(
            self,
            PixelFormat::Nv12 | PixelFormat::Nv21 | PixelFormat::I420 | PixelFormat::Yv12
        )
    }
}

impl fmt::Display for PixelFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.gst_name())
    }
}

impl FromStr for PixelFormat {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_uppercase().as_str() {
            "NV12" => Ok(PixelFormat::Nv12),
            "NV21" => Ok(PixelFormat::Nv21),
            "I420" | "YU12" => Ok(PixelFormat::I420),
            "YV12" => Ok(PixelFormat::Yv12),
            "YUY2" | "YUYV" => Ok(PixelFormat::Yuy2),
            "UYVY" => Ok(PixelFormat::Uyvy),
            "RGB" | "RGB3" => Ok(PixelFormat::Rgb),
            "BGR" | "BGR3" => Ok(PixelFormat::Bgr),
            "MJPEG" | "MJPG" | "JPEG" => Ok(PixelFormat::Mjpeg),
            _ => Err(CoreError::InvalidPixelFormat(s.to_string())),
        }
    }
}

/// Pixel format carried by the virtual camera.
///
/// Both are produced on the GPU by `cudaconvert`. YUYV is deliberately absent:
/// `cudaconvert` cannot produce YUY2, so it would need a CPU conversion or a
/// custom CUDA kernel (future work).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize)]
pub enum OutputFormat {
    #[default]
    Nv12,
    I420,
}

impl OutputFormat {
    /// The GStreamer `format=` field value.
    pub fn gst_name(self) -> &'static str {
        match self {
            OutputFormat::Nv12 => "NV12",
            OutputFormat::I420 => "I420",
        }
    }
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            OutputFormat::Nv12 => "nv12",
            OutputFormat::I420 => "i420",
        })
    }
}

impl FromStr for OutputFormat {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "nv12" => Ok(OutputFormat::Nv12),
            "i420" => Ok(OutputFormat::I420),
            _ => Err(CoreError::InvalidOutputFormat(s.to_string())),
        }
    }
}

/// Reduces a fraction by its greatest common divisor.
pub fn reduce_fraction(num: u32, den: u32) -> (u32, u32) {
    fn gcd(mut a: u32, mut b: u32) -> u32 {
        while b != 0 {
            (a, b) = (b, a % b);
        }
        a
    }
    match gcd(num, den) {
        0 => (num, den),
        g => (num / g, den / g),
    }
}

/// `WxH@FPS` (FPS optionally `N/D`), shared by [`VideoMode`] and [`ModeRequest`].
fn parse_size_rate(
    s: &str,
    bad: &dyn Fn(&str) -> CoreError,
) -> Result<(u32, u32, u32, u32), CoreError> {
    let (size, rate) = s.split_once('@').ok_or_else(|| bad("missing '@'"))?;
    let (w, h) = size
        .split_once(['x', 'X'])
        .ok_or_else(|| bad("size must be WIDTHxHEIGHT"))?;
    let width: u32 = w.trim().parse().map_err(|_| bad("width is not a number"))?;
    let height: u32 = h
        .trim()
        .parse()
        .map_err(|_| bad("height is not a number"))?;
    let (num, den) = match rate.split_once('/') {
        Some((n, d)) => (n, d),
        None => (rate, "1"),
    };
    let fps_num: u32 = num
        .trim()
        .parse()
        .map_err(|_| bad("frame rate is not a number"))?;
    let fps_den: u32 = den
        .trim()
        .parse()
        .map_err(|_| bad("frame rate denominator is not a number"))?;
    if width == 0 || height == 0 {
        return Err(bad("width and height must be non-zero"));
    }
    if fps_num == 0 || fps_den == 0 {
        return Err(bad("frame rate must be non-zero"));
    }
    Ok((width, height, fps_num, fps_den))
}

fn check_even(
    format: PixelFormat,
    width: u32,
    height: u32,
    bad: &dyn Fn(&str) -> CoreError,
) -> Result<(), CoreError> {
    if format.is_420() && (!width.is_multiple_of(2) || !height.is_multiple_of(2)) {
        return Err(bad("4:2:0 formats need even width and height"));
    }
    Ok(())
}

/// A concrete capture mode: pixel format, size and frame rate, e.g.
/// `NV12:1920x1080@60`.
///
/// The frame rate is a fraction so NTSC-style rates (`@30000/1001`) are exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct VideoMode {
    pub format: PixelFormat,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
}

impl VideoMode {
    /// Frame rate as a float.
    pub fn fps(&self) -> f64 {
        f64::from(self.fps_num) / f64::from(self.fps_den)
    }

    /// Duration of one frame in nanoseconds.
    pub fn frame_duration_ns(&self) -> u64 {
        (1_000_000_000u64 * u64::from(self.fps_den)) / u64::from(self.fps_num)
    }

    /// GStreamer caps string for this mode as the camera delivers it, e.g.
    /// `video/x-raw,format=NV12,width=1920,height=1080,framerate=60/1` or
    /// `image/jpeg,width=3840,height=2160,framerate=30/1`.
    pub fn to_caps_string(&self) -> String {
        let head = if self.format.is_compressed() {
            "image/jpeg".to_string()
        } else {
            format!("video/x-raw,format={}", self.format.gst_name())
        };
        format!(
            "{head},width={},height={},framerate={}/{}",
            self.width, self.height, self.fps_num, self.fps_den
        )
    }
}

fn fmt_size_rate(f: &mut fmt::Formatter<'_>, w: u32, h: u32, num: u32, den: u32) -> fmt::Result {
    write!(f, "{w}x{h}@")?;
    if den == 1 {
        write!(f, "{num}")
    } else {
        write!(f, "{num}/{den}")
    }
}

impl fmt::Display for VideoMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:", self.format)?;
        fmt_size_rate(f, self.width, self.height, self.fps_num, self.fps_den)
    }
}

impl FromStr for VideoMode {
    type Err = CoreError;

    /// Parses `FORMAT:WxH@FPS`; the format is required.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = |reason: &str| CoreError::InvalidVideoMode {
            input: s.to_string(),
            reason: reason.to_string(),
        };
        let (format, rest) = s.trim().split_once(':').ok_or_else(|| bad("missing ':'"))?;
        let format = format
            .parse::<PixelFormat>()
            .map_err(|e| bad(&e.to_string()))?;
        let (width, height, fps_num, fps_den) = parse_size_rate(rest, &bad)?;
        check_even(format, width, height, &bad)?;
        Ok(VideoMode {
            format,
            width,
            height,
            fps_num,
            fps_den,
        })
    }
}

/// What the user asked for: a size and frame rate, optionally a format.
///
/// Parsed from `WxH@FPS` (any format) or `FORMAT:WxH@FPS`. The pipeline
/// resolves it against a camera's capabilities into a concrete
/// [`VideoMode`]; the request itself may not be offered by any camera.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct ModeRequest {
    /// Preferred pixel format; `None` accepts any.
    pub format: Option<PixelFormat>,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
}

impl Default for ModeRequest {
    /// 1920x1080 at 60 fps, any format.
    fn default() -> Self {
        ModeRequest {
            format: None,
            width: 1920,
            height: 1080,
            fps_num: 60,
            fps_den: 1,
        }
    }
}

impl ModeRequest {
    /// Frame rate as a float.
    pub fn fps(&self) -> f64 {
        f64::from(self.fps_num) / f64::from(self.fps_den)
    }

    /// Whether `mode` is exactly what was asked for: same size, same frame
    /// rate (as a reduced fraction) and, if a format was asked for, that format.
    pub fn is_exact(&self, mode: &VideoMode) -> bool {
        mode.width == self.width
            && mode.height == self.height
            && reduce_fraction(mode.fps_num, mode.fps_den)
                == reduce_fraction(self.fps_num, self.fps_den)
            && self.format.is_none_or(|f| f == mode.format)
    }
}

impl From<VideoMode> for ModeRequest {
    fn from(m: VideoMode) -> Self {
        ModeRequest {
            format: Some(m.format),
            width: m.width,
            height: m.height,
            fps_num: m.fps_num,
            fps_den: m.fps_den,
        }
    }
}

impl fmt::Display for ModeRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(format) = self.format {
            write!(f, "{format}:")?;
        }
        fmt_size_rate(f, self.width, self.height, self.fps_num, self.fps_den)
    }
}

impl FromStr for ModeRequest {
    type Err = CoreError;

    /// Parses `WxH@FPS` or `FORMAT:WxH@FPS`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = |reason: &str| CoreError::InvalidVideoMode {
            input: s.to_string(),
            reason: reason.to_string(),
        };
        let t = s.trim();
        let (format, rest) = match t.split_once(':') {
            Some((f, rest)) => (
                Some(f.parse::<PixelFormat>().map_err(|e| bad(&e.to_string()))?),
                rest,
            ),
            None => (None, t),
        };
        let (width, height, fps_num, fps_den) = parse_size_rate(rest, &bad)?;
        if let Some(format) = format {
            check_even(format, width, height, &bad)?;
        }
        Ok(ModeRequest {
            format,
            width,
            height,
            fps_num,
            fps_den,
        })
    }
}

/// A USB vendor:product id pair, e.g. `1234:abcd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct UsbId {
    pub vendor: u16,
    pub product: u16,
}

impl fmt::Display for UsbId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04x}:{:04x}", self.vendor, self.product)
    }
}

impl FromStr for UsbId {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || CoreError::InvalidUsbId(s.to_string());
        let (v, p) = s.trim().split_once(':').ok_or_else(err)?;
        let hex = |x: &str| {
            let x = x.trim_start_matches("0x").trim_start_matches("0X");
            if x.is_empty() || x.len() > 4 {
                return Err(err());
            }
            u16::from_str_radix(x, 16).map_err(|_| err())
        };
        Ok(UsbId {
            vendor: hex(v)?,
            product: hex(p)?,
        })
    }
}

/// Which physical camera to capture from: one argument, three forms.
///
/// Parsing (`--camera`) is by shape, in this order:
///
/// 1. starts with `/dev/` → [`CameraSelector::Path`] (a `/dev/videoN` node or
///    a stable `/dev/v4l/by-id/...` link);
/// 2. exactly `hhhh:hhhh` (four hex digits each side) → [`CameraSelector::Usb`];
/// 3. anything else → [`CameraSelector::Name`], a case-insensitive substring
///    of the V4L2 card name.
///
/// No selector at all means "pick automatically" (the pipeline crate's
/// `select_camera`); there is no default camera.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraSelector {
    Path(PathBuf),
    Usb(UsbId),
    Name(String),
}

impl CameraSelector {
    /// Whether a device node with this path, card name and USB id satisfies
    /// the selector. Paths compare exactly; resolve symlinks before calling.
    pub fn matches(&self, path: &Path, card_name: &str, usb: Option<UsbId>) -> bool {
        match self {
            CameraSelector::Path(p) => p == path,
            CameraSelector::Usb(id) => usb == Some(*id),
            CameraSelector::Name(want) => card_name
                .to_lowercase()
                .contains(&want.trim().to_lowercase()),
        }
    }
}

/// `hhhh:hhhh`, exactly four hex digits on each side.
fn is_strict_usb_id(s: &str) -> bool {
    let hex4 = |x: &str| x.len() == 4 && x.bytes().all(|b| b.is_ascii_hexdigit());
    s.split_once(':').is_some_and(|(v, p)| hex4(v) && hex4(p))
}

impl FromStr for CameraSelector {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let t = s.trim();
        if t.is_empty() {
            return Err(CoreError::EmptyCameraSelector);
        }
        if t.starts_with("/dev/") {
            return Ok(CameraSelector::Path(PathBuf::from(t)));
        }
        if is_strict_usb_id(t) {
            return t.parse().map(CameraSelector::Usb);
        }
        Ok(CameraSelector::Name(t.to_string()))
    }
}

impl fmt::Display for CameraSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CameraSelector::Path(p) => write!(f, "path {}", p.display()),
            CameraSelector::Usb(u) => write!(f, "usb {u}"),
            CameraSelector::Name(n) => write!(f, "name~{n:?}"),
        }
    }
}

/// Which v4l2loopback device receives the output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum SinkSelector {
    /// Exact v4l2loopback card label (default "Omarchy Broadcast").
    Label(String),
    /// Explicit device node, for testing and overrides.
    Path(PathBuf),
}

impl Default for SinkSelector {
    fn default() -> Self {
        SinkSelector::Label(DEFAULT_SINK_LABEL.to_string())
    }
}

impl fmt::Display for SinkSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SinkSelector::Label(l) => write!(f, "label={l:?}"),
            SinkSelector::Path(p) => write!(f, "path={}", p.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mode() {
        let m: VideoMode = "NV12:1920x1080@60".parse().unwrap();
        assert_eq!(m.format, PixelFormat::Nv12);
        assert_eq!(
            (m.width, m.height, m.fps_num, m.fps_den),
            (1920, 1080, 60, 1)
        );
        assert_eq!(m.to_string(), "NV12:1920x1080@60");
        assert_eq!(m.frame_duration_ns(), 16_666_666);
        assert!((m.fps() - 60.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parses_fractional_rate_and_lowercase() {
        let m: VideoMode = "yuyv:1280X720@30000/1001".parse().unwrap();
        assert_eq!(m.format, PixelFormat::Yuy2);
        assert_eq!((m.width, m.height), (1280, 720));
        assert_eq!((m.fps_num, m.fps_den), (30000, 1001));
        assert_eq!(m.to_string(), "YUY2:1280x720@30000/1001");
    }

    #[test]
    fn caps_strings() {
        let m: VideoMode = "NV12:1920x1080@60".parse().unwrap();
        assert_eq!(
            m.to_caps_string(),
            "video/x-raw,format=NV12,width=1920,height=1080,framerate=60/1"
        );
        let j: VideoMode = "MJPG:3840x2160@30".parse().unwrap();
        assert_eq!(j.format, PixelFormat::Mjpeg);
        assert_eq!(
            j.to_caps_string(),
            "image/jpeg,width=3840,height=2160,framerate=30/1"
        );
    }

    #[test]
    fn rejects_bad_modes() {
        for bad in [
            "",
            "NV12",
            "NV12:1920x1080",
            "NV12:1920@60",
            "H264:1920x1080@60",
            "NV12:0x1080@60",
            "NV12:1920x1080@0",
            "NV12:1920x1080@60/0",
            "NV12:1921x1080@60",
            "NV12:axb@60",
            "1920x1080@60",
        ] {
            assert!(bad.parse::<VideoMode>().is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn mode_request_accepts_both_forms() {
        let any: ModeRequest = "1920x1080@60".parse().unwrap();
        assert_eq!(any, ModeRequest::default());
        assert_eq!(any.format, None);
        assert_eq!(any.to_string(), "1920x1080@60");

        let nv12: ModeRequest = "NV12:1920x1080@60".parse().unwrap();
        assert_eq!(nv12.format, Some(PixelFormat::Nv12));
        assert_eq!(nv12.to_string(), "NV12:1920x1080@60");

        let yuy2: ModeRequest = "YUYV:960x540@30".parse().unwrap();
        assert_eq!(yuy2.format, Some(PixelFormat::Yuy2));
        let mjpeg: ModeRequest = "mjpeg:3840x2160@30".parse().unwrap();
        assert_eq!(mjpeg.format, Some(PixelFormat::Mjpeg));

        for bad in [
            "",
            "1920x1080",
            "1920@60",
            "FOO:1920x1080@60",
            "NV12:1921x1080@60",
        ] {
            assert!(bad.parse::<ModeRequest>().is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn mode_request_exactness() {
        let req: ModeRequest = "1920x1080@30".parse().unwrap();
        let m: VideoMode = "MJPEG:1920x1080@30".parse().unwrap();
        assert!(req.is_exact(&m));
        let m60: VideoMode = "NV12:1920x1080@60/2".parse().unwrap();
        assert!(req.is_exact(&m60), "reduced fractions compare equal");
        let nv12: ModeRequest = "NV12:1920x1080@30".parse().unwrap();
        assert!(!nv12.is_exact(&m), "a requested format must match");
        let smaller: VideoMode = "MJPEG:1280x720@30".parse().unwrap();
        assert!(!req.is_exact(&smaller));
        assert_eq!(ModeRequest::from(m).format, Some(PixelFormat::Mjpeg));
    }

    #[test]
    fn pixel_format_names() {
        for f in PixelFormat::ALL {
            assert_eq!(f.gst_name().parse::<PixelFormat>().unwrap(), f);
        }
        assert_eq!("yuyv".parse::<PixelFormat>().unwrap(), PixelFormat::Yuy2);
        assert_eq!("MJPG".parse::<PixelFormat>().unwrap(), PixelFormat::Mjpeg);
        assert_eq!("YU12".parse::<PixelFormat>().unwrap(), PixelFormat::I420);
        assert!("GREY".parse::<PixelFormat>().is_err());
        assert!(PixelFormat::Mjpeg.is_compressed());
        assert!(!PixelFormat::Yuy2.is_420());
    }

    #[test]
    fn output_format_parse() {
        assert_eq!("NV12".parse::<OutputFormat>().unwrap(), OutputFormat::Nv12);
        assert_eq!("i420".parse::<OutputFormat>().unwrap(), OutputFormat::I420);
        assert!("yuyv".parse::<OutputFormat>().is_err());
        assert_eq!(OutputFormat::I420.gst_name(), "I420");
        assert_eq!(OutputFormat::default(), OutputFormat::Nv12);
    }

    #[test]
    fn usb_id_parse() {
        let id: UsbId = "046d:0944".parse().unwrap();
        assert_eq!(
            id,
            UsbId {
                vendor: 0x046d,
                product: 0x0944
            }
        );
        assert_eq!(id.to_string(), "046d:0944");
        assert_eq!("0x046D:0x0944".parse::<UsbId>().unwrap(), id);
        for bad in ["046d", "046d:", ":0944", "zzzz:0944", "12345:0944"] {
            assert!(bad.parse::<UsbId>().is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn camera_selector_parses_three_forms() {
        assert_eq!(
            "/dev/video4".parse::<CameraSelector>().unwrap(),
            CameraSelector::Path("/dev/video4".into())
        );
        assert_eq!(
            " /dev/v4l/by-id/usb-cam-video-index0 "
                .parse::<CameraSelector>()
                .unwrap(),
            CameraSelector::Path("/dev/v4l/by-id/usb-cam-video-index0".into())
        );
        assert_eq!(
            "046d:0944".parse::<CameraSelector>().unwrap(),
            CameraSelector::Usb(UsbId {
                vendor: 0x046d,
                product: 0x0944
            })
        );
        assert_eq!(
            "5986:1193".parse::<CameraSelector>().unwrap(),
            CameraSelector::Usb(UsbId {
                vendor: 0x5986,
                product: 0x1193
            })
        );
        // Upper-case hex is still a USB id.
        assert!(matches!(
            "046D:0944".parse::<CameraSelector>().unwrap(),
            CameraSelector::Usb(_)
        ));
        // Not exactly hhhh:hhhh: a name.
        for name in [
            "MX Brio",
            "FHD Camera: FHD Camera",
            "46d:944",
            "cam:era1",
            "video4",
        ] {
            assert_eq!(
                name.parse::<CameraSelector>().unwrap(),
                CameraSelector::Name(name.to_string()),
                "{name:?}"
            );
        }
        assert_eq!(
            "   ".parse::<CameraSelector>(),
            Err(CoreError::EmptyCameraSelector)
        );
    }

    #[test]
    fn camera_selector_matching() {
        let brio = UsbId {
            vendor: 0x046d,
            product: 0x0944,
        };
        let p = Path::new("/dev/video4");
        let by_name: CameraSelector = "mx brio".parse().unwrap();
        assert!(by_name.matches(p, "MX Brio", Some(brio)));
        assert!(!by_name.matches(p, "FHD Camera: FHD Camera", None));

        let by_usb: CameraSelector = "046d:0944".parse().unwrap();
        assert!(by_usb.matches(p, "anything", Some(brio)));
        assert!(!by_usb.matches(p, "MX Brio", None));

        let by_path: CameraSelector = "/dev/video4".parse().unwrap();
        assert!(by_path.matches(p, "whatever", None));
        assert!(!by_path.matches(Path::new("/dev/video40"), "whatever", None));
        assert_eq!(by_path.to_string(), "path /dev/video4");
        assert_eq!(by_usb.to_string(), "usb 046d:0944");
    }

    #[test]
    fn fraction_reduction() {
        assert_eq!(reduce_fraction(60, 2), (30, 1));
        assert_eq!(reduce_fraction(15, 2), (15, 2));
        assert_eq!(reduce_fraction(30000, 1001), (30000, 1001));
        assert_eq!(reduce_fraction(0, 0), (0, 0));
    }

    #[test]
    fn sink_selector_default_label() {
        assert_eq!(
            SinkSelector::default(),
            SinkSelector::Label("Omarchy Broadcast".into())
        );
    }
}
