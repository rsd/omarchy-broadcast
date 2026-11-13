//! The HDR look on Maxine: the `TrueHDR` effect, then a tone map back to
//! SDR, so an 8-bit output shows what the HDR pass changed.
//!
//! Per frame, all on the GPU, with no host copy
//! ([`MaxineTrueHdr::process`]):
//!
//! 1. the NV12 frame to RGBA (`NvCVImage_TransferFromYUV`, matrix and range
//!    from the load, as the other stages convert), on the caller's stream;
//! 2. `TrueHDR`, at the SDK's default parameters, from that image into an
//!    RGB10A2 image (PQ transfer, BT.2020 primaries, full range), which stays allocated and owned here
//!    ([`MaxineTrueHdr::hdr_image`]);
//! 3. the tone map of that image to RGBA8 (the [`obc_gpu::PqToSdr`]
//!    kernel, [`obc_gpu::tonemap`]) on the caller's stream: the mid-tones
//!    at their SDR level, SDR white and the highlights rolled off towards
//!    255, BT.709, the sRGB encode;
//! 4. RGBA to NV12 into the destination (`NvCVImage_TransferToYUV`), with
//!    the same matrix and range, on the caller's stream.
//!
//! Run asynchronously on a non-blocking stream such as the frame stream,
//! the effect gave a frame the look of the frame before it: its work is not ordered with such a
//! stream, as the relight effect's is not (crate documentation, step 5).
//! So the effect is bound to a blocking stream of its own
//! (`Loaded::blocking`), as NVIDIA's samples run it, and runs synchronously
//! between two waits: for the caller's stream after step 1, and for its own
//! stream after step 2. Each output is the look of its own input, whatever
//! came before.
//!
//! The effect has no temporal state: its header exposes no state object
//! (`NvVFX_AllocateState` does not apply) and no parameter but the five of
//! the look (contrast, saturation, middle grey, peak luminance, debander),
//! so there is nothing to reset on a new source or a switch, and a loaded
//! effect is kept across switches that keep its geometry.
//!
//! The effect ships no model files and takes no model directory (setting
//! one is refused as unimplemented, so none is set): the feature library is
//! the whole feature ([`Feature::TrueHdr`](crate::Feature)). The source
//! peak the tone map expects is the effect's `Luminance`, read back after
//! the load. The SDK is found and opened exactly as for the effects
//! ([`MaxineFactory`]): one SDK per process, loaded from the same root.

use std::ffi::{CStr, c_void};
use std::path::{Path, PathBuf};
use std::time::Instant;

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use obc_backend::{BackendError, ColourSpec, DeviceNv12, GpuContext, LoadReport};
use obc_gpu::tonemap::{DIFFUSE_GAIN, ToneMap};
use obc_gpu::{CuStream, PqToSdr};

use crate::ffi::{self, NvCvImage, NvCvStatus, Sdk};
use crate::{
    Effect, GpuImage, MaxineBackend, MaxineFactory, colour_flags, cuda_load_err, ms_since, sync,
};

/// The effect selector.
const FX_TRUE_HDR: &CStr = c"TrueHDR";
/// The target peak luminance of the HDR output, in nits (`unsigned int`,
/// 400 to 2000).
const P_LUMINANCE: &CStr = c"Luminance";
/// The SDK's default [`P_LUMINANCE`], used when it cannot be read back.
pub const DEFAULT_LUMINANCE: u32 = 650;
/// `NvCVImage_PixelFormat` RGBA.
const NVCV_RGBA: i32 = 6;
/// The feature library, for error messages.
const LIBRARY: &str = "libnvVFXTrueHDR.so";

/// A failure a test can force on the HDR look, as [`crate::RelightFault`]
/// does on the relight stage: only Rust code can set one
/// ([`TrueHdrFaultSwitch`]).
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrueHdrFault {
    /// Frames report a failed `TrueHDR` run instead of calling the SDK, so
    /// `process` fails.
    Run,
}

/// The fault every HDR look of a factory reads, shared like
/// [`crate::RelightFaultSwitch`]. Disarmed by default.
#[doc(hidden)]
#[derive(Debug, Clone, Default)]
pub struct TrueHdrFaultSwitch(Arc<AtomicU8>);

impl TrueHdrFaultSwitch {
    /// Arms `fault` (`None`: disarms).
    pub fn set(&self, fault: Option<TrueHdrFault>) {
        let v = match fault {
            None => 0,
            Some(TrueHdrFault::Run) => 1,
        };
        self.0.store(v, Ordering::SeqCst);
    }

