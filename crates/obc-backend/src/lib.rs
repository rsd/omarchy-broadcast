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
//! * [`Availability`] says whether something a backend offers (a stage, a
//!   feature) is installed, from the filesystem alone, so that a client can
//!   grey out what it cannot offer without waking the GPU;
//! * a [`StageChainFactory`] and its [`StageChain`]s are the generic seam of
//!   a chain of stages that is not the effect itself (see [`chain`]).
//!
//! # Stages and their parameters
//!
//! A stage is named by a [`StageId`], a short vendor-neutral string
//! (`denoise`, `relight`, ...). This crate gives a meaning to none of them:
//! which stages exist, what they need and where in a frame they run is the
//! backend's business. The well-known ids ([`StageId::BACKGROUND`],
//! [`StageId::ENHANCE`], [`StageId::DENOISE`], [`StageId::RELIGHT`]) are
//! names only.
//!
//! A stage's settings are generic [`Params`]: string keys with typed
//! [`Value`]s. Those that need a reload (for example the light of a
//! relighting stage) travel in the [`LoadRequest`] ([`LoadRequest::params`]);
//! those that may change on every frame (its direction and brightness)
//! travel in each frame's [`FrameParams`] ([`FrameParams::live`]), so
//! changing them never reloads anything. A backend documents the keys of
//! each of its stages, and ignores keys it does not know.
//!
//! # Effects, stages and their order
//!
//! A request has one [`EffectKind`] (mutually exclusive: `none`, `blur`,
//! `replace`) and an ordered list of stackable stages ([`StageList`],
//! [`LoadRequest::stages`]), in the order the caller gives. A backend that
//! composites through a matte typically runs one frame in this order:
//!
//! 1. the conversion of the NV12 frame to the backend's working format;
//! 2. the **pre-mask stages**, those that do not consume the matte, in list
//!    order: each reads the previous one's output, so every later step sees
//!    the processed camera frame;
//! 3. the **mask** (the matte), computed only when the effect needs it
//!    (`blur`, `replace`) or a stage does;
//! 4. the **post-mask stages**, those that consume the matte, in list order.
//!    Such a stage's output is only valid where the matte says foreground (a
//!    relit foreground, for example), so it must be composited;
//! 5. the effect's **composite** through the mask: `none` over the original
//!    frame, `blur` over the blurred frame, `replace` over the background
//!    image. The foreground composited is the last post-mask stage's output,
//!    or the frame of step 2 without one. With `none` and no post-mask stage
//!    there is no mask and nothing to composite;
//! 6. the conversion back to NV12 into the destination.
//!
//! Which stages are pre-mask and which post-mask is the backend's to know;
//! a pre-mask stage always runs before the mask, whatever its place in the
//! list. An empty list adds nothing: no pass, copy or allocation
//! (`docs/adr/0018-video-noise-removal.md`, "off costs nothing").
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
//! A chain of stages on another device (for example on an NPU, or the CPU)
//! takes host frames ([`HostNv12`]) through the generic chain seam
//! ([`StageChain`], [`FrameKind::Host`]); the element then downloads each
//! frame, calls the chain and uploads the result.
//!
//! # Host frames for the effect
//!
//! When the video pipeline runs in system memory, the effect element hands
//! the backend canonical NV12 frames in host memory instead: a factory says
//! which memory its instances take frames in ([`BackendFactory::kinds`],
//! CUDA alone by default), and an instance that takes host frames loads
//! with [`InferenceBackend::load_host`], processes with
//! [`InferenceBackend::process_host`] and forgets its temporal state with
//! [`InferenceBackend::reset_temporal_host`]. No CUDA context exists on that
//! path: every call on an instance loaded with `load_host` (`unload`,
//! `is_loaded` and `timings` included) is made with no context current, and
//! the backend must not create or make current one of its own. The defaults
//! of the three methods refuse ([`BackendError::Unavailable`]), so a
//! CUDA-only backend implements nothing for them. One instance is loaded on
//! one memory at a time: the caller never mixes the CUDA and the host
//! methods on a loaded instance.

