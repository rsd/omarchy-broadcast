//! The enhance stage on Maxine: the `VideoSuperRes` effect (video super
//! resolution) behind [`obc_backend::enhance`].
//!
//! Per frame, all on the GPU and on the caller's stream, with no host copy:
//!
//! 1. the 4:2:0 input (NV12 or I420) to RGBA at the capture size
//!    (`NvCVImage_TransferFromYUV`, matrix, range and chroma siting from the
//!    request);
//! 2. `VideoSuperRes` at the fixed quality level [`QUALITY_HIGH`], from that
//!    image into an RGBA image `scale` times as wide and as high;
//! 3. RGBA to NV12 into the destination (`NvCVImage_TransferToYUV`), with
//!    the same matrix, range and chroma siting.
//!
//! The effect ships no model files and takes no model directory: the
//! feature library is the whole feature
//! ([`Feature::VideoSuperResolution`](obc_backend::Feature)). The SDK is
//! found and opened exactly as for the effects ([`MaxineFactory`]): one SDK
//! per process, loaded from the same root.

use std::cell::Cell;
use std::ffi::{CStr, c_void};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Instant;

use obc_backend::enhance::{
    ChromaSiting, DeviceYuv420, EnhanceRequest, EnhanceTimings, Enhancer, EnhancerFactory,
};
use obc_backend::{BackendError, DeviceNv12, GpuContext, LoadReport};
use obc_gpu::{CuStream, Event};

use crate::ffi::{self, NvCvImage, NvCvStatus, Sdk};
use crate::{
    Effect, FAULT_SELECTOR, GpuImage, MaxineBackend, MaxineFactory, colour_flags, ms_since, record,
    sync,
};

/// The effect selector.
const FX_VIDEO_SUPER_RES: &CStr = c"VideoSuperRes";
/// The quality level parameter (`unsigned int`).
const P_QUALITY_LEVEL: &CStr = c"QualityLevel";
/// The image encoding parameter (`unsigned int`), set before `NvVFX_Load`.
const P_IMAGE_ENCODING_MODE: &CStr = c"ImageEncodingMode";
/// `QualityLevel` "VSR_High": the level the stage always runs at.
pub const QUALITY_HIGH: u32 = 3;
/// `ImageEncodingMode` 8-bit RGBA/BGRA.
const ENCODING_RGB8: u32 = 0;
/// `NvCVImage_PixelFormat` RGBA.
const NVCV_RGBA: i32 = 6;
/// The feature library, for error messages.
const LIBRARY: &str = "libnvVFXVideoSuperRes.so";

/// The NvCV colour-space flags of the request's frames: its matrix and range
/// ([`colour_flags`], whose siting bits are MPEG-2's, all zero) with the
/// chroma siting of the request, so a centre-sited `jpegdec` frame is read
/// and written without a half-pixel chroma shift.
fn transfer_flags(req: &EnhanceRequest) -> u32 {
    let siting = match req.chroma {
        ChromaSiting::Cosited => ffi::NVCV_CHROMA_COSITED,
        ChromaSiting::Interstitial => ffi::NVCV_CHROMA_INTSTITIAL,
        ChromaSiting::TopLeft => ffi::NVCV_CHROMA_TOPLEFT,
    };
    colour_flags(req.colour) | siting
}

/// A failure a test can force on the enhancer. Only Rust code can set one
/// ([`EnhanceFaultSwitch`]): no environment variable, setting or IPC call
/// reaches it.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnhanceFault {
    /// Loads create the effect with a selector the SDK does not know, so
    /// `NvVFX_CreateEffect` fails and so does `load`.
    Load,
    /// Frames report a failed run instead of calling the SDK.
    Run,
}

/// The fault every enhancer of a factory reads, shared by clones.
/// Disarmed by default.
#[doc(hidden)]
#[derive(Debug, Clone, Default)]
pub struct EnhanceFaultSwitch(Arc<AtomicU8>);

impl EnhanceFaultSwitch {
    /// Arms `fault` (`None`: disarms).
    pub fn set(&self, fault: Option<EnhanceFault>) {
        let v = match fault {
            None => 0,
            Some(EnhanceFault::Load) => 1,
            Some(EnhanceFault::Run) => 2,
        };
        self.0.store(v, Ordering::SeqCst);
    }

    /// The armed fault.
    pub fn get(&self) -> Option<EnhanceFault> {
        match self.0.load(Ordering::Relaxed) {
            1 => Some(EnhanceFault::Load),
            2 => Some(EnhanceFault::Run),
            _ => None,
        }
    }
}

/// Finds the Maxine SDK, as [`MaxineFactory`] does, and creates
/// [`MaxineVsr`] instances.
#[derive(Debug, Clone)]
pub struct MaxineVsrFactory {
    sdk: MaxineFactory,
    faults: EnhanceFaultSwitch,
}

