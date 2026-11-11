//! Frame generation on Maxine: the `VideoFrameGeneration` effect, which
//! makes the picture midway between two frames.
//!
//! [`MaxineVfg`] keeps the last two frames it was given ([`MaxineVfg::push`])
//! and writes the frame midway between them ([`MaxineVfg::interpolate`]); it
//! also keeps the last frame as it came, in NV12, to write it again
//! ([`MaxineVfg::write_held`]). Per frame, all on the GPU and on the
//! caller's stream, with no host copy:
//!
//! 1. `push`: the NV12 frame is copied into the held frame, the effect's
//!    current image becomes its previous one, and the frame is converted to
//!    RGBA into the current image (`NvCVImage_TransferFromYUV`, matrix and
//!    range from the load);
//! 2. `interpolate`: `VideoFrameGeneration` from the previous image
//!    (`SrcImage0`) and the current one (`SrcImage1`) at the time step
//!    [`TIMESTEP_MIDWAY`], into an RGBA image, then RGBA to NV12 into the
//!    destination (`NvCVImage_TransferToYUV`).
//!
//! The effect runs in [`MODE_MEDIUM`] with automatic shot-change detection
//! on, so a cut between the two frames is not blended. It ships no model
//! files: the feature library is the whole feature
//! ([`Feature::VideoFrameGeneration`](crate::Feature)). The SDK is found and
//! opened exactly as for the effects ([`MaxineFactory`]): one SDK per
//! process, loaded from the same root.

use std::ffi::{CStr, c_void};
use std::path::{Path, PathBuf};
use std::time::Instant;

use obc_backend::{BackendError, ColourSpec, DeviceNv12, GpuContext, LoadReport};
use obc_gpu::{CuStream, DeviceNv12Buffer};

use crate::ffi::{self, NvCvImage, NvCvStatus, Sdk};
use crate::{Effect, GpuImage, MaxineBackend, MaxineFactory, colour_flags, ms_since, sync};

/// The effect selector.
const FX_VIDEO_FRAME_GENERATION: &CStr = c"VideoFrameGeneration";
/// The second source image: the current frame (`SrcImage0` is the previous).
const P_INPUT_IMAGE_1: &CStr = c"SrcImage1";
/// The exact input size (`unsigned int`), required before `NvVFX_Load`.
const P_INPUT_WIDTH: &CStr = c"InputWidth";
/// See [`P_INPUT_WIDTH`].
const P_INPUT_HEIGHT: &CStr = c"InputHeight";
/// The quality mode (`unsigned int`).
const P_MODE: &CStr = c"Mode";
/// Whether the effect detects cuts itself (`unsigned int`, 0 or 1).
const P_AUTOMATIC_SHOT_CHANGE: &CStr = c"AutomaticShotChangeDetectionEnabled";
/// Where between the two frames the output is (`float`, in (0, 1)).
const P_TIMESTEP: &CStr = c"Timestep";
/// `Mode` medium: the mode the stage runs in.
pub const MODE_MEDIUM: u32 = 1;
/// The time step of the frame midway between the two inputs.
pub const TIMESTEP_MIDWAY: f32 = 0.5;
/// `NvCVImage_PixelFormat` RGBA.
const NVCV_RGBA: i32 = 6;
/// The feature library, for error messages.
const LIBRARY: &str = "libnvVFXVideoFrameGeneration.so";

/// Finds the Maxine SDK, as [`MaxineFactory`] does, and creates
/// [`MaxineVfg`] instances.
#[derive(Debug, Clone)]
pub struct MaxineVfgFactory {
    sdk: MaxineFactory,
}

impl MaxineVfgFactory {
    /// Uses the SDK root in force now, as [`MaxineFactory::from_env`].
    pub fn from_env() -> MaxineVfgFactory {
        MaxineVfgFactory::with_root(crate::effective_root())
    }

    /// Uses `root` as the SDK root (`None`: the library search path only),
    /// as [`MaxineFactory::with_root`].
    pub fn with_root(root: Option<PathBuf>) -> MaxineVfgFactory {
        MaxineVfgFactory {
            sdk: MaxineFactory::with_root(root),
        }
    }

    /// The SDK root this factory looks in first.
    pub fn root(&self) -> Option<&Path> {
        self.sdk.root()
    }

