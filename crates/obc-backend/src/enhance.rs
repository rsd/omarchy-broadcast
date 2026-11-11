//! The seam of the enhance stage: upscaling a low-resolution camera frame
//! at its capture size, before the ingest scales it to the output
//! contract.
//!
//! The stage sits in the ingest, not in the Process stage, because it
//! changes the frame size: an [`Enhancer`] reads a 4:2:0 frame
//! ([`DeviceYuv420`], NV12 or I420) of the capture size and writes an NV12
//! frame [`EnhanceRequest::scale`] times as wide and as high. The ingest's
//! scaler then fits that frame into the output exactly as it fits the camera
//! frame without the stage.
//!
//! The stage hides nothing, so it fails open: an enhancer that cannot load
//! or fails on a frame reports why ([`BackendError`]), and the caller goes
//! on showing the camera frame without it.
//!
//! The threading and context rules are those of
//! [`InferenceBackend`](crate::InferenceBackend): every call has the
//! request's [`GpuContext`] current, and the work is enqueued on its
//! stream.

use crate::{BackendError, ColourSpec, DeviceNv12, GpuContext, LoadReport, Params};

/// The smallest scale factor an enhancer is asked for: below it the stage
/// is not worth its passes.
pub const MIN_SCALE: u32 = 2;

/// The largest scale factor an enhancer is asked for. A source that needs
/// more is enhanced by this factor and the ingest's scaler does the rest.
pub const MAX_SCALE: u32 = 4;

/// The plane layout of a 4:2:0 frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum YuvLayout {
    /// A Y plane and one interleaved U,V plane.
    #[default]
    Nv12,
    /// A Y plane, a U plane and a V plane.
    I420,
}

impl YuvLayout {
    /// The GStreamer format name (`NV12`, `I420`).
    pub fn as_str(self) -> &'static str {
        match self {
            YuvLayout::Nv12 => "NV12",
            YuvLayout::I420 => "I420",
        }
    }
}

/// Where the chroma samples of a 4:2:0 frame sit relative to the luma
/// samples. Reading or writing a frame with the wrong siting shifts its
/// colour by half a pixel against its luma.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ChromaSiting {
    /// Horizontally co-sited with the even luma columns, vertically between
    /// two rows: MPEG-2, most video, and the reading of a frame whose caps
    /// say nothing.
    #[default]
    Cosited,
    /// Between the luma samples in both directions: JPEG and MPEG-1
    /// (`jpegdec`'s `chroma-site=jpeg`).
    Interstitial,
    /// Co-sited with the even luma columns and rows (the top-left sample).
    TopLeft,
}

/// A 4:2:0 frame in CUDA device memory, in either [`YuvLayout`].
///
/// For [`YuvLayout::Nv12`], `u` is the interleaved plane, `v` is `u + 1`
/// and `u_pitch == v_pitch`. Each chroma plane has `height / 2` rows. The
/// pointers are `CUdeviceptr` values valid in the caller's [`GpuContext`];
/// the description owns nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceYuv420 {
    /// The plane layout.
    pub layout: YuvLayout,
    /// Device address of pixel (0, 0) of the Y plane.
    pub y: u64,
    /// Bytes between the starts of two Y rows.
    pub y_pitch: usize,
    /// Device address of the first U sample.
    pub u: u64,
    /// Bytes between the starts of two U rows.
    pub u_pitch: usize,
    /// Device address of the first V sample.
    pub v: u64,
    /// Bytes between the starts of two V rows.
    pub v_pitch: usize,
    /// Width in pixels (even).
    pub width: u32,
    /// Height in pixels (even).
    pub height: u32,
}

impl DeviceYuv420 {
    /// The same frame as a [`DeviceNv12`] describes it.
    pub fn from_nv12(f: &DeviceNv12) -> DeviceYuv420 {
        DeviceYuv420 {
            layout: YuvLayout::Nv12,
            y: f.y,
            y_pitch: f.y_pitch,
            u: f.uv,
            u_pitch: f.uv_pitch,
            v: f.uv + 1,
            v_pitch: f.uv_pitch,
            width: f.width,
            height: f.height,
        }
    }

