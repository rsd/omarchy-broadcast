//! Shared pure types for omarchy-broadcast.
//!
//! Nothing in this crate touches GStreamer or CUDA: it holds the value types
//! every other crate agrees on (video modes, mode requests, device selectors,
//! output formats) and their parsing, so they can be unit-tested in
//! isolation. The only filesystem access is reading the daemon's config files
//! and reading and writing its state file ([`config`]), and writing the one
//! managed drop-in of the config set, `config.d/50-panel.toml`
//! ([`config_d`]), and the daemons' start-up check for an installed
//! NVIDIA Maxine SDK, which reads its environment file and executes the
//! process again with it ([`sdk_env`]). Nothing here knows about a
//! particular camera model.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Serialize;

pub mod battery_saver;
pub mod config;
pub mod config_d;
pub mod config_schema;
pub mod modules;
pub mod sdk_env;

/// Card label of the dedicated virtual camera.
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
    #[error(
        "invalid output contract {input:?}: {reason} (expected WxH@FPS, e.g. 1920x1080@30 or 1920x1080@30000/1001)"
    )]
    InvalidOutputContract { input: String, reason: String },
    #[error("unsupported colour range {0:?} (supported: limited, full)")]
    InvalidColourRange(String),
    #[error("unsupported aspect policy {0:?} (supported: crop, letterbox)")]
    InvalidAspectPolicy(String),
    #[error("unknown effect {0:?} (supported: none, blur, replace)")]
    InvalidEffect(String),
    #[error("unsupported effect failure policy {0:?} (supported: black, image, none)")]
    InvalidEffectOnFailure(String),
    #[error("unsupported privacy mode {0:?} (supported: black, away, freeze)")]
    InvalidPrivacyMode(String),
    #[error("unsupported placeholder {0:?} (supported: black, image)")]
    InvalidOutputPlaceholder(String),
    #[error("unknown relighting light {0:?} (supported: warm, neutral, cool)")]
    InvalidRelightLight(String),
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

/// Colour range of the frames the output delivers.
///
/// Limited range (Y 16..235, chroma 16..240) is what most consumers assume
/// whatever the caps say; full range (0..255) is available for those that
/// honour the label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ColourRange {
    #[default]
    Limited,
    Full,
}

impl fmt::Display for ColourRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ColourRange::Limited => "limited",
            ColourRange::Full => "full",
        })
    }
}

impl FromStr for ColourRange {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "limited" => Ok(ColourRange::Limited),
            "full" => Ok(ColourRange::Full),
            _ => Err(CoreError::InvalidColourRange(s.to_string())),
        }
    }
}

/// How a source whose aspect ratio differs from the output's is fitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AspectPolicy {
    /// Centre crop to fill the whole output frame (no borders).
    #[default]
    Crop,
    /// Scale the whole source into the frame and pad it with black borders.
    Letterbox,
}

impl fmt::Display for AspectPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AspectPolicy::Crop => "crop",
            AspectPolicy::Letterbox => "letterbox",
        })
    }
}

impl FromStr for AspectPolicy {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "crop" => Ok(AspectPolicy::Crop),
            "letterbox" => Ok(AspectPolicy::Letterbox),
            _ => Err(CoreError::InvalidAspectPolicy(s.to_string())),
        }
    }
}

/// The video effect of the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectMode {
    /// No effect: the camera image as it is.
    #[default]
    None,
    /// Background blur.
    Blur,
    /// Background replacement with an image.
    Replace,
}

impl EffectMode {
    /// `none`, `blur` or `replace`: the name in the config, the state file
    /// and on D-Bus.
    pub fn as_str(self) -> &'static str {
        match self {
            EffectMode::None => "none",
            EffectMode::Blur => "blur",
            EffectMode::Replace => "replace",
        }
    }
}

impl fmt::Display for EffectMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EffectMode {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" => Ok(EffectMode::None),
            "blur" => Ok(EffectMode::Blur),
            "replace" => Ok(EffectMode::Replace),
            _ => Err(CoreError::InvalidEffect(s.to_string())),
        }
    }
}

