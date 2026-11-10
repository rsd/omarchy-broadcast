//! NVIDIA Maxine Video Effects backend
//! (`docs/adr/0012-video-effects-and-maxine-backend.md`).
//!
//! The user-installed SDK is opened at run time with `dlopen`, never linked
//! and never vendored. [`MaxineFactory`] finds it (`$MAXINE_VFX_ROOT/lib`;
//! with the variable unset, [`DEFAULT_ROOT`]`/lib` when that directory
//! exists; else the library search path) and reports what to do when it
//! cannot;
//! [`MaxineBackend`] implements [`InferenceBackend`] for `blur` and
//! `replace`, each with or without the denoise stage, and for the denoise
//! stage alone (`none` with [`LoadRequest::denoise`]).
//!
//! Per frame, all on the GPU and on the caller's stream, with no host copy:
//!
//! 1. NV12 to BGR (`NvCVImage_TransferFromYUV`, matrix and range from the
//!    request's [`ColourSpec`]): into the frame image, or, with the denoise
//!    stage, into the stage's own input image;
//! 2. with the denoise stage: the `Denoising` effect (temporal state, the
//!    strength fixed when the factory is made) from that image into the
//!    frame image, so every later stage reads the denoised frame;
//! 3. the `GreenScreen` effect (quality mode, temporal state, CUDA graph)
//!    makes an 8-bit alpha mask;
//! 4. a CUDA kernel ([`obc_gpu::Lut8`]) hardens the mask through a fixed
//!    table into a second mask ([`matte`]): the raw mask lets a faint copy
//!    of the room through, so nothing but the hardened mask is ever used;
//! 5. `blur`: the `BackgroundBlur` effect on frame and hardened mask;
//!    `replace`: `NvCVImage_Composite` of the frame over the background
//!    through the hardened mask;
//! 6. BGR to NV12 into the destination (`NvCVImage_TransferToYUV`).
//!
//! Without the denoise stage step 2 does not exist: the conversion writes
//! the frame image directly, as before the stage was added. With `none`
//! steps 3 to 5 do not exist: the denoised frame goes straight back to
//! NV12, and no green-screen model is loaded.
//!
//! Failing to set the kernel up fails the load ([`BackendError::Load`]);
//! failing to launch it fails the frame ([`BackendError::Run`]). Either way
//! the caller shows its failure output; no path falls back to the raw mask.
//! The denoise stage alone failing is [`BackendError::Denoise`]: a load
//! leaves nothing of the request in force, a frame drops the stage.
//!
//! The process needs the SDK's library directories on `LD_LIBRARY_PATH` and,
//! on a distribution the SDK does not list, the os-release override
//! (`docs/setup/maxine.md`, sections 3 and 4).

use std::cell::Cell;
use std::ffi::{CString, c_void};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU16, Ordering};
use std::time::Instant;

use obc_backend::{
    BackendError, BackendFactory, ColourMatrix, ColourSpec, DeviceNv12, EffectKind, FrameParams,
    GpuContext, HostBgr, InferenceBackend, LoadReport, LoadRequest, StageTimings,
};
use obc_gpu::{CuStream, DeviceBuffer, Event, GpuError, LUT8_BLOCK, Lut8};

mod ffi;
pub mod matte;

use ffi::{NvCvImage, NvCvStatus, Sdk, StateHandle, VfxHandle};

/// The environment variable that names the SDK root (the directory with
/// `lib/`, `features/` and `external/`).
pub const ROOT_ENV: &str = "MAXINE_VFX_ROOT";

/// The SDK root used when [`ROOT_ENV`] is unset or empty and this directory
/// exists: where the `maxine-vfx-sdk-local` package (built from
/// omarchy-nvidia-sdks) installs the SDK tree.
pub const DEFAULT_ROOT: &str = "/opt/nvidia/maxine/videofx";

/// The smallest frame the green-screen effect accepts.
pub const MIN_WIDTH: u32 = 512;
/// The smallest frame the green-screen effect accepts.
pub const MIN_HEIGHT: u32 = 288;
/// The smallest frame the denoise stage accepts ("80p" in the SDK's
/// documentation; narrower frames fail its model load).
pub const DENOISE_MIN_WIDTH: u32 = 80;
/// See [`DENOISE_MIN_WIDTH`].
pub const DENOISE_MIN_HEIGHT: u32 = 80;
/// The largest frame the denoise stage accepts ("1080p"): wider or taller
/// frames, portrait 1080x1920 included, fail its model load.
pub const DENOISE_MAX_WIDTH: u32 = 1920;
/// See [`DENOISE_MAX_WIDTH`].
pub const DENOISE_MAX_HEIGHT: u32 = 1080;

/// The denoise strength a factory uses unless told otherwise: 1, the SDK's
/// strong setting (more noise removed), rather than 0, its weak one (more
/// texture kept). See [`MaxineFactory::with_denoise_strength`].
pub const DEFAULT_DENOISE_STRENGTH: f32 = 1.0;

/// A development knob read by [`MaxineFactory::from_env`] only: `0` or `1`
/// overrides [`DEFAULT_DENOISE_STRENGTH`], so a benchmark or a live look can
/// compare the two without a rebuild. Not a user setting.
pub const DENOISE_STRENGTH_ENV: &str = "OBC_MAXINE_DENOISE_STRENGTH";

/// A failure a test can force on the mask-hardening kernel, through a real
/// CUDA driver error, or a table it can swap in. Only Rust code can set one
/// ([`MaskFaultSwitch`]): no environment variable, setting or IPC call
/// reaches it.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskFault {
    /// Cold loads load garbage instead of the kernel's PTX, so
    /// `cuModuleLoadData` fails and so does `load`.
    ModuleLoad,
    /// Frame launches (not the warm-up launch inside `load`) use a thread
    /// block larger than any GPU allows, so `cuLaunchKernel` fails and so
    /// does `process`.
    Launch,
    /// Cold loads upload a table that maps every raw mask value to this
    /// byte instead of [`matte::HARDENING_LUT`], so the hardened mask is
    /// constant whatever the model sees: a test can then tell whether an
    /// effect reads the hardened mask or the raw one.
    ConstantTable(u8),
}

/// The fault every backend of a factory reads: shared, so a test can arm
/// it after a load. Disarmed by default.
#[doc(hidden)]
#[derive(Debug, Clone, Default)]
pub struct MaskFaultSwitch(Arc<AtomicU16>);

/// The tag of [`MaskFault::ConstantTable`] in a [`MaskFaultSwitch`]; the
/// table byte is the low 8 bits.
const CONSTANT_TABLE_TAG: u16 = 0x100;

impl MaskFaultSwitch {
    /// Arms `fault` (`None`: disarms).
    pub fn set(&self, fault: Option<MaskFault>) {
        let v = match fault {
            None => 0,
            Some(MaskFault::ModuleLoad) => 1,
            Some(MaskFault::Launch) => 2,
            Some(MaskFault::ConstantTable(b)) => CONSTANT_TABLE_TAG | u16::from(b),
        };
        self.0.store(v, Ordering::SeqCst);
    }

    /// The armed fault. Relaxed: the switch publishes no other data, and a
    /// frame that reads the old value just before a change is fine.
    pub fn get(&self) -> Option<MaskFault> {
        match self.0.load(Ordering::Relaxed) {
            1 => Some(MaskFault::ModuleLoad),
            2 => Some(MaskFault::Launch),
            v if v & CONSTANT_TABLE_TAG != 0 => Some(MaskFault::ConstantTable(v as u8)),
            _ => None,
        }
    }
}

/// What [`MaskFault::ModuleLoad`] hands the driver instead of the PTX.
const FAULT_PTX: &std::ffi::CStr = c"forced fault: this is not PTX";
/// What [`MaskFault::Launch`] launches with: 2048 threads, above the 1024
/// every CUDA GPU allows per block.
const FAULT_BLOCK: (u32, u32) = (64, 32);

/// A failure a test can force on the denoise stage. Only Rust code can set
/// one ([`DenoiseFaultSwitch`]): no environment variable, setting or IPC
/// call reaches it.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenoiseFault {
    /// Loads of the stage create the effect with a selector the SDK does
    /// not know, so `NvVFX_CreateEffect` fails and so does `load`.
    Load,
    /// Frames (not the warm-up run inside `load`) report a failed denoise
    /// run instead of calling the SDK, so `process` fails and drops the
    /// stage.
    Run,
}

/// The denoise fault every backend of a factory reads, shared like
/// [`MaskFaultSwitch`]. Disarmed by default.
#[doc(hidden)]
#[derive(Debug, Clone, Default)]
pub struct DenoiseFaultSwitch(Arc<AtomicU8>);

impl DenoiseFaultSwitch {
    /// Arms `fault` (`None`: disarms).
    pub fn set(&self, fault: Option<DenoiseFault>) {
        let v = match fault {
            None => 0,
            Some(DenoiseFault::Load) => 1,
            Some(DenoiseFault::Run) => 2,
        };
        self.0.store(v, Ordering::SeqCst);
    }

    /// The armed fault (relaxed, as [`MaskFaultSwitch::get`]).
    pub fn get(&self) -> Option<DenoiseFault> {
        match self.0.load(Ordering::Relaxed) {
            1 => Some(DenoiseFault::Load),
            2 => Some(DenoiseFault::Run),
            _ => None,
        }
    }
}

/// What [`DenoiseFault::Load`] creates instead of the denoise effect.
const FAULT_SELECTOR: &std::ffi::CStr = c"ForcedFaultNoSuchEffect";

/// Finds the Maxine SDK and creates [`MaxineBackend`] instances.
#[derive(Debug, Clone)]
pub struct MaxineFactory {
    root: Option<PathBuf>,
    names: ffi::LibNames,
    faults: MaskFaultSwitch,
    denoise_faults: DenoiseFaultSwitch,
    /// The denoise strength, or why the development override is unusable
    /// (reported by a load that asks for the stage).
    denoise_strength: Result<f32, String>,
}

/// Reads the [`DENOISE_STRENGTH_ENV`] override: unset or empty is the
/// default, `0` and `1` are the two strengths, anything else is an error.
fn parse_denoise_strength(value: Option<&std::ffi::OsStr>) -> Result<f32, String> {
    let Some(v) = value.filter(|v| !v.is_empty()) else {
        return Ok(DEFAULT_DENOISE_STRENGTH);
    };
    match v.to_str().map(str::trim) {
        Some("0") => Ok(0.0),
        Some("1") => Ok(1.0),
        _ => Err(format!(
            "{DENOISE_STRENGTH_ENV}={} is not 0 or 1; unset it or set it to 0 (weak) or 1 \
             (strong)",
            v.to_string_lossy()
        )),
    }
}

