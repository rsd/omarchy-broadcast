//! The inference backend seam that the effect element talks to
//! (`docs/adr/0003-maxine-first-behind-backend-seam.md`,
//! `docs/adr/0012-video-effects-and-maxine-backend.md`).
//!
//! This crate has no GPU dependency. It defines what an effect element hands
//! to a backend and what it gets back:
//!
//! * a [`BackendFactory`] says whether a backend can run here at all
//!   ([`BackendFactory::probe`], cheap: libraries only, no models and no CUDA
//!   context) and creates instances;
//! * an [`InferenceBackend`] instance loads the models for one effect and
//!   its stages ([`InferenceBackend::load`]), processes one canonical frame
//!   at a time ([`InferenceBackend::process`]) and releases everything again
//!   ([`InferenceBackend::unload`]);
//! * [`Feature`] and [`Availability`] say which of a backend's features are
//!   installed, from the filesystem alone, so that a client can grey out
//!   what it cannot offer without waking the GPU.
//!
//! # Effects, stages and their order
//!
//! A request has one [`EffectKind`] (mutually exclusive: `none`, `blur`,
//! `replace`) and an ordered list of stackable [`Stage`]s
//! ([`StageList`], [`LoadRequest::stages`]). A backend runs one frame in
//! this order:
//!
//! 1. the conversion of the NV12 frame to the backend's working format;
//! 2. the **pre-mask stages**, those whose [`Stage::needs_mask`] is false
//!    ([`Stage::Denoise`]), in list order: each reads the previous one's
//!    output, so every later step sees the processed camera frame;
//! 3. the **mask** (the green-screen matte), computed only when the effect
//!    needs it (`blur`, `replace`) or a stage does;
//! 4. the **post-mask stages**, those whose [`Stage::needs_mask`] is true
//!    ([`Stage::Relight`]), in list order. Such a stage consumes the matte,
//!    and its output is only valid where the matte says foreground (a relit
//!    foreground, for example), so it must be composited;
//! 5. the effect's **composite** through the mask: `none` over the original
//!    frame, `blur` over the blurred frame, `replace` over the background
//!    image. The foreground composited is the last post-mask stage's output,
//!    or the frame of step 2 without one. With `none` and no post-mask stage
//!    there is no mask and nothing to composite;
//! 6. the conversion back to NV12 into the destination.
//!
//! An empty list adds nothing: no pass, copy or allocation
//! (`docs/adr/0018-video-noise-removal.md`, "off costs nothing").
//!
//! The values of a stage that can change on every frame (the direction and
//! the gain of [`Stage::Relight`]) travel in [`FrameParams`], so changing
//! them never reloads anything; those that need a switch (its light) are
//! in the [`LoadRequest`].
//!
//! # Frames and the CUDA context
//!
//! Frames are canonical NV12 frames in CUDA device memory, described by
//! [`DeviceNv12`]: plane pointers, pitches and size, in the CUDA context the
//! element shares with the stream ([`GpuContext`]). The colour matrix and
//! range of the canonical label travel in [`ColourSpec`].
//!
//! The caller makes [`GpuContext::context`] current on the calling thread
//! (for example with `obc_gpu::ContextGuard`) around **every** call into an
//! [`InferenceBackend`], including `unload` and `timings`. A backend never
//! creates, retains or makes current a CUDA context of its own; in
//! particular it must not activate the device's primary context.
//!
//! # A backend that does not run on CUDA
//!
//! A backend on another device (for example an OpenVINO backend on an NPU)
//! would add a host-memory variant of the frame description next to
//! [`DeviceNv12`]; the element would then download each frame, call the
//! backend and upload the result. That variant is deliberately not defined
//! until such a backend exists.

use std::ffi::c_void;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

pub mod enhance;

/// The effect the Process stage applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum EffectKind {
    /// No effect: the element passes frames through and never calls a
    /// backend, unless the request has a stage ([`LoadRequest::stages`]),
    /// which then runs alone.
    #[default]
    None,
    /// Background blur through the green-screen mask.
    Blur,
    /// Background replacement with an image, through the green-screen mask.
    Replace,
}

impl EffectKind {
    /// The lower-case name used in configuration, D-Bus and the CLI.
    pub fn as_str(self) -> &'static str {
        match self {
            EffectKind::None => "none",
            EffectKind::Blur => "blur",
            EffectKind::Replace => "replace",
        }
    }
}

impl fmt::Display for EffectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EffectKind {
    type Err = BackendError;
    /// Parses `none`, `blur` or `replace`, ignoring ASCII case.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "none" => Ok(EffectKind::None),
            "blur" => Ok(EffectKind::Blur),
            "replace" => Ok(EffectKind::Replace),
            _ => Err(BackendError::InvalidInput(format!(
                "unknown effect {s:?} (none|blur|replace)"
            ))),
        }
    }
}

/// A stackable stage of the chain, which runs together with any
/// [`EffectKind`] (`docs/adr/0018-video-noise-removal.md`). The declaration
/// order is the canonical order a [`StageList`] keeps: the pre-mask stages
/// first, then the post-mask ones (see the crate documentation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Stage {
    /// Video noise removal, on the camera frame before the mask and the
    /// effect. A pre-mask stage.
    Denoise,
    /// Relighting of the foreground (the person) through the green-screen
    /// matte, with the light of [`LoadRequest::relight_light`] from the
    /// direction and at the gain of [`FrameParams`]. A post-mask stage: its
    /// output is composited over the effect's background, so the background
    /// itself is never relit.
    Relight,
}

impl Stage {
    /// Every stage, in canonical order.
    pub const ALL: [Stage; 2] = [Stage::Denoise, Stage::Relight];

