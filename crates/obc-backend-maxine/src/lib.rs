//! NVIDIA Maxine Video Effects backend
//! (`docs/adr/0012-video-effects-and-maxine-backend.md`).
//!
//! The user-installed SDK is opened at run time with `dlopen`, never linked
//! and never vendored. [`MaxineFactory`] finds it (`$MAXINE_VFX_ROOT/lib`,
//! else the library search path) and reports what to do when it cannot;
//! [`MaxineBackend`] implements [`InferenceBackend`] for `blur` and
//! `replace`.
//!
//! Per frame, all on the GPU and on the caller's stream, with no host copy:
//!
//! 1. NV12 to BGR (`NvCVImage_TransferFromYUV`, matrix and range from the
//!    request's [`ColourSpec`]);
//! 2. the `GreenScreen` effect (quality mode, temporal state, CUDA graph)
//!    makes an 8-bit alpha mask;
//! 3. a CUDA kernel ([`obc_gpu::Lut8`]) hardens the mask through a fixed
//!    table into a second mask ([`matte`]): the raw mask lets a faint copy
//!    of the room through, so nothing but the hardened mask is ever used;
//! 4. `blur`: the `BackgroundBlur` effect on frame and hardened mask;
//!    `replace`: `NvCVImage_Composite` of the frame over the background
//!    through the hardened mask;
//! 5. BGR to NV12 into the destination (`NvCVImage_TransferToYUV`).
//!
//! Failing to set the kernel up fails the load ([`BackendError::Load`]);
//! failing to launch it fails the frame ([`BackendError::Run`]). Either way
//! the caller shows its failure output; no path falls back to the raw mask.
//!
//! The process needs the SDK's library directories on `LD_LIBRARY_PATH` and,
//! on a distribution the SDK does not list, the os-release override
//! (`docs/setup/maxine.md`, sections 7 and 8).

use std::cell::Cell;
use std::ffi::{CString, c_void};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
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

/// The smallest frame the green-screen effect accepts.
pub const MIN_WIDTH: u32 = 512;
/// The smallest frame the green-screen effect accepts.
pub const MIN_HEIGHT: u32 = 288;

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

/// Finds the Maxine SDK and creates [`MaxineBackend`] instances.
#[derive(Debug, Clone)]
pub struct MaxineFactory {
    root: Option<PathBuf>,
    names: ffi::LibNames,
    faults: MaskFaultSwitch,
}

impl MaxineFactory {
    /// Uses `$MAXINE_VFX_ROOT` (unset or empty: the library search path only).
    pub fn from_env() -> MaxineFactory {
        let root = std::env::var_os(ROOT_ENV)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        MaxineFactory::with_root(root)
    }

    /// Uses `root` as the SDK root (`None`: the library search path only).
    pub fn with_root(root: Option<PathBuf>) -> MaxineFactory {
        MaxineFactory {
            root,
            names: ffi::SDK_LIBS,
            faults: MaskFaultSwitch::default(),
        }
    }

    /// Makes every backend this factory creates read `switch` (tests only).
    #[doc(hidden)]
    pub fn with_mask_faults(mut self, switch: MaskFaultSwitch) -> MaxineFactory {
        self.faults = switch;
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
            loaded: None,
            last_timings: Cell::new(None),
        })
    }

    fn sdk(&self) -> Result<&'static Sdk, BackendError> {
        ffi::open(self.root.as_deref(), &self.names).map_err(BackendError::Unavailable)
    }
}