impl MaxineVsrFactory {
    /// Uses the SDK root in force now, as [`MaxineFactory::from_env`].
    pub fn from_env() -> MaxineVsrFactory {
        MaxineVsrFactory::with_root(crate::effective_root())
    }

    /// Uses `root` as the SDK root (`None`: the library search path only),
    /// as [`MaxineFactory::with_root`].
    pub fn with_root(root: Option<PathBuf>) -> MaxineVsrFactory {
        MaxineVsrFactory {
            sdk: MaxineFactory::with_root(root),
            faults: EnhanceFaultSwitch::default(),
        }
    }

    /// Makes every enhancer this factory creates read `switch` (tests only).
    #[doc(hidden)]
    pub fn with_faults(mut self, switch: EnhanceFaultSwitch) -> MaxineVsrFactory {
        self.faults = switch;
        self
    }

    /// The SDK root this factory looks in first.
    pub fn root(&self) -> Option<&Path> {
        self.sdk.root()
    }

    /// Creates a concrete, unloaded [`MaxineVsr`].
    pub fn create_vsr(&self) -> Result<MaxineVsr, BackendError> {
        let sdk = self.sdk.sdk()?;
        Ok(MaxineVsr {
            sdk,
            // Only for error messages: the effect takes no model directory.
            model_dir: sdk.model_dir.clone().unwrap_or_default(),
            faults: self.faults.clone(),
            loaded: None,
            last_timings: Cell::new(None),
        })
    }
}

impl EnhancerFactory for MaxineVsrFactory {
    fn name(&self) -> &'static str {
        "maxine-vsr"
    }

    /// Opens the SDK libraries (once per process). Loads no model and
    /// creates no CUDA context.
    fn probe(&self) -> Result<(), BackendError> {
        self.sdk.sdk().map(|_| ())
    }

    fn create(&self) -> Result<Box<dyn Enhancer>, BackendError> {
        Ok(Box::new(self.create_vsr()?))
    }
}

/// Event indices of a frame: before the conversion, after it, after the
/// upscale, after the conversion back.
const EV_START: usize = 0;
const EV_IN: usize = 1;
const EV_UPSCALE: usize = 2;
const EV_OUT: usize = 3;

/// Everything loaded for one request. Field order is drop order: the effect,
/// which holds both images, goes first.
struct Loaded {
    effect: Effect,
    /// The converted input frame (capture size): the effect's source.
    src: GpuImage,
    /// The upscaled frame: the effect's destination.
    dst: GpuImage,
    /// The SDK's scratch images for the two transfers.
    staging_in: Box<NvCvImage>,
    staging_out: Box<NvCvImage>,
    req: EnhanceRequest,
    stream: CuStream,
    events: [Event; 4],
    /// All four events of the last frame were recorded.
    recorded: bool,
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

/// The Maxine implementation of [`Enhancer`]. Follow the context rules of
/// [`obc_backend::enhance`].
pub struct MaxineVsr {
    sdk: &'static Sdk,
    model_dir: PathBuf,
    faults: EnhanceFaultSwitch,
    loaded: Option<Loaded>,
    last_timings: Cell<Option<EnhanceTimings>>,
}

// SAFETY: as for `MaxineBackend`: the SDK handles and CUDA objects are used
// with the request's context pushed on whichever thread calls, and all
// access goes through `&mut self` (or `&self` for the timings).
unsafe impl Send for MaxineVsr {}

impl MaxineVsr {
    /// Whether a request is loaded.
    pub fn is_loaded(&self) -> bool {
        self.loaded.is_some()
    }

    /// A load error for `call` with the SDK's status `rc`, naming the feature
    /// library when the SDK could not open it.
    fn create_err(&self, rc: NvCvStatus) -> BackendError {
        let mut message = format!(
            "NvVFX_CreateEffect(VideoSuperRes) returned {rc} ({})",
            self.sdk.error_string(rc)
        );
        if rc == ffi::NVCV_ERR_UNIMPLEMENTED {
            message.push_str(&format!(
                ": the SDK could not open the video super resolution feature ({LIBRARY}); \
                 install it into the SDK root's features/nvvfxvideosuperres (docs/setup/maxine.md, \
                 section 4)"
            ));
        }
        BackendError::Load { code: rc, message }
    }