    /// The lower-case name used in logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Denoise => "denoise",
            Stage::Relight => "relight",
        }
    }

    /// Whether the stage consumes the green-screen matte. Such a stage runs
    /// after the mask (a post-mask stage), and its request makes the
    /// backend compute the mask even under [`EffectKind::None`]; a stage
    /// that does not runs before the mask (a pre-mask stage).
    pub fn needs_mask(self) -> bool {
        match self {
            Stage::Denoise => false,
            Stage::Relight => true,
        }
    }

    /// The backend features the stage needs installed: its own, and for a
    /// post-mask stage the green screen that makes its matte.
    pub fn features(self) -> &'static [Feature] {
        match self {
            Stage::Denoise => &[Feature::Denoise],
            Stage::Relight => &[Feature::GreenScreen, Feature::Relighting],
        }
    }

    /// Whether the stage can be offered, from the availability of each
    /// feature (as a backend reports it, for example
    /// `obc_backend_maxine::feature_availability`): installed when every
    /// feature of [`Stage::features`] is, else not installed with the
    /// reasons of those that are not (a feature missing from `features`
    /// counts as not installed).
    pub fn availability(self, features: &[(Feature, Availability)]) -> Availability {
        let missing: Vec<String> = self
            .features()
            .iter()
            .filter_map(|f| match features.iter().find(|(g, _)| g == f) {
                Some((_, Availability::Installed)) => None,
                Some((_, Availability::NotInstalled(r))) => Some(format!("{f}: {r}")),
                None => Some(format!("{f}: not reported")),
            })
            .collect();
        if missing.is_empty() {
            Availability::Installed
        } else {
            Availability::NotInstalled(missing.join("; "))
        }
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The stages of a request, in canonical order ([`Stage`]'s declaration
/// order: pre-mask stages first) and each at most once. Only
/// [`StageList::new`] builds a list from several stages, and it refuses
/// any other order, so a list a backend receives is always valid. The empty
/// list ([`StageList::default`]) allocates nothing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct StageList(Vec<Stage>);

impl StageList {
    /// The list of `stages`, which must be in canonical order with no
    /// stage twice; [`BackendError::InvalidInput`] otherwise.
    pub fn new(stages: impl IntoIterator<Item = Stage>) -> Result<StageList, BackendError> {
        let stages: Vec<Stage> = stages.into_iter().collect();
        if let Some(w) = stages.windows(2).find(|w| w[0] >= w[1]) {
            let what = if w[0] == w[1] {
                format!("the stage {} is listed twice", w[0])
            } else {
                format!("the stage {} must come before {}", w[1], w[0])
            };
            return Err(BackendError::InvalidInput(format!(
                "invalid stage list {}: {what}",
                StageList(stages.clone())
            )));
        }
        Ok(StageList(stages))
    }

    /// Whether `stage` is in the list.
    pub fn contains(&self, stage: Stage) -> bool {
        self.0.contains(&stage)
    }

    /// Whether the list is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The stages, in order.
    pub fn as_slice(&self) -> &[Stage] {
        &self.0
    }

    /// Whether any stage consumes the mask ([`Stage::needs_mask`]).
    pub fn needs_mask(&self) -> bool {
        self.0.iter().any(|s| s.needs_mask())
    }
}

impl From<Stage> for StageList {
    /// The list of the one stage.
    fn from(stage: Stage) -> StageList {
        StageList(vec![stage])
    }
}

impl fmt::Display for StageList {
    /// `[denoise, ...]`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[")?;
        for (i, s) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            f.write_str(s.as_str())?;
        }
        f.write_str("]")
    }
}

/// A feature of a backend that is installed separately (for Maxine, a
/// feature directory of the SDK with its library and its models). Not every
/// feature is used by the chain yet; the list says what a client can offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Feature {
    /// The green-screen segmentation: the mask of `blur` and `replace`.
    GreenScreen,
    /// The background blur of `blur`.
    BackgroundBlur,
    /// Video noise removal ([`Stage::Denoise`]).
    Denoise,
    /// Relighting of the foreground through the green-screen matte.
    Relighting,
    /// Video super resolution (upscaling, and same-size denoise and
    /// deblur modes).
    VideoSuperResolution,
    /// A lightweight upscaler.
    Upscale,
    /// Video frame generation (frames interpolated between two inputs).
    VideoFrameGeneration,
}

impl Feature {
    /// Every feature, in declaration order.
    pub const ALL: [Feature; 7] = [
        Feature::GreenScreen,
        Feature::BackgroundBlur,
        Feature::Denoise,
        Feature::Relighting,
        Feature::VideoSuperResolution,
        Feature::Upscale,
        Feature::VideoFrameGeneration,
    ];

    /// A stable lower-case name for logs and interfaces.
    pub fn as_str(self) -> &'static str {
        match self {
            Feature::GreenScreen => "green-screen",
            Feature::BackgroundBlur => "background-blur",
            Feature::Denoise => "denoise",
            Feature::Relighting => "relighting",
            Feature::VideoSuperResolution => "video-super-resolution",
            Feature::Upscale => "upscale",
            Feature::VideoFrameGeneration => "video-frame-generation",
        }
    }
}

impl fmt::Display for Feature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether a [`Feature`] is installed, as far as the filesystem shows. It
/// says nothing about whether the feature loads on this GPU: that is known
/// only when it is loaded.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Availability {
    /// The feature's library and the models it needs are present.
    Installed,
    /// Something is missing; the reason says what, for logs (the library,
    /// or the models for this GPU).
    NotInstalled(String),
}

impl Availability {
    /// Whether the feature is installed.
    pub fn is_installed(&self) -> bool {
        matches!(self, Availability::Installed)
    }

