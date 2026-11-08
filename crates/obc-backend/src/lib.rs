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
//! * an [`InferenceBackend`] instance loads the models for one effect
//!   ([`InferenceBackend::load`]), processes one canonical frame at a time
//!   ([`InferenceBackend::process`]) and releases everything again
//!   ([`InferenceBackend::unload`]).
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

/// The effect the Process stage applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum EffectKind {
    /// No effect: the element passes frames through and never calls a
    /// backend.
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
    /// [`EffectKind::Blur`] or [`EffectKind::Replace`]; `None` is rejected
    /// (the element does not call a backend for it).
    pub effect: EffectKind,
    /// Frame width in pixels; every frame passed to `process` has it.
    pub width: u32,
    /// Frame height in pixels; every frame passed to `process` has it.
    pub height: u32,
    /// The colour matrix and range of the frames.
    pub colour: ColourSpec,
    /// The background image, already scaled and cropped to `width x height`.
    /// Required for [`EffectKind::Replace`], ignored otherwise.
    pub background: Option<Arc<HostBgr>>,
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
    /// `true` when the mask model of a previous load was kept (same size and
    /// colour), so the load cost only the effect-specific part.
    pub reused_model: bool,
}

/// Per-frame parameters that change without a reload.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameParams {
    /// Blur strength, 0.0 to 1.0 (values outside are clamped). Used by
    /// [`EffectKind::Blur`] only.
    pub blur_strength: f32,
}

impl Default for FrameParams {
    fn default() -> Self {
        FrameParams { blur_strength: 0.5 }
    }
}

/// GPU time of each stage of one processed frame, in milliseconds, measured
/// with CUDA events on the backend's stream.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct StageTimings {
    /// NV12 to the backend's input format (BGR for Maxine).
    pub convert_in_ms: f32,
    /// The segmentation mask.
    pub mask_ms: f32,
    /// The blur or the composite over the background.
    pub effect_ms: f32,
    /// Back to NV12 into the destination frame.
    pub convert_out_ms: f32,
}

impl StageTimings {
    /// The sum of all stages.
    pub fn total_ms(&self) -> f32 {
        self.convert_in_ms + self.mask_ms + self.effect_ms + self.convert_out_ms
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
        /// The backend's status code (for Maxine, an `NvCV_Status`).
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
    /// streaming thread. If a switch on a loaded instance fails, the
    /// previous effect stays loaded; if a full load fails, the instance is
    /// left unloaded.
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
    }

    #[test]
    fn timings_total() {
        let t = StageTimings {
            convert_in_ms: 0.25,
            mask_ms: 1.0,
            effect_ms: 0.5,
            convert_out_ms: 0.25,
        };
        assert_eq!(t.total_ms(), 2.0);
    }

    #[test]
    fn traits_are_object_safe() {
        fn _takes(_: &dyn InferenceBackend, _: &dyn BackendFactory) {}
        fn _send<T: Send>() {}
        _send::<GpuContext>();
        _send::<Box<dyn InferenceBackend>>();
    }
}