/// What the virtual camera shows when a requested effect cannot run: the
/// one policy resolved from `effect.privacy` and `effect.privacy_mode`
/// (or, when neither is set, from the legacy `effect.on_failure`; see
/// [`config::EffectConfig::policy`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectOnFailure {
    /// Black frames, the camera closed and the daemon in `error`: the
    /// background stays hidden (fails closed). Privacy on, `black`.
    Black,
    /// The camera image without the effect, with the reason in `Error`.
    /// Privacy off (the default).
    #[default]
    None,
    /// As [`EffectOnFailure::Black`], with the still image of
    /// `effect.fallback_image` in place of the black frames (black when it
    /// is not set or cannot be read, with a `Warning` saying so). Privacy
    /// on, `away`.
    Image,
    /// As [`EffectOnFailure::Black`], with the last frame the effect
    /// produced held in place of the black frames; with none yet, the away
    /// image when one is set, else black. Privacy on, `freeze`.
    Freeze,
}

impl EffectOnFailure {
    /// `black`, `none`, `image` or `freeze`.
    pub fn as_str(self) -> &'static str {
        match self {
            EffectOnFailure::Black => "black",
            EffectOnFailure::None => "none",
            EffectOnFailure::Image => "image",
            EffectOnFailure::Freeze => "freeze",
        }
    }

    /// Whether the camera is hidden when the effect cannot run: `black`,
    /// `image` and `freeze` hide it (every privacy rule treats them alike),
    /// `none` shows it.
    pub fn hides(self) -> bool {
        !matches!(self, EffectOnFailure::None)
    }

    /// The policy of `effect.privacy` and `effect.privacy_mode`: off is
    /// [`EffectOnFailure::None`], on follows the mode.
    pub fn of_privacy(privacy: bool, mode: PrivacyMode) -> EffectOnFailure {
        match (privacy, mode) {
            (false, _) => EffectOnFailure::None,
            (true, PrivacyMode::Black) => EffectOnFailure::Black,
            (true, PrivacyMode::Away) => EffectOnFailure::Image,
            (true, PrivacyMode::Freeze) => EffectOnFailure::Freeze,
        }
    }

    /// The `effect.privacy` and `effect.privacy_mode` this policy stands
    /// for: `none` is privacy off, with no mode.
    pub fn privacy_keys(self) -> (bool, Option<PrivacyMode>) {
        match self {
            EffectOnFailure::None => (false, None),
            EffectOnFailure::Black => (true, Some(PrivacyMode::Black)),
            EffectOnFailure::Image => (true, Some(PrivacyMode::Away)),
            EffectOnFailure::Freeze => (true, Some(PrivacyMode::Freeze)),
        }
    }
}

impl fmt::Display for EffectOnFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Parses a value of the legacy key `effect.on_failure`: `black`, `image`
/// or `none` (`freeze` was never one of its values).
impl FromStr for EffectOnFailure {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "black" => Ok(EffectOnFailure::Black),
            "none" => Ok(EffectOnFailure::None),
            "image" => Ok(EffectOnFailure::Image),
            _ => Err(CoreError::InvalidEffectOnFailure(s.to_string())),
        }
    }
}

/// What privacy shows in place of the camera when it hides it
/// (`effect.privacy_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyMode {
    /// Black frames.
    #[default]
    Black,
    /// The away image (`effect.fallback_image`).
    Away,
    /// The last frame the effect produced; with none yet, the away image
    /// when one is set, else black.
    Freeze,
}

impl PrivacyMode {
    /// `black`, `away` or `freeze`.
    pub fn as_str(self) -> &'static str {
        match self {
            PrivacyMode::Black => "black",
            PrivacyMode::Away => "away",
            PrivacyMode::Freeze => "freeze",
        }
    }
}

impl fmt::Display for PrivacyMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PrivacyMode {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "black" => Ok(PrivacyMode::Black),
            "away" => Ok(PrivacyMode::Away),
            "freeze" => Ok(PrivacyMode::Freeze),
            _ => Err(CoreError::InvalidPrivacyMode(s.to_string())),
        }
    }
}

/// What the virtual camera shows while there is no camera picture
/// (`[output] placeholder`): the camera off, a start before its first
/// frame, a session that cannot start or stream, an input gap past the
/// hold. Never while the camera picture streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputPlaceholder {
    /// Black frames.
    #[default]
    Black,
    /// The still image of `effect.fallback_image` (the "away image") in
    /// place of the black frames (black when it is not set or cannot be
    /// read, with a `Warning` saying so).
    Image,
}

impl OutputPlaceholder {
    /// `black` or `image`.
    pub fn as_str(self) -> &'static str {
        match self {
            OutputPlaceholder::Black => "black",
            OutputPlaceholder::Image => "image",
        }
    }
}

impl fmt::Display for OutputPlaceholder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for OutputPlaceholder {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "black" => Ok(OutputPlaceholder::Black),
            "image" => Ok(OutputPlaceholder::Image),
            _ => Err(CoreError::InvalidOutputPlaceholder(s.to_string())),
        }
    }
}