use std::borrow::{Borrow, Cow};
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

pub mod chain;
pub mod enhance;

pub use chain::{ChainFrame, ChainRequest, FrameKind, HostNv12, StageChain, StageChainFactory};

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

/// The name of a stage: a vendor-neutral string of 1 to 32 characters, a
/// lower-case ASCII letter followed by lower-case letters, digits and `-`
/// (`^[a-z][a-z0-9-]{0,31}$`), other than the names the elements' status
/// messages use for their own fields ([`StageId::RESERVED`], and any name
/// ending in `-reason`). Only [`StageId::new`] (and the constants) make
/// one, so every id is valid.
///
/// The constants are the well-known ids. [`StageId::BACKGROUND`] (the matte
/// and the core compositor modes of [`EffectKind`]) and
/// [`StageId::ENHANCE`] (the ingest upscaler) have a meaning to the core;
/// every other id is opaque to it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StageId(Cow<'static, str>);

impl StageId {
    /// The matte and the compositor of [`EffectKind`] (`blur`, `replace`).
    pub const BACKGROUND: StageId = StageId(Cow::Borrowed("background"));
    /// The upscaler of low-resolution camera modes, in the ingest.
    pub const ENHANCE: StageId = StageId(Cow::Borrowed("enhance"));
    /// Video noise removal.
    pub const DENOISE: StageId = StageId(Cow::Borrowed("denoise"));
    /// Relighting of the person.
    pub const RELIGHT: StageId = StageId(Cow::Borrowed("relight"));

    /// The longest id, in bytes.
    pub const MAX_LEN: usize = 32;

    /// The names a stage id cannot take: the fixed fields of the elements'
    /// status messages, which carry one field per stage (`<stage>` and
    /// `<stage>-reason`). A name ending in [`StageId::RESERVED_SUFFIX`] is
    /// reserved too.
    pub const RESERVED: [&'static str; 4] = ["status", "effect", "reason", "module"];

    /// The suffix of the status messages' per-stage reason fields; no stage
    /// id ends with it.
    pub const RESERVED_SUFFIX: &'static str = "-reason";

    /// The id `id`, or [`BackendError::InvalidInput`] with the rule it
    /// breaks.
    pub fn new(id: impl Into<String>) -> Result<StageId, BackendError> {
        let id = id.into();
        if !Self::is_well_formed(&id) {
            return Err(BackendError::InvalidInput(format!(
                "invalid stage id {id:?}: a stage id is a lower-case letter followed by up to \
                 {} lower-case letters, digits or '-'",
                Self::MAX_LEN - 1
            )));
        }
        if Self::is_reserved(&id) {
            return Err(BackendError::InvalidInput(format!(
                "invalid stage id {id:?}: {} and names ending in {:?} are reserved for the \
                 status messages",
                Self::RESERVED.join(", "),
                Self::RESERVED_SUFFIX
            )));
        }
        Ok(StageId(Cow::Owned(id)))
    }

    /// Whether `id` is a valid stage id: well formed and not reserved.
    pub fn is_valid(id: &str) -> bool {
        Self::is_well_formed(id) && !Self::is_reserved(id)
    }

    fn is_well_formed(id: &str) -> bool {
        let b = id.as_bytes();
        !b.is_empty()
            && b.len() <= Self::MAX_LEN
            && b[0].is_ascii_lowercase()
            && b[1..]
                .iter()
                .all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
    }

    fn is_reserved(id: &str) -> bool {
        Self::RESERVED.contains(&id) || id.ends_with(Self::RESERVED_SUFFIX)
    }

    /// The id as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for StageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for StageId {
    type Err = BackendError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        StageId::new(s)
    }
}