/// The SDK root from the [`ROOT_ENV`] value: set and non-empty is that
/// root; unset or empty is `default` when it is a directory, else `None`
/// (the library search path only).
fn resolve_root(value: Option<&std::ffi::OsStr>, default: &Path) -> Option<PathBuf> {
    match value.filter(|v| !v.is_empty()) {
        Some(v) => Some(PathBuf::from(v)),
        None => default.is_dir().then(|| default.to_path_buf()),
    }
}

impl MaxineFactory {
    /// Uses `$MAXINE_VFX_ROOT`; unset or empty, [`DEFAULT_ROOT`] when that
    /// directory exists, else the library search path only.
    ///
    /// For development, [`DENOISE_STRENGTH_ENV`] (`0` or `1`) overrides
    /// [`DEFAULT_DENOISE_STRENGTH`]. Any other value makes every load that
    /// asks for the denoise stage fail with [`BackendError::Denoise`] naming
    /// the variable; loads without it are unaffected.
    pub fn from_env() -> MaxineFactory {
        let root = resolve_root(
            std::env::var_os(ROOT_ENV).as_deref(),
            Path::new(DEFAULT_ROOT),
        );
        let mut f = MaxineFactory::with_root(root);
        f.denoise_strength =
            parse_denoise_strength(std::env::var_os(DENOISE_STRENGTH_ENV).as_deref());
        f
    }

    /// Uses `root` as the SDK root (`None`: the library search path only).
    pub fn with_root(root: Option<PathBuf>) -> MaxineFactory {
        MaxineFactory {
            root,
            names: ffi::SDK_LIBS,
            faults: MaskFaultSwitch::default(),
            denoise_faults: DenoiseFaultSwitch::default(),
            denoise_strength: Ok(DEFAULT_DENOISE_STRENGTH),
        }
    }

    /// Sets the strength of the denoise stage of every backend this factory
    /// creates: `0.0` (the SDK's weak setting, more texture kept) or `1.0`
    /// (strong, more noise removed). The SDK has no strength in between, so
    /// any other value is refused with [`BackendError::InvalidInput`]. Fixed
    /// for the backend's life: a loaded stage keeps the strength it was
    /// loaded with.
    pub fn with_denoise_strength(mut self, strength: f32) -> Result<MaxineFactory, BackendError> {
        if strength != 0.0 && strength != 1.0 {
            return Err(BackendError::InvalidInput(format!(
                "denoise strength {strength} is not supported: the Maxine denoise filter takes \
                 0 (weak) or 1 (strong)"
            )));
        }
        self.denoise_strength = Ok(strength);
        Ok(self)
    }

    /// The denoise strength backends of this factory load the stage with,
    /// or the [`BackendError::Denoise`] such a load would fail with.
    pub fn denoise_strength(&self) -> Result<f32, BackendError> {
        self.denoise_strength.clone().map_err(strength_err)
    }

    /// Makes every backend this factory creates read `switch` (tests only).
    #[doc(hidden)]
    pub fn with_mask_faults(mut self, switch: MaskFaultSwitch) -> MaxineFactory {
        self.faults = switch;
        self
    }

    /// Makes every backend this factory creates read `switch` for its denoise
    /// stage (tests only).
    #[doc(hidden)]
    pub fn with_denoise_faults(mut self, switch: DenoiseFaultSwitch) -> MaxineFactory {
        self.denoise_faults = switch;
        self
    }

    /// The SDK root this factory looks in first.
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// The model directory of the SDK, once it has been found.
    pub fn model_dir(&self) -> Result<PathBuf, BackendError> {
        let sdk = self.sdk()?;
        model_dir(sdk)
    }

    /// Creates a concrete, unloaded [`MaxineBackend`] (also available as a
    /// trait object through [`BackendFactory::create`]).
    pub fn create_maxine(&self) -> Result<MaxineBackend, BackendError> {
        let sdk = self.sdk()?;
        let model_dir = model_dir(sdk)?;
        Ok(MaxineBackend {
            sdk,
            model_dir,
            faults: self.faults.clone(),
            denoise_faults: self.denoise_faults.clone(),
            denoise_strength: self.denoise_strength.clone(),
            loaded: None,
            last_timings: Cell::new(None),
        })
    }

    fn sdk(&self) -> Result<&'static Sdk, BackendError> {
        ffi::open(self.root.as_deref(), &self.names).map_err(BackendError::Unavailable)
    }
}

/// The error of a load that asks for the denoise stage when the strength
/// override is unusable.
fn strength_err(message: String) -> BackendError {
    BackendError::Denoise {
        code: ffi::NVCV_ERR_PARAMETER,
        message,
    }
}

fn model_dir(sdk: &Sdk) -> Result<PathBuf, BackendError> {
    let dir = sdk.model_dir.clone().ok_or_else(|| {
        BackendError::Unavailable(
            "the Maxine model directory could not be located (the loader did not report where \
             libVideoFX.so is); set MAXINE_VFX_ROOT (docs/setup/maxine.md, section 4)"
                .into(),
        )
    })?;
    if !dir.is_dir() {
        return Err(BackendError::Unavailable(format!(
            "the Maxine model directory {} does not exist; install the models for this GPU \
             into the SDK root's lib/models (docs/setup/maxine.md, section 7)",
            dir.display()
        )));
    }
    Ok(dir)
}

impl BackendFactory for MaxineFactory {
    fn name(&self) -> &'static str {
        "maxine"
    }

    /// Opens the SDK libraries (once per process) and checks the model
    /// directory exists. Loads no model and creates no CUDA context.
    fn probe(&self) -> Result<(), BackendError> {
        self.model_dir().map(|_| ())
    }

    fn create(&self) -> Result<Box<dyn InferenceBackend>, BackendError> {
        Ok(Box::new(self.create_maxine()?))
    }
}

/// The NvCV colour-space flags for `colour` (matrix, range, chroma
/// co-sited horizontally, as MPEG-2 and GStreamer's NV12 default).
fn colour_flags(colour: ColourSpec) -> u32 {
    let matrix = match colour.matrix {
        ColourMatrix::Bt601 => ffi::NVCV_601,
        ColourMatrix::Bt709 => ffi::NVCV_709,
    };
    let range = if colour.full_range {
        ffi::NVCV_FULL_RANGE
    } else {
        ffi::NVCV_VIDEO_RANGE
    };
    matrix | range | ffi::NVCV_CHROMA_COSITED
}

/// What a failed SDK call means and what to do about it.
fn explain(sdk: &Sdk, call: &str, code: NvCvStatus, model_dir: &Path) -> String {
    let base = format!("{call} returned {code} ({})", sdk.error_string(code));
    match code {
        ffi::NVCV_ERR_UNIMPLEMENTED if call.starts_with("NvVFX_CreateEffect") => format!(
            "{base}: the SDK could not open the effect's feature library \
             ({}). Put the SDK's features/*/lib directories on LD_LIBRARY_PATH before the \
             process starts (docs/setup/maxine.md, section 4)",
            feature_library(call)
        ),
        ffi::NVCV_ERR_CUDA_KERNEL if call.starts_with("NvVFX_Load") => format!(
            "{base}: the SDK rejected this distribution (its model loader reads \
             /etc/os-release). Run the process with the os-release override \
             (docs/setup/maxine.md, section 3)"
        ),
        ffi::NVCV_ERR_FILE => format!(
            "{base}: the models were not found in {}; install the models for this GPU \
             (docs/setup/maxine.md, section 7)",
            model_dir.display()
        ),
        _ => base,
    }
}

/// The feature library of the effect a call (`NvVFX_CreateEffect(<name>)`)
/// names.
fn feature_library(call: &str) -> &'static str {
    if call.contains("(GreenScreen)") {
        "libnvVFXGreenScreen.so"
    } else if call.contains("(BackgroundBlur)") {
        "libnvVFXBackgroundBlur.so"
    } else if call.contains("(Denoising)") {
        "libnvVFXDenoising.so"
    } else {
        "libnvVFXGreenScreen.so / libnvVFXBackgroundBlur.so / libnvVFXDenoising.so"
    }
}

/// An effect instance, destroyed on drop.
struct Effect {
    sdk: &'static Sdk,
    handle: VfxHandle,
    name: &'static str,
}

impl Effect {
    fn create(
        sdk: &'static Sdk,
        selector: &'static std::ffi::CStr,
        model_dir: &Path,
    ) -> Result<Effect, BackendError> {
        // The selectors are the SDK's effect names, and ASCII.
        let name = selector.to_str().unwrap_or("effect");
        let mut handle: VfxHandle = std::ptr::null_mut();
        // SAFETY: `selector` is NUL-terminated and static; `handle` is a
        // valid out-pointer.
        let rc = unsafe { (sdk.NvVFX_CreateEffect)(selector.as_ptr(), &mut handle) };
        if rc != ffi::NVCV_SUCCESS || handle.is_null() {
            return Err(BackendError::Load {
                code: rc,
                message: explain(sdk, &format!("NvVFX_CreateEffect({name})"), rc, model_dir),
            });
        }
        Ok(Effect { sdk, handle, name })
    }

    /// Maps a status of `what` on this effect to a load error.
    fn check(&self, what: &str, rc: NvCvStatus, model_dir: &Path) -> Result<(), BackendError> {
        if rc == ffi::NVCV_SUCCESS {
            return Ok(());
        }
        Err(BackendError::Load {
            code: rc,
            message: explain(self.sdk, &format!("{what}({})", self.name), rc, model_dir),
        })
    }

    fn set_u32(&self, p: &std::ffi::CStr, v: u32, dir: &Path) -> Result<(), BackendError> {
        // SAFETY: live handle; static NUL-terminated parameter name.
        let rc = unsafe { (self.sdk.NvVFX_SetU32)(self.handle, p.as_ptr(), v) };
        self.check(&format!("NvVFX_SetU32 {}", p.to_string_lossy()), rc, dir)
    }

    fn set_f32(&self, p: &std::ffi::CStr, v: f32) -> NvCvStatus {
        // SAFETY: live handle; static NUL-terminated parameter name.
        unsafe { (self.sdk.NvVFX_SetF32)(self.handle, p.as_ptr(), v) }
    }