    /// Opens the SDK libraries (once per process). Loads no effect and
    /// creates no CUDA context.
    pub fn probe(&self) -> Result<(), BackendError> {
        self.sdk.sdk().map(|_| ())
    }

    /// Creates an unloaded [`MaxineVfg`].
    pub fn create_vfg(&self) -> Result<MaxineVfg, BackendError> {
        let sdk = self.sdk.sdk()?;
        Ok(MaxineVfg {
            sdk,
            // Only for error messages: the effect takes no model directory.
            model_dir: sdk.model_dir.clone().unwrap_or_default(),
            loaded: None,
        })
    }
}

/// Everything loaded for one geometry. Field order is drop order: the
/// effect, which holds the images, goes first.
struct Loaded {
    effect: Effect,
    /// The previous frame, in RGBA: `SrcImage0`.
    prev: GpuImage,
    /// The current frame, in RGBA: `SrcImage1`.
    cur: GpuImage,
    /// The generated frame: `DstImage0`.
    out: GpuImage,
    /// The current frame as it came, in NV12.
    held: DeviceNv12Buffer,
    /// The SDK's scratch images for the two transfers.
    staging_in: Box<NvCvImage>,
    staging_out: Box<NvCvImage>,
    width: u32,
    height: u32,
    flags: u32,
    stream: CuStream,
}

impl Drop for Loaded {
    fn drop(&mut self) {
        let sdk = self.effect.sdk;
        // SAFETY: the staging images are empty or were grown by the SDK's
        // transfers; each is freed once.
        unsafe {
            (sdk.NvCVImage_Dealloc)(&mut *self.staging_in);
            (sdk.NvCVImage_Dealloc)(&mut *self.staging_out);
        }
    }
}

/// The frame generation effect for one stream. Its calls follow the
/// context rules of [`obc_backend`]: the context of the load is current on
/// the calling thread, and the work goes on the stream of the load.
pub struct MaxineVfg {
    sdk: &'static Sdk,
    model_dir: PathBuf,
    loaded: Option<Loaded>,
}

// SAFETY: as for `MaxineBackend`: the SDK handles and CUDA objects are used
// with the load's context pushed on whichever thread calls, and all access
// goes through `&mut self`.
unsafe impl Send for MaxineVfg {}

impl MaxineVfg {
    /// Whether a geometry is loaded.
    pub fn is_loaded(&self) -> bool {
        self.loaded.is_some()
    }

    /// Whether the loaded effect is for frames of `width` x `height` in
    /// `colour`, on `gpu`'s stream.
    pub fn is_loaded_for(
        &self,
        gpu: &GpuContext,
        width: u32,
        height: u32,
        colour: ColourSpec,
    ) -> bool {
        self.loaded.as_ref().is_some_and(|l| {
            (l.width, l.height) == (width, height)
                && l.flags == colour_flags(colour)
                && l.stream == gpu.stream
        })
    }