    /// Why the feature is not installed, if it is not.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Availability::Installed => None,
            Availability::NotInstalled(r) => Some(r),
        }
    }
}

/// The YUV colour matrix of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ColourMatrix {
    /// ITU-R BT.601, the matrix UVC cameras and JPEG use. The canonical
    /// label (`2:4:5:1` / `1:4:5:1`) uses it.
    #[default]
    Bt601,
    /// ITU-R BT.709.
    Bt709,
}

/// The colour matrix and range of NV12 frames: what a backend needs to
/// convert them to RGB and back.
///
/// The canonical label today is [`ColourSpec::LIMITED_BT601`] (GStreamer
/// colorimetry `2:4:5:1`) or [`ColourSpec::FULL_BT601`] (`1:4:5:1`). Chroma
/// is sited as GStreamer's default for NV12 (MPEG-2: horizontally co-sited
/// with luma).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ColourSpec {
    /// The YUV matrix.
    pub matrix: ColourMatrix,
    /// `true` for full range (Y and chroma 0 to 255), `false` for limited
    /// ("video") range (Y 16 to 235, chroma 16 to 240).
    pub full_range: bool,
}

impl ColourSpec {
    /// BT.601 matrix, limited range: the canonical `2:4:5:1` label.
    pub const LIMITED_BT601: ColourSpec = ColourSpec {
        matrix: ColourMatrix::Bt601,
        full_range: false,
    };
    /// BT.601 matrix, full range: the canonical `1:4:5:1` label.
    pub const FULL_BT601: ColourSpec = ColourSpec {
        matrix: ColourMatrix::Bt601,
        full_range: true,
    };

    /// The `(Y, U/V)` byte values of black in this range: `(16, 128)`
    /// limited, `(0, 128)` full.
    pub fn black_nv12(self) -> (u8, u8) {
        if self.full_range { (0, 128) } else { (16, 128) }
    }
}

/// A canonical NV12 frame in CUDA device memory.
///
/// The Y plane is `height` rows of at least `width` bytes, `y_pitch` bytes
/// apart; the interleaved UV plane is `height / 2` rows of at least `width`
/// bytes (`width / 2` U,V pairs), `uv_pitch` bytes apart. `width` and
/// `height` are even. The pointers are `CUdeviceptr` values valid in the
/// caller's [`GpuContext`]. A `DeviceNv12` borrows memory; it owns nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceNv12 {
    /// Device address of pixel (0, 0) of the Y plane (`CUdeviceptr`).
    pub y: u64,
    /// Bytes between the starts of two Y rows.
    pub y_pitch: usize,
    /// Device address of the first U,V pair of the UV plane (`CUdeviceptr`).
    pub uv: u64,
    /// Bytes between the starts of two UV rows.
    pub uv_pitch: usize,
    /// Width in pixels (even).
    pub width: u32,
    /// Height in pixels (even).
    pub height: u32,
}

impl DeviceNv12 {
    /// Checks the description is self-consistent: non-null planes, even
    /// non-zero size, pitches of at least `width` bytes.
    pub fn validate(&self) -> Result<(), BackendError> {
        if self.y == 0 || self.uv == 0 {
            return Err(BackendError::InvalidInput(
                "NV12 frame has a null plane pointer".into(),
            ));
        }
        if self.width == 0
            || self.height == 0
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
        {
            return Err(BackendError::InvalidInput(format!(
                "NV12 frame size {}x{} must be even and non-zero",
                self.width, self.height
            )));
        }
        let w = self.width as usize;
        if self.y_pitch < w || self.uv_pitch < w {
            return Err(BackendError::InvalidInput(format!(
                "NV12 pitches (Y {}, UV {}) are smaller than the width {}",
                self.y_pitch, self.uv_pitch, self.width
            )));
        }
        Ok(())
    }
}

/// A packed 8-bit BGR image in host memory (B, G, R byte order per pixel),
/// for example a decoded background image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostBgr {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bytes between the starts of two rows (at least `3 * width`).
    pub stride: usize,
    /// `height * stride` bytes (the last row may be only `3 * width`).
    pub data: Vec<u8>,
}

impl HostBgr {
    /// Checks that `stride` and `data` can hold `width x height` BGR pixels.
    pub fn validate(&self) -> Result<(), BackendError> {
        let row = self.width as usize * 3;
        if self.width == 0 || self.height == 0 || self.stride < row {
            return Err(BackendError::InvalidInput(format!(
                "BGR image {}x{} with stride {} is malformed",
                self.width, self.height, self.stride
            )));
        }
        let need = self.stride * (self.height as usize - 1) + row;
        if self.data.len() < need {
            return Err(BackendError::InvalidInput(format!(
                "BGR image {}x{} needs {need} bytes, has {}",
                self.width,
                self.height,
                self.data.len()
            )));
        }
        Ok(())
    }
}

/// The CUDA context and stream a backend runs in: those of the GStreamer
/// stream the element belongs to.
///
/// Raw driver handles (`CUcontext`, `CUstream`); the caller owns both and
/// keeps them alive for as long as a backend has anything loaded with them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuContext {
    /// The `CUcontext`. Never null for a real call.
    pub context: *mut c_void,
    /// The `CUstream` all GPU work is enqueued on. Null means the context's
    /// legacy default stream.
    pub stream: *mut c_void,
}

// SAFETY: `GpuContext` is a pair of opaque CUDA driver handles. CUDA
// contexts and streams may be used from any thread (a context is made
// current per thread with cuCtxPushCurrent); the struct holds no
// thread-affine state and is never dereferenced on the Rust side. Moving it
// to the load worker thread is exactly what the element does.
unsafe impl Send for GpuContext {}