    fn set_model_dir(&self, dir: &Path) -> Result<(), BackendError> {
        let dir_c = CString::new(dir.as_os_str().as_encoded_bytes()).map_err(|_| {
            BackendError::InvalidInput(format!(
                "model directory {} contains a NUL byte",
                dir.display()
            ))
        })?;
        // SAFETY: live handle; both strings are NUL-terminated (the SDK
        // copies the value).
        let rc = unsafe {
            (self.sdk.NvVFX_SetString)(self.handle, ffi::P_MODEL_DIRECTORY.as_ptr(), dir_c.as_ptr())
        };
        self.check("NvVFX_SetString ModelDir", rc, dir)
    }

    fn set_stream(&self, stream: CuStream, dir: &Path) -> Result<(), BackendError> {
        // SAFETY: live handle; the stream is the caller's, valid while loaded.
        let rc = unsafe {
            (self.sdk.NvVFX_SetCudaStream)(self.handle, ffi::P_CUDA_STREAM.as_ptr(), stream)
        };
        self.check("NvVFX_SetCudaStream", rc, dir)
    }

    /// Binds `image` to parameter `p`. The SDK keeps the pointer, so `image`
    /// must stay at its address (boxed) for as long as the effect lives.
    fn set_image(
        &self,
        p: &std::ffi::CStr,
        image: &mut NvCvImage,
        dir: &Path,
    ) -> Result<(), BackendError> {
        // SAFETY: live handle; `image` is a valid descriptor whose address
        // is stable (callers pass boxed images that outlive the effect).
        let rc = unsafe { (self.sdk.NvVFX_SetImage)(self.handle, p.as_ptr(), image) };
        self.check(&format!("NvVFX_SetImage {}", p.to_string_lossy()), rc, dir)
    }

    fn load(&self, dir: &Path) -> Result<(), BackendError> {
        // SAFETY: live, configured handle.
        let rc = unsafe { (self.sdk.NvVFX_Load)(self.handle) };
        self.check("NvVFX_Load", rc, dir)
    }

    fn run(&self, asynchronous: bool) -> NvCvStatus {
        // SAFETY: live, loaded handle with its images bound.
        unsafe { (self.sdk.NvVFX_Run)(self.handle, i32::from(asynchronous)) }
    }
}

impl Drop for Effect {
    fn drop(&mut self) {
        // SAFETY: the handle was created by NvVFX_CreateEffect and is
        // destroyed once; callers synchronise the stream first.
        unsafe { (self.sdk.NvVFX_DestroyEffect)(self.handle) };
    }
}

/// A GPU image allocated by the SDK (`NvCVImage_Alloc`), boxed so its
/// address is stable for `NvVFX_SetImage`, and freed on drop.
struct GpuImage {
    sdk: &'static Sdk,
    image: Box<NvCvImage>,
}

impl GpuImage {
    /// An image with the pitch the SDK chooses for the GPU.
    fn alloc(
        sdk: &'static Sdk,
        width: u32,
        height: u32,
        format: i32,
    ) -> Result<GpuImage, BackendError> {
        Self::alloc_aligned(sdk, width, height, format, 0)
    }

    /// An image whose rows are exactly `width` pixels apart. The denoise
    /// effect binds only such images (`NvVFX_SetImage` refuses a padded
    /// pitch as an invalid pitch argument).
    fn alloc_tight(
        sdk: &'static Sdk,
        width: u32,
        height: u32,
        format: i32,
    ) -> Result<GpuImage, BackendError> {
        Self::alloc_aligned(sdk, width, height, format, ffi::NVCV_TIGHT_PITCH)
    }

    fn alloc_aligned(
        sdk: &'static Sdk,
        width: u32,
        height: u32,
        format: i32,
        alignment: u32,
    ) -> Result<GpuImage, BackendError> {
        let mut image = Box::new(NvCvImage::empty());
        // SAFETY: `image` is an empty descriptor; alignment 0 lets the SDK
        // choose the GPU pitch, 1 makes it tight.
        let rc = unsafe {
            (sdk.NvCVImage_Alloc)(
                &mut *image,
                width,
                height,
                format,
                ffi::NVCV_U8,
                ffi::NVCV_CHUNKY,
                ffi::NVCV_GPU,
                alignment,
            )
        };
        let img = GpuImage { sdk, image };
        if rc != ffi::NVCV_SUCCESS {
            return Err(BackendError::Load {
                code: rc,
                message: format!(
                    "NvCVImage_Alloc {width}x{height} returned {rc} ({})",
                    sdk.error_string(rc)
                ),
            });
        }
        Ok(img)
    }

    fn ptr(&self) -> u64 {
        self.image.pixels as u64
    }

    fn pitch(&self) -> usize {
        self.image.pitch as usize
    }
}

impl Drop for GpuImage {
    fn drop(&mut self) {
        // SAFETY: the descriptor was filled by NvCVImage_Alloc (or is still
        // empty, which Dealloc accepts) and is freed once.
        unsafe { (self.sdk.NvCVImage_Dealloc)(&mut *self.image) };
    }
}

/// The frame images every chain has, whatever its stages.
struct Frames {
    /// The frame the stages after the conversion read: the conversion
    /// writes it without the denoise stage, the denoise effect with it.
    /// Tightly pitched, so that the denoise stage can be added to a running
    /// chain without rebinding the effects that read it.
    frame_bgr: GpuImage,
    /// What the blur or the composite writes.
    out_bgr: GpuImage,
    /// The SDK's scratch image for the NV12 transfers.
    staging: Box<NvCvImage>,
}

impl Frames {
    fn alloc(sdk: &'static Sdk, width: u32, height: u32) -> Result<Frames, BackendError> {
        Ok(Frames {
            frame_bgr: GpuImage::alloc_tight(sdk, width, height, ffi::NVCV_BGR)?,
            out_bgr: GpuImage::alloc(sdk, width, height, ffi::NVCV_BGR)?,
            staging: Box::new(NvCvImage::empty()),
        })
    }
}

impl Drop for Frames {
    fn drop(&mut self) {
        // SAFETY: the staging image is empty or was grown by the SDK's
        // transfers; it is freed once.
        unsafe { (self.frame_bgr.sdk.NvCVImage_Dealloc)(&mut *self.staging) };
    }
}

/// The green-screen effect with its temporal state, the mask-hardening
/// kernel and its masks. Reads [`Frames::frame_bgr`].
struct Mask {
    // Field order is drop order: the state is released explicitly in Drop
    // before the effect, the images after it.
    effect: Effect,
    state: Box<[StateHandle; 1]>,
    /// The raw mask the green-screen effect writes. Only the kernel reads it
    /// (and `read_mask`): the effects use `hard`.
    mask: GpuImage,
    /// The hardened mask: `matte::HARDENING_LUT` applied to `mask`. The one
    /// mask the composite and the blur effect read.
    hard: GpuImage,
    /// `matte::HARDENING_LUT` in device memory, uploaded once.
    lut: DeviceBuffer,
    kernel: Lut8,
}

impl Drop for Mask {
    fn drop(&mut self) {
        if !self.state[0].is_null() {
            // SAFETY: the state was allocated for this effect, which is still
            // alive, and is released once.
            unsafe { (self.effect.sdk.NvVFX_DeallocateState)(self.effect.handle, self.state[0]) };
        }
    }
}

impl Mask {
    /// Enqueues the hardening of `mask` into `hard` on `stream`, with
    /// thread blocks of `block` (`LUT8_BLOCK` except under a forced fault).
    ///
    /// # Safety
    /// The mask's context is current and `stream` is a live stream of it.
    unsafe fn harden(
        &self,
        width: u32,
        height: u32,
        stream: CuStream,
        block: (u32, u32),
    ) -> Result<(), GpuError> {
        // SAFETY: both masks are live `width x height` SDK allocations with
        // `pitch >= width`, the table is 256 live bytes, all of the current
        // context, and `hard` overlaps neither; the caller guarantees the
        // stream.
        unsafe {
            self.kernel.launch_with_block(
                self.mask.ptr(),
                self.mask.pitch(),
                self.hard.ptr(),
                self.hard.pitch(),
                width,
                height,
                self.lut.ptr(),
                stream,
                block,
            )
        }
    }
}

/// The denoise effect with its temporal state and its input image. Writes
/// [`Frames::frame_bgr`].
struct Denoise {
    // Field order is drop order: the effect, which holds the state and both
    // images, goes first.
    effect: Effect,
    /// The `StateSize` bytes of device memory the effect keeps its temporal
    /// state in. Zero means "no history": the next frame starts a new video.
    state_mem: DeviceBuffer,
    /// The one-entry array of state pointers handed over with
    /// `NvVFX_SetObject`; boxed, because the SDK keeps its address.
    state: Box<[*mut c_void; 1]>,
    /// The converted camera frame: the conversion writes it, the effect
    /// reads it.
    raw_bgr: GpuImage,
}

impl Denoise {
    /// Enqueues zeroing the temporal state on `stream`.
    ///
    /// # Safety
    /// The stage's context is current and `stream` is a live stream of it.
    unsafe fn zero_state(&self, stream: CuStream) -> Result<(), GpuError> {
        let m = &self.state_mem;
        // SAFETY: one row of `width_bytes <= pitch` bytes of a live
        // allocation of the current context; frames that use the state are
        // queued on the same stream, so the fill is ordered with them.
        unsafe { obc_gpu::memset_2d_async(m.ptr(), m.pitch(), 0, m.width_bytes(), 1, stream) }
    }
}

/// The stages the last frame whose work was fully enqueued ran, so that
/// [`InferenceBackend::timings`] knows which of its events it recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stages {
    denoise: bool,
    /// The green screen and the effect (they are loaded together).
    mask: bool,
}

/// The stage boundaries of a frame, as indices into [`Loaded::events`]:
/// before the conversion, after it, after the denoise run, after the mask,
/// after the effect, after the conversion back. A stage that is not loaded
/// records no event, and measures 0.
const EV_START: usize = 0;
const EV_IN: usize = 1;
const EV_DENOISE: usize = 2;
const EV_MASK: usize = 3;
const EV_EFFECT: usize = 4;
const EV_OUT: usize = 5;

/// The stage times of a frame that ran `stages`, from `span(a, b)`, the
/// time between events `a` and `b`; `None` if a span is unavailable.
fn stage_timings(
    stages: Stages,
    span: impl Fn(usize, usize) -> Option<f32>,
) -> Option<StageTimings> {
    let convert_in_ms = span(EV_START, EV_IN)?;
    let mut last = EV_IN;
    let denoise_ms = if stages.denoise {
        last = EV_DENOISE;
        span(EV_IN, EV_DENOISE)?
    } else {
        0.0
    };
    let (mask_ms, effect_ms) = if stages.mask {
        let m = span(last, EV_MASK)?;
        last = EV_EFFECT;
        (m, span(EV_MASK, EV_EFFECT)?)
    } else {
        (0.0, 0.0)
    };
    Some(StageTimings {
        convert_in_ms,
        denoise_ms,
        mask_ms,
        effect_ms,
        convert_out_ms: span(last, EV_OUT)?,
    })
}