    /// The armed fault.
    pub fn get(&self) -> Option<TrueHdrFault> {
        match self.0.load(Ordering::Relaxed) {
            1 => Some(TrueHdrFault::Run),
            _ => None,
        }
    }
}

/// Finds the Maxine SDK, as [`MaxineFactory`] does, and creates
/// [`MaxineTrueHdr`] instances.
#[derive(Debug, Clone)]
pub struct MaxineTrueHdrFactory {
    sdk: MaxineFactory,
    faults: TrueHdrFaultSwitch,
}

impl MaxineTrueHdrFactory {
    /// Uses the SDK root in force now, as [`MaxineFactory::from_env`].
    pub fn from_env() -> MaxineTrueHdrFactory {
        MaxineTrueHdrFactory::with_root(crate::effective_root())
    }

    /// Uses `root` as the SDK root (`None`: the library search path only),
    /// as [`MaxineFactory::with_root`].
    pub fn with_root(root: Option<PathBuf>) -> MaxineTrueHdrFactory {
        MaxineTrueHdrFactory {
            sdk: MaxineFactory::with_root(root),
            faults: TrueHdrFaultSwitch::default(),
        }
    }

    /// The factory with `switch` as the fault every HDR look it creates
    /// reads. For tests.
    #[doc(hidden)]
    pub fn with_faults(mut self, switch: TrueHdrFaultSwitch) -> MaxineTrueHdrFactory {
        self.faults = switch;
        self
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

    /// Creates an unloaded [`MaxineTrueHdr`].
    pub fn create_truehdr(&self) -> Result<MaxineTrueHdr, BackendError> {
        let sdk = self.sdk.sdk()?;
        Ok(MaxineTrueHdr {
            sdk,
            // Only for error messages: the effect takes no model directory.
            model_dir: sdk.model_dir.clone().unwrap_or_default(),
            loaded: None,
            faults: self.faults.clone(),
        })
    }
}

/// The HDR image of the last frame [`MaxineTrueHdr::process`] ran:
/// `height` rows of `width` RGB10A2 words (R in bits 0-9, G in 10-19, B in
/// 20-29, A in 30-31; PQ, BT.2020, full range), `pitch` bytes apart, in
/// device memory of the load's context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceRgb10a2 {
    /// The device address of the first row.
    pub ptr: u64,
    /// The distance between rows, in bytes.
    pub pitch: usize,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// Everything loaded for one geometry. Field order is drop order: the
/// effect, which holds the images, goes first, and its stream last.
struct Loaded {
    effect: Effect,
    /// The frame in RGBA: the effect's source.
    rgba: GpuImage,
    /// The effect's RGB10A2 output.
    hdr: GpuImage,
    /// The tone-mapped frame in RGBA.
    sdr: GpuImage,
    kernel: PqToSdr,
    /// The SDK's scratch images for the two transfers.
    staging_in: Box<NvCvImage>,
    staging_out: Box<NvCvImage>,
    /// The tone map, with the effect's `Luminance` as the source peak.
    tm: ToneMap,
    width: u32,
    height: u32,
    flags: u32,
    /// The caller's stream (the frame stream).
    stream: CuStream,
    /// The effect's own blocking stream (see the module documentation).
    blocking: obc_gpu::Stream,
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

/// The HDR look for one stream. Its calls follow the context rules of
/// [`obc_backend`]: the context of the load is current on the calling
/// thread, and the work goes on the stream of the load.
pub struct MaxineTrueHdr {
    sdk: &'static Sdk,
    model_dir: PathBuf,
    loaded: Option<Loaded>,
    faults: TrueHdrFaultSwitch,
}

// SAFETY: as for `MaxineBackend`: the SDK handles and CUDA objects are used
// with the load's context pushed on whichever thread calls, and all access
// goes through `&mut self` (or `&self` for the queries).
unsafe impl Send for MaxineTrueHdr {}

impl MaxineTrueHdr {
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

    /// The source peak the tone map uses (the effect's `Luminance`), in
    /// nits, once loaded.
    pub fn peak_luminance(&self) -> Option<f32> {
        self.loaded.as_ref().map(|l| l.tm.peak())
    }