impl AsRef<str> for StageId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// Maps keyed by [`StageId`] can be looked up with a `&str`: an id orders,
/// hashes and compares as its string.
impl Borrow<str> for StageId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq<str> for StageId {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for StageId {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<StageId> for str {
    fn eq(&self, other: &StageId) -> bool {
        self == other.as_str()
    }
}

impl PartialEq<StageId> for &str {
    fn eq(&self, other: &StageId) -> bool {
        *self == other.as_str()
    }
}

/// The stages of a request, in the order the caller gives and each at most
/// once. Only [`StageList::new`] builds a list from several stages, and it
/// refuses a stage listed twice, so a list a backend receives never repeats
/// one. The empty list ([`StageList::default`]) allocates nothing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct StageList(Vec<StageId>);

impl StageList {
    /// The list of `stages`, in the order given, with no stage twice;
    /// [`BackendError::InvalidInput`] otherwise.
    pub fn new(stages: impl IntoIterator<Item = StageId>) -> Result<StageList, BackendError> {
        let stages: Vec<StageId> = stages.into_iter().collect();
        if let Some(s) = stages
            .iter()
            .enumerate()
            .find_map(|(i, s)| stages[..i].contains(s).then_some(s))
        {
            return Err(BackendError::InvalidInput(format!(
                "invalid stage list {}: the stage {s} is listed twice",
                StageList(stages.clone())
            )));
        }
        Ok(StageList(stages))
    }

    /// Whether `stage` is in the list.
    pub fn contains(&self, stage: &str) -> bool {
        self.0.iter().any(|s| s == stage)
    }

    /// Whether the list is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The number of stages.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// The stages, in order.
    pub fn as_slice(&self) -> &[StageId] {
        &self.0
    }

    /// The stages, in order.
    pub fn iter(&self) -> std::slice::Iter<'_, StageId> {
        self.0.iter()
    }

    /// The same list without `stage` (the same list if it is not in it).
    pub fn without(&self, stage: &str) -> StageList {
        StageList(self.0.iter().filter(|s| *s != stage).cloned().collect())
    }
}

impl From<StageId> for StageList {
    /// The list of the one stage.
    fn from(stage: StageId) -> StageList {
        StageList(vec![stage])
    }
}

impl<'a> IntoIterator for &'a StageList {
    type Item = &'a StageId;
    type IntoIter = std::slice::Iter<'a, StageId>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
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

/// The value of one stage parameter.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A switch.
    Bool(bool),
    /// A whole number (degrees, percent, a count).
    Int(i64),
    /// A real number.
    Float(f64),
    /// A string (for example a choice among names).
    Str(String),
}

impl Value {
    /// The switch, if this is a [`Value::Bool`].
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The number, if this is a [`Value::Int`].
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// The number, if this is a [`Value::Float`] or a [`Value::Int`].
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Float(f) => Some(*f),
            Value::Int(i) => Some(*i as f64),
            _ => None,
        }
    }

    /// The string, if this is a [`Value::Str`].
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    /// The kind of value, for messages: `bool`, `int`, `float` or `string`.
    pub fn kind(&self) -> &'static str {
        match self {
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Str(_) => "string",
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => write!(f, "{x}"),
            Value::Str(s) => f.write_str(s),
        }
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Value {
        Value::Bool(b)
    }
}

impl From<i64> for Value {
    fn from(i: i64) -> Value {
        Value::Int(i)
    }
}

impl From<i32> for Value {
    fn from(i: i32) -> Value {
        Value::Int(i.into())
    }
}

impl From<u32> for Value {
    fn from(i: u32) -> Value {
        Value::Int(i.into())
    }
}