/// What [`InferenceBackend::load`] should prepare.
#[derive(Debug, Clone)]
pub struct LoadRequest {
    /// [`EffectKind::Blur`] or [`EffectKind::Replace`]; `None` is accepted
    /// only together with at least one stage (the element does not call a
    /// backend for `none` without one).
    pub effect: EffectKind,
    /// The stackable stages to run with the effect, or alone with
    /// [`EffectKind::None`], in the order of the crate documentation. A
    /// stage that is not listed loads no model and adds no pass; the empty
    /// list is the effect alone.
    pub stages: StageList,
    /// Frame width in pixels; every frame passed to `process` has it.
    pub width: u32,
    /// Frame height in pixels; every frame passed to `process` has it.
    pub height: u32,
    /// The colour matrix and range of the frames.
    pub colour: ColourSpec,
    /// The background image, already scaled and cropped to `width x height`.
    /// Required for [`EffectKind::Replace`], ignored otherwise.
    pub background: Option<Arc<HostBgr>>,
    /// The light of [`Stage::Relight`]; ignored without the stage. A new
    /// light on a loaded stage is a switch that loads no model (Maxine binds
    /// a new lighting environment to the loaded effect).
    pub relight_light: RelightLight,
}

/// The outcome of a successful [`InferenceBackend::load`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LoadReport {
    /// Wall time of the load itself (models, buffers), in milliseconds.
    pub load_ms: f64,
    /// Wall time of a warm-up run done inside `load`, if the backend does
    /// one, in milliseconds. The first run after a model load is slow
    /// (seconds on a cold cache); a backend that warms up in `load` keeps
    /// that off the streaming thread.
    pub warmup_ms: Option<f64>,
    /// `true` when the instance was reused (same size and colour) and no
    /// model had to be loaded: the models of the previous load that the
    /// request needs were kept (the mask model, the stages' models), so the
    /// load cost only the effect-specific part.
    pub reused_model: bool,
}

/// The light of [`Stage::Relight`]: the colour temperature of its key
/// light.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RelightLight {
    /// A warm key light, about 3200 K (tungsten).
    Warm,
    /// A neutral key light, about 5000 K.
    #[default]
    Neutral,
    /// A cool key light, about 6500 K (daylight).
    Cool,
}

impl RelightLight {
    /// Every light, in declaration order.
    pub const ALL: [RelightLight; 3] = [
        RelightLight::Warm,
        RelightLight::Neutral,
        RelightLight::Cool,
    ];

    /// The lower-case name used in configuration, D-Bus and the CLI.
    pub fn as_str(self) -> &'static str {
        match self {
            RelightLight::Warm => "warm",
            RelightLight::Neutral => "neutral",
            RelightLight::Cool => "cool",
        }
    }

    /// The colour temperature of the key light, in kelvin.
    pub fn kelvin(self) -> f32 {
        match self {
            RelightLight::Warm => 3200.0,
            RelightLight::Neutral => 5000.0,
            RelightLight::Cool => 6500.0,
        }
    }
}

impl fmt::Display for RelightLight {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RelightLight {
    type Err = BackendError;
    /// Parses `warm`, `neutral` or `cool`, ignoring ASCII case.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "warm" => Ok(RelightLight::Warm),
            "neutral" => Ok(RelightLight::Neutral),
            "cool" => Ok(RelightLight::Cool),
            _ => Err(BackendError::InvalidInput(format!(
                "unknown relight light {s:?} (warm|neutral|cool)"
            ))),
        }
    }
}

/// What [`Stage::Relight`] does to a frame: the light, where it comes from
/// and how bright the relit foreground is, as a client sets them. The light
/// goes into the load request ([`LoadRequest::relight_light`]: a new light
/// is a switch that loads no model), the direction and the gain into
/// each frame's [`FrameParams`] ([`RelightParams::frame_params`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RelightParams {
    /// The key light's colour.
    pub light: RelightLight,
    /// The direction of the light, in degrees around the vertical axis:
    /// 0 is the default position, positive values turn it one way and
    /// negative values the other ([`RelightParams::MIN_PAN_DEG`] to
    /// [`RelightParams::MAX_PAN_DEG`]).
    pub pan_deg: f32,
    /// The brightness of the relit foreground, 1.0 unchanged
    /// ([`RelightParams::MIN_GAIN`] to [`RelightParams::MAX_GAIN`]).
    pub gain: f32,
}

impl RelightParams {
    /// The smallest light direction, in degrees.
    pub const MIN_PAN_DEG: f32 = -180.0;
    /// The largest light direction, in degrees.
    pub const MAX_PAN_DEG: f32 = 180.0;
    /// The smallest foreground gain.
    pub const MIN_GAIN: f32 = 0.5;
    /// The largest foreground gain.
    pub const MAX_GAIN: f32 = 2.0;
    /// The default foreground gain.
    pub const DEFAULT_GAIN: f32 = 1.0;

    /// The parameters with the direction and the gain within their ranges;
    /// a value that is not finite becomes its default.
    pub fn clamped(self) -> RelightParams {
        let finite_or = |v: f32, d: f32| if v.is_finite() { v } else { d };
        RelightParams {
            light: self.light,
            pan_deg: finite_or(self.pan_deg, 0.0).clamp(Self::MIN_PAN_DEG, Self::MAX_PAN_DEG),
            gain: finite_or(self.gain, Self::DEFAULT_GAIN).clamp(Self::MIN_GAIN, Self::MAX_GAIN),
        }
    }