    /// The HDR image the effect writes, once loaded. It holds the effect's
    /// output for the last frame [`Self::process`] ran (complete when
    /// `process` returns), and is valid until [`Self::unload`] or the next
    /// load.
    pub fn hdr_image(&self) -> Option<DeviceRgb10a2> {
        self.loaded.as_ref().map(|l| DeviceRgb10a2 {
            ptr: l.hdr.ptr(),
            pitch: l.hdr.pitch(),
            width: l.width,
            height: l.height,
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
            notice: None,
        })
    }

    fn create_err(&self, rc: NvCvStatus) -> BackendError {
        BackendError::Load {
            code: rc,
            message: create_message(&self.sdk.error_string(rc), rc),
        }
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
        let blocking = obc_gpu::Stream::new(false)
            .map_err(|e| cuda_load_err(e, "creating the HDR look's stream failed"))?;
        let effect = Effect::create(sdk, FX_TRUE_HDR, dir).map_err(|e| match e {
            BackendError::Load { code, .. } => self.create_err(code),
            other => other,
        })?;
        effect.set_stream(blocking.raw(), dir)?;
        let mut rgba = GpuImage::alloc_tight(sdk, width, height, NVCV_RGBA)?;
        let mut hdr = GpuImage::alloc_rgb10a2_tight(sdk, width, height)?;
        let sdr = GpuImage::alloc_tight(sdk, width, height, NVCV_RGBA)?;
        effect.set_image(ffi::P_INPUT_IMAGE_0, &mut rgba.image, dir)?;
        effect.set_image(ffi::P_OUTPUT_IMAGE_0, &mut hdr.image, dir)?;
        effect.load(dir)?;
        blocking
            .synchronize()
            .map_err(|e| cuda_load_err(e, "the HDR look's load failed on the GPU"))?;
        let peak = effect.get_u32(P_LUMINANCE).unwrap_or(DEFAULT_LUMINANCE) as f32;
        let tm = ToneMap::new(DIFFUSE_GAIN, peak).map_err(|e| BackendError::Load {
            code: ffi::NVCV_ERR_PARAMETER,
            message: format!("the TrueHDR peak luminance does not fit the tone map: {e}"),
        })?;
        let kernel = PqToSdr::load()
            .map_err(|e| cuda_load_err(e, "the HDR tone-map kernel did not load"))?;
        Ok(Loaded {
            effect,
            rgba,
            hdr,
            sdr,
            kernel,
            staging_in: Box::new(NvCvImage::empty()),
            staging_out: Box::new(NvCvImage::empty()),
            tm,
            width,
            height,
            flags: colour_flags(colour),
            stream,
            blocking,
        })
    }

    fn loaded_for(&mut self, gpu: &GpuContext) -> Result<&mut Loaded, BackendError> {
        let l = self.loaded.as_mut().ok_or_else(|| {
            BackendError::InvalidInput("the HDR look called before a successful load".into())
        })?;
        if gpu.stream != l.stream {
            return Err(BackendError::InvalidInput(
                "the HDR look must use the stream given to load()".into(),
            ));
        }
        Ok(l)
    }