impl From<f64> for Value {
    fn from(f: f64) -> Value {
        Value::Float(f)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Value {
        Value::Str(s.to_owned())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Value {
        Value::Str(s)
    }
}

/// The parameters of one stage, by key. The keys and their meaning are the
/// backend's (see the crate documentation).
pub type Params = BTreeMap<String, Value>;

/// Whether something a backend offers (a stage, or a feature it is made of)
/// is installed, as far as the filesystem shows. It says nothing about
/// whether it loads on this GPU: that is known only when it is loaded.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Availability {
    /// Its library and the models it needs are present.
    Installed,
    /// Something is missing; the reason says what, for logs (the library,
    /// or the models for this GPU).
    NotInstalled(String),
}

impl Availability {
    /// Whether it is installed.
    pub fn is_installed(&self) -> bool {
        matches!(self, Availability::Installed)
    }

    /// Why it is not installed, if it is not.
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
    /// [`EffectKind::None`], in the caller's order (see the crate
    /// documentation for how a backend runs them). A stage that is not
    /// listed loads no model and adds no pass; the empty list is the effect
    /// alone.
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
    /// The load-time parameters of the stages (for example the light of a
    /// relighting stage), by stage; a stage without an entry has its
    /// defaults, and an entry for a stage that is not listed is ignored. A
    /// new value on a loaded stage is a switch; the backend documents what
    /// it costs.
    pub params: BTreeMap<StageId, Params>,
}

impl LoadRequest {
    /// The load-time parameters of `stage`, if the request has any.
    pub fn stage_params(&self, stage: &str) -> Option<&Params> {
        self.params.get(stage)
    }
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

/// Per-frame parameters that change without a reload.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameParams {
    /// Blur strength, 0.0 to 1.0 (values outside are clamped). Used by
    /// [`EffectKind::Blur`] only.
    pub blur_strength: f32,
    /// The live parameters of the stages (for example the direction and
    /// brightness of a relighting stage), by stage; a stage without an
    /// entry has its defaults, and a backend clamps values outside their
    /// range. An entry for a stage that is not loaded is ignored.
    pub live: BTreeMap<StageId, Params>,
}

impl FrameParams {
    /// The live parameters of `stage`, if the frame has any.
    pub fn stage_params(&self, stage: &str) -> Option<&Params> {
        self.live.get(stage)
    }
}

impl Default for FrameParams {
    fn default() -> Self {
        FrameParams {
            blur_strength: 0.5,
            live: BTreeMap::new(),
        }
    }
}

/// GPU time of each step of one processed frame, in milliseconds, in the
/// order the steps ran, measured by the backend (with CUDA events on its
/// stream, for a CUDA backend). A step is a stage, by its id, or one of the
/// steps of [`StageTimings`]'s constants; a stage that did not run on the
/// frame has no entry.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StageTimings {
    steps: Vec<(String, f32)>,
}

impl StageTimings {
    /// The conversion of NV12 to the backend's working format.
    pub const CONVERT_IN: &'static str = "convert-in";
    /// The matte (segmentation mask).
    pub const MASK: &'static str = "mask";
    /// The effect's blur or composite over the background, through the
    /// matte.
    pub const COMPOSITE: &'static str = "composite";
    /// The conversion back to NV12 into the destination frame.
    pub const CONVERT_OUT: &'static str = "convert-out";

    /// Timings with no step.
    pub fn new() -> StageTimings {
        StageTimings::default()
    }

    /// Appends the time of `step`.
    pub fn push(&mut self, step: impl Into<String>, ms: f32) {
        self.steps.push((step.into(), ms));
    }

    /// `self` with the time of `step` appended.
    pub fn with(mut self, step: impl Into<String>, ms: f32) -> StageTimings {
        self.push(step, ms);
        self
    }

    /// The steps and their times, in the order they ran.
    pub fn steps(&self) -> &[(String, f32)] {
        &self.steps
    }

    /// The sum of all steps.
    pub fn total_ms(&self) -> f32 {
        self.steps.iter().map(|(_, ms)| ms).sum()
    }

    /// The time of `step` (a stage id or a step constant), if it ran.
    pub fn step_ms(&self, step: &str) -> Option<f32> {
        self.steps
            .iter()
            .find(|(s, _)| s == step)
            .map(|(_, ms)| *ms)
    }
}