/// The key light of the relighting stage (`effect.relight_light`): its
/// colour temperature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelightLight {
    /// A warm light (tungsten).
    Warm,
    /// A neutral light.
    #[default]
    Neutral,
    /// A cool light (daylight).
    Cool,
}

impl RelightLight {
    /// Every light, in display order.
    pub const ALL: [RelightLight; 3] = [
        RelightLight::Warm,
        RelightLight::Neutral,
        RelightLight::Cool,
    ];

    /// `warm`, `neutral` or `cool`: the name in the config, the state file
    /// and the settings tree.
    pub fn as_str(self) -> &'static str {
        match self {
            RelightLight::Warm => "warm",
            RelightLight::Neutral => "neutral",
            RelightLight::Cool => "cool",
        }
    }
}

impl fmt::Display for RelightLight {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RelightLight {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "warm" => Ok(RelightLight::Warm),
            "neutral" => Ok(RelightLight::Neutral),
            "cool" => Ok(RelightLight::Cool),
            _ => Err(CoreError::InvalidRelightLight(s.to_string())),
        }
    }
}

/// What the output delivers, whatever feeds it: format, size, frame rate and
/// colour range. Set by configuration and never derived from a source.
///
/// Parsed from `WxH@FPS` (the format and range keep their defaults; set them
/// separately). Both dimensions must be even: every output format is 4:2:0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct OutputContract {
    pub format: OutputFormat,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub range: ColourRange,
}

impl Default for OutputContract {
    /// NV12, 1920x1080 at 30 fps, limited range.
    fn default() -> Self {
        OutputContract {
            format: OutputFormat::Nv12,
            width: 1920,
            height: 1080,
            fps_num: 30,
            fps_den: 1,
            range: ColourRange::Limited,
        }
    }
}

impl OutputContract {
    /// Highest output frame rate accepted, in frames per second.
    pub const MAX_FPS: u32 = 240;
    /// Largest output width or height accepted, in pixels (the largest
    /// frame v4l2loopback accepts).
    pub const MAX_DIMENSION: u32 = 8192;

    /// Frame rate as a float.
    pub fn fps(&self) -> f64 {
        f64::from(self.fps_num) / f64::from(self.fps_den)
    }

    /// Duration of one output frame in nanoseconds, rounded down (the exact
    /// value is `1e9 * fps_den / fps_num`; use the fraction for timestamps).
    pub fn frame_duration_ns(&self) -> u64 {
        (1_000_000_000u64 * u64::from(self.fps_den)) / u64::from(self.fps_num)
    }

    /// GStreamer colorimetry label of the output frames: BT.601 matrix (the
    /// matrix UVC cameras and JPEG use), BT.709 transfer and primaries, and
    /// the contract's range. Limited is `2:4:5:1`, full is `1:4:5:1`.
    pub fn colorimetry(&self) -> &'static str {
        match self.range {
            ColourRange::Limited => "2:4:5:1",
            ColourRange::Full => "1:4:5:1",
        }
    }

    /// Checks the invariants [`FromStr`] enforces, for contracts built
    /// field by field: non-zero even size up to [`Self::MAX_DIMENSION`],
    /// non-zero frame rate up to [`Self::MAX_FPS`].
    pub fn validate(&self) -> Result<(), CoreError> {
        let bad = |reason: &str| CoreError::InvalidOutputContract {
            input: self.to_string(),
            reason: reason.to_string(),
        };
        if self.width == 0 || self.height == 0 {
            return Err(bad("width and height must be non-zero"));
        }
        if self.fps_num == 0 || self.fps_den == 0 {
            return Err(bad("frame rate must be non-zero"));
        }
        if !self.width.is_multiple_of(2) || !self.height.is_multiple_of(2) {
            return Err(bad("width and height must be even (4:2:0 output)"));
        }
        if self.width > Self::MAX_DIMENSION || self.height > Self::MAX_DIMENSION {
            return Err(bad(&format!(
                "width and height must be at most {}",
                Self::MAX_DIMENSION
            )));
        }
        // fps_num / fps_den > MAX_FPS, without overflow.
        if u64::from(self.fps_num) > u64::from(Self::MAX_FPS) * u64::from(self.fps_den) {
            return Err(bad(&format!(
                "frame rate must be at most {} fps",
                Self::MAX_FPS
            )));
        }
        Ok(())
    }
}