    fn check_frame(l: &Loaded, f: &DeviceNv12) -> Result<(), BackendError> {
        f.validate()?;
        if (f.width, f.height) != (l.width, l.height) {
            return Err(BackendError::InvalidInput(format!(
                "a {}x{} frame for the HDR look loaded for {}x{}",
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

    /// Runs the HDR look from `src` into `dst` (both NV12 of the loaded
    /// size; they may be the same frame). Waits for the caller's stream
    /// once, before the effect runs, and for the effect's own stream
    /// before the conversion into `dst` (see the module documentation);
    /// the tone map and that conversion are queued on the caller's stream.
    pub fn process(
        &mut self,
        gpu: &GpuContext,
        src: &DeviceNv12,
        dst: &DeviceNv12,
    ) -> Result<(), BackendError> {
        let sdk = self.sdk;
        let fault = self.faults.get();
        let l = self.loaded_for(gpu)?;
        Self::check_frame(l, src)?;
        Self::check_frame(l, dst)?;
        let stream = l.stream;
        // SAFETY: `src` is live NV12 device memory of the current context
        // (the caller's contract), read only; U and V interleave (V one
        // byte after U). The destination and staging images are ours.
        let rc = unsafe {
            (sdk.NvCVImage_TransferFromYUV)(
                src.y as *const c_void,
                1,
                src.y_pitch as i32,
                src.uv as *const c_void,
                (src.uv + 1) as *const c_void,
                2,
                src.uv_pitch as i32,
                ffi::NVCV_YUV420,
                ffi::NVCV_U8,
                l.flags,
                ffi::NVCV_GPU,
                &mut *l.rgba.image,
                std::ptr::null(),
                1.0,
                stream,
                &mut *l.staging_in,
            )
        };
        if rc != ffi::NVCV_SUCCESS {
            return Err(Self::run_err(sdk, "NvCVImage_TransferFromYUV", rc));
        }
        // The effect's input in place before it runs, whatever stream its
        // work goes on.
        sync(stream)?;
        let rc = match fault {
            Some(TrueHdrFault::Run) => ffi::NVCV_ERR_GENERAL,
            None => l.effect.run(false),
        };
        if rc != ffi::NVCV_SUCCESS {
            return Err(Self::run_err(sdk, "NvVFX_Run(TrueHDR)", rc));
        }
        // Its output complete before the tone map reads it.
        l.blocking.synchronize().map_err(BackendError::from)?;
        // SAFETY: both images are ours, allocated by the SDK on the GPU of
        // the current context with the loaded size, 4 bytes a pixel, each
        // `pitch` bytes a row, distinct; the effect's output is complete
        // (above).
        unsafe {
            l.kernel.launch(
                l.hdr.ptr(),
                l.hdr.pitch(),
                l.sdr.ptr(),
                l.sdr.pitch(),
                l.width,
                l.height,
                l.tm,
                stream,
            )
        }
        .map_err(|e| BackendError::Run {
            code: ffi::NVCV_ERR_GENERAL,
            message: format!("the HDR tone-map kernel did not launch: {e}"),
        })?;
        Self::to_nv12(sdk, l, dst)
    }

    /// The tone-mapped image to NV12 into `dst`.
    fn to_nv12(sdk: &Sdk, l: &mut Loaded, dst: &DeviceNv12) -> Result<(), BackendError> {
        // SAFETY: `dst` is live NV12 device memory of the current context,
        // of the loaded size (checked by the caller); U and V interleave.
        // The source and staging images are ours.
        let rc = unsafe {
            (sdk.NvCVImage_TransferToYUV)(
                &*l.sdr.image,
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
                l.stream,
                &mut *l.staging_out,
            )
        };
        if rc != ffi::NVCV_SUCCESS {
            return Err(Self::run_err(sdk, "NvCVImage_TransferToYUV", rc));
        }
        Ok(())
    }

    /// Releases the effect, its images and the kernel. Idempotent.
    pub fn unload(&mut self) {
        if let Some(l) = self.loaded.take() {
            // Nothing queued may still use the images when they are freed.
            let _ = sync(l.stream);
            let _ = l.blocking.synchronize();
            drop(l);
        }
    }
}

impl Drop for MaxineTrueHdr {
    fn drop(&mut self) {
        self.unload();
    }
}

/// The message of a failed `NvVFX_CreateEffect(TrueHDR)` with status `rc`
/// (`what` its SDK description), naming the feature library and its
/// directory when the SDK could not open it.
fn create_message(what: &str, rc: NvCvStatus) -> String {
    let mut message = format!("NvVFX_CreateEffect(TrueHDR) returned {rc} ({what})");
    if rc == ffi::NVCV_ERR_UNIMPLEMENTED {
        message.push_str(&format!(
            ": the SDK could not open the TrueHDR feature ({LIBRARY}); install it into the SDK \
             root's features/{} (docs/setup/maxine.md, section 4)",
            crate::feature_dir(crate::Feature::TrueHdr)
        ));
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_root_is_unavailable_with_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = MaxineTrueHdrFactory::with_root(Some(dir.path().to_path_buf()));
        // Names that cannot exist, so the search-path fallback fails too.
        f.sdk.names = ffi::LibNames {
            videofx: "libobc-test-no-such-videofx.so",
            nvcvimage: "libobc-test-no-such-nvcvimage.so",
        };
        assert_eq!(f.root(), Some(dir.path()));
        assert!(matches!(f.probe(), Err(BackendError::Unavailable(_))));
        assert!(matches!(
            f.create_truehdr(),
            Err(BackendError::Unavailable(_))
        ));
    }

    #[test]
    fn a_missing_feature_library_is_named_with_its_directory() {
        let m = create_message("unimplemented", ffi::NVCV_ERR_UNIMPLEMENTED);
        assert!(m.contains("libnvVFXTrueHDR.so"), "{m}");
        assert!(m.contains("features/nvvfxtruehdr"), "{m}");
        let m = create_message("general", ffi::NVCV_ERR_GENERAL);
        assert!(!m.contains("features/"), "{m}");
    }
}