/// Everything loaded for one request. Field order is drop order: the blur
/// effect uses the mask's hardened mask and the frame images, the denoise
/// and green-screen effects the frame image, so the frame images go last.
struct Loaded {
    width: u32,
    height: u32,
    colour: ColourSpec,
    stream: CuStream,
    effect: EffectKind,
    blur: Option<(Effect, f32)>,
    background: Option<GpuImage>,
    /// `Some` iff the request asked for the denoise stage (and it has not
    /// failed since).
    denoise: Option<Denoise>,
    /// `Some` iff `effect` is blur or replace.
    mask: Option<Mask>,
    frames: Frames,
    /// Stage boundaries of the last recorded frame (`EV_*`).
    events: [Event; 6],
    /// The stages of the last frame whose events were all recorded; `None`
    /// before the first, and while a frame is being enqueued.
    recorded: Option<Stages>,
}

impl Loaded {
    /// The frame images and events for `req`, with no stage yet.
    fn new(sdk: &'static Sdk, req: &LoadRequest, stream: CuStream) -> Result<Loaded, BackendError> {
        Ok(Loaded {
            width: req.width,
            height: req.height,
            colour: req.colour,
            stream,
            effect: EffectKind::None,
            blur: None,
            background: None,
            denoise: None,
            mask: None,
            frames: Frames::alloc(sdk, req.width, req.height)?,
            events: [
                Event::new()?,
                Event::new()?,
                Event::new()?,
                Event::new()?,
                Event::new()?,
                Event::new()?,
            ],
            recorded: None,
        })
    }
}

/// What [`MaxineBackend::configure`] loaded.
struct Configured {
    /// The wall time of the warm-up runs of the models it loaded, if any.
    warmup_ms: Option<f64>,
    /// Whether it loaded a model (the denoise or the green-screen one).
    loaded_model: bool,
}

/// The Maxine implementation of [`InferenceBackend`].
///
/// Create it with [`MaxineFactory::create_maxine`] or through
/// [`BackendFactory::create`]. Follow the context rules of
/// [`InferenceBackend`]: the request's context current on the calling
/// thread around every call, including drop.
pub struct MaxineBackend {
    sdk: &'static Sdk,
    model_dir: PathBuf,
    faults: MaskFaultSwitch,
    denoise_faults: DenoiseFaultSwitch,
    denoise_strength: Result<f32, String>,
    loaded: Option<Loaded>,
    last_timings: Cell<Option<StageTimings>>,
}

// SAFETY: the backend holds SDK handles, SDK-allocated GPU images, device
// memory, CUDA events and the caller's stream handle. None of them is bound
// to a thread: the SDK and CUDA use the context made current by the caller
// on whichever thread calls. All access goes through `&mut self` (or `&self`
// for the read-only queries), so calls are never concurrent.
unsafe impl Send for MaxineBackend {}

/// Synchronises the stream the backend was loaded with.
fn sync(stream: CuStream) -> Result<(), BackendError> {
    // SAFETY: `stream` is the stream of the load request, which the caller
    // keeps alive while anything is loaded with it (`GpuContext` contract).
    unsafe { obc_gpu::stream_synchronize(stream) }.map_err(Into::into)
}

/// Records `ev` on the stream the backend was loaded with.
fn record(ev: &Event, stream: CuStream) -> Result<(), BackendError> {
    // SAFETY: as in `sync`; the event belongs to the same context.
    unsafe { ev.record(stream) }.map_err(Into::into)
}