fn model_dir(sdk: &Sdk) -> Result<PathBuf, BackendError> {
    let dir = sdk.model_dir.clone().ok_or_else(|| {
        BackendError::Unavailable(
            "the Maxine model directory could not be located (the loader did not report where \
             libVideoFX.so is); set MAXINE_VFX_ROOT (docs/setup/maxine.md)"
                .into(),
        )
    })?;
    if !dir.is_dir() {
        return Err(BackendError::Unavailable(format!(
            "the Maxine model directory {} does not exist; install the models for this GPU \
             into $MAXINE_VFX_ROOT/lib/models (docs/setup/maxine.md)",
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
             (libnvVFXGreenScreen.so / libnvVFXBackgroundBlur.so). Put the SDK's \
             features/*/lib directories on LD_LIBRARY_PATH before the process starts \
             (docs/setup/maxine.md, section 8)"
        ),
        ffi::NVCV_ERR_CUDA_KERNEL if call.starts_with("NvVFX_Load") => format!(
            "{base}: the SDK rejected this distribution (its model loader reads \
             /etc/os-release). Run the process with the os-release override \
             (docs/setup/maxine.md, section 7)"
        ),
        ffi::NVCV_ERR_FILE => format!(
            "{base}: the models were not found in {}; install the models for this GPU \
             (docs/setup/maxine.md, section 10)",
            model_dir.display()
        ),
        _ => base,
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
        let name = if selector == ffi::FX_GREEN_SCREEN {
            "GreenScreen"
        } else {
            "BackgroundBlur"
        };
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
    fn alloc(
        sdk: &'static Sdk,
        width: u32,
        height: u32,
        format: i32,
    ) -> Result<GpuImage, BackendError> {
        let mut image = Box::new(NvCvImage::empty());
        // SAFETY: `image` is an empty descriptor; alignment 0 lets the SDK
        // choose the GPU pitch.
        let rc = unsafe {
            (sdk.NvCVImage_Alloc)(
                &mut *image,
                width,
                height,
                format,
                ffi::NVCV_U8,
                ffi::NVCV_CHUNKY,
                ffi::NVCV_GPU,
                0,
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

/// The green-screen effect with its temporal state, the mask-hardening
/// kernel, and the buffers that depend only on the frame size.
struct Mask {
    // Field order is drop order: the state is released explicitly in Drop
    // before the effect, the images after it.
    effect: Effect,
    state: Box<[StateHandle; 1]>,
    frame_bgr: GpuImage,
    /// The raw mask the green-screen effect writes. Only the kernel reads it
    /// (and `read_mask`): the effects use `hard`.
    mask: GpuImage,
    /// The hardened mask: `matte::HARDENING_LUT` applied to `mask`. The one
    /// mask the composite and the blur effect read.
    hard: GpuImage,
    out_bgr: GpuImage,
    staging: Box<NvCvImage>,
    /// `matte::HARDENING_LUT` in device memory, uploaded once.
    lut: DeviceBuffer,
    kernel: Lut8,
}

impl Drop for Mask {
    fn drop(&mut self) {
        let sdk = self.effect.sdk;
        if !self.state[0].is_null() {
            // SAFETY: the state was allocated for this effect, which is still
            // alive, and is released once.
            unsafe { (sdk.NvVFX_DeallocateState)(self.effect.handle, self.state[0]) };
        }
        // SAFETY: the staging image is empty or was grown by the SDK's
        // transfers; it is freed once.
        unsafe { (sdk.NvCVImage_Dealloc)(&mut *self.staging) };
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

/// Everything loaded for one request. Field order is drop order: the blur
/// effect uses the mask's images (the frame and the hardened mask), so it
/// goes first.
struct Loaded {
    width: u32,
    height: u32,
    colour: ColourSpec,
    stream: CuStream,
    effect: EffectKind,
    blur: Option<(Effect, f32)>,
    background: Option<GpuImage>,
    mask: Mask,
    /// Stage boundaries of the last recorded frame.
    events: [Event; 5],
    recorded: bool,
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
    loaded: Option<Loaded>,
    last_timings: Cell<Option<StageTimings>>,
}

// SAFETY: the backend holds SDK handles, SDK-allocated GPU images, CUDA
// events and the caller's stream handle. None of them is bound to a thread:
// the SDK and CUDA use the context made current by the caller on whichever
// thread calls. All access goes through `&mut self` (or `&self` for the
// read-only timing query), so calls are never concurrent.
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

impl MaxineBackend {
    /// The model directory passed to the SDK.
    pub fn model_dir(&self) -> &Path {
        &self.model_dir
    }

    /// The effect currently loaded, if any.
    pub fn loaded_effect(&self) -> Option<EffectKind> {
        self.loaded.as_ref().map(|l| l.effect)
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
        let hard = &l.mask.hard;
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
        sync(l.stream)?;
        let (w, h) = (l.width as usize, l.height as usize);
        let img = pick(&l.mask);
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

    fn validate(req: &LoadRequest) -> Result<(), BackendError> {
        if req.effect == EffectKind::None {
            return Err(BackendError::InvalidInput(
                "the Maxine backend has nothing to load for effect none".into(),
            ));
        }
        if req.width < MIN_WIDTH
            || req.height < MIN_HEIGHT
            || !req.width.is_multiple_of(2)
            || !req.height.is_multiple_of(2)
        {
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
        Ok(())
    }

    /// Creates and loads the green-screen effect and the size-dependent
    /// buffers, then runs it once to absorb the first-run warm-up.
    fn load_mask(&self, req: &LoadRequest, stream: CuStream) -> Result<(Mask, f64), BackendError> {
        let sdk = self.sdk;
        let dir = self.model_dir.as_path();
        let (w, h) = (req.width, req.height);
        let mut frame_bgr = GpuImage::alloc(sdk, w, h, ffi::NVCV_BGR)?;
        let mut mask_img = GpuImage::alloc(sdk, w, h, ffi::NVCV_A)?;
        let hard = GpuImage::alloc(sdk, w, h, ffi::NVCV_A)?;
        let out_bgr = GpuImage::alloc(sdk, w, h, ffi::NVCV_BGR)?;
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
        let dir_c = CString::new(dir.as_os_str().as_encoded_bytes()).map_err(|_| {
            BackendError::InvalidInput(format!(
                "model directory {} contains a NUL byte",
                dir.display()
            ))
        })?;
        // SAFETY: live handle; both strings are NUL-terminated (the SDK
        // copies the value).
        let rc = unsafe {
            (sdk.NvVFX_SetString)(
                effect.handle,
                ffi::P_MODEL_DIRECTORY.as_ptr(),
                dir_c.as_ptr(),
            )
        };
        effect.check("NvVFX_SetString ModelDir", rc, dir)?;
        effect.set_stream(stream, dir)?;
        effect.set_u32(ffi::P_MODE, 0, dir)?; // quality, chairs as foreground
        effect.set_u32(ffi::P_CUDA_GRAPH, 1, dir)?;
        effect.set_u32(ffi::P_MAX_INPUT_WIDTH, w, dir)?;
        effect.set_u32(ffi::P_MAX_INPUT_HEIGHT, h, dir)?;
        effect.set_u32(ffi::P_MAX_NUMBER_STREAMS, 1, dir)?;
        effect.set_image(ffi::P_INPUT_IMAGE_0, &mut frame_bgr.image, dir)?;
        effect.set_image(ffi::P_OUTPUT_IMAGE_0, &mut mask_img.image, dir)?;
        let mut mask = Mask {
            effect,
            state: Box::new([std::ptr::null_mut()]),
            frame_bgr,
            mask: mask_img,
            hard,
            out_bgr,
            staging: Box::new(NvCvImage::empty()),
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
        // `pitch >= 3w` bytes in the current context.
        unsafe {
            obc_gpu::memset_2d_async(
                mask.frame_bgr.ptr(),
                mask.frame_bgr.pitch(),
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

    /// Creates and loads the blur effect on the mask's buffers.
    fn load_blur(
        &self,
        mask: &mut Mask,
        stream: CuStream,
        strength: f32,
    ) -> Result<Effect, BackendError> {
        let dir = self.model_dir.as_path();
        let blur = Effect::create(self.sdk, ffi::FX_BACKGROUND_BLUR, dir)?;
        blur.set_stream(stream, dir)?;
        blur.set_image(ffi::P_INPUT_IMAGE_0, &mut mask.frame_bgr.image, dir)?;
        // The hardened mask, never the raw one.
        blur.set_image(ffi::P_INPUT_IMAGE_1, &mut mask.hard.image, dir)?;
        blur.set_image(ffi::P_OUTPUT_IMAGE_0, &mut mask.out_bgr.image, dir)?;
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
        let m = &mut l.mask;

        record(&l.events[0], stream)?;
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
                &mut *m.frame_bgr.image,
                std::ptr::null(),
                1.0,
                stream,
                &mut *m.staging,
            )
        };
        if rc != ffi::NVCV_SUCCESS {
            return Err(run_err("NvCVImage_TransferFromYUV", rc));
        }
        record(&l.events[1], stream)?;
        let rc = m.effect.run(true);
        if rc != ffi::NVCV_SUCCESS {
            return Err(run_err("NvVFX_Run(GreenScreen)", rc));
        }
        // SAFETY: the masks, the table and the kernel are ours, in the
        // current context; the effect's run is queued on `stream` before the
        // kernel, which therefore reads the finished mask.
        unsafe { m.harden(l.width, l.height, stream, block) }?;
        record(&l.events[2], stream)?;
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
                // SAFETY: all four images are live GPU allocations of the
                // same size in the current context.
                let rc = unsafe {
                    (sdk.NvCVImage_Composite)(
                        &*m.frame_bgr.image,
                        &*bg.image,
                        // The hardened mask, never the raw one.
                        &*m.hard.image,
                        &mut *m.out_bgr.image,
                        stream,
                    )
                };
                if rc != ffi::NVCV_SUCCESS {
                    return Err(run_err("NvCVImage_Composite", rc));
                }
            }
            EffectKind::None => return Err(not_loaded()),
        }
        record(&l.events[3], stream)?;
        // SAFETY: `dst` describes live, writable NV12 device memory of the
        // current context (caller's contract); the source and staging images
        // are ours.
        let rc = unsafe {
            (sdk.NvCVImage_TransferToYUV)(
                &*m.out_bgr.image,
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
                &mut *m.staging,
            )
        };
        if rc != ffi::NVCV_SUCCESS {
            return Err(run_err("NvCVImage_TransferToYUV", rc));
        }
        record(&l.events[4], stream)?;
        l.recorded = true;
        Ok(())
    }
}

fn not_loaded() -> BackendError {
    BackendError::InvalidInput("the Maxine backend has no effect loaded".into())
}

impl InferenceBackend for MaxineBackend {
    fn name(&self) -> &'static str {
        "maxine"
    }

    fn load(&mut self, gpu: &GpuContext, req: &LoadRequest) -> Result<LoadReport, BackendError> {
        Self::check_context(gpu)?;
        Self::validate(req)?;
        let t0 = Instant::now();
        let reuse = self.loaded.as_ref().is_some_and(|l| {
            (l.width, l.height, l.colour, l.stream)
                == (req.width, req.height, req.colour, gpu.stream)
        });
        let mut warmup_ms = None;
        let mut loaded = if reuse {
            let l = self.loaded.take().expect("checked above");
            // Queued frames may still read the blur effect or the background.
            if let Err(e) = sync(l.stream) {
                self.loaded = Some(l);
                return Err(e);
            }
            l
        } else {
            self.unload();
            let (mask, warm) = self.load_mask(req, gpu.stream)?;
            warmup_ms = Some(warm);
            Loaded {
                width: req.width,
                height: req.height,
                colour: req.colour,
                stream: gpu.stream,
                effect: req.effect,
                blur: None,
                background: None,
                mask,
                events: [
                    Event::new()?,
                    Event::new()?,
                    Event::new()?,
                    Event::new()?,
                    Event::new()?,
                ],
                recorded: false,
            }
        };
        // Build the new part before dropping the old one, so a failed switch
        // on a reused instance leaves the previous effect running.
        let result = (|| -> Result<(), BackendError> {
            match req.effect {
                EffectKind::Blur => {
                    if loaded.blur.is_none() {
                        let strength = FrameParams::default().blur_strength;
                        let blur = self.load_blur(&mut loaded.mask, gpu.stream, strength)?;
                        loaded.blur = Some((blur, strength));
                    }
                    loaded.background = None;
                }
                EffectKind::Replace => {
                    let bg = req.background.as_ref().expect("validated");
                    self.upload_background(bg, &mut loaded.background)?;
                    loaded.blur = None;
                }
                EffectKind::None => unreachable!("validated"),
            }
            loaded.effect = req.effect;
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.loaded = Some(loaded);
                Ok(LoadReport {
                    load_ms: ms_since(t0) - warmup_ms.unwrap_or(0.0),
                    warmup_ms,
                    reused_model: reuse,
                })
            }
            Err(e) => {
                if reuse {
                    // `loaded.effect` is unchanged: the previous effect stays.
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
        if !l.recorded {
            return None;
        }
        if l.events[4].is_complete().unwrap_or(false) {
            let e = &l.events;
            let span = |a: usize, b: usize| Event::elapsed_ms(&e[a], &e[b]).ok();
            if let (Some(convert_in_ms), Some(mask_ms), Some(effect_ms), Some(convert_out_ms)) =
                (span(0, 1), span(1, 2), span(2, 3), span(3, 4))
            {
                self.last_timings.set(Some(StageTimings {
                    convert_in_ms,
                    mask_ms,
                    effect_ms,
                    convert_out_ms,
                }));
            }
        }
        self.last_timings.get()
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
            faults: MaskFaultSwitch::default(),
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
        assert!(m.contains("MAXINE_VFX_ROOT="), "{m}");
        assert!(m.contains(&dir.path().display().to_string()), "{m}");
        assert!(m.contains("docs/setup/maxine.md"), "{m}");
        assert!(f.create().is_err());
    }

    #[test]
    fn probe_without_a_root_says_the_variable_is_unset() {
        let e = failing(None).probe().unwrap_err();
        let m = e.to_string();
        assert!(matches!(e, BackendError::Unavailable(_)));
        assert!(m.contains("MAXINE_VFX_ROOT is not set"), "{m}");
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
}