    /// `params` with this direction and gain (clamped).
    pub fn frame_params(&self, params: FrameParams) -> FrameParams {
        let c = self.clamped();
        FrameParams {
            relight_pan_deg: c.pan_deg,
            relight_gain: c.gain,
            ..params
        }
    }

    /// Checks the parameters are finite and within their ranges, with the
    /// reason if not (what a client is told; a backend clamps instead).
    pub fn validate(&self) -> Result<(), String> {
        if !self.pan_deg.is_finite()
            || !(Self::MIN_PAN_DEG..=Self::MAX_PAN_DEG).contains(&self.pan_deg)
        {
            return Err(format!(
                "relight direction {} is outside {} to {} degrees",
                self.pan_deg,
                Self::MIN_PAN_DEG,
                Self::MAX_PAN_DEG
            ));
        }
        if !self.gain.is_finite() || !(Self::MIN_GAIN..=Self::MAX_GAIN).contains(&self.gain) {
            return Err(format!(
                "relight gain {} is outside {} to {}",
                self.gain,
                Self::MIN_GAIN,
                Self::MAX_GAIN
            ));
        }
        Ok(())
    }
}

impl Default for RelightParams {
    fn default() -> Self {
        RelightParams {
            light: RelightLight::default(),
            pan_deg: 0.0,
            gain: Self::DEFAULT_GAIN,
        }
    }
}

/// Per-frame parameters that change without a reload.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameParams {
    /// Blur strength, 0.0 to 1.0 (values outside are clamped). Used by
    /// [`EffectKind::Blur`] only.
    pub blur_strength: f32,
    /// The direction of the light of [`Stage::Relight`], in degrees
    /// ([`RelightParams::pan_deg`]; clamped to its range). Ignored without
    /// the stage.
    pub relight_pan_deg: f32,
    /// The brightness of the relit foreground of [`Stage::Relight`]
    /// ([`RelightParams::gain`]; clamped to its range). Ignored without the
    /// stage.
    pub relight_gain: f32,
}

impl FrameParams {
    /// The relight direction and gain, clamped as a backend applies them.
    pub fn relight(&self) -> (f32, f32) {
        let c = RelightParams {
            light: RelightLight::default(),
            pan_deg: self.relight_pan_deg,
            gain: self.relight_gain,
        }
        .clamped();
        (c.pan_deg, c.gain)
    }
}

impl Default for FrameParams {
    fn default() -> Self {
        FrameParams {
            blur_strength: 0.5,
            relight_pan_deg: 0.0,
            relight_gain: RelightParams::DEFAULT_GAIN,
        }
    }
}

/// GPU time of each step of one processed frame, in milliseconds, measured
/// with CUDA events on the backend's stream. Each [`Stage`] has a field of
/// its own (0 when the stage is not loaded), so that tools can report the
/// stages by name.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct StageTimings {
    /// NV12 to the backend's input format (BGR for Maxine).
    pub convert_in_ms: f32,
    /// The denoise stage; 0 when it is not loaded.
    pub denoise_ms: f32,
    /// The segmentation mask.
    pub mask_ms: f32,
    /// The relight stage; 0 when it is not loaded.
    pub relight_ms: f32,
    /// The blur or the composite over the background.
    pub effect_ms: f32,
    /// Back to NV12 into the destination frame.
    pub convert_out_ms: f32,
}

impl StageTimings {
    /// The sum of all stages.
    pub fn total_ms(&self) -> f32 {
        self.convert_in_ms
            + self.denoise_ms
            + self.mask_ms
            + self.relight_ms
            + self.effect_ms
            + self.convert_out_ms
    }

    /// The time of `stage` ([`StageTimings::denoise_ms`],
    /// [`StageTimings::relight_ms`]).
    pub fn stage_ms(&self, stage: Stage) -> f32 {
        match stage {
            Stage::Denoise => self.denoise_ms,
            Stage::Relight => self.relight_ms,
        }
    }
}

/// Why a backend is unavailable or a call failed. Every variant carries a
/// human-readable reason fit for the daemon's `Error` property: what went
/// wrong and, where known, what to do about it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BackendError {
    /// The backend cannot run on this machine (library missing, no GPU,
    /// SDK not configured).
    #[error("{0}")]
    Unavailable(String),
    /// Loading an effect failed; `code` is the backend's status code.
    #[error("{message}")]
    Load {
        /// The backend's status code (for Maxine, an `NvCV_Status`; for a
        /// CUDA driver call, a `CUresult`, and the message names the call).
        code: i32,
        /// What failed and what to do about it.
        message: String,
    },
    /// Processing a frame failed; `code` is the backend's status code.
    #[error("{message}")]
    Run {
        /// The backend's status code (for Maxine, an `NvCV_Status`; for a
        /// CUDA driver call, a `CUresult`).
        code: i32,
        /// What failed.
        message: String,
    },
    /// The caller passed something the backend cannot accept (wrong size,
    /// missing background, no context current, not loaded).
    #[error("{0}")]
    InvalidInput(String),
    /// One stage alone failed to load or to run; the rest of the request is
    /// intact. A stage fails open: it hides nothing, so the caller goes on
    /// without it. From `load`: nothing of the request is in force (a
    /// reused instance keeps its previous chain; a fresh load leaves it
    /// unloaded), so the caller may load again without `stage`. One switch
    /// cannot keep the previous chain whole: a new light the loaded relight
    /// stage fails to take leaves the stage in an unknown state, so the
    /// reused instance keeps its previous chain **without** `stage`; the
    /// caller then knows the previous chain without `stage` is in force
    /// (without it if it was not in it, the same). From
    /// `process`: the backend has dropped `stage`, and the next frame runs
    /// the chain without it; this frame's `dst` is not written.
    #[error("{message}")]
    Stage {
        /// The stage that failed.
        stage: Stage,
        /// The backend's status code, as for [`BackendError::Load`].
        code: i32,
        /// What failed and what to do about it.
        message: String,
    },
}