    fn build(&self, req: &EnhanceRequest, stream: CuStream) -> Result<Loaded, BackendError> {
        let sdk = self.sdk;
        let dir = self.model_dir.as_path();
        let selector = match self.faults.get() {
            Some(EnhanceFault::Load) => FAULT_SELECTOR,
            _ => FX_VIDEO_SUPER_RES,
        };
        let effect = Effect::create(sdk, selector, dir).map_err(|e| match e {
            BackendError::Load { code, .. } => self.create_err(code),
            other => other,
        })?;
        effect.set_stream(stream, dir)?;
        effect.set_u32(P_QUALITY_LEVEL, QUALITY_HIGH, dir)?;
        effect.set_u32(P_IMAGE_ENCODING_MODE, ENCODING_RGB8, dir)?;
        let (ow, oh) = req.output_size();
        let mut src = GpuImage::alloc(sdk, req.width, req.height, NVCV_RGBA)?;
        let mut dst = GpuImage::alloc(sdk, ow, oh, NVCV_RGBA)?;
        effect.set_image(ffi::P_INPUT_IMAGE_0, &mut src.image, dir)?;
        effect.set_image(ffi::P_OUTPUT_IMAGE_0, &mut dst.image, dir)?;
        effect.load(dir)?;
        Ok(Loaded {
            effect,
            src,
            dst,
            staging_in: Box::new(NvCvImage::empty()),
            staging_out: Box::new(NvCvImage::empty()),
            req: *req,
            stream,
            events: [Event::new()?, Event::new()?, Event::new()?, Event::new()?],
            recorded: false,
        })
    }
}

/// Checks a frame against the loaded size.
fn check_size(what: &str, got: (u32, u32), want: (u32, u32)) -> Result<(), BackendError> {
    if got != want {
        return Err(BackendError::InvalidInput(format!(
            "the enhance {what} frame is {}x{}, the enhancer is loaded for {}x{}",
            got.0, got.1, want.0, want.1
        )));
    }
    Ok(())
}

impl Enhancer for MaxineVsr {
    fn name(&self) -> &'static str {
        "maxine-vsr"
    }

    fn load(&mut self, gpu: &GpuContext, req: &EnhanceRequest) -> Result<LoadReport, BackendError> {
        MaxineBackend::check_context(gpu)?;
        req.validate()?;
        self.unload();
        let t = Instant::now();
        let loaded = self.build(req, gpu.stream)?;
        // Surface an asynchronous fault of the load here, not on a frame.
        sync(gpu.stream)?;
        self.loaded = Some(loaded);
        Ok(LoadReport {
            load_ms: ms_since(t),
            warmup_ms: None,
            reused_model: false,
        })
    }

    fn process(
        &mut self,
        gpu: &GpuContext,
        src: &DeviceYuv420,
        dst: &DeviceNv12,
    ) -> Result<(), BackendError> {
        let sdk = self.sdk;
        let fault = self.faults.get() == Some(EnhanceFault::Run);
        let l = self.loaded.as_mut().ok_or_else(|| {
            BackendError::InvalidInput("process() called before a successful load()".into())
        })?;
        if gpu.stream != l.stream {
            return Err(BackendError::InvalidInput(
                "process() must use the stream given to load()".into(),
            ));
        }
        src.validate()?;
        dst.validate()?;
        if src.layout != l.req.layout {
            return Err(BackendError::InvalidInput(format!(
                "the enhance input is {}, the enhancer is loaded for {}",
                src.layout.as_str(),
                l.req.layout.as_str()
            )));
        }
        if src.u_pitch != src.v_pitch {
            return Err(BackendError::InvalidInput(format!(
                "the enhance input's U and V pitches differ ({} and {})",
                src.u_pitch, src.v_pitch
            )));
        }
        check_size(
            "input",
            (src.width, src.height),
            (l.req.width, l.req.height),
        )?;
        check_size("output", (dst.width, dst.height), l.req.output_size())?;
        let run_err = |call: &str, rc: NvCvStatus| BackendError::Run {
            code: rc,
            message: format!("{call} returned {rc} ({})", sdk.error_string(rc)),
        };
        if fault {
            return Err(run_err(
                "NvVFX_Run(VideoSuperRes) [forced fault]",
                ffi::NVCV_ERR_GENERAL,
            ));
        }
        let stream = l.stream;
        let flags = transfer_flags(&l.req);
        l.recorded = false;
        record(&l.events[EV_START], stream)?;
        // SAFETY: `src` describes live 4:2:0 device memory of the current
        // context (caller's contract), read only, with one chroma pitch;
        // the destination and staging images are ours.
        let rc = unsafe {
            (sdk.NvCVImage_TransferFromYUV)(
                src.y as *const c_void,
                1,
                src.y_pitch as i32,
                src.u as *const c_void,
                src.v as *const c_void,
                src.chroma_step() as i32,
                src.u_pitch as i32,
                ffi::NVCV_YUV420,
                ffi::NVCV_U8,
                flags,
                ffi::NVCV_GPU,
                &mut *l.src.image,
                std::ptr::null(),
                1.0,
                stream,
                &mut *l.staging_in,
            )
        };
        if rc != ffi::NVCV_SUCCESS {
            return Err(run_err("NvCVImage_TransferFromYUV", rc));
        }
        record(&l.events[EV_IN], stream)?;
        let rc = l.effect.run(true);
        if rc != ffi::NVCV_SUCCESS {
            return Err(run_err("NvVFX_Run(VideoSuperRes)", rc));
        }
        record(&l.events[EV_UPSCALE], stream)?;
        // SAFETY: `dst` describes live NV12 device memory of the current
        // context, of the output size; U and V interleave (V one byte
        // after U). The source and staging images are ours.
        let rc = unsafe {
            (sdk.NvCVImage_TransferToYUV)(
                &*l.dst.image,
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
                flags,
                ffi::NVCV_GPU,
                1.0,
                stream,
                &mut *l.staging_out,
            )
        };
        if rc != ffi::NVCV_SUCCESS {
            return Err(run_err("NvCVImage_TransferToYUV", rc));
        }
        record(&l.events[EV_OUT], stream)?;
        l.recorded = true;
        Ok(())
    }