fn ms_since(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// Maps a CUDA failure during a load to a load error whose message starts
/// with `what` (its code is the call's `CUresult`); the driver missing
/// altogether stays `Unavailable`.
fn cuda_load_err(e: GpuError, what: &str) -> BackendError {
    match e {
        GpuError::Call { code, .. } => BackendError::Load {
            code,
            message: format!("{what}: {e}"),
        },
        other => other.into(),
    }
}

/// Maps a CUDA failure while setting the mask kernel up (`what`) to a load
/// error, as [`cuda_load_err`].
fn kernel_setup_err(what: &'static str) -> impl Fn(GpuError) -> BackendError {
    move |e| cuda_load_err(e, &format!("the mask-hardening kernel failed while {what}"))
}

/// What a failed synchronisation after the warm-up reports: an asynchronous
/// fault of the green-screen run or of the kernel launch surfaces there, and
/// the two cannot be told apart.
const WARMUP_SYNC_FAILED: &str =
    "the warm-up run (the green screen, then the mask-hardening kernel) failed on the GPU";

/// Turns any failure while loading the denoise stage into
/// [`BackendError::Denoise`], so the caller can tell that the rest of its
/// request would load. The driver missing (or parked) stays `Unavailable`:
/// nothing else would load either.
fn denoise_load_err(e: BackendError) -> BackendError {
    let (code, message) = match e {
        BackendError::Load { code, message }
        | BackendError::Run { code, message }
        | BackendError::Denoise { code, message } => (code, message),
        BackendError::InvalidInput(message) => (ffi::NVCV_ERR_GENERAL, message),
        other @ BackendError::Unavailable(_) => return other,
    };
    BackendError::Denoise {
        code,
        message: format!("the video noise removal could not be loaded: {message}"),
    }
}

impl MaxineBackend {
    /// The model directory passed to the SDK.
    pub fn model_dir(&self) -> &Path {
        &self.model_dir
    }

    /// The effect currently loaded, if any ([`EffectKind::None`]: the
    /// denoise stage alone).
    pub fn loaded_effect(&self) -> Option<EffectKind> {
        self.loaded.as_ref().map(|l| l.effect)
    }

    /// Whether the denoise stage is loaded (it is dropped when a frame's
    /// denoise run fails).
    pub fn denoise_loaded(&self) -> bool {
        self.loaded.as_ref().is_some_and(|l| l.denoise.is_some())
    }

    /// Copies the raw mask of the last processed frame, as the green-screen
    /// effect wrote it, to the host (one byte per pixel, 0 background to 255
    /// foreground), after synchronising the stream. The effects never use
    /// this mask; see [`MaxineBackend::read_hardened_mask`]. A diagnostic for
    /// tests and tools, not for the streaming path.
    pub fn read_mask(&self) -> Result<Vec<u8>, BackendError> {
        self.read_plane(|m| &m.mask)
    }

    /// Copies the hardened mask of the last processed frame, the one the
    /// effects used ([`matte::HARDENING_LUT`] applied to
    /// [`MaxineBackend::read_mask`]), like `read_mask`.
    pub fn read_hardened_mask(&self) -> Result<Vec<u8>, BackendError> {
        self.read_plane(|m| &m.hard)
    }

    /// Fills the hardened mask with `value` and waits, so that a test can
    /// tell a mask the kernel wrote from one it did not touch, whatever the
    /// model sees. Tests only.
    #[doc(hidden)]
    pub fn fill_hardened_mask(&self, value: u8) -> Result<(), BackendError> {
        let l = self.loaded.as_ref().ok_or_else(not_loaded)?;
        let hard = &l.mask.as_ref().ok_or_else(no_mask)?.hard;
        // SAFETY: the hardened mask is a live SDK allocation of `height`
        // rows of `pitch >= width` bytes in the current context; `l.stream`
        // is the load's stream.
        unsafe {
            obc_gpu::memset_2d_async(
                hard.ptr(),
                hard.pitch(),
                value,
                l.width as usize,
                l.height as usize,
                l.stream,
            )
        }?;
        sync(l.stream)
    }

    fn read_plane(&self, pick: fn(&Mask) -> &GpuImage) -> Result<Vec<u8>, BackendError> {
        let l = self.loaded.as_ref().ok_or_else(not_loaded)?;
        let img = pick(l.mask.as_ref().ok_or_else(no_mask)?);
        sync(l.stream)?;
        let (w, h) = (l.width as usize, l.height as usize);
        let mut out = vec![0u8; w * h];
        // SAFETY: both masks are live SDK allocations of `h` rows of
        // `pitch >= w` bytes in the current context.
        unsafe { obc_gpu::download_2d(img.ptr(), img.pitch(), &mut out, w, w, h) }?;
        Ok(out)
    }

    fn check_context(gpu: &GpuContext) -> Result<(), BackendError> {
        let current = obc_gpu::current_context()?;
        if gpu.context.is_null() || current != gpu.context {
            return Err(BackendError::InvalidInput(
                "the request's CUDA context is not current on the calling thread".into(),
            ));
        }
        Ok(())
    }

    /// Checks `req` before any GPU call. Everything but the denoise stage
    /// first, so that [`BackendError::Denoise`] means the rest would load.
    fn validate(req: &LoadRequest) -> Result<(), BackendError> {
        if req.effect == EffectKind::None && !req.denoise {
            return Err(BackendError::InvalidInput(
                "the Maxine backend has nothing to load for effect none without denoise".into(),
            ));
        }
        if req.width == 0
            || req.height == 0
            || !req.width.is_multiple_of(2)
            || !req.height.is_multiple_of(2)
        {
            return Err(BackendError::InvalidInput(format!(
                "frame size {}x{} is not supported: the dimensions must be even and non-zero",
                req.width, req.height
            )));
        }
        if req.effect != EffectKind::None && (req.width < MIN_WIDTH || req.height < MIN_HEIGHT) {
            return Err(BackendError::InvalidInput(format!(
                "frame size {}x{} is not supported: the green screen needs at least \
                 {MIN_WIDTH}x{MIN_HEIGHT}, even dimensions",
                req.width, req.height
            )));
        }
        if req.effect == EffectKind::Replace {
            let bg = req.background.as_ref().ok_or_else(|| {
                BackendError::InvalidInput("replace needs a background image".into())
            })?;
            bg.validate()?;
            if (bg.width, bg.height) != (req.width, req.height) {
                return Err(BackendError::InvalidInput(format!(
                    "the background is {}x{}, the frames are {}x{}; scale it first",
                    bg.width, bg.height, req.width, req.height
                )));
            }
        }
        if req.denoise
            && !((DENOISE_MIN_WIDTH..=DENOISE_MAX_WIDTH).contains(&req.width)
                && (DENOISE_MIN_HEIGHT..=DENOISE_MAX_HEIGHT).contains(&req.height))
        {
            return Err(BackendError::Denoise {
                code: ffi::NVCV_ERR_RESOLUTION,
                message: format!(
                    "the video noise removal does not support {}x{} frames: it takes \
                     {DENOISE_MIN_WIDTH}x{DENOISE_MIN_HEIGHT} up to \
                     {DENOISE_MAX_WIDTH}x{DENOISE_MAX_HEIGHT}",
                    req.width, req.height
                ),
            });
        }
        Ok(())
    }

    /// Creates and loads the denoise effect from a new input image into the
    /// frame image, with its temporal state, then runs it once to absorb the
    /// first-run warm-up and zeroes the state again. Every failure is
    /// [`BackendError::Denoise`] (or `Unavailable`).
    fn load_denoise(
        &self,
        frames: &mut Frames,
        (w, h): (u32, u32),
        stream: CuStream,
        strength: f32,
    ) -> Result<(Denoise, f64), BackendError> {
        self.build_denoise(frames, (w, h), stream, strength)
            .map_err(denoise_load_err)
    }

    fn build_denoise(
        &self,
        frames: &mut Frames,
        (w, h): (u32, u32),
        stream: CuStream,
        strength: f32,
    ) -> Result<(Denoise, f64), BackendError> {
        let sdk = self.sdk;
        let dir = self.model_dir.as_path();
        let mut raw_bgr = GpuImage::alloc_tight(sdk, w, h, ffi::NVCV_BGR)?;
        let selector = match self.denoise_faults.get() {
            Some(DenoiseFault::Load) => FAULT_SELECTOR,
            _ => ffi::FX_DENOISING,
        };
        let effect = Effect::create(sdk, selector, dir)?;
        effect.set_model_dir(dir)?;
        effect.set_stream(stream, dir)?;
        // The strength picks the model, so it is set before the load.
        let rc = effect.set_f32(ffi::P_STRENGTH, strength);
        effect.check("NvVFX_SetF32 Strength", rc, dir)?;
        // The load picks the model for the size of the bound images, so they
        // are bound first. (The effect accepts no maximum input size.)
        effect.set_image(ffi::P_INPUT_IMAGE_0, &mut raw_bgr.image, dir)?;
        effect.set_image(ffi::P_OUTPUT_IMAGE_0, &mut frames.frame_bgr.image, dir)?;
        effect.load(dir)?;
        let mut bytes = 0u32;
        // SAFETY: live, loaded handle; static NUL-terminated parameter name;
        // `bytes` is a valid out-pointer.
        let rc =
            unsafe { (sdk.NvVFX_GetU32)(effect.handle, ffi::P_STATE_SIZE.as_ptr(), &mut bytes) };
        effect.check("NvVFX_GetU32 StateSize", rc, dir)?;
        let state_mem = DeviceBuffer::alloc_pitch(bytes as usize, 1)
            .map_err(|e| cuda_load_err(e, &format!("allocating its {bytes}-byte state")))?;
        let state = Box::new([state_mem.ptr() as *mut c_void]);
        let mut d = Denoise {
            effect,
            state_mem,
            state,
            raw_bgr,
        };
        // SAFETY: as in `Denoise::zero_state`; the stream is the load's.
        unsafe { d.zero_state(stream) }.map_err(|e| cuda_load_err(e, "zeroing its state"))?;
        // SAFETY: live handle; the boxed array (one state pointer) has a
        // stable address for the life of the effect, and the memory it
        // points to lives as long as the effect (field order).
        let rc = unsafe {
            (sdk.NvVFX_SetObject)(
                d.effect.handle,
                ffi::P_STATE.as_ptr(),
                d.state.as_mut_ptr().cast::<c_void>(),
            )
        };
        d.effect.check("NvVFX_SetObject State", rc, dir)?;
        // Warm-up: one synchronous run on a mid-grey frame, then zero the
        // state so the first real frame starts a new video.
        let t1 = Instant::now();
        // SAFETY: the input image is a live allocation of `h` rows of
        // `pitch >= 3w` bytes in the current context.
        unsafe {
            obc_gpu::memset_2d_async(
                d.raw_bgr.ptr(),
                d.raw_bgr.pitch(),
                128,
                w as usize * 3,
                h as usize,
                stream,
            )
        }?;
        let rc = d.effect.run(false);
        d.effect.check("NvVFX_Run (warm-up)", rc, dir)?;
        // SAFETY: as in `sync`.
        unsafe { obc_gpu::stream_synchronize(stream) }
            .map_err(|e| cuda_load_err(e, "its warm-up run failed on the GPU"))?;
        // SAFETY: as above.
        unsafe { d.zero_state(stream) }.map_err(|e| cuda_load_err(e, "zeroing its state"))?;
        Ok((d, ms_since(t1)))
    }

    /// Creates and loads the green-screen effect on the frame image, with
    /// the masks and the hardening kernel, then runs it once to absorb the
    /// first-run warm-up.
    fn load_mask(
        &self,
        frames: &mut Frames,
        (w, h): (u32, u32),
        stream: CuStream,
    ) -> Result<(Mask, f64), BackendError> {
        let sdk = self.sdk;
        let dir = self.model_dir.as_path();
        let mut mask_img = GpuImage::alloc(sdk, w, h, ffi::NVCV_A)?;
        let hard = GpuImage::alloc(sdk, w, h, ffi::NVCV_A)?;
        // The hardening kernel first, before the model: the driver compiles
        // its PTX here, which counts as warm-up.
        let t_kernel = Instant::now();
        let fault = self.faults.get();
        let kernel = match fault {
            Some(MaskFault::ModuleLoad) => Lut8::load_ptx(FAULT_PTX),
            _ => Lut8::load(),
        }
        .map_err(kernel_setup_err("loading"))?;
        let table = match fault {
            Some(MaskFault::ConstantTable(b)) => [b; 256],
            _ => matte::HARDENING_LUT,
        };
        let lut = DeviceBuffer::alloc_pitch(256, 1)
            .and_then(|b| b.upload(&table, 256).map(|()| b))
            .map_err(kernel_setup_err("uploading its table"))?;
        // The upload is synchronous on the legacy default stream, but its DMA
        // may still be in flight when it returns; finish it before `stream`
        // (which may not synchronise with the legacy stream) reads the table.
        // SAFETY: the null stream is always valid in the current context.
        unsafe { obc_gpu::stream_synchronize(std::ptr::null_mut()) }
            .map_err(kernel_setup_err("uploading its table"))?;
        let kernel_ms = ms_since(t_kernel);
        let effect = Effect::create(sdk, ffi::FX_GREEN_SCREEN, dir)?;
        effect.set_model_dir(dir)?;
        effect.set_stream(stream, dir)?;
        effect.set_u32(ffi::P_MODE, 0, dir)?; // quality, chairs as foreground
        effect.set_u32(ffi::P_CUDA_GRAPH, 1, dir)?;
        effect.set_u32(ffi::P_MAX_INPUT_WIDTH, w, dir)?;
        effect.set_u32(ffi::P_MAX_INPUT_HEIGHT, h, dir)?;
        effect.set_u32(ffi::P_MAX_NUMBER_STREAMS, 1, dir)?;
        effect.set_image(ffi::P_INPUT_IMAGE_0, &mut frames.frame_bgr.image, dir)?;
        effect.set_image(ffi::P_OUTPUT_IMAGE_0, &mut mask_img.image, dir)?;
        let mut mask = Mask {
            effect,
            state: Box::new([std::ptr::null_mut()]),
            mask: mask_img,
            hard,
            lut,
            kernel,
        };
        // SAFETY: live handle; the out-pointer is the boxed array's slot.
        let rc = unsafe { (sdk.NvVFX_AllocateState)(mask.effect.handle, &mut mask.state[0]) };
        mask.effect.check("NvVFX_AllocateState", rc, dir)?;
        // SAFETY: live handle; the boxed array (one state) has a stable
        // address for the life of the effect.
        let rc = unsafe {
            (sdk.NvVFX_SetStateObjectHandleArray)(
                mask.effect.handle,
                ffi::P_STATE.as_ptr(),
                mask.state.as_mut_ptr(),
            )
        };
        mask.effect
            .check("NvVFX_SetStateObjectHandleArray", rc, dir)?;
        mask.effect.load(dir)?;
        // Warm-up: one synchronous run on a mid-grey frame and one kernel
        // launch, then reset the temporal state so the first real frame
        // starts clean.
        let t1 = Instant::now();
        // SAFETY: the frame image is a live allocation of `h` rows of
        // `pitch >= 3w` bytes in the current context; the caller has
        // synchronised the stream, so no queued frame uses it.
        unsafe {
            obc_gpu::memset_2d_async(
                frames.frame_bgr.ptr(),
                frames.frame_bgr.pitch(),
                128,
                w as usize * 3,
                h as usize,
                stream,
            )
        }?;
        let rc = mask.effect.run(false);
        mask.effect.check("NvVFX_Run (warm-up)", rc, dir)?;
        // SAFETY: as in `enqueue`.
        unsafe { mask.harden(w, h, stream, LUT8_BLOCK) }
            .map_err(kernel_setup_err("running its warm-up"))?;
        // SAFETY: as in `sync`.
        unsafe { obc_gpu::stream_synchronize(stream) }
            .map_err(|e| cuda_load_err(e, WARMUP_SYNC_FAILED))?;
        // SAFETY: live handle and its own state.
        let rc = unsafe { (sdk.NvVFX_ResetState)(mask.effect.handle, mask.state[0]) };
        mask.effect.check("NvVFX_ResetState", rc, dir)?;
        Ok((mask, kernel_ms + ms_since(t1)))
    }

    /// Creates and loads the blur effect on the frame images and the
    /// hardened mask.
    fn load_blur(
        &self,
        frames: &mut Frames,
        mask: &mut Mask,
        stream: CuStream,
        strength: f32,
    ) -> Result<Effect, BackendError> {
        let dir = self.model_dir.as_path();
        let blur = Effect::create(self.sdk, ffi::FX_BACKGROUND_BLUR, dir)?;
        blur.set_stream(stream, dir)?;
        blur.set_image(ffi::P_INPUT_IMAGE_0, &mut frames.frame_bgr.image, dir)?;
        // The hardened mask, never the raw one.
        blur.set_image(ffi::P_INPUT_IMAGE_1, &mut mask.hard.image, dir)?;
        blur.set_image(ffi::P_OUTPUT_IMAGE_0, &mut frames.out_bgr.image, dir)?;
        let rc = blur.set_f32(ffi::P_STRENGTH, strength);
        blur.check("NvVFX_SetF32 Strength", rc, dir)?;
        blur.load(dir)?;
        Ok(blur)
    }

    /// Uploads `bg` into `slot`: in place when the image there has the same
    /// size, else into a new image that replaces it once filled.
    fn upload_background(
        &self,
        bg: &HostBgr,
        slot: &mut Option<GpuImage>,
    ) -> Result<(), BackendError> {
        let fresh = match slot {
            Some(img) if (img.image.width, img.image.height) == (bg.width, bg.height) => None,
            _ => Some(GpuImage::alloc(
                self.sdk,
                bg.width,
                bg.height,
                ffi::NVCV_BGR,
            )?),
        };
        let img = fresh
            .as_ref()
            .or(slot.as_ref())
            .expect("one of the two exists");
        // SAFETY: the image is a live allocation of `height` rows of
        // `pitch >= 3 * width` bytes; the caller synchronised the stream, so
        // no queued composite reads it.
        unsafe {
            obc_gpu::upload_2d(
                &bg.data,
                bg.stride,
                img.ptr(),
                img.pitch(),
                bg.width as usize * 3,
                bg.height as usize,
            )
        }?;
        if fresh.is_some() {
            *slot = fresh;
        }
        Ok(())
    }

    /// Makes `l`, whose stream is idle, run `req` (already validated; the
    /// denoise stage at `strength` iff `Some`). The new parts are built
    /// first, before anything of `l` changes: the denoise stage (it fails
    /// with [`BackendError::Denoise`]), then the green screen, then the blur
    /// or the background. On failure `l` is as it was. Only then are the
    /// parts the request no longer needs dropped.
    fn configure(
        &self,
        l: &mut Loaded,
        req: &LoadRequest,
        strength: Option<f32>,
    ) -> Result<Configured, BackendError> {
        let (size, stream) = ((l.width, l.height), l.stream);
        let mut warmup_ms: Option<f64> = None;
        let new_denoise = match strength {
            Some(s) if l.denoise.is_none() => {
                let (d, ms) = self.load_denoise(&mut l.frames, size, stream, s)?;
                *warmup_ms.get_or_insert(0.0) += ms;
                Some(d)
            }
            _ => None,
        };
        let needs_mask = req.effect != EffectKind::None;
        let mut new_mask = if needs_mask && l.mask.is_none() {
            let (m, ms) = self.load_mask(&mut l.frames, size, stream)?;
            *warmup_ms.get_or_insert(0.0) += ms;
            Some(m)
        } else {
            None
        };
        let new_blur = if req.effect == EffectKind::Blur && l.blur.is_none() {
            let mask = new_mask
                .as_mut()
                .or(l.mask.as_mut())
                .expect("blur has a mask: loaded above or kept");
            let strength = FrameParams::default().blur_strength;
            Some((
                self.load_blur(&mut l.frames, mask, stream, strength)?,
                strength,
            ))
        } else {
            None
        };
        if req.effect == EffectKind::Replace {
            let bg = req.background.as_ref().expect("validated");
            self.upload_background(bg, &mut l.background)?;
        }
        // Commit; nothing below fails. The parts dropped here have no queued
        // work: the caller synchronised the stream.
        let loaded_model = new_denoise.is_some() || new_mask.is_some();
        if new_denoise.is_some() {
            l.denoise = new_denoise;
        } else if strength.is_none() {
            l.denoise = None;
        }
        match req.effect {
            EffectKind::Blur => {
                if new_blur.is_some() {
                    l.blur = new_blur;
                }
                l.background = None;
            }
            EffectKind::Replace => l.blur = None,
            EffectKind::None => {
                l.blur = None;
                l.background = None;
            }
        }
        // After the blur effect, which reads the hardened mask.
        if new_mask.is_some() {
            l.mask = new_mask;
        } else if !needs_mask {
            l.mask = None;
        }
        l.effect = req.effect;
        Ok(Configured {
            warmup_ms,
            loaded_model,
        })
    }

    fn enqueue(
        &mut self,
        gpu: &GpuContext,
        params: &FrameParams,
        src: &DeviceNv12,
        dst: &DeviceNv12,
    ) -> Result<(), BackendError> {
        let sdk = self.sdk;
        let block = match self.faults.get() {
            Some(MaskFault::Launch) => FAULT_BLOCK,
            _ => LUT8_BLOCK,
        };
        let denoise_fault = self.denoise_faults.get() == Some(DenoiseFault::Run);
        let l = self.loaded.as_mut().ok_or_else(not_loaded)?;
        if gpu.stream != l.stream {
            return Err(BackendError::InvalidInput(
                "process() must use the stream given to load()".into(),
            ));
        }
        for (what, f) in [("source", src), ("destination", dst)] {
            f.validate()?;
            if (f.width, f.height) != (l.width, l.height) {
                return Err(BackendError::InvalidInput(format!(
                    "{what} frame is {}x{}, the backend is loaded for {}x{}",
                    f.width, f.height, l.width, l.height
                )));
            }
        }
        let stream = l.stream;
        let flags = colour_flags(l.colour);
        let run_err = |call: &str, rc: NvCvStatus| BackendError::Run {
            code: rc,
            message: format!("{call} returned {rc} ({})", sdk.error_string(rc)),
        };
        let stages = Stages {
            denoise: l.denoise.is_some(),
            mask: l.mask.is_some(),
        };
        l.recorded = None;
        let Frames {
            frame_bgr,
            out_bgr,
            staging,
        } = &mut l.frames;

        record(&l.events[EV_START], stream)?;
        // With the denoise stage the conversion writes the stage's input,
        // without it the frame image itself: no extra pass when it is off.
        let converted = match l.denoise.as_mut() {
            Some(d) => &mut *d.raw_bgr.image,
            None => &mut *frame_bgr.image,
        };
        // SAFETY: `src` describes live NV12 device memory of the current
        // context (caller's contract), read only; U and V interleave in the UV
        // plane (V one byte after U, two bytes per pair). The destination and
        // staging images are ours.
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
                flags,
                ffi::NVCV_GPU,
                converted,
                std::ptr::null(),
                1.0,
                stream,
                &mut **staging,
            )
        };
        if rc != ffi::NVCV_SUCCESS {
            return Err(run_err("NvCVImage_TransferFromYUV", rc));
        }
        record(&l.events[EV_IN], stream)?;
        if let Some(d) = l.denoise.as_ref() {
            let rc = if denoise_fault {
                ffi::NVCV_ERR_GENERAL
            } else {
                d.effect.run(true)
            };
            if rc != ffi::NVCV_SUCCESS {
                let what = if denoise_fault {
                    "a forced fault failed NvVFX_Run(Denoising)".to_owned()
                } else {
                    format!(
                        "NvVFX_Run(Denoising) returned {rc} ({})",
                        sdk.error_string(rc)
                    )
                };
                // The conversion into the stage's input (and whatever part of
                // the run was queued) must finish before the stage goes.
                let synced = sync(stream);
                l.denoise = None;
                let mut message = format!(
                    "the video noise removal failed and was turned off; the next frame runs \
                     without it: {what}"
                );
                if let Err(e) = synced {
                    message.push_str(&format!(
                        " (waiting for the stream before that failed: {e})"
                    ));
                }
                return Err(BackendError::Denoise { code: rc, message });
            }
            record(&l.events[EV_DENOISE], stream)?;
        }
        let out: &NvCvImage = match l.mask.as_ref() {
            None => &frame_bgr.image,
            Some(m) => {
                let rc = m.effect.run(true);
                if rc != ffi::NVCV_SUCCESS {
                    return Err(run_err("NvVFX_Run(GreenScreen)", rc));
                }
                // SAFETY: the masks, the table and the kernel are ours, in the
                // current context; the effect's run is queued on `stream`
                // before the kernel, which therefore reads the finished mask.
                unsafe { m.harden(l.width, l.height, stream, block) }?;
                record(&l.events[EV_MASK], stream)?;
                match l.effect {
                    EffectKind::Blur => {
                        let (blur, current) = l.blur.as_mut().ok_or_else(not_loaded)?;
                        let strength = params.blur_strength.clamp(0.0, 1.0);
                        if strength != *current {
                            let rc = blur.set_f32(ffi::P_STRENGTH, strength);
                            if rc != ffi::NVCV_SUCCESS {
                                return Err(run_err("NvVFX_SetF32(Strength)", rc));
                            }
                            *current = strength;
                        }
                        let rc = blur.run(true);
                        if rc != ffi::NVCV_SUCCESS {
                            return Err(run_err("NvVFX_Run(BackgroundBlur)", rc));
                        }
                    }
                    EffectKind::Replace => {
                        let bg = l.background.as_ref().ok_or_else(not_loaded)?;
                        // SAFETY: all four images are live GPU allocations of
                        // the same size in the current context.
                        let rc = unsafe {
                            (sdk.NvCVImage_Composite)(
                                &*frame_bgr.image,
                                &*bg.image,
                                // The hardened mask, never the raw one.
                                &*m.hard.image,
                                &mut *out_bgr.image,
                                stream,
                            )
                        };
                        if rc != ffi::NVCV_SUCCESS {
                            return Err(run_err("NvCVImage_Composite", rc));
                        }
                    }
                    EffectKind::None => return Err(not_loaded()),
                }
                record(&l.events[EV_EFFECT], stream)?;
                &out_bgr.image
            }
        };
        // SAFETY: `dst` describes live, writable NV12 device memory of the
        // current context (caller's contract); the source and staging images
        // are ours.
        let rc = unsafe {
            (sdk.NvCVImage_TransferToYUV)(
                out,
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
                &mut **staging,
            )
        };
        if rc != ffi::NVCV_SUCCESS {
            return Err(run_err("NvCVImage_TransferToYUV", rc));
        }
        record(&l.events[EV_OUT], stream)?;
        l.recorded = Some(stages);
        Ok(())
    }
}