/// A named numeric value a stage reports about the last processed frame
/// (auto-frame's current zoom, for one): the stage, the value's name,
/// unique within the stage, and the value.
#[derive(Debug, Clone, PartialEq)]
pub struct StageValue {
    pub stage: StageId,
    pub name: String,
    pub value: f64,
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
    /// cannot keep the previous chain whole: a new load-time parameter that
    /// a loaded stage fails to take (a new light of a relighting stage, for
    /// example) can leave the stage in an unknown state, so the reused
    /// instance keeps its previous chain **without** `stage`; the caller
    /// then knows the previous chain without `stage` is in force (without
    /// it if it was not in it, the same). From
    /// `process`: the backend has dropped `stage`, and the next frame runs
    /// the chain without it; this frame's `dst` is not written.
    #[error("{message}")]
    Stage {
        /// The stage that failed.
        stage: StageId,
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
/// * `process` and `load` take the same [`GpuContext`] (same stream);
/// * an instance loaded on host frames ([`InferenceBackend::load_host`]) is
///   called with no context current instead, for every method.
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
    /// streaming thread. Adding or removing a stage, or changing a stage's
    /// load-time parameters ([`LoadRequest::params`]), is such a switch
    /// too, and costs the stage's model load when it is added (the backend
    /// documents what a new parameter costs). If a switch on a loaded
    /// instance fails, the previous effect (and its stages) stays
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

    /// Whether a chain is loaded and can process frames; asked after a
    /// successful load. A backend that can lose its chain on its own (a
    /// backend over a module, unloaded after the module panicked) says so
    /// here, so that a caller does not take what it loaded last as still
    /// loaded. The default is `true`: a built-in backend loses its chain
    /// only through [`unload`] or a failed full load, which the caller sees.
    ///
    /// [`unload`]: InferenceBackend::unload
    fn is_loaded(&self) -> bool {
        true
    }

    /// Per-stage GPU times of the most recent frame whose work has
    /// completed, if the backend measures them.
    fn timings(&self) -> Option<StageTimings>;

    /// The status values the loaded stages report after the most recent
    /// processed frame (a module of ABI 1.3 or later); empty when none
    /// does. The default reports none.
    fn status_values(&self) -> Vec<StageValue> {
        Vec::new()
    }

    /// Forgets the temporal state of the loaded stages and of the mask, so
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

    /// [`InferenceBackend::load`] for frames in host memory: the same
    /// request and rules (a switch on a loaded instance, a failure that
    /// keeps the previous chain, [`BackendError::Stage`]), with no CUDA
    /// context (see "Host frames for the effect" in the crate docs). Called
    /// only when the factory's [`BackendFactory::kinds`] lists
    /// [`FrameKind::Host`]. The default refuses with
    /// [`BackendError::Unavailable`].
    fn load_host(&mut self, req: &LoadRequest) -> Result<LoadReport, BackendError> {
        let _ = req;
        Err(cuda_only(self.name()))
    }

    /// [`InferenceBackend::process`] for frames in host memory: reads `src`,
    /// writes `dst`, both of the loaded size, before it returns. `src` is
    /// only read: its planes may be mapped read-only, and the backend never
    /// writes through them (the output clock may push the same input
    /// again). `src` and `dst` never overlap. A [`BackendError::Stage`]
    /// means what it means for `process`: the stage is dropped and `dst` is
    /// not written. The default refuses with [`BackendError::Unavailable`].
    fn process_host(
        &mut self,
        params: &FrameParams,
        src: &HostNv12,
        dst: &mut HostNv12,
    ) -> Result<(), BackendError> {
        let _ = (params, src, dst);
        Err(cuda_only(self.name()))
    }

    /// [`InferenceBackend::reset_temporal`] for an instance loaded with
    /// [`InferenceBackend::load_host`]: the same meaning, and the same
    /// returned list of the stages dropped instead of reset, with no CUDA
    /// context. The default refuses with [`BackendError::Unavailable`].
    fn reset_temporal_host(&mut self) -> Result<StageList, BackendError> {
        Err(cuda_only(self.name()))
    }
}

/// What the host-frame methods of a backend that takes CUDA frames only
/// return.
fn cuda_only(name: &str) -> BackendError {
    BackendError::Unavailable(format!("{name} takes CUDA frames only"))
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

    /// The memory the instances take frames in, in order of preference:
    /// [`FrameKind::Cuda`] for [`InferenceBackend::load`] and `process`,
    /// [`FrameKind::Host`] for [`InferenceBackend::load_host`] and
    /// `process_host`. The default is CUDA alone. Never empty.
    fn kinds(&self) -> &[FrameKind] {
        &[FrameKind::Cuda]
    }

    /// Checks the parameters a client sets for `stage` (`params` at load
    /// time, `live` per frame) without loading anything, with the reason if
    /// they are malformed or out of range: what a client is told before it
    /// applies them (a backend clamps a live value instead). Keys the
    /// backend does not know are accepted. The default accepts everything.
    fn check_params(&self, stage: &StageId, params: &Params, live: &Params) -> Result<(), String> {
        let _ = (stage, params, live);
        Ok(())
    }
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
            stage: StageId::DENOISE,
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
    fn stage_ids_are_validated() {
        for ok in ["denoise", "relight", "a", "eye-contact", "x2", "a-b-c-1"] {
            let id = StageId::new(ok).unwrap();
            assert_eq!(id.as_str(), ok);
            assert_eq!(id.to_string(), ok);
            assert_eq!(ok.parse::<StageId>().unwrap(), id);
        }
        let longest = format!("a{}", "b".repeat(31));
        assert!(StageId::new(longest.clone()).is_ok());
        for bad in [
            "",
            "Denoise",
            "1denoise",
            "-x",
            "de_noise",
            "de noise",
            "dé",
            &format!("{longest}c"),
        ] {
            let e = StageId::new(bad).unwrap_err();
            assert!(matches!(e, BackendError::InvalidInput(_)), "{bad:?}");
            assert!(e.to_string().contains("invalid stage id"), "{e}");
            assert!(!StageId::is_valid(bad), "{bad:?}");
        }
        // The status messages' own field names.
        for reserved in [
            "status",
            "effect",
            "reason",
            "module",
            "relight-reason",
            "x-reason",
        ] {
            let e = StageId::new(reserved).unwrap_err();
            assert!(
                matches!(&e, BackendError::InvalidInput(m) if m.contains("reserved")),
                "{reserved}: {e:?}"
            );
            assert!(!StageId::is_valid(reserved), "{reserved:?}");
        }
        for ok in ["statuses", "reasons", "reason-x", "modules"] {
            assert!(StageId::new(ok).is_ok(), "{ok}");
        }
        for (c, s) in [
            (StageId::BACKGROUND, "background"),
            (StageId::ENHANCE, "enhance"),
            (StageId::DENOISE, "denoise"),
            (StageId::RELIGHT, "relight"),
        ] {
            assert!(StageId::is_valid(c.as_str()));
            assert_eq!(
                c,
                StageId::new(s).unwrap(),
                "a constant equals the parsed id"
            );
            assert_eq!(c, s);
        }
        // A map keyed by id is looked up with a string.
        let mut m = BTreeMap::new();
        m.insert(StageId::RELIGHT, 1);
        assert_eq!(m.get("relight"), Some(&1));
    }

    #[test]
    fn stage_lists_keep_the_callers_order_once() {
        let empty = StageList::default();
        assert!(empty.is_empty());
        assert_eq!(empty.as_slice(), &[] as &[StageId]);
        assert_eq!(empty.to_string(), "[]");
        assert_eq!(StageList::new([]).unwrap(), empty);

        let one = StageList::new([StageId::DENOISE]).unwrap();
        assert_eq!(one, StageList::from(StageId::DENOISE));
        assert!(one.contains("denoise") && !one.is_empty());
        assert_eq!(one.to_string(), "[denoise]");
        assert!(!empty.contains("denoise"));

        let e = StageList::new([StageId::DENOISE, StageId::DENOISE]).unwrap_err();
        assert!(matches!(e, BackendError::InvalidInput(_)), "{e:?}");
        assert!(e.to_string().contains("denoise is listed twice"), "{e}");

        let both = StageList::new([StageId::DENOISE, StageId::RELIGHT]).unwrap();
        assert!(both.contains("denoise") && both.contains("relight"));
        assert_eq!(both.to_string(), "[denoise, relight]");
        assert_eq!(both.as_slice(), &[StageId::DENOISE, StageId::RELIGHT]);
        assert_eq!(both.len(), 2);
        assert_eq!(both.without("denoise"), StageList::from(StageId::RELIGHT));
        assert_eq!(both.without("hello"), both);
        // The caller's order is kept, whatever it is.
        let reversed = StageList::new([StageId::RELIGHT, StageId::DENOISE]).unwrap();
        assert_eq!(reversed.to_string(), "[relight, denoise]");
        assert_ne!(reversed, both);
        let e = StageList::new([StageId::DENOISE, StageId::RELIGHT, StageId::RELIGHT]).unwrap_err();
        assert!(e.to_string().contains("relight is listed twice"), "{e}");
    }

    #[test]
    fn values_read_as_their_kind() {
        assert_eq!(Value::from(true).as_bool(), Some(true));
        assert_eq!(Value::from(-30).as_int(), Some(-30));
        assert_eq!(Value::from(150u32).as_f64(), Some(150.0));
        assert_eq!(Value::from(1.5).as_f64(), Some(1.5));
        assert_eq!(Value::from(1.5).as_int(), None);
        assert_eq!(Value::from("warm").as_str(), Some("warm"));
        assert_eq!(Value::from("warm").as_int(), None);
        assert_eq!(Value::Int(3).as_bool(), None);
        assert_eq!(
            [
                Value::Bool(false).kind(),
                Value::Int(0).kind(),
                Value::Float(0.0).kind(),
                Value::Str(String::new()).kind()
            ],
            ["bool", "int", "float", "string"]
        );
        assert_eq!(Value::from("cool").to_string(), "cool");
        assert_eq!(Value::from(-5).to_string(), "-5");
    }

    #[test]
    fn requests_and_frames_carry_params_by_stage() {
        let mut live = BTreeMap::new();
        live.insert(
            StageId::RELIGHT,
            Params::from([("direction".to_string(), Value::Int(45))]),
        );
        let f = FrameParams {
            blur_strength: 0.25,
            live,
        };
        assert_eq!(
            f.stage_params("relight").and_then(|p| p.get("direction")),
            Some(&Value::Int(45))
        );
        assert_eq!(f.stage_params("denoise"), None);
        let d = FrameParams::default();
        assert_eq!((d.blur_strength, d.live.len()), (0.5, 0));
        let req = LoadRequest {
            effect: EffectKind::None,
            stages: StageId::RELIGHT.into(),
            width: 1280,
            height: 720,
            colour: ColourSpec::default(),
            background: None,
            params: BTreeMap::from([(
                StageId::RELIGHT,
                Params::from([("light".to_string(), Value::from("warm"))]),
            )]),
        };
        assert_eq!(
            req.stage_params("relight").and_then(|p| p.get("light")),
            Some(&Value::from("warm"))
        );
        assert_eq!(req.stage_params("denoise"), None);
    }

    #[test]
    fn availability_has_its_reason() {
        assert!(Availability::Installed.is_installed());
        assert_eq!(Availability::Installed.reason(), None);
        let missing = Availability::NotInstalled("no library".into());
        assert!(!missing.is_installed());
        assert_eq!(missing.reason(), Some("no library"));
        fn _send_clone<T: Send + Sync + Clone>() {}
        _send_clone::<Availability>();
        _send_clone::<StageId>();
        _send_clone::<StageList>();
        _send_clone::<Params>();
    }

    #[test]
    fn the_default_factory_accepts_any_params() {
        struct Factory;
        impl BackendFactory for Factory {
            fn name(&self) -> &'static str {
                "any"
            }
            fn probe(&self) -> Result<(), BackendError> {
                Ok(())
            }
            fn create(&self) -> Result<Box<dyn InferenceBackend>, BackendError> {
                Err(BackendError::Unavailable("none".into()))
            }
        }
        let p = Params::from([("x".to_string(), Value::Int(1))]);
        assert_eq!(Factory.check_params(&StageId::DENOISE, &p, &p), Ok(()));
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
    fn a_cuda_backend_refuses_host_frames_by_default() {
        struct CudaOnly;
        impl InferenceBackend for CudaOnly {
            fn name(&self) -> &'static str {
                "cuda-only"
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
        struct Factory;
        impl BackendFactory for Factory {
            fn name(&self) -> &'static str {
                "cuda-only"
            }
            fn probe(&self) -> Result<(), BackendError> {
                Ok(())
            }
            fn create(&self) -> Result<Box<dyn InferenceBackend>, BackendError> {
                Ok(Box::new(CudaOnly))
            }
        }
        assert_eq!(Factory.kinds(), &[FrameKind::Cuda]);
        let mut b = Factory.create().unwrap();
        let refused = BackendError::Unavailable("cuda-only takes CUDA frames only".into());
        let req = LoadRequest {
            effect: EffectKind::Blur,
            stages: StageList::default(),
            width: 4,
            height: 2,
            colour: ColourSpec::default(),
            background: None,
            params: BTreeMap::new(),
        };
        assert_eq!(b.load_host(&req), Err(refused.clone()));
        let (mut y, mut uv) = (vec![0u8; 8], vec![0u8; 4]);
        let mut frame = || HostNv12 {
            y: y.as_mut_ptr(),
            y_pitch: 4,
            uv: uv.as_mut_ptr(),
            uv_pitch: 4,
            width: 4,
            height: 2,
        };
        let (src, mut dst) = (frame(), frame());
        assert_eq!(
            b.process_host(&FrameParams::default(), &src, &mut dst),
            Err(refused.clone())
        );
        assert_eq!(b.reset_temporal_host(), Err(refused));
    }

    #[test]
    fn timings_total() {
        let t = StageTimings::new()
            .with(StageTimings::CONVERT_IN, 0.25)
            .with(StageTimings::MASK, 1.0)
            .with(StageTimings::COMPOSITE, 0.5)
            .with(StageTimings::CONVERT_OUT, 0.25);
        assert_eq!(t.total_ms(), 2.0);
        assert_eq!(t.step_ms("denoise"), None, "a stage that did not run");
        let with_denoise = StageTimings::new()
            .with(StageTimings::CONVERT_IN, 0.25)
            .with("denoise", 1.5)
            .with(StageTimings::MASK, 1.0)
            .with(StageTimings::COMPOSITE, 0.5)
            .with(StageTimings::CONVERT_OUT, 0.25);
        assert_eq!(with_denoise.total_ms(), 3.5);
        let mut with_both = with_denoise.clone();
        with_both.push("relight", 4.0);
        assert_eq!(with_both.total_ms(), 7.5);
        assert_eq!(with_both.step_ms("denoise"), Some(1.5));
        assert_eq!(with_both.step_ms("relight"), Some(4.0));
        assert_eq!(with_both.step_ms(StageTimings::MASK), Some(1.0));
        assert_eq!(with_both.steps().len(), 6);
        assert_eq!(with_both.steps()[1], ("denoise".to_string(), 1.5));
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