/// One instance of an inference backend: the models and buffers for one
/// effect at one frame size.
///
/// Threading and context rules:
/// * every method is called with the request's [`GpuContext::context`]
///   current on the calling thread;
/// * calls on one instance are serialised by `&mut self`, but may come from
///   different threads (`load` from a worker, `process` from the streaming
///   thread), hence `Send`;
/// * `process` and `load` take the same [`GpuContext`] (same stream).
pub trait InferenceBackend: Send {
    /// A short name for logs (`"maxine"`).
    fn name(&self) -> &'static str;

    /// Loads the models and allocates the buffers for `req`. May block for
    /// seconds; the element runs it on a worker thread.
    ///
    /// Loading again on a loaded instance replaces the previous effect. When
    /// only the effect or the background changes (same size and colour),
    /// the mask model stays loaded and the call costs milliseconds, so
    /// switching between blur and replace does not reload it
    /// ([`LoadReport::reused_model`]); such a switch may run on the
    /// streaming thread. Adding or removing a stage, or changing the light
    /// of the relight stage, is such a switch too, and costs the stage's
    /// model load when it is added (a new light loads no model). If a
    /// switch on a
    /// loaded instance fails, the previous effect (and its stages) stays
    /// loaded, less the stage of a [`BackendError::Stage`] (see there); if
    /// a full load fails, the instance is left unloaded.
    fn load(&mut self, gpu: &GpuContext, req: &LoadRequest) -> Result<LoadReport, BackendError>;

    /// Enqueues the effect for one frame on `gpu.stream`: reads `src`, writes
    /// `dst`. Never writes `src` (the output clock may push the same input
    /// again). `src` and `dst` have the loaded size; they may be the same
    /// frame only if the caller accepts that `src` is overwritten. Returns
    /// once the work is enqueued; the caller synchronises the stream (or
    /// relies on stream order) before reading `dst`.
    fn process(
        &mut self,
        gpu: &GpuContext,
        params: &FrameParams,
        src: &DeviceNv12,
        dst: &DeviceNv12,
    ) -> Result<(), BackendError>;

    /// Releases models, buffers and CUDA resources, so their GPU memory
    /// returns. Idempotent; also done on drop (with the context current, so
    /// callers should unload explicitly before releasing the context).
    fn unload(&mut self);

    /// Per-stage GPU times of the most recent frame whose work has
    /// completed, if the backend measures them.
    fn timings(&self) -> Option<StageTimings>;

    /// Forgets the temporal state of the loaded stages and of the mask (the
    /// denoise state, the green screen's, and any a relight stage keeps), so
    /// the next frame is treated as the first
    /// of a new video: the element calls it when the input switches to
    /// another source. Enqueued on `gpu.stream`; a no-op when nothing is
    /// loaded. The default does nothing.
    ///
    /// A stage whose state cannot be forgotten in place is dropped instead,
    /// and returned: its next frames would carry the previous source into
    /// the new one (a relit person's look, which the effect may have hidden
    /// in the previous source's background). The chain runs without the
    /// returned stages from the next frame, as after a frame failure of
    /// theirs, and the caller loads them again (a switch that adds them,
    /// with a fresh state) before they run again. They did not fail: the
    /// caller does not record a failure for them.
    fn reset_temporal(&mut self, gpu: &GpuContext) -> Result<StageList, BackendError> {
        let _ = gpu;
        Ok(StageList::default())
    }
}

/// Creates instances of one kind of backend and reports whether it can run.
pub trait BackendFactory: Send + Sync {
    /// A short name for logs (`"maxine"`).
    fn name(&self) -> &'static str;

    /// Checks cheaply whether the backend can run: libraries present and
    /// configured. Loads no models and creates no CUDA context.
    fn probe(&self) -> Result<(), BackendError>;

    /// Creates an unloaded instance. Does what `probe` does, and no more.
    fn create(&self) -> Result<Box<dyn InferenceBackend>, BackendError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effect_kind_parses_and_displays_lower_case() {
        for (s, k) in [
            ("none", EffectKind::None),
            ("blur", EffectKind::Blur),
            ("replace", EffectKind::Replace),
        ] {
            assert_eq!(s.parse::<EffectKind>().unwrap(), k);
            assert_eq!(k.to_string(), s);
        }
        assert_eq!("BLUR".parse::<EffectKind>().unwrap(), EffectKind::Blur);
        let err = "sepia".parse::<EffectKind>().unwrap_err();
        assert!(matches!(err, BackendError::InvalidInput(_)));
        assert!(err.to_string().contains("none|blur|replace"), "{err}");
        assert_eq!(EffectKind::default(), EffectKind::None);
    }

    #[test]
    fn black_levels_follow_the_range() {
        assert_eq!(ColourSpec::LIMITED_BT601.black_nv12(), (16, 128));
        assert_eq!(ColourSpec::FULL_BT601.black_nv12(), (0, 128));
        assert_eq!(ColourSpec::default(), ColourSpec::LIMITED_BT601);
    }

    fn frame() -> DeviceNv12 {
        DeviceNv12 {
            y: 0x1000,
            y_pitch: 2048,
            uv: 0x2000,
            uv_pitch: 2048,
            width: 1920,
            height: 1080,
        }
    }