fn not_loaded() -> BackendError {
    BackendError::InvalidInput("the Maxine backend has no effect loaded".into())
}

fn no_mask() -> BackendError {
    BackendError::InvalidInput(
        "the Maxine backend has no mask loaded (effect none runs the denoise stage only)".into(),
    )
}

impl InferenceBackend for MaxineBackend {
    fn name(&self) -> &'static str {
        "maxine"
    }

    fn load(&mut self, gpu: &GpuContext, req: &LoadRequest) -> Result<LoadReport, BackendError> {
        Self::check_context(gpu)?;
        Self::validate(req)?;
        let strength = if req.denoise {
            Some(self.denoise_strength.clone().map_err(strength_err)?)
        } else {
            None
        };
        let t0 = Instant::now();
        let reuse = self.loaded.as_ref().is_some_and(|l| {
            (l.width, l.height, l.colour, l.stream)
                == (req.width, req.height, req.colour, gpu.stream)
        });
        let mut loaded = if reuse {
            let l = self.loaded.take().expect("checked above");
            // Queued frames may still read the effects, the background or
            // the frame images.
            if let Err(e) = sync(l.stream) {
                self.loaded = Some(l);
                return Err(e);
            }
            l
        } else {
            self.unload();
            Loaded::new(self.sdk, req, gpu.stream)?
        };
        match self.configure(&mut loaded, req, strength) {
            Ok(c) => {
                self.loaded = Some(loaded);
                Ok(LoadReport {
                    load_ms: ms_since(t0) - c.warmup_ms.unwrap_or(0.0),
                    warmup_ms: c.warmup_ms,
                    reused_model: reuse && !c.loaded_model,
                })
            }
            Err(e) => {
                if reuse {
                    // `configure` changed nothing: the previous chain stays.
                    self.loaded = Some(loaded);
                } else {
                    drop(loaded);
                    self.last_timings.set(None);
                }
                Err(e)
            }
        }
    }

    fn process(
        &mut self,
        gpu: &GpuContext,
        params: &FrameParams,
        src: &DeviceNv12,
        dst: &DeviceNv12,
    ) -> Result<(), BackendError> {
        Self::check_context(gpu)?;
        self.enqueue(gpu, params, src, dst)
    }

    fn unload(&mut self) {
        if let Some(l) = self.loaded.take() {
            // Let queued frames finish before their buffers go away. An error
            // here (a broken context) leaves nothing better to do than free.
            let _ = sync(l.stream);
            drop(l);
        }
        self.last_timings.set(None);
    }

    fn timings(&self) -> Option<StageTimings> {
        let l = self.loaded.as_ref()?;
        let stages = l.recorded?;
        if l.events[EV_OUT].is_complete().unwrap_or(false) {
            let e = &l.events;
            let span = |a: usize, b: usize| Event::elapsed_ms(&e[a], &e[b]).ok();
            if let Some(t) = stage_timings(stages, span) {
                self.last_timings.set(Some(t));
            }
        }
        self.last_timings.get()
    }

    /// Zeroes the denoise state (on the stream) and resets the green
    /// screen's (after waiting for the stream: the SDK does not say which
    /// stream `NvVFX_ResetState` works on, and queued frames use the state).
    fn reset_temporal(&mut self, gpu: &GpuContext) -> Result<(), BackendError> {
        Self::check_context(gpu)?;
        let Some(l) = self.loaded.as_ref() else {
            return Ok(());
        };
        if gpu.stream != l.stream {
            return Err(BackendError::InvalidInput(
                "reset_temporal() must use the stream given to load()".into(),
            ));
        }
        if let Some(d) = l.denoise.as_ref() {
            // SAFETY: the stage is loaded in the current context, on this
            // stream.
            unsafe { d.zero_state(l.stream) }?;
        }
        if let Some(m) = l.mask.as_ref() {
            sync(l.stream)?;
            // SAFETY: live handle and its own state; no queued frame uses it.
            let rc = unsafe { (self.sdk.NvVFX_ResetState)(m.effect.handle, m.state[0]) };
            if rc != ffi::NVCV_SUCCESS {
                return Err(BackendError::Run {
                    code: rc,
                    message: format!(
                        "NvVFX_ResetState(GreenScreen) returned {rc} ({})",
                        self.sdk.error_string(rc)
                    ),
                });
            }
        }
        Ok(())
    }
}