    /// Loads the effect for NV12 frames of `width` x `height` in `colour`,
    /// on `gpu`'s stream. Whatever was loaded is released first.
    pub fn load(
        &mut self,
        gpu: &GpuContext,
        width: u32,
        height: u32,
        colour: ColourSpec,
    ) -> Result<LoadReport, BackendError> {
        MaxineBackend::check_context(gpu)?;
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(BackendError::InvalidInput(format!(
                "frame size {width}x{height} must be even and non-zero"
            )));
        }
        self.unload();
        let t = Instant::now();
        let loaded = self.build(width, height, colour, gpu.stream)?;
        // Surface an asynchronous fault of the load here, not on a frame.
        sync(gpu.stream)?;
        self.loaded = Some(loaded);
        Ok(LoadReport {
            load_ms: ms_since(t),
            warmup_ms: None,
            reused_model: false,
        })
    }

    fn create_err(&self, rc: NvCvStatus) -> BackendError {
        let mut message = format!(
            "NvVFX_CreateEffect(VideoFrameGeneration) returned {rc} ({})",
            self.sdk.error_string(rc)
        );
        if rc == ffi::NVCV_ERR_UNIMPLEMENTED {
            message.push_str(&format!(
                ": the SDK could not open the video frame generation feature ({LIBRARY}); \
                 install it into the SDK root's features/nvvfxvideoframegeneration \
                 (docs/setup/maxine.md, section 4)"
            ));
        }
        BackendError::Load { code: rc, message }
    }

    fn build(
        &self,
        width: u32,
        height: u32,
        colour: ColourSpec,
        stream: CuStream,
    ) -> Result<Loaded, BackendError> {
        let sdk = self.sdk;
        let dir = self.model_dir.as_path();
        let effect = Effect::create(sdk, FX_VIDEO_FRAME_GENERATION, dir).map_err(|e| match e {
            BackendError::Load { code, .. } => self.create_err(code),
            other => other,
        })?;
        effect.set_stream(stream, dir)?;
        effect.set_u32(P_INPUT_WIDTH, width, dir)?;
        effect.set_u32(P_INPUT_HEIGHT, height, dir)?;
        effect.set_u32(P_MODE, MODE_MEDIUM, dir)?;
        effect.set_u32(P_AUTOMATIC_SHOT_CHANGE, 1, dir)?;
        let mut prev = GpuImage::alloc(sdk, width, height, NVCV_RGBA)?;
        let mut cur = GpuImage::alloc(sdk, width, height, NVCV_RGBA)?;
        let mut out = GpuImage::alloc(sdk, width, height, NVCV_RGBA)?;
        effect.set_image(ffi::P_INPUT_IMAGE_0, &mut prev.image, dir)?;
        effect.set_image(P_INPUT_IMAGE_1, &mut cur.image, dir)?;
        effect.set_image(ffi::P_OUTPUT_IMAGE_0, &mut out.image, dir)?;
        effect.load(dir)?;
        let rc = effect.set_f32(P_TIMESTEP, TIMESTEP_MIDWAY);
        effect.check("NvVFX_SetF32 Timestep", rc, dir)?;
        let held = DeviceNv12Buffer::alloc(width, height).map_err(|e| BackendError::Load {
            code: ffi::NVCV_ERR_GENERAL,
            message: format!("could not allocate the held frame: {e}"),
        })?;
        Ok(Loaded {
            effect,
            prev,
            cur,
            out,
            held,
            staging_in: Box::new(NvCvImage::empty()),
            staging_out: Box::new(NvCvImage::empty()),
            width,
            height,
            flags: colour_flags(colour),
            stream,
        })
    }

    fn loaded_for(&mut self, gpu: &GpuContext) -> Result<&mut Loaded, BackendError> {
        let l = self.loaded.as_mut().ok_or_else(|| {
            BackendError::InvalidInput("frame generation called before a successful load".into())
        })?;
        if gpu.stream != l.stream {
            return Err(BackendError::InvalidInput(
                "frame generation must use the stream given to load()".into(),
            ));
        }
        Ok(l)
    }

    fn check_frame(l: &Loaded, f: &DeviceNv12) -> Result<(), BackendError> {
        f.validate()?;
        if (f.width, f.height) != (l.width, l.height) {
            return Err(BackendError::InvalidInput(format!(
                "a {}x{} frame for frame generation loaded for {}x{}",
                f.width, f.height, l.width, l.height
            )));
        }
        Ok(())
    }

    fn run_err(sdk: &Sdk, call: &str, rc: NvCvStatus) -> BackendError {
        BackendError::Run {
            code: rc,
            message: format!("{call} returned {rc} ({})", sdk.error_string(rc)),
        }
    }

    /// Takes `frame` as the current frame: the current one becomes the
    /// previous, and `frame` is kept as it is (for [`Self::write_held`])
    /// and in RGBA (for [`Self::interpolate`]). Only reads `frame`.
    pub fn push(&mut self, gpu: &GpuContext, frame: &DeviceNv12) -> Result<(), BackendError> {
        let sdk = self.sdk;
        let l = self.loaded_for(gpu)?;
        Self::check_frame(l, frame)?;
        let stream = l.stream;
        // SAFETY: `frame` is live NV12 device memory of the current context
        // (the caller's contract), read only; the held buffer is ours, of
        // the same size.
        unsafe { obc_gpu::copy_nv12_async(frame, &l.held.frame(), stream) }?;
        // The bindings stay; the pixels move: SrcImage0 gets SrcImage1's.
        let (cur, prev) = (&*l.cur.image, &*l.prev.image);
        // SAFETY: both images are ours, allocated by the SDK on the GPU of
        // the current context with the same size and format (RGBA, 4 bytes
        // a pixel), each `pitch` bytes a row; the copy is ordered on the
        // stream.
        unsafe {
            obc_gpu::copy_2d_async(
                cur.pixels as u64,
                cur.pitch as usize,
                prev.pixels as u64,
                prev.pitch as usize,
                l.width as usize * 4,
                l.height as usize,
                stream,
            )
        }?;
        // SAFETY: as above for `frame`; U and V interleave (V one byte after
        // U). The destination and staging images are ours.
        let rc = unsafe {
            (sdk.NvCVImage_TransferFromYUV)(
                frame.y as *const c_void,
                1,
                frame.y_pitch as i32,
                frame.uv as *const c_void,
                (frame.uv + 1) as *const c_void,
                2,
                frame.uv_pitch as i32,
                ffi::NVCV_YUV420,
                ffi::NVCV_U8,
                l.flags,
                ffi::NVCV_GPU,
                &mut *l.cur.image,
                std::ptr::null(),
                1.0,
                stream,
                &mut *l.staging_in,
            )
        };
        if rc != ffi::NVCV_SUCCESS {
            return Err(Self::run_err(sdk, "NvCVImage_TransferFromYUV", rc));
        }
        Ok(())
    }

    /// Writes the frame midway between the previous and the current frame
    /// into `dst`. Meaningful once two frames were pushed.
    pub fn interpolate(&mut self, gpu: &GpuContext, dst: &DeviceNv12) -> Result<(), BackendError> {
        let sdk = self.sdk;
        let l = self.loaded_for(gpu)?;
        Self::check_frame(l, dst)?;
        let stream = l.stream;
        let rc = l.effect.run(true);
        if rc != ffi::NVCV_SUCCESS {
            return Err(Self::run_err(sdk, "NvVFX_Run(VideoFrameGeneration)", rc));
        }
        // SAFETY: `dst` is live NV12 device memory of the current context,
        // of the loaded size; U and V interleave. The source and staging
        // images are ours.
        let rc = unsafe {
            (sdk.NvCVImage_TransferToYUV)(
                &*l.out.image,
                std::ptr::null(),
                dst.y as *const c_void,
                1,
                dst.y_pitch as i32,
                dst.uv as *const c_void,
                (dst.uv + 1) as *const c_void,
                2,
                dst.uv_pitch as i32,
                ffi::NVCV_YUV420,
                ffi::NVCV_U8,
                l.flags,
                ffi::NVCV_GPU,
                1.0,
                stream,
                &mut *l.staging_out,
            )
        };
        if rc != ffi::NVCV_SUCCESS {
            return Err(Self::run_err(sdk, "NvCVImage_TransferToYUV", rc));
        }
        Ok(())
    }

    /// Writes the current frame, as it was pushed, into `dst`.
    pub fn write_held(&mut self, gpu: &GpuContext, dst: &DeviceNv12) -> Result<(), BackendError> {
        let l = self.loaded_for(gpu)?;
        Self::check_frame(l, dst)?;
        // SAFETY: our buffer and `dst`, live NV12 device memory of the
        // current context of the same size (the caller's contract).
        unsafe { obc_gpu::copy_nv12_async(&l.held.frame(), dst, l.stream) }?;
        Ok(())
    }

    /// Releases the effect and its images. Idempotent.
    pub fn unload(&mut self) {
        if let Some(l) = self.loaded.take() {
            // Nothing queued may still use the images when they are freed.
            let _ = sync(l.stream);
            drop(l);
        }
    }
}

impl Drop for MaxineVfg {
    fn drop(&mut self) {
        self.unload();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_root_is_unavailable_with_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = MaxineVfgFactory::with_root(Some(dir.path().to_path_buf()));
        // Names that cannot exist, so the search-path fallback fails too.
        f.sdk.names = ffi::LibNames {
            videofx: "libobc-test-no-such-videofx.so",
            nvcvimage: "libobc-test-no-such-nvcvimage.so",
        };
        assert_eq!(f.root(), Some(dir.path()));
        assert!(matches!(f.probe(), Err(BackendError::Unavailable(_))));
        assert!(matches!(f.create_vfg(), Err(BackendError::Unavailable(_))));
    }
}