    #[test]
    fn nv12_validation() {
        assert!(frame().validate().is_ok());
        assert!(DeviceNv12 { y: 0, ..frame() }.validate().is_err());
        assert!(
            DeviceNv12 {
                width: 1919,
                ..frame()
            }
            .validate()
            .is_err()
        );
        assert!(
            DeviceNv12 {
                height: 0,
                ..frame()
            }
            .validate()
            .is_err()
        );
        assert!(
            DeviceNv12 {
                uv_pitch: 1000,
                ..frame()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn bgr_validation() {
        let ok = HostBgr {
            width: 4,
            height: 2,
            stride: 12,
            data: vec![0; 24],
        };
        assert!(ok.validate().is_ok());
        // The last row may be unpadded.
        let padded = HostBgr {
            stride: 16,
            data: vec![0; 28],
            ..ok.clone()
        };
        assert!(padded.validate().is_ok());
        assert!(
            HostBgr {
                data: vec![0; 23],
                ..ok.clone()
            }
            .validate()
            .is_err()
        );
        assert!(HostBgr { stride: 11, ..ok }.validate().is_err());
    }

    #[test]
    fn errors_display_their_reason() {
        let e = BackendError::Load {
            code: -1209,
            message: "the SDK rejected this distribution".into(),
        };
        assert_eq!(e.to_string(), "the SDK rejected this distribution");
        assert_eq!(
            BackendError::Unavailable("no SDK".into()).to_string(),
            "no SDK"
        );
        let e = BackendError::Stage {
            stage: Stage::Denoise,
            code: -16,
            message: "the video noise removal does not support 2048x1080 frames".into(),
        };
        assert_eq!(
            e.to_string(),
            "the video noise removal does not support 2048x1080 frames"
        );
        assert_ne!(
            e,
            BackendError::Load {
                code: -16,
                message: e.to_string()
            },
            "a stage failure is told apart from a load failure"
        );
    }

    #[test]
    fn stage_lists_keep_the_canonical_order_once() {
        let empty = StageList::default();
        assert!(empty.is_empty() && !empty.needs_mask());
        assert_eq!(empty.as_slice(), &[] as &[Stage]);
        assert_eq!(empty.to_string(), "[]");
        assert_eq!(StageList::new([]).unwrap(), empty);

        let one = StageList::new([Stage::Denoise]).unwrap();
        assert_eq!(one, StageList::from(Stage::Denoise));
        assert!(one.contains(Stage::Denoise) && !one.is_empty());
        assert_eq!(one.to_string(), "[denoise]");
        assert!(!empty.contains(Stage::Denoise));

        let e = StageList::new([Stage::Denoise, Stage::Denoise]).unwrap_err();
        assert!(matches!(e, BackendError::InvalidInput(_)), "{e:?}");
        assert!(e.to_string().contains("denoise is listed twice"), "{e}");
    }

    #[test]
    fn denoise_is_a_pre_mask_stage() {
        assert!(!Stage::Denoise.needs_mask());
        assert!(!StageList::from(Stage::Denoise).needs_mask());
        assert_eq!(Stage::Denoise.to_string(), "denoise");
    }

    #[test]
    fn relight_is_a_post_mask_stage_after_denoise() {
        assert!(Stage::Relight.needs_mask());
        assert!(StageList::from(Stage::Relight).needs_mask());
        assert_eq!(Stage::Relight.to_string(), "relight");
        assert_eq!(Stage::ALL, [Stage::Denoise, Stage::Relight]);

        let both = StageList::new([Stage::Denoise, Stage::Relight]).unwrap();
        assert!(both.needs_mask());
        assert!(both.contains(Stage::Denoise) && both.contains(Stage::Relight));
        assert_eq!(both.to_string(), "[denoise, relight]");
        assert_eq!(both.as_slice(), &[Stage::Denoise, Stage::Relight]);

        let e = StageList::new([Stage::Relight, Stage::Denoise]).unwrap_err();
        assert!(matches!(e, BackendError::InvalidInput(_)), "{e:?}");
        assert!(
            e.to_string().contains("denoise must come before relight"),
            "{e}"
        );
        let e = StageList::new([Stage::Denoise, Stage::Relight, Stage::Relight]).unwrap_err();
        assert!(e.to_string().contains("relight is listed twice"), "{e}");
    }

    #[test]
    fn a_stage_is_available_when_all_its_features_are() {
        use Availability::*;
        let all_installed: Vec<(Feature, Availability)> =
            Feature::ALL.iter().map(|f| (*f, Installed)).collect();
        for stage in Stage::ALL {
            assert_eq!(stage.availability(&all_installed), Installed, "{stage}");
        }
        assert_eq!(Stage::Denoise.features(), &[Feature::Denoise]);
        assert_eq!(
            Stage::Relight.features(),
            &[Feature::GreenScreen, Feature::Relighting]
        );
        let without_green_screen: Vec<(Feature, Availability)> = all_installed
            .iter()
            .map(|(f, a)| match f {
                Feature::GreenScreen => (*f, NotInstalled("no models".into())),
                _ => (*f, a.clone()),
            })
            .collect();
        assert_eq!(
            Stage::Relight.availability(&without_green_screen),
            NotInstalled("green-screen: no models".into())
        );
        assert_eq!(
            Stage::Denoise.availability(&without_green_screen),
            Installed
        );
        let a = Stage::Relight.availability(&[(Feature::GreenScreen, Installed)]);
        assert_eq!(a, NotInstalled("relighting: not reported".into()));
        let a = Stage::Relight.availability(&[]);
        assert_eq!(
            a.reason(),
            Some("green-screen: not reported; relighting: not reported")
        );
    }

    #[test]
    fn relight_lights_parse_and_display_lower_case() {
        for (s, l, k) in [
            ("warm", RelightLight::Warm, 3200.0),
            ("neutral", RelightLight::Neutral, 5000.0),
            ("cool", RelightLight::Cool, 6500.0),
        ] {
            assert_eq!(s.parse::<RelightLight>().unwrap(), l);
            assert_eq!(l.to_string(), s);
            assert_eq!(l.kelvin(), k);
        }
        assert_eq!("COOL".parse::<RelightLight>().unwrap(), RelightLight::Cool);
        let e = "pink".parse::<RelightLight>().unwrap_err();
        assert!(e.to_string().contains("warm|neutral|cool"), "{e}");
        assert_eq!(RelightLight::default(), RelightLight::Neutral);
        assert_eq!(RelightLight::ALL.len(), 3);
    }

    #[test]
    fn relight_params_clamp_and_validate() {
        let d = RelightParams::default();
        assert_eq!(
            (d.light, d.pan_deg, d.gain),
            (RelightLight::Neutral, 0.0, 1.0)
        );
        assert_eq!(d.clamped(), d);
        assert!(d.validate().is_ok());
        let p = |pan_deg, gain| RelightParams {
            light: RelightLight::Warm,
            pan_deg,
            gain,
        };
        assert_eq!(p(270.0, 3.0).clamped(), p(180.0, 2.0));
        assert_eq!(p(-400.0, 0.1).clamped(), p(-180.0, 0.5));
        assert_eq!(p(f32::NAN, f32::INFINITY).clamped(), p(0.0, 1.0));
        assert_eq!(p(-45.0, 1.25).clamped(), p(-45.0, 1.25));
        for (pan, gain) in [(180.0, 2.0), (-180.0, 0.5), (12.5, 1.5)] {
            assert!(p(pan, gain).validate().is_ok(), "{pan} {gain}");
        }
        for pan in [180.5, -181.0, f32::NAN] {
            let e = p(pan, 1.0).validate().unwrap_err();
            assert!(e.contains("-180 to 180 degrees"), "{e}");
        }
        for gain in [0.49, 2.01, f32::INFINITY] {
            let e = p(0.0, gain).validate().unwrap_err();
            assert!(e.contains("0.5 to 2"), "{e}");
        }
        let f = FrameParams::default();
        assert_eq!(f.relight(), (0.0, 1.0));
        let f = p(-400.0, 3.0).frame_params(FrameParams {
            blur_strength: 0.25,
            ..FrameParams::default()
        });
        assert_eq!(
            (f.blur_strength, f.relight_pan_deg, f.relight_gain),
            (0.25, -180.0, 2.0)
        );
        let raw = FrameParams {
            relight_pan_deg: 999.0,
            relight_gain: f32::NAN,
            ..FrameParams::default()
        };
        assert_eq!(raw.relight(), (180.0, 1.0), "a backend clamps");
    }

    #[test]
    fn features_have_stable_names_and_availability_its_reason() {
        let names: Vec<&str> = Feature::ALL.iter().map(|f| f.as_str()).collect();
        assert_eq!(
            names,
            [
                "green-screen",
                "background-blur",
                "denoise",
                "relighting",
                "video-super-resolution",
                "upscale",
                "video-frame-generation"
            ]
        );
        assert_eq!(Feature::Relighting.to_string(), "relighting");
        assert!(Availability::Installed.is_installed());
        assert_eq!(Availability::Installed.reason(), None);
        let missing = Availability::NotInstalled("no library".into());
        assert!(!missing.is_installed());
        assert_eq!(missing.reason(), Some("no library"));
        fn _send_clone<T: Send + Sync + Clone>() {}
        _send_clone::<Availability>();
        _send_clone::<Feature>();
        _send_clone::<StageList>();
    }

    #[test]
    fn reset_temporal_defaults_to_a_no_op() {
        struct Stateless;
        impl InferenceBackend for Stateless {
            fn name(&self) -> &'static str {
                "stateless"
            }
            fn load(
                &mut self,
                _: &GpuContext,
                _: &LoadRequest,
            ) -> Result<LoadReport, BackendError> {
                Ok(LoadReport::default())
            }
            fn process(
                &mut self,
                _: &GpuContext,
                _: &FrameParams,
                _: &DeviceNv12,
                _: &DeviceNv12,
            ) -> Result<(), BackendError> {
                Ok(())
            }
            fn unload(&mut self) {}
            fn timings(&self) -> Option<StageTimings> {
                None
            }
        }
        let gpu = GpuContext {
            context: std::ptr::null_mut(),
            stream: std::ptr::null_mut(),
        };
        assert_eq!(Stateless.reset_temporal(&gpu), Ok(StageList::default()));
    }

    #[test]
    fn timings_total() {
        let t = StageTimings {
            convert_in_ms: 0.25,
            denoise_ms: 0.0,
            mask_ms: 1.0,
            relight_ms: 0.0,
            effect_ms: 0.5,
            convert_out_ms: 0.25,
        };
        assert_eq!(t.total_ms(), 2.0);
        let with_denoise = StageTimings {
            denoise_ms: 1.5,
            ..t
        };
        assert_eq!(with_denoise.total_ms(), 3.5);
        let with_both = StageTimings {
            relight_ms: 4.0,
            ..with_denoise
        };
        assert_eq!(with_both.total_ms(), 7.5);
        assert_eq!(with_both.stage_ms(Stage::Denoise), 1.5);
        assert_eq!(with_both.stage_ms(Stage::Relight), 4.0);
        assert_eq!(StageTimings::default().total_ms(), 0.0);
    }

    #[test]
    fn traits_are_object_safe() {
        fn _takes(_: &dyn InferenceBackend, _: &dyn BackendFactory) {}
        fn _send<T: Send>() {}
        _send::<GpuContext>();
        _send::<Box<dyn InferenceBackend>>();
    }
}