impl Drop for MaxineBackend {
    fn drop(&mut self) {
        self.unload();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn failing(root: Option<PathBuf>) -> MaxineFactory {
        MaxineFactory {
            root,
            names: ffi::LibNames {
                videofx: "libobc-no-such-VideoFX.so",
                nvcvimage: "libobc-no-such-NVCVImage.so",
            },
            ..MaxineFactory::with_root(None)
        }
    }

    #[test]
    fn probe_with_an_empty_root_is_unavailable_with_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        let f = failing(Some(dir.path().to_path_buf()));
        let e = f.probe().unwrap_err();
        let BackendError::Unavailable(m) = &e else {
            panic!("expected Unavailable, got {e:?}");
        };
        assert!(m.contains("SDK was not found"), "{m}");
        assert!(m.contains("The SDK root"), "{m}");
        assert!(m.contains("MAXINE_VFX_ROOT"), "{m}");
        assert!(m.contains(&dir.path().display().to_string()), "{m}");
        assert!(!m.contains("is not set,"), "a root is in use: {m}");
        assert!(m.contains("docs/setup/maxine.md"), "{m}");
        assert!(f.create().is_err());
    }

    #[test]
    fn probe_without_a_root_says_the_variable_is_unset() {
        let e = failing(None).probe().unwrap_err();
        let m = e.to_string();
        assert!(matches!(e, BackendError::Unavailable(_)));
        assert!(m.contains("MAXINE_VFX_ROOT is unset or empty"), "{m}");
        assert!(m.contains(DEFAULT_ROOT), "{m}");
        assert!(m.contains("maxine-vfx-sdk-local"), "{m}");
    }