    /// The bytes between two horizontally adjacent samples of a chroma
    /// plane: 2 in NV12 (U and V interleaved), 1 in I420.
    pub fn chroma_step(&self) -> usize {
        match self.layout {
            YuvLayout::Nv12 => 2,
            YuvLayout::I420 => 1,
        }
    }

    /// Checks the description is self-consistent: non-null planes, even
    /// non-zero size, pitches that hold a row.
    pub fn validate(&self) -> Result<(), BackendError> {
        if self.y == 0 || self.u == 0 || self.v == 0 {
            return Err(BackendError::InvalidInput(
                "4:2:0 frame has a null plane pointer".into(),
            ));
        }
        if self.width == 0
            || self.height == 0
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
        {
            return Err(BackendError::InvalidInput(format!(
                "4:2:0 frame size {}x{} must be even and non-zero",
                self.width, self.height
            )));
        }
        let w = self.width as usize;
        let chroma_row = w / 2 * self.chroma_step();
        if self.y_pitch < w || self.u_pitch < chroma_row || self.v_pitch < chroma_row {
            return Err(BackendError::InvalidInput(format!(
                "{} pitches (Y {}, U {}, V {}) are too small for the width {}",
                self.layout.as_str(),
                self.y_pitch,
                self.u_pitch,
                self.v_pitch,
                self.width
            )));
        }
        Ok(())
    }
}

/// What [`Enhancer::load`] should prepare.
#[derive(Debug, Clone, PartialEq)]
pub struct EnhanceRequest {
    /// Input width in pixels (even); every input frame has it.
    pub width: u32,
    /// Input height in pixels (even); every input frame has it.
    pub height: u32,
    /// The integer factor, [`MIN_SCALE`] to [`MAX_SCALE`]: the output is
    /// `scale * width` by `scale * height`.
    pub scale: u32,
    /// The plane layout of the input frames.
    pub layout: YuvLayout,
    /// The colour matrix and range of the input frames; the output frames
    /// have the same.
    pub colour: ColourSpec,
    /// The chroma siting of the input frames; the output frames have the
    /// same.
    pub chroma: ChromaSiting,
    /// The load-time parameters of the `enhance` stage (for example the
    /// method of a module that has more than one), as
    /// [`LoadRequest::params`](crate::LoadRequest::params) carries them for
    /// the effect's stages; empty: the enhancer's defaults. A new value is a
    /// new request, so the caller loads again.
    pub params: Params,
}

impl EnhanceRequest {
    /// The output frame size.
    pub fn output_size(&self) -> (u32, u32) {
        (
            self.width.saturating_mul(self.scale),
            self.height.saturating_mul(self.scale),
        )
    }

    /// Checks the size and the factor.
    pub fn validate(&self) -> Result<(), BackendError> {
        if self.width == 0
            || self.height == 0
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
        {
            return Err(BackendError::InvalidInput(format!(
                "the enhance input size {}x{} must be even and non-zero",
                self.width, self.height
            )));
        }
        if !(MIN_SCALE..=MAX_SCALE).contains(&self.scale) {
            return Err(BackendError::InvalidInput(format!(
                "the enhance scale {} is outside {MIN_SCALE} to {MAX_SCALE}",
                self.scale
            )));
        }
        Ok(())
    }
}

/// GPU time of each step of one enhanced frame, in milliseconds, measured
/// with CUDA events on the enhancer's stream.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct EnhanceTimings {
    /// The input frame to the enhancer's working format.
    pub convert_in_ms: f32,
    /// The upscale itself.
    pub upscale_ms: f32,
    /// Back to NV12 into the destination frame.
    pub convert_out_ms: f32,
}

impl EnhanceTimings {
    /// The sum of all steps.
    pub fn total_ms(&self) -> f32 {
        self.convert_in_ms + self.upscale_ms + self.convert_out_ms
    }
}

/// One instance of an enhancer: the model and buffers for one input size
/// and factor. Calls follow the rules of the module documentation.
pub trait Enhancer: Send {
    /// A short name for logs (`"maxine-vsr"`).
    fn name(&self) -> &'static str;