impl fmt::Display for OutputContract {
    /// `nv12:1920x1080@30 limited`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:", self.format)?;
        fmt_size_rate(f, self.width, self.height, self.fps_num, self.fps_den)?;
        write!(f, " {}", self.range)
    }
}

impl FromStr for OutputContract {
    type Err = CoreError;

    /// Parses `WxH@FPS` (FPS optionally `N/D`) with the default format and
    /// range.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = |reason: &str| CoreError::InvalidOutputContract {
            input: s.to_string(),
            reason: reason.to_string(),
        };
        let (width, height, fps_num, fps_den) = parse_size_rate(s.trim(), &bad)?;
        if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(bad("width and height must be even (4:2:0 output)"));
        }
        let contract = OutputContract {
            width,
            height,
            fps_num,
            fps_den,
            ..OutputContract::default()
        };
        contract.validate().map_err(|e| match e {
            CoreError::InvalidOutputContract { reason, .. } => bad(&reason),
            other => other,
        })?;
        Ok(contract)
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
    fn output_contract_default_and_labels() {
        let c = OutputContract::default();
        assert_eq!(
            (c.format, c.width, c.height, c.fps_num, c.fps_den, c.range),
            (OutputFormat::Nv12, 1920, 1080, 30, 1, ColourRange::Limited)
        );
        assert_eq!(c.colorimetry(), "2:4:5:1");
        assert_eq!(c.frame_duration_ns(), 33_333_333);
        assert_eq!(c.to_string(), "nv12:1920x1080@30 limited");
        let full = OutputContract {
            range: ColourRange::Full,
            format: OutputFormat::I420,
            ..c
        };
        assert_eq!(full.colorimetry(), "1:4:5:1");
        assert_eq!(full.to_string(), "i420:1920x1080@30 full");
        assert!(c.validate().is_ok());
    }

    #[test]
    fn output_contract_parse() {
        let c: OutputContract = "1280x720@60".parse().unwrap();
        assert_eq!(
            (c.width, c.height, c.fps_num, c.fps_den),
            (1280, 720, 60, 1)
        );
        assert_eq!(c.format, OutputFormat::Nv12);
        assert_eq!(c.range, ColourRange::Limited);
        let ntsc: OutputContract = " 1920X1080@30000/1001 ".parse().unwrap();
        assert_eq!((ntsc.fps_num, ntsc.fps_den), (30000, 1001));
        assert_eq!(ntsc.frame_duration_ns(), 33_366_666);
        assert!((ntsc.fps() - 29.97).abs() < 0.001);
        assert_eq!(ntsc.to_string(), "nv12:1920x1080@30000/1001 limited");
        for bad in [
            "",
            "1920x1080",
            "1920@30",
            "0x1080@30",
            "1920x0@30",
            "1920x1080@0",
            "1920x1080@30/0",
            "1921x1080@30",
            "1920x1081@30",
            "NV12:1920x1080@30",
            "axb@30",
            "1920x1080@241",
            "1920x1080@481/2",
            "8194x1080@30",
            "1920x8194@30",
        ] {
            let e = bad.parse::<OutputContract>();
            assert!(
                matches!(e, Err(CoreError::InvalidOutputContract { .. })),
                "{bad:?} should fail: {e:?}"
            );
        }
        let odd = OutputContract {
            width: 641,
            ..OutputContract::default()
        };
        assert!(odd.validate().is_err());
        let zero_rate = OutputContract {
            fps_num: 0,
            ..OutputContract::default()
        };
        assert!(zero_rate.validate().is_err());
    }

    #[test]
    fn output_contract_bounds() {
        let at = |width, height, fps_num, fps_den| OutputContract {
            width,
            height,
            fps_num,
            fps_den,
            ..OutputContract::default()
        };
        assert!(at(8192, 8192, 240, 1).validate().is_ok());
        assert!(at(2, 2, 1, 1000).validate().is_ok());
        assert!(at(1920, 1080, 480, 2).validate().is_ok());
        assert!(at(1920, 1080, 240_001, 1000).validate().is_err());
        assert!(at(1920, 1080, 241, 1).validate().is_err());
        assert!(at(1920, 1080, u32::MAX, 1).validate().is_err());
        assert!(at(8194, 1080, 30, 1).validate().is_err());
        assert!(at(1920, 8194, 30, 1).validate().is_err());
        let e = at(1920, 1080, 300, 1).validate().unwrap_err().to_string();
        assert!(e.contains("240"), "{e}");
        let parsed: OutputContract = "8192x4320@240".parse().unwrap();
        assert_eq!((parsed.width, parsed.fps_num), (8192, 240));
    }

    #[test]
    fn colour_range_and_aspect_parse() {
        assert_eq!(
            "Limited".parse::<ColourRange>().unwrap(),
            ColourRange::Limited
        );
        assert_eq!("full".parse::<ColourRange>().unwrap(), ColourRange::Full);
        assert!("tv".parse::<ColourRange>().is_err());
        assert_eq!(ColourRange::Full.to_string(), "full");
        assert_eq!(ColourRange::default(), ColourRange::Limited);

        assert_eq!("crop".parse::<AspectPolicy>().unwrap(), AspectPolicy::Crop);
        assert_eq!(
            " LETTERBOX".parse::<AspectPolicy>().unwrap(),
            AspectPolicy::Letterbox
        );
        assert!("stretch".parse::<AspectPolicy>().is_err());
        assert_eq!(AspectPolicy::Letterbox.to_string(), "letterbox");
        assert_eq!(AspectPolicy::default(), AspectPolicy::Crop);
    }

    #[test]
    fn effect_mode_and_failure_policy_parse() {
        for (text, mode) in [
            ("none", EffectMode::None),
            ("blur", EffectMode::Blur),
            (" Replace", EffectMode::Replace),
        ] {
            assert_eq!(text.parse::<EffectMode>().unwrap(), mode);
            assert_eq!(mode.to_string().parse::<EffectMode>().unwrap(), mode);
        }
        let e = "sparkle".parse::<EffectMode>().unwrap_err().to_string();
        assert!(
            e.contains("\"sparkle\"") && e.contains("none, blur, replace"),
            "{e}"
        );
        assert_eq!(EffectMode::default(), EffectMode::None);
        assert_eq!(EffectMode::Replace.as_str(), "replace");

        assert_eq!(
            "black".parse::<EffectOnFailure>().unwrap(),
            EffectOnFailure::Black
        );
        assert_eq!(
            "NONE".parse::<EffectOnFailure>().unwrap(),
            EffectOnFailure::None
        );
        assert!("raw".parse::<EffectOnFailure>().is_err());
        assert_eq!(EffectOnFailure::default(), EffectOnFailure::None);
        assert!(
            "freeze".parse::<EffectOnFailure>().is_err(),
            "not a legacy value"
        );
        assert_eq!(EffectOnFailure::None.to_string(), "none");
        assert_eq!(
            "Image".parse::<EffectOnFailure>().unwrap(),
            EffectOnFailure::Image
        );
        assert_eq!(EffectOnFailure::Image.to_string(), "image");
        assert!(EffectOnFailure::Black.hides() && EffectOnFailure::Image.hides());
        assert!(EffectOnFailure::Freeze.hides());
        assert!(!EffectOnFailure::None.hides());
        assert_eq!(EffectOnFailure::Freeze.as_str(), "freeze");
        for mode in [PrivacyMode::Black, PrivacyMode::Away, PrivacyMode::Freeze] {
            assert_eq!(mode.as_str().parse::<PrivacyMode>().unwrap(), mode);
            assert_eq!(
                EffectOnFailure::of_privacy(false, mode),
                EffectOnFailure::None
            );
            let on = EffectOnFailure::of_privacy(true, mode);
            assert!(on.hides());
            assert_eq!(on.privacy_keys(), (true, Some(mode)));
        }
        assert_eq!(EffectOnFailure::None.privacy_keys(), (false, None));
        assert!("raw".parse::<PrivacyMode>().is_err());
        assert_eq!(PrivacyMode::default(), PrivacyMode::Black);
        assert_eq!(
            "Image".parse::<OutputPlaceholder>().unwrap(),
            OutputPlaceholder::Image
        );
        assert_eq!(
            " black ".parse::<OutputPlaceholder>().unwrap(),
            OutputPlaceholder::Black
        );
        assert!("none".parse::<OutputPlaceholder>().is_err());
        assert_eq!(OutputPlaceholder::default(), OutputPlaceholder::Black);
        assert_eq!(OutputPlaceholder::Image.to_string(), "image");
    }

    #[test]
    fn relight_light_parses() {
        for light in RelightLight::ALL {
            assert_eq!(light.as_str().parse::<RelightLight>().unwrap(), light);
        }
        assert_eq!(" Warm".parse::<RelightLight>().unwrap(), RelightLight::Warm);
        let e = "pink".parse::<RelightLight>().unwrap_err().to_string();
        assert!(
            e.contains("\"pink\"") && e.contains("warm, neutral, cool"),
            "{e}"
        );
        assert_eq!(RelightLight::default(), RelightLight::Neutral);
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