    #[test]
    fn a_library_that_is_present_but_unloadable_names_ld_library_path() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("lib");
        std::fs::create_dir(&lib).unwrap();
        // Present under the expected name, but not an ELF object.
        std::fs::write(lib.join("libobc-no-such-NVCVImage.so.1"), b"not a library").unwrap();
        let e = failing(Some(dir.path().to_path_buf())).probe().unwrap_err();
        let m = e.to_string();
        assert!(m.contains("could not be loaded"), "{m}");
        assert!(m.contains("LD_LIBRARY_PATH"), "{m}");
    }

    #[test]
    fn with_root_and_name() {
        let f = MaxineFactory::with_root(Some("/opt/vfx".into()));
        assert_eq!(f.root(), Some(Path::new("/opt/vfx")));
        assert_eq!(BackendFactory::name(&f), "maxine");
    }

    #[test]
    fn mask_faults_are_off_unless_armed_from_rust_and_shared_by_clones() {
        let f = MaxineFactory::with_root(None);
        assert_eq!(f.faults.get(), None, "a plain factory forces no fault");
        assert_eq!(MaxineFactory::from_env().faults.get(), None);
        let switch = MaskFaultSwitch::default();
        let f = f.with_mask_faults(switch.clone());
        let copy = f.clone();
        for fault in [
            Some(MaskFault::ModuleLoad),
            Some(MaskFault::Launch),
            Some(MaskFault::ConstantTable(0x00)),
            Some(MaskFault::ConstantTable(0xFF)),
            Some(MaskFault::ConstantTable(0x5A)),
            None,
        ] {
            switch.set(fault);
            assert_eq!(f.faults.get(), fault);
            assert_eq!(copy.faults.get(), fault);
        }
        assert!(
            FAULT_BLOCK.0 * FAULT_BLOCK.1 > 1024,
            "the fault block must be refused"
        );
    }

    #[test]
    fn kernel_setup_failures_fail_the_load() {
        let e = kernel_setup_err("loading")(GpuError::Call {
            call: "cuModuleLoadData",
            code: 222,
            name: "CUDA_ERROR_UNSUPPORTED_PTX_VERSION".into(),
            description: "the provided PTX was compiled with an unsupported toolchain".into(),
        });
        match e {
            BackendError::Load { code, message } => {
                assert_eq!(code, 222);
                assert!(message.contains("mask-hardening kernel"), "{message}");
                assert!(message.contains("cuModuleLoadData"), "{message}");
            }
            other => panic!("expected Load, got {other:?}"),
        }
        assert!(matches!(
            kernel_setup_err("loading")(GpuError::Unavailable("no driver".into())),
            BackendError::Unavailable(_)
        ));
    }

    #[test]
    fn a_failed_warm_up_sync_blames_both_warm_up_steps() {
        let e = cuda_load_err(
            GpuError::Call {
                call: "cuStreamSynchronize",
                code: 700,
                name: "CUDA_ERROR_ILLEGAL_ADDRESS".into(),
                description: "an illegal memory access was encountered".into(),
            },
            WARMUP_SYNC_FAILED,
        );
        match e {
            BackendError::Load { code, message } => {
                assert_eq!(code, 700);
                assert!(message.contains("green screen"), "{message}");
                assert!(message.contains("mask-hardening kernel"), "{message}");
                assert!(message.contains("cuStreamSynchronize"), "{message}");
            }
            other => panic!("expected Load, got {other:?}"),
        }
    }

    #[test]
    fn colour_flags_follow_the_spec() {
        assert_eq!(colour_flags(ColourSpec::LIMITED_BT601), 0x00);
        assert_eq!(colour_flags(ColourSpec::FULL_BT601), 0x04);
        assert_eq!(
            colour_flags(ColourSpec {
                matrix: ColourMatrix::Bt709,
                full_range: false
            }),
            0x01
        );
        assert_eq!(
            colour_flags(ColourSpec {
                matrix: ColourMatrix::Bt709,
                full_range: true
            }),
            0x05
        );
    }

    #[test]
    fn load_requests_are_validated_before_any_gpu_call() {
        let bg = Arc::new(HostBgr {
            width: 1280,
            height: 720,
            stride: 1280 * 3,
            data: vec![0; 1280 * 720 * 3],
        });
        let req = |effect, width, height, background| LoadRequest {
            effect,
            denoise: false,
            width,
            height,
            colour: ColourSpec::LIMITED_BT601,
            background,
        };
        assert!(MaxineBackend::validate(&req(EffectKind::Blur, 1920, 1080, None)).is_ok());
        assert!(MaxineBackend::validate(&req(EffectKind::None, 1920, 1080, None)).is_err());
        assert!(MaxineBackend::validate(&req(EffectKind::Blur, 320, 240, None)).is_err());
        assert!(MaxineBackend::validate(&req(EffectKind::Replace, 1920, 1080, None)).is_err());
        let wrong =
            MaxineBackend::validate(&req(EffectKind::Replace, 1920, 1080, Some(bg.clone())))
                .unwrap_err();
        assert!(wrong.to_string().contains("scale it first"), "{wrong}");
        assert!(MaxineBackend::validate(&req(EffectKind::Replace, 1280, 720, Some(bg))).is_ok());
    }

    fn denoise_req(effect: EffectKind, width: u32, height: u32) -> LoadRequest {
        LoadRequest {
            effect,
            denoise: true,
            width,
            height,
            colour: ColourSpec::LIMITED_BT601,
            background: None,
        }
    }

    #[test]
    fn none_needs_the_denoise_stage_and_skips_the_green_screen_minimum() {
        let off = LoadRequest {
            denoise: false,
            ..denoise_req(EffectKind::None, 1920, 1080)
        };
        let e = MaxineBackend::validate(&off).unwrap_err();
        assert!(matches!(e, BackendError::InvalidInput(_)), "{e:?}");
        assert!(e.to_string().contains("without denoise"), "{e}");
        // Below the green screen's 512x288: fine for denoise alone, not for
        // an effect, with or without denoise.
        assert!(MaxineBackend::validate(&denoise_req(EffectKind::None, 320, 180)).is_ok());
        assert!(MaxineBackend::validate(&denoise_req(EffectKind::None, 80, 80)).is_ok());
        assert!(MaxineBackend::validate(&denoise_req(EffectKind::None, 1920, 1080)).is_ok());
        assert!(MaxineBackend::validate(&denoise_req(EffectKind::Blur, 1920, 1080)).is_ok());
        let e = MaxineBackend::validate(&denoise_req(EffectKind::Blur, 320, 180)).unwrap_err();
        assert!(matches!(e, BackendError::InvalidInput(_)), "{e:?}");
        assert!(e.to_string().contains("green screen"), "{e}");
        let e = MaxineBackend::validate(&denoise_req(EffectKind::None, 321, 180)).unwrap_err();
        assert!(matches!(e, BackendError::InvalidInput(_)), "{e:?}");
    }

    #[test]
    fn a_size_outside_the_denoise_range_is_a_denoise_error_that_names_the_range() {
        for (w, h) in [
            (1920, 1088),
            (2048, 1080),
            (1080, 1920),
            (3840, 2160),
            (78, 80),
            (80, 78),
        ] {
            let e = MaxineBackend::validate(&denoise_req(EffectKind::None, w, h)).unwrap_err();
            let BackendError::Denoise { code, message } = &e else {
                panic!("{w}x{h}: expected Denoise, got {e:?}");
            };
            assert_eq!(*code, ffi::NVCV_ERR_RESOLUTION);
            assert!(message.contains(&format!("{w}x{h}")), "{message}");
            assert!(message.contains("80x80 up to 1920x1080"), "{message}");
        }
        // The same size with an effect: the effect part is fine, so the
        // error is the denoise stage's, and without denoise it loads.
        let e = MaxineBackend::validate(&denoise_req(EffectKind::Blur, 2048, 1152)).unwrap_err();
        assert!(matches!(e, BackendError::Denoise { .. }), "{e:?}");
        let off = LoadRequest {
            denoise: false,
            ..denoise_req(EffectKind::Blur, 2048, 1152)
        };
        assert!(MaxineBackend::validate(&off).is_ok());
        // A replace without a background is the effect's error, not the
        // denoise stage's, even at a size denoise refuses.
        let e = MaxineBackend::validate(&denoise_req(EffectKind::Replace, 2048, 1152)).unwrap_err();
        assert!(matches!(e, BackendError::InvalidInput(_)), "{e:?}");
    }

    #[test]
    fn denoise_strength_is_zero_or_one() {
        let f = MaxineFactory::with_root(None);
        assert_eq!(f.denoise_strength(), Ok(DEFAULT_DENOISE_STRENGTH));
        const { assert!(DEFAULT_DENOISE_STRENGTH == 0.0 || DEFAULT_DENOISE_STRENGTH == 1.0) };
        for s in [0.0f32, 1.0] {
            let f = MaxineFactory::with_root(None)
                .with_denoise_strength(s)
                .unwrap();
            assert_eq!(f.denoise_strength(), Ok(s));
        }
        for s in [0.5f32, -1.0, 2.0, f32::NAN] {
            let Err(e) = MaxineFactory::with_root(None).with_denoise_strength(s) else {
                panic!("strength {s} accepted");
            };
            assert!(matches!(e, BackendError::InvalidInput(_)), "{e:?}");
            assert!(e.to_string().contains("0 (weak) or 1 (strong)"), "{e}");
        }
    }

    #[test]
    fn the_root_is_the_variable_else_the_default_directory_else_none() {
        use std::ffi::OsStr;
        let default = tempfile::tempdir().unwrap();
        let present = default.path();
        let absent = present.join("no-sdk");
        // Set and non-empty: that root, whether or not the default exists.
        for d in [present, absent.as_path()] {
            assert_eq!(
                resolve_root(Some(OsStr::new("/some/sdk")), d),
                Some(PathBuf::from("/some/sdk"))
            );
        }
        // Unset or empty: the default when it is a directory.
        for v in [None, Some(OsStr::new(""))] {
            assert_eq!(resolve_root(v, present), Some(present.to_path_buf()));
            assert_eq!(resolve_root(v, &absent), None);
        }
        // A file at the default path is not an SDK root.
        let file = present.join("file");
        std::fs::write(&file, b"").unwrap();
        assert_eq!(resolve_root(None, &file), None);
        assert_eq!(DEFAULT_ROOT, "/opt/nvidia/maxine/videofx");
    }

    #[test]
    fn the_strength_override_takes_0_or_1_and_names_itself_otherwise() {
        use std::ffi::OsStr;
        assert_eq!(parse_denoise_strength(None), Ok(DEFAULT_DENOISE_STRENGTH));
        assert_eq!(
            parse_denoise_strength(Some(OsStr::new(""))),
            Ok(DEFAULT_DENOISE_STRENGTH)
        );
        assert_eq!(parse_denoise_strength(Some(OsStr::new("0"))), Ok(0.0));
        assert_eq!(parse_denoise_strength(Some(OsStr::new("1"))), Ok(1.0));
        assert_eq!(parse_denoise_strength(Some(OsStr::new(" 1\n"))), Ok(1.0));
        for bad in ["0.5", "strong", "2", "1.0"] {
            let m = parse_denoise_strength(Some(OsStr::new(bad))).unwrap_err();
            assert!(m.contains(DENOISE_STRENGTH_ENV), "{m}");
            assert!(m.contains(bad.trim()), "{m}");
        }
        // An unusable override is a denoise error, so the caller can load
        // without the stage.
        let f = MaxineFactory {
            denoise_strength: parse_denoise_strength(Some(OsStr::new("x"))),
            ..MaxineFactory::with_root(None)
        };
        let e = f.denoise_strength().unwrap_err();
        assert!(matches!(e, BackendError::Denoise { .. }), "{e:?}");
    }

    #[test]
    fn denoise_faults_are_off_unless_armed_from_rust_and_shared_by_clones() {
        let f = MaxineFactory::with_root(None);
        assert_eq!(f.denoise_faults.get(), None);
        assert_eq!(MaxineFactory::from_env().denoise_faults.get(), None);
        let switch = DenoiseFaultSwitch::default();
        let f = f.with_denoise_faults(switch.clone());
        let copy = f.clone();
        for fault in [Some(DenoiseFault::Load), Some(DenoiseFault::Run), None] {
            switch.set(fault);
            assert_eq!(f.denoise_faults.get(), fault);
            assert_eq!(copy.denoise_faults.get(), fault);
        }
        assert_ne!(FAULT_SELECTOR, ffi::FX_DENOISING);
    }

    #[test]
    fn denoise_load_failures_become_denoise_errors_except_a_missing_driver() {
        let e = denoise_load_err(BackendError::Load {
            code: -10,
            message: "NvVFX_Load(Denoising) returned -10".into(),
        });
        match e {
            BackendError::Denoise { code, message } => {
                assert_eq!(code, -10);
                assert!(message.contains("video noise removal"), "{message}");
                assert!(message.contains("NvVFX_Load(Denoising)"), "{message}");
            }
            other => panic!("expected Denoise, got {other:?}"),
        }
        assert!(matches!(
            denoise_load_err(BackendError::InvalidInput("x".into())),
            BackendError::Denoise { .. }
        ));
        assert!(matches!(
            denoise_load_err(BackendError::Run {
                code: 2,
                message: "oom".into()
            }),
            BackendError::Denoise { code: 2, .. }
        ));
        assert!(matches!(
            denoise_load_err(BackendError::Unavailable("parked".into())),
            BackendError::Unavailable(_)
        ));
    }

    #[test]
    fn a_missing_feature_library_is_named_per_effect() {
        assert_eq!(
            feature_library("NvVFX_CreateEffect(Denoising)"),
            "libnvVFXDenoising.so"
        );
        assert_eq!(
            feature_library("NvVFX_CreateEffect(GreenScreen)"),
            "libnvVFXGreenScreen.so"
        );
        assert_eq!(
            feature_library("NvVFX_CreateEffect(BackgroundBlur)"),
            "libnvVFXBackgroundBlur.so"
        );
        assert!(feature_library("NvVFX_CreateEffect(Other)").contains("libnvVFXDenoising.so"));
    }

    #[test]
    fn stages_that_did_not_run_measure_zero() {
        // A fake span: one unit per event index it covers, so each figure shows
        // which events it was measured between.
        let span = |a: usize, b: usize| Some((b - a) as f32);
        let all = stage_timings(
            Stages {
                denoise: true,
                mask: true,
            },
            span,
        )
        .unwrap();
        assert_eq!(
            all,
            StageTimings {
                convert_in_ms: 1.0,
                denoise_ms: 1.0,
                mask_ms: 1.0,
                effect_ms: 1.0,
                convert_out_ms: 1.0,
            }
        );
        let effect_only = stage_timings(
            Stages {
                denoise: false,
                mask: true,
            },
            span,
        )
        .unwrap();
        assert_eq!(effect_only.denoise_ms, 0.0);
        assert_eq!(effect_only.mask_ms, 2.0, "the mask follows the conversion");
        assert_eq!(effect_only.total_ms(), 5.0);
        let denoise_only = stage_timings(
            Stages {
                denoise: true,
                mask: false,
            },
            span,
        )
        .unwrap();
        assert_eq!((denoise_only.mask_ms, denoise_only.effect_ms), (0.0, 0.0));
        assert_eq!(denoise_only.convert_out_ms, 3.0, "out follows the denoise");
        assert_eq!(denoise_only.total_ms(), 5.0);
        let none = stage_timings(
            Stages {
                denoise: false,
                mask: false,
            },
            span,
        )
        .unwrap();
        assert_eq!(none.convert_out_ms, 4.0);
        // A span that cannot be read gives no timings at all.
        assert_eq!(
            stage_timings(
                Stages {
                    denoise: true,
                    mask: true
                },
                |a, b| (a != EV_IN).then_some((b - a) as f32)
            ),
            None
        );
    }
}