    /// Loads the model and allocates the buffers for `req`, replacing what
    /// was loaded. May block for a fraction of a second. If it fails, the
    /// instance is left unloaded.
    fn load(&mut self, gpu: &GpuContext, req: &EnhanceRequest) -> Result<LoadReport, BackendError>;

    /// Enqueues one frame on `gpu.stream`: reads `src` (the loaded input
    /// size and layout), writes `dst` (the loaded output size). Never
    /// writes `src`. Returns once the work is enqueued.
    fn process(
        &mut self,
        gpu: &GpuContext,
        src: &DeviceYuv420,
        dst: &DeviceNv12,
    ) -> Result<(), BackendError>;

    /// Releases the model, the buffers and the CUDA resources, so their GPU
    /// memory returns. Idempotent; also done on drop (with the context
    /// current, so callers unload explicitly before releasing the context).
    fn unload(&mut self);

    /// The step times of the most recent frame whose work has completed, if
    /// the enhancer measures them.
    fn timings(&self) -> Option<EnhanceTimings>;
}

/// Creates enhancers and reports whether they can run.
pub trait EnhancerFactory: Send + Sync {
    /// A short name for logs (`"maxine-vsr"`).
    fn name(&self) -> &'static str;

    /// Checks cheaply whether the enhancer can run: libraries present and
    /// configured. Loads no model and creates no CUDA context.
    fn probe(&self) -> Result<(), BackendError>;

    /// Creates an unloaded instance. Does what `probe` does, and no more.
    fn create(&self) -> Result<Box<dyn Enhancer>, BackendError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nv12() -> DeviceNv12 {
        DeviceNv12 {
            y: 0x1000,
            y_pitch: 1024,
            uv: 0x9000,
            uv_pitch: 1024,
            width: 960,
            height: 540,
        }
    }

    #[test]
    fn an_nv12_frame_is_a_4_2_0_frame_with_interleaved_chroma() {
        let f = DeviceYuv420::from_nv12(&nv12());
        assert_eq!(f.layout, YuvLayout::Nv12);
        assert_eq!((f.u, f.v), (0x9000, 0x9001));
        assert_eq!(f.chroma_step(), 2);
        assert!(f.validate().is_ok());
    }

    #[test]
    fn frame_validation_checks_pointers_size_and_pitches() {
        let ok = DeviceYuv420 {
            layout: YuvLayout::I420,
            y: 1,
            y_pitch: 640,
            u: 2,
            u_pitch: 320,
            v: 3,
            v_pitch: 320,
            width: 640,
            height: 360,
        };
        assert!(ok.validate().is_ok());
        assert!(DeviceYuv420 { v: 0, ..ok }.validate().is_err());
        assert!(DeviceYuv420 { width: 641, ..ok }.validate().is_err());
        assert!(DeviceYuv420 { u_pitch: 319, ..ok }.validate().is_err());
        // NV12 needs a full row of interleaved chroma.
        let nv = DeviceYuv420 {
            layout: YuvLayout::Nv12,
            ..ok
        };
        assert!(nv.validate().is_err());
    }

    #[test]
    fn requests_take_even_sizes_and_factors_2_to_4() {
        let req = EnhanceRequest {
            width: 960,
            height: 540,
            scale: 2,
            layout: YuvLayout::Nv12,
            colour: ColourSpec::LIMITED_BT601,
            chroma: ChromaSiting::default(),
            params: Params::new(),
        };
        assert!(req.validate().is_ok());
        assert_eq!(req.chroma, ChromaSiting::Cosited, "MPEG-2 unless told");
        assert_eq!(req.output_size(), (1920, 1080));
        for scale in [0, 1, 5] {
            let e = EnhanceRequest {
                scale,
                ..req.clone()
            }
            .validate()
            .unwrap_err();
            assert!(e.to_string().contains("2 to 4"), "{e}");
        }
        assert!(
            EnhanceRequest {
                height: 541,
                ..req.clone()
            }
            .validate()
            .is_err()
        );
        assert_eq!(YuvLayout::I420.as_str(), "I420");
    }
}