    fn unload(&mut self) {
        if let Some(l) = self.loaded.take() {
            // Nothing queued may still use the images when they are freed.
            let _ = sync(l.stream);
            drop(l);
        }
        self.last_timings.set(None);
    }

    fn timings(&self) -> Option<EnhanceTimings> {
        let l = self.loaded.as_ref()?;
        if l.recorded && l.events[EV_OUT].is_complete().unwrap_or(false) {
            let e = &l.events;
            let span = |a: usize, b: usize| Event::elapsed_ms(&e[a], &e[b]).ok();
            if let (Some(i), Some(u), Some(o)) = (
                span(EV_START, EV_IN),
                span(EV_IN, EV_UPSCALE),
                span(EV_UPSCALE, EV_OUT),
            ) {
                self.last_timings.set(Some(EnhanceTimings {
                    convert_in_ms: i,
                    upscale_ms: u,
                    convert_out_ms: o,
                }));
            }
        }
        self.last_timings.get()
    }
}

impl Drop for MaxineVsr {
    fn drop(&mut self) {
        self.unload();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn faults_are_off_unless_armed_from_rust_and_shared_by_clones() {
        let s = EnhanceFaultSwitch::default();
        assert_eq!(s.get(), None);
        let f = MaxineVsrFactory::with_root(None).with_faults(s.clone());
        s.set(Some(EnhanceFault::Load));
        assert_eq!(f.clone().faults.get(), Some(EnhanceFault::Load));
        s.set(Some(EnhanceFault::Run));
        assert_eq!(f.faults.get(), Some(EnhanceFault::Run));
        s.set(None);
        assert_eq!(f.faults.get(), None);
        assert_eq!(MaxineVsrFactory::from_env().faults.get(), None);
    }

    #[test]
    fn the_transfer_flags_carry_the_chroma_siting_of_the_request() {
        use obc_backend::enhance::YuvLayout;
        use obc_backend::{ColourMatrix, ColourSpec};
        let req = EnhanceRequest {
            width: 640,
            height: 360,
            scale: 3,
            layout: YuvLayout::I420,
            colour: ColourSpec::FULL_BT601,
            chroma: ChromaSiting::Interstitial,
        };
        // jpegdec: full-range BT.601, centre-sited.
        assert_eq!(transfer_flags(&req), 0x04 | 0x08);
        let cosited = EnhanceRequest {
            chroma: ChromaSiting::Cosited,
            colour: ColourSpec::LIMITED_BT601,
            ..req
        };
        assert_eq!(transfer_flags(&cosited), 0x00);
        let top_left = EnhanceRequest {
            chroma: ChromaSiting::TopLeft,
            colour: ColourSpec {
                matrix: ColourMatrix::Bt709,
                full_range: false,
            },
            ..req
        };
        assert_eq!(transfer_flags(&top_left), 0x01 | 0x10);
    }

    #[test]
    fn an_empty_root_is_unavailable_with_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = MaxineVsrFactory::with_root(Some(dir.path().to_path_buf()));
        // Names that cannot exist, so the search-path fallback fails too.
        f.sdk.names = ffi::LibNames {
            videofx: "libobc-test-no-such-videofx.so",
            nvcvimage: "libobc-test-no-such-nvcvimage.so",
        };
        assert_eq!(f.name(), "maxine-vsr");
        assert_eq!(f.root(), Some(dir.path()));
        let e = f.probe().unwrap_err();
        assert!(matches!(e, BackendError::Unavailable(_)), "{e:?}");
        assert!(matches!(f.create(), Err(BackendError::Unavailable(_))));
    }
}
